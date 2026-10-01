//! Streaming archive codecs with a descriptor-bound, validated staging tree.
use super::fsops::{self, Target, error, name, parse, version};
use super::transfer::Staged;
use super::{CHUNK_BYTES, FileError, Guard, MAX_FILE_BYTES, Result};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};
const MAX_ENTRIES: usize = 10000;
const MAX_NAMES: usize = 8 * 1024 * 1024;
const TIME: Duration = Duration::from_secs(120);
fn budget(guard: &Guard, start: Instant) -> Result<()> {
    guard.check()?;
    if start.elapsed() > TIME {
        return Err("files_resource_limit".into());
    }
    Ok(())
}
fn relative(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(n) => {
                name(n.to_str().ok_or(FileError::from("files_invalid_name"))?)?;
                out.push(n);
            }
            Component::CurDir => {}
            _ => return Err("files_archive_unsafe".into()),
        }
    }
    if out.as_os_str().is_empty()
        || out.as_os_str().as_encoded_bytes().len() > 4096
        || out.components().count() > 64
    {
        return Err("files_archive_unsafe".into());
    }
    Ok(out)
}
fn link_target(parent: &Path, target: &Path) -> Result<PathBuf> {
    if target.is_absolute() {
        return Err("files_archive_unsafe".into());
    }
    let mut out = parent.to_path_buf();
    for c in target.components() {
        match c {
            Component::Normal(n) => {
                name(n.to_str().ok_or(FileError::from("files_invalid_name"))?)?;
                out.push(n);
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return Err("files_archive_unsafe".into());
                }
            }
            _ => return Err("files_archive_unsafe".into()),
        }
    }
    if out.components().count() > 64 {
        return Err("files_archive_unsafe".into());
    }
    Ok(out)
}
struct CappedWriter<'a> {
    inner: &'a mut File,
    count: u64,
}
impl Write for CappedWriter<'_> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if fsops::available(&*self.inner).unwrap_or(0) < b.len() as u64 + 16 * 1024 * 1024 {
            return Err(std::io::Error::from_raw_os_error(libc::ENOSPC));
        }
        if self.count + b.len() as u64 > MAX_FILE_BYTES {
            return Err(std::io::Error::other("archive limit"));
        }
        let n = self.inner.write(b)?;
        self.count += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
struct Counter {
    entries: usize,
    bytes: u64,
    names: usize,
    start: Instant,
    guard: Guard,
    inodes: BTreeMap<(u64, u64), (PathBuf, String)>,
}
impl Counter {
    fn new(guard: &Guard) -> Self {
        Self {
            entries: 0,
            bytes: 0,
            names: 0,
            start: Instant::now(),
            guard: guard.clone(),
            inodes: BTreeMap::new(),
        }
    }
    fn add(&mut self, p: &Path, size: u64) -> Result<()> {
        budget(&self.guard, self.start)?;
        self.entries += 1;
        self.bytes = self
            .bytes
            .checked_add(size)
            .ok_or(FileError::from("files_resource_limit"))?;
        self.names += p.as_os_str().as_encoded_bytes().len();
        if self.entries > MAX_ENTRIES || self.bytes > MAX_FILE_BYTES || self.names > MAX_NAMES {
            return Err("files_resource_limit".into());
        }
        Ok(())
    }
}
fn append<W: Write>(
    builder: &mut tar::Builder<W>,
    parent: &File,
    n: &CString,
    archive_path: &Path,
    c: &mut Counter,
    depth: usize,
) -> Result<()> {
    if depth > 64 {
        return Err("files_resource_limit".into());
    }
    let local = fsops::fd_path(parent).join(n.to_string_lossy().as_ref());
    let m = fs::symlink_metadata(&local).map_err(error)?;
    relative(archive_path)?;
    let previous = if m.is_file() && m.nlink() > 1 {
        c.inodes.get(&(m.dev(), m.ino())).cloned()
    } else {
        None
    };
    c.add(
        archive_path,
        if m.is_file() && previous.is_none() {
            m.len()
        } else {
            0
        },
    )?;
    let mut h = tar::Header::new_gnu();
    h.set_metadata(&m);
    if let Some((p, v)) = previous {
        if version(&m) != v {
            return Err("files_version_conflict".into());
        }
        h.set_size(0);
        h.set_entry_type(tar::EntryType::Link);
        builder
            .append_link(&mut h, archive_path, p)
            .map_err(error)?;
        return Ok(());
    }
    if m.is_dir() {
        let dir = fsops::open_at(parent, n, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        if version(&dir.metadata().map_err(error)?) != version(&m) {
            return Err("files_version_conflict".into());
        }
        h.set_size(0);
        h.set_cksum();
        builder
            .append_data(&mut h, archive_path, std::io::empty())
            .map_err(error)?;
        for child in fs::read_dir(fsops::fd_path(&dir)).map_err(error)? {
            let child = child.map_err(error)?;
            let nm = child.file_name();
            let text = nm.to_str().ok_or(FileError::from("files_invalid_name"))?;
            append(
                builder,
                &dir,
                &name(text)?,
                &archive_path.join(text),
                c,
                depth + 1,
            )?;
        }
        if version(&dir.metadata().map_err(error)?) != version(&m) {
            return Err("files_version_conflict".into());
        }
    } else if m.is_file() {
        let mut f = fsops::open_at(parent, n, libc::O_RDONLY, 0)?;
        if version(&f.metadata().map_err(error)?) != version(&m) {
            return Err("files_version_conflict".into());
        }
        h.set_cksum();
        let mut reader = BudgetReader {
            inner: &mut f,
            start: c.start,
            guard: c.guard.clone(),
        };
        let result = builder.append_data(&mut h, archive_path, &mut reader);
        c.guard.check()?;
        result.map_err(error)?;
        if version(&f.metadata().map_err(error)?) != version(&m) {
            return Err("files_version_conflict".into());
        }
        if m.nlink() > 1 {
            c.inodes.insert(
                (m.dev(), m.ino()),
                (archive_path.to_path_buf(), version(&m)),
            );
        }
    } else if m.file_type().is_symlink() {
        let target = fs::read_link(local).map_err(error)?;
        link_target(archive_path.parent().unwrap_or(Path::new("")), &target)?;
        h.set_size(0);
        h.set_entry_type(tar::EntryType::Symlink);
        builder
            .append_link(&mut h, archive_path, target)
            .map_err(error)?;
    } else {
        return Err("files_invalid_type".into());
    }
    Ok(())
}
struct BudgetReader<R> {
    inner: R,
    start: Instant,
    guard: Guard,
}
impl<R: Read> Read for BudgetReader<R> {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        if self.start.elapsed() > TIME || self.guard.check().is_err() {
            return Err(std::io::Error::other("archive time limit"));
        }
        self.inner.read(b)
    }
}
pub(super) fn compress(p: Value, guard: &Guard) -> Result<Value> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Req {
        path: String,
        expected_version: String,
    }
    let p: Req = parse(p)?;
    let target = Target::new(&p.path)?;
    let meta = target.check(&p.expected_version)?;
    if !meta.is_dir() && !meta.is_file() {
        return Err("files_invalid_type".into());
    }
    let dest = target
        .path
        .with_file_name(format!("{}.tar.gz", target.name.to_string_lossy()));
    let mut stage = Staged::new(
        Target::new(dest.to_str().ok_or(FileError::from("files_invalid_name"))?)?,
        None,
    )?;
    let out = CappedWriter {
        inner: &mut stage.file,
        count: 0,
    };
    let gzip = flate2::write::GzEncoder::new(out, flate2::Compression::default());
    let mut builder = tar::Builder::new(gzip);
    let mut counter = Counter::new(guard);
    append(
        &mut builder,
        &target.parent,
        &target.name,
        Path::new(target.name.to_str().unwrap()),
        &mut counter,
        0,
    )?;
    builder.finish().map_err(error)?;
    builder
        .into_inner()
        .map_err(error)?
        .finish()
        .map_err(error)?;
    target.check(&p.expected_version)?;
    let entry = stage.publish(guard)?;
    Ok(json!({"entry":entry}))
}
#[derive(Clone)]
enum Kind {
    File,
    Dir,
    Sym { raw: PathBuf, target: PathBuf },
    Hard { target: PathBuf },
}
struct Item {
    path: PathBuf,
    kind: Kind,
    mode: u32,
    uid: u32,
    gid: u32,
    size: u64,
}
struct Stage {
    dir: File,
    parent: File,
    name: CString,
    items: BTreeMap<PathBuf, Item>,
    counter: Counter,
}
impl Stage {
    fn new(parent: &File, guard: &Guard) -> Result<Self> {
        let name = name(&format!(".datad-extract-{:032x}", rand::random::<u128>()))?;
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            return Err(error(std::io::Error::last_os_error()));
        }
        let dir = fsops::open_at(parent, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        Ok(Self {
            dir,
            parent: parent.try_clone().map_err(error)?,
            name,
            items: BTreeMap::new(),
            counter: Counter::new(guard),
        })
    }
    fn parent_dir(&self, path: &Path, create: bool) -> Result<File> {
        walk(&self.dir, path.parent().unwrap_or(Path::new("")), create)
    }
    fn add(
        &mut self,
        path: PathBuf,
        kind: Kind,
        attrs: (u32, u32, u32),
        size: u64,
        reader: &mut dyn Read,
    ) -> Result<()> {
        let (mode, uid, gid) = attrs;
        self.counter.add(&path, size)?;
        if self.items.contains_key(&path) {
            return Err("files_archive_unsafe".into());
        }
        for a in path.ancestors().skip(1) {
            if self
                .items
                .get(a)
                .is_some_and(|i| !matches!(i.kind, Kind::Dir))
            {
                return Err("files_archive_unsafe".into());
            }
        }
        if !matches!(kind, Kind::Dir)
            && self
                .items
                .keys()
                .any(|p| p != &path && p.starts_with(&path))
        {
            return Err("files_archive_unsafe".into());
        }
        let parent = self.parent_dir(&path, true)?;
        let nm = name(path.file_name().unwrap().to_str().unwrap())?;
        match &kind {
            Kind::File => {
                let mut f = fsops::open_at(
                    &parent,
                    &nm,
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                    0o600,
                )?;
                copy(
                    reader,
                    &mut f,
                    size,
                    &self.counter.guard,
                    self.counter.start,
                )?;
                f.sync_all().map_err(error)?;
            }
            Kind::Dir => {
                if unsafe { libc::mkdirat(parent.as_raw_fd(), nm.as_ptr(), 0o700) } != 0
                    && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
                {
                    return Err(error(std::io::Error::last_os_error()));
                }
                fsops::open_at(&parent, &nm, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
            }
            _ => {
                if size != 0 {
                    return Err("files_archive_unsafe".into());
                }
            }
        }
        self.items.insert(
            path.clone(),
            Item {
                path,
                kind,
                mode: mode & 0o7777,
                uid,
                gid,
                size,
            },
        );
        Ok(())
    }
    fn resolve_links(&self, path: &Path) -> Result<PathBuf> {
        let mut current = path.to_path_buf();
        let mut seen = BTreeSet::new();
        for _ in 0..64 {
            if !seen.insert(current.clone()) {
                return Err("files_archive_unsafe".into());
            }
            let mut prefix = PathBuf::new();
            let mut next = None;
            for component in current.components() {
                prefix.push(component.as_os_str());
                if let Some(item) = self.items.get(&prefix) {
                    match &item.kind {
                        Kind::Sym { target, .. } | Kind::Hard { target } => {
                            let rest = current.strip_prefix(&prefix).unwrap();
                            next = Some(target.join(rest));
                            break;
                        }
                        _ => {}
                    }
                }
            }
            if let Some(n) = next {
                current = n;
            } else {
                return Ok(current);
            }
        }
        Err("files_archive_unsafe".into())
    }
    fn validate_links(&self) -> Result<()> {
        for i in self.items.values() {
            match &i.kind {
                Kind::Hard { target } => {
                    let mut p = target.clone();
                    let mut seen = BTreeSet::new();
                    loop {
                        if !seen.insert(p.clone()) || seen.len() > 64 {
                            return Err("files_archive_unsafe".into());
                        }
                        match self.items.get(&p).map(|i| &i.kind) {
                            Some(Kind::File) => break,
                            Some(Kind::Hard { target }) => p = target.clone(),
                            _ => return Err("files_archive_unsafe".into()),
                        }
                    }
                }
                Kind::Sym { target, .. } => {
                    self.resolve_links(target)?;
                }

                _ => {}
            }
        }
        Ok(())
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        let local = fsops::fd_path(&self.parent).join(self.name.to_string_lossy().as_ref());
        if let (Ok(a), Ok(b)) = (self.dir.metadata(), fs::symlink_metadata(&local))
            && a.ino() == b.ino()
            && a.dev() == b.dev()
        {
            let _ = cleanup(&self.dir, 0);
            let _ = fsops::unlink(&self.parent, &self.name, true);
            let _ = self.parent.sync_all();
        }
    }
}
fn cleanup(dir: &File, depth: usize) -> Result<()> {
    if depth > 64 {
        return Err("files_resource_limit".into());
    }
    for e in fs::read_dir(fsops::fd_path(dir)).map_err(error)? {
        let e = e.map_err(error)?;
        let n = name(
            e.file_name()
                .to_str()
                .ok_or(FileError::from("files_invalid_name"))?,
        )?;
        let m = fs::symlink_metadata(e.path()).map_err(error)?;
        if m.is_dir() {
            cleanup(
                &fsops::open_at(dir, &n, libc::O_RDONLY | libc::O_DIRECTORY, 0)?,
                depth + 1,
            )?;
        }
        fsops::unlink(dir, &n, m.is_dir())?;
    }
    Ok(())
}
fn walk(root: &File, path: &Path, create: bool) -> Result<File> {
    let mut dir = root.try_clone().map_err(error)?;
    for c in path.components() {
        let Component::Normal(n) = c else {
            return Err("files_archive_unsafe".into());
        };
        let n = name(n.to_str().ok_or(FileError::from("files_invalid_name"))?)?;
        if create
            && unsafe { libc::mkdirat(dir.as_raw_fd(), n.as_ptr(), 0o755) } != 0
            && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
        {
            return Err(error(std::io::Error::last_os_error()));
        }
        dir = fsops::open_at(&dir, &n, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    }
    Ok(dir)
}
fn copy(
    reader: &mut dyn Read,
    writer: &mut File,
    size: u64,
    guard: &Guard,
    start: Instant,
) -> Result<()> {
    let mut buf = vec![0u8; CHUNK_BYTES];
    let mut count = 0;
    loop {
        budget(guard, start)?;
        let n = reader
            .read(&mut buf)
            .map_err(|_| FileError::from("files_archive_invalid"))?;
        if n == 0 {
            break;
        }
        count += n as u64;
        if count > size || count > MAX_FILE_BYTES {
            return Err("files_resource_limit".into());
        }
        if fsops::available(writer)? < (n as u64 + 16 * 1024 * 1024) {
            return Err("files_no_space".into());
        }
        writer.write_all(&buf[..n]).map_err(error)?;
    }
    if count != size {
        return Err("files_archive_invalid".into());
    }
    Ok(())
}
fn extension_path(bytes: &[u8]) -> Result<PathBuf> {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    let text =
        std::str::from_utf8(&bytes[..end]).map_err(|_| FileError::from("files_archive_unsafe"))?;
    Ok(PathBuf::from(text))
}
fn pax(bytes: &[u8]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let mut at = 0;
    while at < bytes.len() {
        let space = bytes[at..]
            .iter()
            .position(|b| *b == b' ')
            .ok_or(FileError::from("files_archive_invalid"))?
            + at;
        let len = std::str::from_utf8(&bytes[at..space])
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .ok_or(FileError::from("files_archive_invalid"))?;
        let end = at
            .checked_add(len)
            .ok_or(FileError::from("files_archive_invalid"))?;
        if len == 0 || end > bytes.len() || end <= space + 1 || bytes[end - 1] != b'\n' {
            return Err("files_archive_invalid".into());
        }
        let text = std::str::from_utf8(&bytes[space + 1..end - 1])
            .map_err(|_| FileError::from("files_archive_invalid"))?;
        let (k, v) = text
            .split_once('=')
            .ok_or(FileError::from("files_archive_invalid"))?;
        if k.starts_with("GNU.sparse") || k.len() > 255 || v.len() > 4096 {
            return Err("files_archive_unsafe".into());
        }
        out.insert(k.to_owned(), v.to_owned());
        at = end;
        if out.len() > 128 {
            return Err("files_resource_limit".into());
        }
    }
    Ok(out)
}
fn drain_tar(reader: &mut dyn Read, stage: &Stage) -> Result<()> {
    let mut buf = vec![0u8; CHUNK_BYTES];
    let mut total = 0;
    loop {
        budget(&stage.counter.guard, stage.counter.start)?;
        let n = reader
            .read(&mut buf)
            .map_err(|_| FileError::from("files_archive_invalid"))?;
        if n == 0 {
            return Ok(());
        }
        total += n;
        if total > 64 * 1024 * 1024 {
            return Err("files_resource_limit".into());
        }
        if buf[..n].iter().any(|b| *b != 0) {
            return Err("files_archive_invalid".into());
        }
    }
}
fn tar_decode(mut reader: Box<dyn Read>, stage: &mut Stage) -> Result<()> {
    let mut global = BTreeMap::new();
    let mut local = BTreeMap::new();
    let mut longname = None;
    let mut longlink = None;
    let mut physical = 0;
    loop {
        budget(&stage.counter.guard, stage.counter.start)?;
        let mut block = [0u8; 512];
        let mut filled = 0;
        while filled < 512 {
            let n = reader
                .read(&mut block[filled..])
                .map_err(|_| FileError::from("files_archive_invalid"))?;
            if n == 0 {
                if filled == 0 {
                    return Ok(());
                }
                return Err("files_archive_invalid".into());
            }
            filled += n;
        }
        if block.iter().all(|b| *b == 0) {
            return drain_tar(&mut reader, stage);
        }
        physical += 1;
        if physical > MAX_ENTRIES * 4 {
            return Err("files_resource_limit".into());
        }
        let h = tar::Header::from_byte_slice(&block);
        let mut sum = 0u32;
        for (i, b) in block.iter().enumerate() {
            sum += if (148..156).contains(&i) {
                b' ' as u32
            } else {
                *b as u32
            };
        }
        if h.cksum()
            .map_err(|_| FileError::from("files_archive_invalid"))?
            != sum
        {
            return Err("files_archive_invalid".into());
        }
        let ty = h.entry_type();
        let size = h
            .size()
            .map_err(|_| FileError::from("files_archive_invalid"))?;
        if ty.is_gnu_longname()
            || ty.is_gnu_longlink()
            || ty.is_pax_local_extensions()
            || ty.is_pax_global_extensions()
        {
            if size > 8192 {
                return Err("files_resource_limit".into());
            }
            let mut bytes = vec![0u8; size as usize];
            reader
                .read_exact(&mut bytes)
                .map_err(|_| FileError::from("files_archive_invalid"))?;
            if ty.is_gnu_longname() {
                if longname.is_some() {
                    return Err("files_archive_invalid".into());
                }
                longname = Some(extension_path(&bytes)?);
            } else if ty.is_gnu_longlink() {
                if longlink.is_some() {
                    return Err("files_archive_invalid".into());
                }
                longlink = Some(extension_path(&bytes)?);
            } else {
                let values = pax(&bytes)?;
                if ty.is_pax_global_extensions() {
                    if values.contains_key("path") || values.contains_key("linkpath") {
                        return Err("files_archive_unsafe".into());
                    }
                    global.extend(values);
                    if global.len() > 128 {
                        return Err("files_resource_limit".into());
                    }
                } else {
                    if !local.is_empty() {
                        return Err("files_archive_invalid".into());
                    }
                    local = values;
                }
            }
        } else {
            let mut attrs = global.clone();
            attrs.append(&mut local);
            let p = relative(&if let Some(p) = attrs.get("path") {
                PathBuf::from(p)
            } else if let Some(p) = longname.take() {
                p
            } else {
                h.path()
                    .map_err(|_| FileError::from("files_archive_unsafe"))?
                    .into_owned()
            })?;
            let link = if let Some(p) = attrs.get("linkpath") {
                Some(PathBuf::from(p))
            } else if let Some(p) = longlink.take() {
                Some(p)
            } else {
                h.link_name()
                    .map_err(|_| FileError::from("files_archive_unsafe"))?
                    .map(|p| p.into_owned())
            };
            let kind = if ty.is_file() {
                Kind::File
            } else if ty.is_dir() {
                Kind::Dir
            } else if ty.is_symlink() {
                let raw = link.ok_or(FileError::from("files_archive_unsafe"))?;
                let target = link_target(p.parent().unwrap_or(Path::new("")), &raw)?;
                Kind::Sym { raw, target }
            } else if ty.is_hard_link() {
                Kind::Hard {
                    target: relative(&link.ok_or(FileError::from("files_archive_unsafe"))?)?,
                }
            } else {
                return Err("files_archive_unsafe".into());
            };
            if attrs
                .get("size")
                .is_some_and(|s| s.parse::<u64>().ok() != Some(size))
            {
                return Err("files_archive_invalid".into());
            }
            let number = |key: &str, original: std::io::Result<u64>| -> Result<u32> {
                let n = if let Some(s) = attrs.get(key) {
                    s.parse::<u64>()
                        .map_err(|_| FileError::from("files_archive_invalid"))?
                } else {
                    original.map_err(|_| FileError::from("files_archive_invalid"))?
                };
                u32::try_from(n).map_err(|_| "files_archive_invalid".into())
            };
            let uid = number("uid", h.uid())?;
            let gid = number("gid", h.gid())?;
            let mode = h
                .mode()
                .map_err(|_| FileError::from("files_archive_invalid"))?;
            longname = None;
            longlink = None;
            let mut body = (&mut reader).take(size);
            stage.add(p, kind, (mode, uid, gid), size, &mut body)?;
            if body.limit() != 0 {
                return Err("files_archive_invalid".into());
            }
        }
        let pad = (512 - size % 512) % 512;
        let mut padding = [0u8; 511];
        reader
            .read_exact(&mut padding[..pad as usize])
            .map_err(|_| FileError::from("files_archive_invalid"))?;
    }
}

fn zip_preflight(file: &mut File) -> Result<()> {
    let size = file.metadata().map_err(error)?.len();
    let n = size.min(65557) as usize;
    file.seek(SeekFrom::End(-(n as i64))).map_err(error)?;
    let mut tail = vec![0u8; n];
    file.read_exact(&mut tail).map_err(error)?;
    let i = (0..n.saturating_sub(21))
        .rev()
        .find(|i| {
            tail[*i..*i + 4] == [0x50, 0x4b, 0x05, 0x06]
                && *i + 22 + u16::from_le_bytes([tail[*i + 20], tail[*i + 21]]) as usize == n
        })
        .ok_or(FileError::from("files_archive_invalid"))?;
    let u16at = |a| u16::from_le_bytes([tail[i + a], tail[i + a + 1]]);
    let u32at = |a| {
        u32::from_le_bytes([
            tail[i + a],
            tail[i + a + 1],
            tail[i + a + 2],
            tail[i + a + 3],
        ])
    };
    if u16at(4) != 0
        || u16at(6) != 0
        || u16at(8) != u16at(10)
        || u16at(10) as usize > MAX_ENTRIES
        || u32at(12) > 16 * 1024 * 1024
        || u32at(16) as u64 + u32at(12) as u64 > size
    {
        return Err("files_resource_limit".into());
    }
    file.seek(SeekFrom::Start(0)).map_err(error)?;
    Ok(())
}
struct LzmaOutput<'a> {
    file: &'a mut File,
    size: u64,
    count: u64,
    crc: crc32fast::Hasher,
    guard: &'a Guard,
    start: Instant,
}
impl Write for LzmaOutput<'_> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if self.guard.check().is_err() || self.start.elapsed() > TIME {
            return Err(std::io::Error::other("cancelled"));
        }
        if self.count + b.len() as u64 > self.size {
            return Err(std::io::Error::from_raw_os_error(libc::EFBIG));
        }
        if fsops::available(self.file).unwrap_or(0) < b.len() as u64 + 16 * 1024 * 1024 {
            return Err(std::io::Error::from_raw_os_error(libc::ENOSPC));
        }
        let n = self.file.write(b)?;
        self.count += n as u64;
        self.crc.update(&b[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
fn zip_lzma(mut raw: zip::read::ZipFile<'_>, stage: &mut Stage) -> Result<()> {
    let path = relative(Path::new(raw.name()))?;
    let size = raw.size();
    if size > MAX_FILE_BYTES {
        return Err("files_resource_limit".into());
    }
    if raw.is_dir() || raw.is_symlink() {
        return Err("files_archive_unsafe".into());
    }
    let mode = raw.unix_mode().unwrap_or(0o644);
    let expected_crc = raw.crc32();
    let mut header = [0u8; 9];
    raw.read_exact(&mut header)
        .map_err(|_| FileError::from("files_archive_invalid"))?;
    if u16::from_le_bytes([header[2], header[3]]) != 5 {
        return Err("files_archive_invalid".into());
    }
    let dict = u32::from_le_bytes([header[5], header[6], header[7], header[8]]);
    if dict > 64 * 1024 * 1024 {
        return Err("files_resource_limit".into());
    }
    let nm = name(&format!(".decoded-{:032x}", rand::random::<u128>()))?;
    let mut file = fsops::open_at(
        &stage.dir,
        &nm,
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )?;
    let result = (|| {
        let prefix = std::io::Cursor::new(header[4..].to_vec());
        let reader = BudgetReader {
            inner: prefix.chain(&mut raw),
            start: stage.counter.start,
            guard: stage.counter.guard.clone(),
        };
        let mut reader = std::io::BufReader::new(reader);
        let mut writer = LzmaOutput {
            file: &mut file,
            size,
            count: 0,
            crc: crc32fast::Hasher::new(),
            guard: &stage.counter.guard,
            start: stage.counter.start,
        };
        let options = lzma_rs::decompress::Options {
            unpacked_size: lzma_rs::decompress::UnpackedSize::UseProvided(Some(size)),
            memlimit: Some(64 * 1024 * 1024),
            allow_incomplete: false,
        };
        let decoded = lzma_rs::lzma_decompress_with_options(&mut reader, &mut writer, &options);
        stage.counter.guard.check()?;
        match decoded {
            Ok(()) => {}
            Err(lzma_rs::error::Error::IoError(e)) => return Err(error(e)),
            Err(_) => return Err("files_archive_invalid".into()),
        };
        if writer.count != size || writer.crc.finalize() != expected_crc {
            return Err("files_archive_invalid".into());
        }
        file.seek(SeekFrom::Start(0)).map_err(error)?;
        stage.add(
            path,
            Kind::File,
            (mode, unsafe { libc::geteuid() }, unsafe { libc::getegid() }),
            size,
            &mut file,
        )
    })();
    let _ = fsops::unlink(&stage.dir, &nm, false);
    result
}
fn zip_decode(mut file: File, stage: &mut Stage) -> Result<()> {
    zip_preflight(&mut file)?;
    let mut a = zip::ZipArchive::new(file).map_err(|_| FileError::from("files_archive_invalid"))?;
    if a.len() > MAX_ENTRIES {
        return Err("files_resource_limit".into());
    }
    for idx in 0..a.len() {
        let raw = a
            .by_index_raw(idx)
            .map_err(|_| FileError::from("files_archive_invalid"))?;
        if raw.encrypted() {
            return Err("files_archive_unsupported".into());
        }
        if raw.compression() == zip::CompressionMethod::LZMA {
            zip_lzma(raw, stage)?;
            continue;
        }
        drop(raw);
        let mut e = a
            .by_index(idx)
            .map_err(|_| FileError::from("files_archive_invalid"))?;
        let p = relative(Path::new(e.name()))?;
        let mode = e
            .unix_mode()
            .unwrap_or(if e.is_dir() { 0o755 } else { 0o644 });
        if e.is_symlink() {
            if e.size() > 4096 {
                return Err("files_archive_unsafe".into());
            }
            let mut bytes = Vec::new();
            (&mut e)
                .take(4097)
                .read_to_end(&mut bytes)
                .map_err(|_| FileError::from("files_archive_invalid"))?;
            if bytes.len() > 4096 {
                return Err("files_archive_unsafe".into());
            }
            let raw = PathBuf::from(
                String::from_utf8(bytes).map_err(|_| FileError::from("files_archive_unsafe"))?,
            );
            let target = link_target(p.parent().unwrap_or(Path::new("")), &raw)?;
            stage.add(
                p,
                Kind::Sym { raw, target },
                (mode, unsafe { libc::geteuid() }, unsafe { libc::getegid() }),
                0,
                &mut std::io::empty(),
            )?;
        } else {
            let kind = if e.is_dir() {
                Kind::Dir
            } else if mode & 0o170000 == 0 || mode & 0o170000 == 0o100000 {
                Kind::File
            } else {
                return Err("files_archive_unsafe".into());
            };
            let size = e.size();
            stage.add(
                p,
                kind,
                (mode, unsafe { libc::geteuid() }, unsafe { libc::getegid() }),
                size,
                &mut e,
            )?;
        }
    }
    Ok(())
}
fn binding(dest: &File, base: &Path, p: &Path, create: bool) -> Result<Target> {
    let parent = walk(dest, p.parent().unwrap_or(Path::new("")), create)?;
    Ok(Target {
        path: base.join(p),
        parent,
        name: name(p.file_name().unwrap().to_str().unwrap())?,
    })
}
fn hard_regular(stage: &Stage, p: &Path) -> Result<PathBuf> {
    let mut p = p.to_path_buf();
    for _ in 0..64 {
        match stage.items.get(&p).map(|i| &i.kind) {
            Some(Kind::File) => return Ok(p),
            Some(Kind::Hard { target }) => p = target.clone(),
            _ => return Err("files_archive_unsafe".into()),
        }
    }
    Err("files_archive_unsafe".into())
}
fn publish(stage: &Stage, dest: &File, base: &Path, overwrite: bool) -> Result<()> {
    stage.counter.guard.check()?;
    // Preflight the entire destination before any publication. Paths are walked
    // under the bound root and never canonicalized through archive-created links.
    let mut expected = BTreeMap::<PathBuf, Option<String>>::new();
    for i in stage.items.values() {
        budget(&stage.counter.guard, stage.counter.start)?;
        let t = match binding(dest, base, &i.path, false) {
            Ok(t) => Some(t),
            Err(e) if e.code == "files_not_found" => None,
            Err(e) => return Err(e),
        };
        let old = if let Some(t) = t {
            match t.metadata() {
                Ok(m) => {
                    if matches!(i.kind, Kind::Dir) {
                        if !m.is_dir() {
                            return Err("files_exists".into());
                        }
                    } else if !overwrite {
                        return Err("files_exists".into());
                    } else if !m.is_file() && !m.file_type().is_symlink() {
                        return Err("files_invalid_type".into());
                    }
                    Some(version(&m))
                }
                Err(e) if e.code == "files_not_found" => None,
                Err(e) => return Err(e),
            }
        } else {
            None
        };
        expected.insert(i.path.clone(), old);
    }
    for i in stage.items.values().filter(|i| matches!(i.kind, Kind::Dir)) {
        stage.counter.guard.check()?;
        let parent = walk(dest, i.path.parent().unwrap_or(Path::new("")), true)?;
        let n = name(i.path.file_name().unwrap().to_str().unwrap())?;
        if unsafe { libc::mkdirat(parent.as_raw_fd(), n.as_ptr(), 0o755) } != 0
            && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
        {
            return Err(error(std::io::Error::last_os_error()));
        }
        walk(dest, &i.path, false)?;
    }
    for i in stage
        .items
        .values()
        .filter(|i| matches!(i.kind, Kind::File))
    {
        budget(&stage.counter.guard, stage.counter.start)?;
        let t = binding(dest, base, &i.path, true)?;
        let mut out = Staged::new(t, expected[&i.path].clone())?;
        let parent = stage.parent_dir(&i.path, false)?;
        let mut source = fsops::open_at(
            &parent,
            &name(i.path.file_name().unwrap().to_str().unwrap())?,
            libc::O_RDONLY,
            0,
        )?;
        copy(
            &mut source,
            &mut out.file,
            i.size,
            &stage.counter.guard,
            stage.counter.start,
        )?;
        if out.original.is_none() {
            out.new_attrs = Some((i.mode, i.uid, i.gid));
        }
        out.publish(&stage.counter.guard)?;
    }
    for i in stage
        .items
        .values()
        .filter(|i| matches!(i.kind, Kind::Sym { .. } | Kind::Hard { .. }))
    {
        budget(&stage.counter.guard, stage.counter.start)?;
        let t = binding(dest, base, &i.path, true)?;
        let old = expected[&i.path].as_deref();
        check_expected(&t, old)?;
        let temp = name(&format!(".datad-link-{:032x}", rand::random::<u128>()))?;
        match &i.kind {
            Kind::Sym { raw, target } => {
                let resolved = stage.resolve_links(target)?;
                if !resolved.as_os_str().is_empty() {
                    match binding(dest, base, &resolved, false) {
                        Ok(t) => match t.metadata() {
                            Ok(m) if m.file_type().is_symlink() => {
                                return Err("files_archive_unsafe".into());
                            }
                            Ok(_) => {}
                            Err(e) if e.code == "files_not_found" => {}
                            Err(e) => return Err(e),
                        },
                        Err(e) if e.code == "files_not_found" => {}
                        Err(e) => return Err(e),
                    }
                }
                let raw = CString::new(raw.as_os_str().as_encoded_bytes())
                    .map_err(|_| FileError::from("files_archive_unsafe"))?;
                if unsafe { libc::symlinkat(raw.as_ptr(), t.parent.as_raw_fd(), temp.as_ptr()) }
                    != 0
                {
                    return Err(error(std::io::Error::last_os_error()));
                }
            }
            Kind::Hard { target } => {
                let regular = hard_regular(stage, target)?;
                let source = binding(dest, base, &regular, false)?;
                let m = source.metadata()?;
                if !m.is_file() {
                    return Err("files_archive_unsafe".into());
                }
                let source_fd = source.open(libc::O_RDONLY, 0)?;
                let bound = source_fd.metadata().map_err(error)?;
                if version(&bound) != version(&m) {
                    return Err("files_version_conflict".into());
                }
                if unsafe {
                    libc::linkat(
                        source.parent.as_raw_fd(),
                        source.name.as_ptr(),
                        t.parent.as_raw_fd(),
                        temp.as_ptr(),
                        0,
                    )
                } != 0
                {
                    return Err(error(std::io::Error::last_os_error()));
                }
                let linked = fs::symlink_metadata(
                    fsops::fd_path(&t.parent).join(temp.to_string_lossy().as_ref()),
                )
                .map_err(error)?;
                if !linked.is_file() || linked.ino() != bound.ino() || linked.dev() != bound.dev() {
                    let _ = fsops::unlink(&t.parent, &temp, false);
                    return Err("files_archive_unsafe".into());
                }
            }
            _ => unreachable!(),
        };
        let result = (|| {
            stage.counter.guard.check()?;
            check_expected(&t, old)?;
            fsops::rename_at(&t.parent, &temp, &t.name, old.is_some())
        })();
        if result.is_err() {
            let _ = fsops::unlink(&t.parent, &temp, false);
        }
        result?;
    }
    // Apply directory attributes last so read-only archived modes never prevent
    // completing child extraction. Existing directory ownership/modes are retained.
    for i in stage
        .items
        .values()
        .filter(|i| matches!(i.kind, Kind::Dir))
        .rev()
    {
        stage.counter.guard.check()?;
        if expected[&i.path].is_none() {
            set_attrs(&walk(dest, &i.path, false)?, i)?;
        }
    }
    dest.sync_all().map_err(error)
}
fn check_expected(t: &Target, expected: Option<&str>) -> Result<()> {
    if let Some(v) = expected {
        t.check(v)?;
    } else {
        match t.metadata() {
            Ok(_) => return Err("files_exists".into()),
            Err(e) if e.code == "files_not_found" => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
fn set_attrs(file: &File, i: &Item) -> Result<()> {
    let m = file.metadata().map_err(error)?;
    if (m.uid() != i.uid || m.gid() != i.gid)
        && unsafe { libc::fchown(file.as_raw_fd(), i.uid, i.gid) } != 0
    {
        return Err(error(std::io::Error::last_os_error()));
    }
    file.set_permissions(fs::Permissions::from_mode(i.mode))
        .map_err(error)?;
    file.sync_all().map_err(error)
}
pub(super) fn extract(p: Value, guard: &Guard) -> Result<Value> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Req {
        path: String,
        destination: String,
        expected_version: String,
        overwrite: bool,
    }
    let p: Req = parse(p)?;
    let source = Target::new(&p.path)?;
    let m = source.check(&p.expected_version)?;
    if !m.is_file() || m.len() > MAX_FILE_BYTES {
        return Err("files_invalid_type".into());
    }
    let input = source.open(libc::O_RDONLY, 0)?;
    if version(&input.metadata().map_err(error)?) != p.expected_version {
        return Err("files_version_conflict".into());
    }
    let base = fs::canonicalize(fsops::path(&p.destination)?).map_err(error)?;
    let dest = fsops::directory(&base)?;
    let mut stage = Stage::new(&dest, guard)?;
    let n = source.name.to_string_lossy().to_ascii_lowercase();
    if n.ends_with(".zip") {
        zip_decode(input, &mut stage)?;
    } else {
        let read: Box<dyn Read> = if n.ends_with(".tar.gz") || n.ends_with(".tgz") {
            Box::new(flate2::read::MultiGzDecoder::new(input))
        } else if n.ends_with(".tar.bz2") || n.ends_with(".tbz2") {
            Box::new(bzip2::read::MultiBzDecoder::new(input))
        } else if n.ends_with(".tar.xz") || n.ends_with(".txz") {
            let stream = xz2::stream::Stream::new_stream_decoder(64 * 1024 * 1024, 0)
                .map_err(|_| FileError::from("files_archive_invalid"))?;
            Box::new(xz2::read::XzDecoder::new_stream(input, stream))
        } else if n.ends_with(".tar") {
            Box::new(input)
        } else {
            return Err("files_archive_unsupported".into());
        };
        tar_decode(read, &mut stage)?;
    }
    stage.validate_links()?;
    source.check(&p.expected_version)?;
    publish(&stage, &dest, &base, p.overwrite)?;
    Ok(
        json!({"destination":base.to_string_lossy(),"entries":stage.counter.entries,"bytes":stage.counter.bytes,"verified":true}),
    )
}
