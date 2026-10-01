//! Descriptor-bound filesystem primitives; no shell expansion.
use super::{CHUNK_BYTES, FileError, Guard, MAX_FILE_BYTES, Result};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::{self, File, Metadata, OpenOptions},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

pub(super) fn parse<T: DeserializeOwned>(params: Value) -> Result<T> {
    serde_json::from_value(params).map_err(|_| "invalid_parameter".into())
}
pub(super) fn error(e: std::io::Error) -> FileError {
    match e.raw_os_error() {
        Some(libc::ENOSPC | libc::EDQUOT | libc::EFBIG) => "files_no_space",
        Some(libc::ENOENT) => "files_not_found",
        Some(libc::EACCES | libc::EPERM) => "files_permission_denied",
        Some(libc::ELOOP) => "files_unsafe_link",
        Some(libc::EEXIST) => "files_exists",
        _ => "files_io_failed",
    }
    .into()
}
pub(super) fn name(name: &str) -> Result<CString> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.len() > 255
        || name.chars().any(|c| c.is_control())
    {
        return Err("invalid_parameter".into());
    }
    CString::new(name).map_err(|_| "invalid_parameter".into())
}
pub(super) fn path(input: &str) -> Result<PathBuf> {
    if !input.starts_with('/') || input.len() > 4096 || input.chars().any(|c| c.is_control()) {
        return Err("invalid_parameter".into());
    }
    let mut out = PathBuf::from("/");
    for part in input.split('/').filter(|s| !s.is_empty()) {
        name(part)?;
        out.push(part);
    }
    Ok(out)
}
pub(super) fn fd_path(file: &File) -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        Path::new("/proc/self/fd").join(file.as_raw_fd().to_string())
    }
    #[cfg(target_os = "macos")]
    {
        let mut buf = [0u8; libc::PATH_MAX as usize];
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) } != 0 {
            return PathBuf::from("/nonexistent-datad-fd");
        }
        let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        PathBuf::from(String::from_utf8_lossy(&buf[..len]).into_owned())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Path::new("/dev/fd").join(file.as_raw_fd().to_string())
    }
}

pub(super) fn directory(path: &Path) -> Result<File> {
    let mut dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")
        .map_err(error)?;
    for component in path.components().skip(1) {
        let n = name(
            component
                .as_os_str()
                .to_str()
                .ok_or(FileError::from("files_invalid_name"))?,
        )?;
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                n.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(error(std::io::Error::last_os_error()));
        }
        dir = unsafe { File::from_raw_fd(fd) };
    }
    Ok(dir)
}
pub(super) fn open_at(dir: &File, name: &CString, flags: i32, mode: u32) -> Result<File> {
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            mode,
        )
    };
    if fd < 0 {
        return Err(error(std::io::Error::last_os_error()));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
pub(super) struct Target {
    pub path: PathBuf,
    pub parent: File,
    pub name: CString,
}
impl Target {
    pub fn new(input: &str) -> Result<Self> {
        let input = path(input)?;
        if input == Path::new("/") {
            return Err("files_root_protected".into());
        }
        let n = name(
            input
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or(FileError::from("invalid_parameter"))?,
        )?;
        let parentpath = fs::canonicalize(input.parent().unwrap()).map_err(error)?;
        let parent = directory(&parentpath)?;
        let path = parentpath.join(input.file_name().unwrap());
        Ok(Self {
            path,
            parent,
            name: n,
        })
    }
    pub fn open(&self, flags: i32, mode: u32) -> Result<File> {
        open_at(&self.parent, &self.name, flags, mode)
    }
    pub fn verify_parent(&self) -> Result<()> {
        let a = self.parent.metadata().map_err(error)?;
        let b = fs::symlink_metadata(self.path.parent().unwrap())
            .map_err(|_| FileError::from("files_version_conflict"))?;
        if !b.is_dir() || a.ino() != b.ino() || a.dev() != b.dev() {
            return Err("files_version_conflict".into());
        }
        Ok(())
    }
    pub fn metadata(&self) -> Result<Metadata> {
        self.verify_parent()?;
        fs::symlink_metadata(
            fd_path(&self.parent).join(
                self.name
                    .to_str()
                    .map_err(|_| FileError::from("files_invalid_name"))?,
            ),
        )
        .map_err(error)
    }
    pub fn check(&self, expected: &str) -> Result<Metadata> {
        if !valid_version(expected) {
            return Err("invalid_parameter".into());
        }
        let m = self.metadata()?;
        if version(&m) != expected {
            return Err("files_version_conflict".into());
        }
        Ok(m)
    }
    pub fn entry(&self) -> Result<Value> {
        Ok(entry(&self.path, &self.metadata()?))
    }
    pub fn sync(&self) -> Result<()> {
        self.parent.sync_all().map_err(error)
    }
}
pub(super) fn valid_version(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(super) fn version(m: &Metadata) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!(
            "{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
            m.dev(),
            m.ino(),
            m.mode(),
            m.uid(),
            m.gid(),
            m.nlink(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
            m.rdev()
        ))
    )
}
fn account_name(id: u32, group: bool) -> String {
    let mut buf = [0u8; 8192];
    let ptr = if group {
        let mut value = std::mem::MaybeUninit::<libc::group>::uninit();
        let mut result = std::ptr::null_mut();
        if unsafe {
            libc::getgrgid_r(
                id,
                value.as_mut_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        } != 0
            || result.is_null()
        {
            return id.to_string();
        }
        unsafe { (*result).gr_name }
    } else {
        let mut value = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        if unsafe {
            libc::getpwuid_r(
                id,
                value.as_mut_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        } != 0
            || result.is_null()
        {
            return id.to_string();
        }
        unsafe { (*result).pw_name }
    };
    if ptr.is_null() {
        return id.to_string();
    }
    let text = unsafe { std::ffi::CStr::from_ptr(ptr) }.to_string_lossy();
    if text.len() > 255 {
        return id.to_string();
    }
    text.into_owned()
}
pub(super) fn entry(path: &Path, m: &Metadata) -> Value {
    let kind = if m.is_dir() {
        "directory"
    } else if m.file_type().is_symlink() {
        "symlink"
    } else if m.is_file() {
        "file"
    } else {
        "other"
    };
    json!({"name":path.file_name().and_then(|s|s.to_str()).unwrap_or("/"),"path":path.to_string_lossy(),"kind":kind,"is_dir":m.is_dir(),"is_link":m.file_type().is_symlink(),"size":m.len(),"mtime_epoch":m.mtime(),"mode":format!("{:04o}",m.mode()&0o7777),"perm":permissions(m.mode()),"uid":m.uid(),"gid":m.gid(),"owner":account_name(m.uid(),false),"group":account_name(m.gid(),true),"nlink":m.nlink(),"version":version(m)})
}
pub(super) fn unlink(dir: &File, n: &CString, is_dir: bool) -> Result<()> {
    if unsafe {
        libc::unlinkat(
            dir.as_raw_fd(),
            n.as_ptr(),
            if is_dir { libc::AT_REMOVEDIR } else { 0 },
        )
    } != 0
    {
        return Err(error(std::io::Error::last_os_error()));
    }
    Ok(())
}
pub(super) fn exchange_at(dir: &File, a: &CString, b: &CString) -> Result<()> {
    #[cfg(target_os = "linux")]
    let rc = unsafe {
        libc::renameat2(
            dir.as_raw_fd(),
            a.as_ptr(),
            dir.as_raw_fd(),
            b.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    #[cfg(target_os = "macos")]
    let rc = unsafe {
        libc::renameatx_np(
            dir.as_raw_fd(),
            a.as_ptr(),
            dir.as_raw_fd(),
            b.as_ptr(),
            libc::RENAME_SWAP,
        )
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Err("unsupported_action".into());
    if rc != 0 {
        return Err(error(std::io::Error::last_os_error()));
    }
    Ok(())
}
pub(super) fn rename_at(dir: &File, from: &CString, to: &CString, overwrite: bool) -> Result<()> {
    let rc = if overwrite {
        unsafe { libc::renameat(dir.as_raw_fd(), from.as_ptr(), dir.as_raw_fd(), to.as_ptr()) }
    } else {
        #[cfg(target_os = "linux")]
        {
            unsafe {
                libc::renameat2(
                    dir.as_raw_fd(),
                    from.as_ptr(),
                    dir.as_raw_fd(),
                    to.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            }
        }
        #[cfg(target_os = "macos")]
        {
            unsafe {
                libc::renameatx_np(
                    dir.as_raw_fd(),
                    from.as_ptr(),
                    dir.as_raw_fd(),
                    to.as_ptr(),
                    libc::RENAME_EXCL,
                )
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            return Err("unsupported_action".into());
        }
    };
    if rc != 0 {
        return Err(error(std::io::Error::last_os_error()));
    }
    dir.sync_all().map_err(error)
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PathRequest {
    path: String,
}
pub(super) fn status(params: Value) -> Result<Value> {
    let p: std::collections::BTreeMap<String, Value> = parse(params)?;
    if !p.is_empty() {
        return Err("invalid_parameter".into());
    }

    Ok(
        json!({"supported":true,"operations":["files.list","files.stat","files.disk","files.mkdir","files.touch","files.rename","files.chmod","files.remove","files.compress","files.extract","files.upload.begin","files.upload.chunk","files.upload.commit","files.upload.abort","files.download.begin","files.download.chunk","files.download.end"],"formats":["tar","tar.gz","tar.bz2","tar.xz","zip"],"limits":{"upload_max_bytes":MAX_FILE_BYTES,"chunk_bytes":CHUNK_BYTES,"session_seconds":3600,"max_transfers":4,"list_limit":256},"shortcuts":[]}),
    )
}
fn permissions(mode: u32) -> String {
    let mut out = String::new();
    for (mask, ch) in [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ] {
        out.push(if mode & mask != 0 { ch } else { '-' });
    }
    for (bit, index, lower, upper) in [
        (0o4000, 2, 's', 'S'),
        (0o2000, 5, 's', 'S'),
        (0o1000, 8, 't', 'T'),
    ] {
        if mode & bit != 0 {
            out.replace_range(
                index..index + 1,
                &if out.as_bytes()[index] == b'x' {
                    lower
                } else {
                    upper
                }
                .to_string(),
            );
        }
    }
    out
}

pub(super) fn stat(params: Value) -> Result<Value> {
    let p: PathRequest = parse(params)?;
    let p = path(&p.path)?;
    if p == Path::new("/") {
        return Ok(json!({"entry":entry(&p,&directory(&p)?.metadata().map_err(error)?)}));
    }
    Ok(json!({"entry":Target::new(p.to_str().unwrap())?.entry()?}))
}
pub(super) fn list(params: Value) -> Result<Value> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Req {
        path: String,
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default = "default_limit")]
        limit: usize,
    }
    fn default_limit() -> usize {
        128
    }
    let p: Req = parse(params)?;
    if !(1..=256).contains(&p.limit) {
        return Err("invalid_parameter".into());
    }
    let input = path(&p.path)?;
    let real = fs::canonicalize(&input).map_err(error)?;
    let dir = directory(&real)?;
    let before = version(&dir.metadata().map_err(error)?);
    let after_name = if let Some(cursor) = p.cursor {
        if cursor.len() > 1024 {
            return Err("invalid_parameter".into());
        }
        let raw = URL_SAFE_NO_PAD
            .decode(cursor)
            .map_err(|_| FileError::from("invalid_parameter"))?;
        let pair: (String, String) =
            serde_json::from_slice(&raw).map_err(|_| FileError::from("invalid_parameter"))?;
        if pair.0 != before {
            return Err("files_version_conflict".into());
        }
        name(&pair.1)?;
        pair.1
    } else {
        String::new()
    };
    let mut selected = BTreeMap::new();
    let mut scanned = 0;
    for item in fs::read_dir(fd_path(&dir)).map_err(error)? {
        scanned += 1;
        if scanned > 100000 {
            return Err("files_resource_limit".into());
        }
        let item = item.map_err(error)?;
        let n = item
            .file_name()
            .into_string()
            .map_err(|_| FileError::from("files_invalid_name"))?;
        if n <= after_name {
            continue;
        }
        let m = fs::symlink_metadata(item.path()).map_err(error)?;
        selected.insert(n.clone(), entry(&real.join(&n), &m));
        if selected.len() > p.limit + 1 {
            selected.pop_last();
        }
    }
    if version(&dir.metadata().map_err(error)?) != before {
        return Err("files_version_conflict".into());
    }
    let mut complete = selected.len() <= p.limit;
    if !complete {
        selected.pop_last();
    }
    let mut cost=serde_json::to_vec(&json!({"path":input.to_string_lossy(),"real_path":real.to_string_lossy(),"entries":[],"next_cursor":null,"complete":false})).unwrap().len()+1024;
    let mut entries = Vec::new();
    let mut last_name = String::new();
    for (n, e) in selected {
        let bytes = serde_json::to_vec(&e).unwrap().len() + 1;
        if cost + bytes > 220 * 1024 {
            complete = false;
            break;
        }
        cost += bytes;
        last_name = n;
        entries.push(e);
    }
    if !complete && last_name.is_empty() {
        return Err("files_resource_limit".into());
    }
    let cursor = if complete {
        None
    } else {
        Some(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&(before, last_name)).unwrap()))
    };
    Ok(
        json!({"path":input.to_string_lossy(),"real_path":real.to_string_lossy(),"entries":entries,"next_cursor":cursor,"complete":complete}),
    )
}
#[allow(clippy::unnecessary_cast)] // statvfs counters vary by libc target width.
pub(super) fn available<T: AsRawFd>(file: &T) -> Result<u64> {
    let mut st = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::fstatvfs(file.as_raw_fd(), st.as_mut_ptr()) } != 0 {
        return Err(error(std::io::Error::last_os_error()));
    }
    let st = unsafe { st.assume_init() };
    Ok((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}
#[allow(clippy::unnecessary_cast)] // statvfs field widths differ on libc targets.
pub(super) fn disk(params: Value) -> Result<Value> {
    let p: PathRequest = parse(params)?;
    let real = fs::canonicalize(path(&p.path)?).map_err(error)?;
    let file = if real.is_dir() {
        directory(&real)?
    } else {
        Target::new(real.to_str().ok_or(FileError::from("files_invalid_name"))?)?
            .open(libc::O_RDONLY, 0)?
    };
    let mut st = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::fstatvfs(file.as_raw_fd(), st.as_mut_ptr()) } != 0 {
        return Err(error(std::io::Error::last_os_error()));
    }
    let st = unsafe { st.assume_init() };
    #[allow(clippy::unnecessary_cast)] // statvfs fields differ on supported libc targets.
    let unit = st.f_frsize as u64;
    let total = (st.f_blocks as u64).saturating_mul(unit);
    let free = (st.f_bfree as u64).saturating_mul(unit);
    Ok(
        json!({"total":total,"used":total.saturating_sub(free),"available":(st.f_bavail as u64).saturating_mul(unit)}),
    )
}
pub(super) fn mkdir(params: Value) -> Result<Value> {
    let p: PathRequest = parse(params)?;
    let p = path(&p.path)?;
    let mut existing = p.as_path();
    let mut missing = Vec::new();
    let real = loop {
        match fs::canonicalize(existing) {
            Ok(p) => break p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    existing
                        .file_name()
                        .ok_or(FileError::from("invalid_parameter"))?
                        .to_owned(),
                );
                existing = existing
                    .parent()
                    .ok_or(FileError::from("invalid_parameter"))?;
            }
            Err(e) => return Err(error(e)),
        }
    };
    let mut dir = directory(&real)?;
    for n in missing.into_iter().rev() {
        let n = name(n.to_str().ok_or(FileError::from("files_invalid_name"))?)?;
        if unsafe { libc::mkdirat(dir.as_raw_fd(), n.as_ptr(), 0o755) } != 0
            && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
        {
            return Err(error(std::io::Error::last_os_error()));
        }
        dir = open_at(&dir, &n, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    }
    dir.sync_all().map_err(error)?;
    Ok(json!({"entry":entry(&p,&dir.metadata().map_err(error)?)}))
}
pub(super) fn touch(params: Value) -> Result<Value> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Req {
        path: String,
        #[serde(default)]
        expected_version: Option<String>,
    }
    let p: Req = parse(params)?;
    let t = Target::new(&p.path)?;
    let f = match &p.expected_version {
        Some(v) => {
            let m = t.check(v)?;
            if !m.is_file() || m.nlink() != 1 {
                return Err("files_invalid_type".into());
            }
            let f = t.open(libc::O_RDWR, 0)?;
            if version(&f.metadata().map_err(error)?) != *v {
                return Err("files_version_conflict".into());
            }
            t.check(v)?;
            if unsafe { libc::futimens(f.as_raw_fd(), std::ptr::null()) } != 0 {
                return Err(error(std::io::Error::last_os_error()));
            }
            f
        }
        None => t.open(libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o644)?,
    };
    f.sync_all().map_err(error)?;
    t.sync()?;
    Ok(json!({"entry":t.entry()?}))
}
pub(super) fn rename(params: Value) -> Result<Value> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Req {
        path: String,
        name: String,
        expected_version: String,
    }
    let p: Req = parse(params)?;
    let t = Target::new(&p.path)?;
    let n = name(&p.name)?;
    t.check(&p.expected_version)?;
    rename_at(&t.parent, &t.name, &n, false)?;
    let path = t.path.parent().unwrap().join(p.name);
    Ok(json!({"entry":Target::new(path.to_str().unwrap())?.entry()?}))
}
pub(super) fn chmod(params: Value) -> Result<Value> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Req {
        path: String,
        mode: String,
        expected_version: String,
    }
    let p: Req = parse(params)?;
    if p.mode.len() != 3 || !p.mode.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return Err("invalid_parameter".into());
    }
    let t = Target::new(&p.path)?;
    let before = t.check(&p.expected_version)?;
    if before.file_type().is_symlink() || (!before.is_file() && !before.is_dir()) {
        return Err("files_invalid_type".into());
    }
    let f = t.open(libc::O_RDONLY, 0)?;
    if version(&f.metadata().map_err(error)?) != p.expected_version {
        return Err("files_version_conflict".into());
    }
    t.check(&p.expected_version)?;
    if unsafe {
        libc::fchmod(
            f.as_raw_fd(),
            u32::from_str_radix(&p.mode, 8).unwrap() as libc::mode_t,
        )
    } != 0
    {
        return Err(error(std::io::Error::last_os_error()));
    }
    f.sync_all().map_err(error)?;
    Ok(json!({"entry":t.entry()?}))
}
#[cfg(test)]
thread_local! {static CANCEL_REMOVE_AFTER:std::cell::Cell<usize>=const{std::cell::Cell::new(0)};}
#[cfg(test)]
pub(super) fn cancel_remove_after(n: usize) {
    CANCEL_REMOVE_AFTER.with(|c| c.set(n));
}
fn remove_tree(
    dir: &File,
    depth: usize,
    count: &mut usize,
    device: u64,
    guard: &Guard,
) -> Result<()> {
    if depth > 64 {
        return Err("files_resource_limit".into());
    }
    for e in fs::read_dir(fd_path(dir)).map_err(error)? {
        guard.check()?;
        *count += 1;
        #[cfg(test)]
        CANCEL_REMOVE_AFTER.with(|c| {
            if c.get() != 0 && *count > c.get() {
                c.set(0);
                guard
                    .cancel
                    .store(true, std::sync::atomic::Ordering::Release);
            }
        });
        guard.check()?;
        if *count > 10000 {
            return Err("files_resource_limit".into());
        }
        let e = e.map_err(error)?;
        let n = name(
            e.file_name()
                .to_str()
                .ok_or(FileError::from("files_invalid_name"))?,
        )?;
        let m = fs::symlink_metadata(e.path()).map_err(error)?;
        if m.is_dir() {
            if m.dev() != device {
                return Err("files_resource_limit".into());
            }
            let child = open_at(dir, &n, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
            let cm = child.metadata().map_err(error)?;
            if cm.ino() != m.ino() || cm.dev() != m.dev() {
                return Err("files_version_conflict".into());
            }
            remove_tree(&child, depth + 1, count, device, guard)?;
        }
        guard.check()?;
        unlink(dir, &n, m.is_dir())?;
    }
    Ok(())
}
pub(super) fn remove(params: Value, guard: &Guard) -> Result<Value> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Req {
        path: String,
        recursive: bool,
        expected_version: String,
    }
    let p: Req = parse(params)?;
    let t = Target::new(&p.path)?;
    let m = t.check(&p.expected_version)?;
    if m.is_dir() && p.recursive {
        let dir = t.open(libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        if version(&dir.metadata().map_err(error)?) != p.expected_version {
            return Err("files_version_conflict".into());
        }
        remove_tree(&dir, 0, &mut 0, m.dev(), guard)?;
        let latest = t.metadata()?;
        if latest.ino() != m.ino() || latest.dev() != m.dev() {
            return Err("files_version_conflict".into());
        }
    } else {
        t.check(&p.expected_version)?;
    }
    guard.check()?;
    unlink(&t.parent, &t.name, m.is_dir())?;
    t.sync()?;
    Ok(json!({"removed":true}))
}
