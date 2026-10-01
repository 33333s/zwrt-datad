use super::fsops::{self, Target, error, name, parse, version};
use super::{CHUNK_BYTES, FileError, Guard, MAX_FILE_BYTES, Result, Session};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::{self, File, Metadata},
    io::{Read, Seek, SeekFrom, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, PermissionsExt},
    },
};

pub(super) struct Staged {
    pub target: Target,
    pub file: File,
    pub temp: CString,
    pub expected: Option<String>,
    pub original: Option<Metadata>,
    pub new_attrs: Option<(u32, u32, u32)>,
    published: bool,
    #[cfg(test)]
    pub before_publish: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)]
    pub after_exchange: Option<Box<dyn FnOnce() + Send>>,
}
impl Staged {
    pub fn new(target: Target, expected: Option<String>) -> Result<Self> {
        let original = match expected.as_deref() {
            Some(v) => {
                let m = target.check(v)?;
                if !m.is_file() || m.nlink() != 1 {
                    return Err("files_invalid_type".into());
                }
                Some(m)
            }
            None => match target.metadata() {
                Ok(_) => return Err("files_exists".into()),
                Err(e) if e.code == "files_not_found" => None,
                Err(e) => return Err(e),
            },
        };
        target.verify_parent()?;
        let temp = name(&format!(".datad-file-{:032x}", rand::random::<u128>()))?;
        let file = fsops::open_at(
            &target.parent,
            &temp,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        Ok(Self {
            target,
            file,
            temp,
            expected,
            original,
            new_attrs: None,
            published: false,
            #[cfg(test)]
            before_publish: None,
            #[cfg(test)]
            after_exchange: None,
        })
    }
    pub fn check_target(&self) -> Result<()> {
        self.target.verify_parent()?;
        match self.expected.as_deref() {
            Some(v) => {
                self.target.check(v)?;
            }
            None => match self.target.metadata() {
                Ok(_) => return Err("files_exists".into()),
                Err(e) if e.code == "files_not_found" => {}
                Err(e) => return Err(e),
            },
        };
        Ok(())
    }
    fn recover_displaced(&self, expected: &Metadata) -> Result<()> {
        let current = fs::symlink_metadata(
            fsops::fd_path(&self.target.parent).join(self.temp.to_string_lossy().as_ref()),
        )
        .map_err(error)?;
        if !same_object(&current, expected) {
            return Err("files_version_conflict".into());
        }
        let folder = name(&format!(".datad-recovery-{:032x}", rand::random::<u128>()))?;
        if unsafe { libc::mkdirat(self.target.parent.as_raw_fd(), folder.as_ptr(), 0o700) } != 0 {
            return Err(error(std::io::Error::last_os_error()));
        }
        let dir = fsops::open_at(
            &self.target.parent,
            &folder,
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        )?;
        let dst = name("original")?;
        if unsafe {
            libc::renameat(
                self.target.parent.as_raw_fd(),
                self.temp.as_ptr(),
                dir.as_raw_fd(),
                dst.as_ptr(),
            )
        } != 0
        {
            return Err(error(std::io::Error::last_os_error()));
        }
        dir.sync_all().map_err(error)?;
        self.target.parent.sync_all().map_err(error)
    }
    pub fn publish(&mut self, guard: &Guard) -> Result<Value> {
        guard.check()?;
        self.file.sync_all().map_err(error)?;
        self.check_target()?;
        if let Some(m) = &self.original {
            let fresh = self.file.metadata().map_err(error)?;
            if (fresh.uid() != m.uid() || fresh.gid() != m.gid())
                && unsafe { libc::fchown(self.file.as_raw_fd(), m.uid(), m.gid()) } != 0
            {
                return Err(error(std::io::Error::last_os_error()));
            }
            self.file
                .set_permissions(fs::Permissions::from_mode(m.mode() & 0o7777))
                .map_err(error)?;
        } else if let Some((mode, uid, gid)) = self.new_attrs {
            let fresh = self.file.metadata().map_err(error)?;
            if (fresh.uid() != uid || fresh.gid() != gid)
                && unsafe { libc::fchown(self.file.as_raw_fd(), uid, gid) } != 0
            {
                return Err(error(std::io::Error::last_os_error()));
            }
            self.file
                .set_permissions(fs::Permissions::from_mode(mode))
                .map_err(error)?;
        } else {
            self.file
                .set_permissions(fs::Permissions::from_mode(0o644))
                .map_err(error)?;
        }
        self.file.sync_all().map_err(error)?;
        self.check_target()?;
        guard.check()?;
        #[cfg(test)]
        if let Some(hook) = self.before_publish.take() {
            hook();
        }
        if let Some(original) = &self.original {
            let staged = self.file.metadata().map_err(error)?;
            fsops::exchange_at(&self.target.parent, &self.temp, &self.target.name)?;
            #[cfg(test)]
            if let Some(hook) = self.after_exchange.take() {
                hook();
            }
            let displaced_path =
                fsops::fd_path(&self.target.parent).join(self.temp.to_string_lossy().as_ref());
            let displaced = fs::symlink_metadata(&displaced_path).map_err(error)?;
            if !same_object(&displaced, original) {
                let current_path = fsops::fd_path(&self.target.parent)
                    .join(self.target.name.to_string_lossy().as_ref());
                let safe = fs::symlink_metadata(&current_path)
                    .is_ok_and(|m| same_object(&m, &staged))
                    && fs::symlink_metadata(&displaced_path)
                        .is_ok_and(|m| same_object(&m, &displaced));
                if safe
                    && fsops::exchange_at(&self.target.parent, &self.temp, &self.target.name)
                        .is_ok()
                {
                    let _ = self.target.parent.sync_all();
                    return Err("files_version_conflict".into());
                }
                // Never remove a displaced external update. If the new target
                // was itself edited, preserve the displaced object in a private
                // sibling directory on the same filesystem for recovery.
                self.published = true;
                self.recover_displaced(&displaced)?;
                return Err("files_version_conflict".into());
            }
            self.published = true;
            fsops::unlink(&self.target.parent, &self.temp, false)?;
            self.target.parent.sync_all().map_err(error)?;
        } else {
            fsops::rename_at(&self.target.parent, &self.temp, &self.target.name, false)?;
        }
        self.published = true;
        let m = self.file.metadata().map_err(error)?;
        let actual = self.target.metadata()?;
        if actual.ino() != m.ino() || actual.dev() != m.dev() || actual.len() != m.len() {
            return Err("files_readback_failed".into());
        }
        Ok(fsops::entry(&self.target.path, &actual))
    }
}
impl Drop for Staged {
    fn drop(&mut self) {
        if !self.published
            && let (Ok(a), Ok(b)) = (
                self.file.metadata(),
                fs::symlink_metadata(
                    fsops::fd_path(&self.target.parent).join(self.temp.to_string_lossy().as_ref()),
                ),
            )
            && a.ino() == b.ino()
            && a.dev() == b.dev()
        {
            let _ = fsops::unlink(&self.target.parent, &self.temp, false);
            let _ = self.target.parent.sync_all();
        }
    }
}
// Rename/exchange can change ctime. Stable inode, ownership, permissions,
// length and mtime still distinguish an external replacement or content edit.
fn same_object(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.mode() == b.mode()
        && a.uid() == b.uid()
        && a.gid() == b.gid()
        && a.nlink() == b.nlink()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
}
pub(super) struct Upload {
    pub(super) staged: Staged,
    size: u64,
    digest: String,
    hasher: Sha256,
    offset: u64,
}
pub(super) struct Download {
    file: File,
    target: Target,
    version: String,
    size: u64,
    offset: u64,
    hasher: Sha256,
    done: bool,
}
fn transfer_id(id: &str) -> Result<()> {
    if id.len() != 32
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invalid_parameter".into());
    }
    Ok(())
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Id {
    transfer_id: String,
}
pub(super) fn hash_file(file: &mut File, guard: &Guard) -> Result<String> {
    file.seek(SeekFrom::Start(0)).map_err(error)?;
    let mut buf = vec![0u8; CHUNK_BYTES];
    let mut hash = Sha256::new();
    loop {
        guard.check()?;
        let n = file.read(&mut buf).map_err(error)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
impl Session {
    fn slot(&self) -> Result<()> {
        if self.uploads.len() + self.downloads.len() >= 4 {
            return Err("files_transfer_limit".into());
        }
        Ok(())
    }
    fn reserve(&self, parent: &File, extra: u64) -> Result<()> {
        let device = parent.metadata().map_err(error)?.dev();
        let mut required = extra + 16 * 1024 * 1024;
        for u in self.uploads.values() {
            if u.staged.target.parent.metadata().map_err(error)?.dev() == device {
                required = required
                    .checked_add(u.size - u.offset)
                    .ok_or(FileError::from("files_no_space"))?;
            }
        }
        let mut st = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        if unsafe { libc::fstatvfs(parent.as_raw_fd(), st.as_mut_ptr()) } != 0 {
            return Err(error(std::io::Error::last_os_error()));
        }
        let st = unsafe { st.assume_init() };
        #[allow(clippy::unnecessary_cast)]
        let available = (st.f_bavail as u64).saturating_mul(st.f_frsize as u64);
        if available < required {
            return Err("files_no_space".into());
        }
        Ok(())
    }
    pub(super) fn upload_begin(&mut self, p: Value) -> Result<Value> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Req {
            path: String,
            size: u64,
            sha256: String,
            #[serde(default)]
            expected_version: Option<String>,
        }
        let p: Req = parse(p)?;
        self.slot()?;
        if p.size > MAX_FILE_BYTES {
            return Err("files_too_large".into());
        }
        if !fsops::valid_version(&p.sha256) {
            return Err("invalid_parameter".into());
        }
        let destination = fsops::path(&p.path)?;
        let parent = destination
            .parent()
            .ok_or(FileError::from("invalid_parameter"))?;
        fsops::mkdir(json!({"path":parent.to_string_lossy()}))?;
        let target = Target::new(&p.path)?;
        self.reserve(&target.parent, p.size)?;
        let staged = Staged::new(target, p.expected_version)?;
        let id = format!("{:032x}", rand::random::<u128>());
        self.uploads.insert(
            id.clone(),
            Upload {
                staged,
                size: p.size,
                digest: p.sha256,
                hasher: Sha256::new(),
                offset: 0,
            },
        );
        Ok(json!({"transfer_id":id,"chunk_size":CHUNK_BYTES}))
    }
    pub(super) fn upload_chunk(&mut self, p: Value) -> Result<Value> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Req {
            transfer_id: String,
            offset: u64,
            data: String,
        }
        let p: Req = parse(p)?;
        transfer_id(&p.transfer_id)?;
        if p.data.len() > CHUNK_BYTES.div_ceil(3) * 4 {
            return Err("files_too_large".into());
        }
        let bytes = STANDARD
            .decode(p.data)
            .map_err(|_| FileError::from("invalid_parameter"))?;
        if bytes.is_empty() || bytes.len() > CHUNK_BYTES {
            return Err("invalid_parameter".into());
        }
        let u = self
            .uploads
            .get(&p.transfer_id)
            .ok_or(FileError::from("files_transfer_not_found"))?;
        if p.offset != u.offset {
            return Err("files_offset_mismatch".into());
        }
        if u.offset + bytes.len() as u64 > u.size {
            return Err("files_size_mismatch".into());
        }
        self.guard.check()?;
        self.reserve(&u.staged.target.parent, 0)?;
        let mut u = self.uploads.remove(&p.transfer_id).unwrap();
        u.staged.file.write_all(&bytes).map_err(error)?;
        u.hasher.update(&bytes);
        u.offset += bytes.len() as u64;
        let offset = u.offset;
        self.uploads.insert(p.transfer_id, u);
        Ok(json!({"offset":offset}))
    }
    pub(super) fn upload_commit(&mut self, p: Value) -> Result<Value> {
        let p: Id = parse(p)?;
        transfer_id(&p.transfer_id)?;
        let mut u = self
            .uploads
            .remove(&p.transfer_id)
            .ok_or(FileError::from("files_transfer_not_found"))?;
        if u.offset != u.size {
            return Err("files_size_mismatch".into());
        }
        let digest = format!("{:x}", u.hasher.finalize());
        if digest != u.digest {
            return Err("files_digest_mismatch".into());
        }
        if hash_file(&mut u.staged.file, &self.guard)? != digest {
            return Err("files_readback_failed".into());
        }
        let entry = u.staged.publish(&self.guard)?;
        let mut read = u.staged.target.open(libc::O_RDONLY, 0)?;
        if hash_file(&mut read, &self.guard)? != digest {
            return Err("files_readback_failed".into());
        }
        Ok(json!({"entry":entry,"sha256":digest}))
    }
    pub(super) fn upload_abort(&mut self, p: Value) -> Result<Value> {
        let p: Id = parse(p)?;
        transfer_id(&p.transfer_id)?;
        self.uploads
            .remove(&p.transfer_id)
            .ok_or(FileError::from("files_transfer_not_found"))?;
        Ok(json!({"aborted":true}))
    }
    pub(super) fn download_begin(&mut self, p: Value) -> Result<Value> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Req {
            path: String,
            expected_version: String,
        }
        let p: Req = parse(p)?;
        self.slot()?;
        let target = Target::new(&p.path)?;
        let m = target.check(&p.expected_version)?;
        if !m.is_file() {
            return Err("files_invalid_type".into());
        }
        let mut file = target.open(libc::O_RDONLY, 0)?;
        if version(&file.metadata().map_err(error)?) != p.expected_version {
            return Err("files_version_conflict".into());
        }
        if m.len() == 0 {
            let mut byte = [0u8; 1];
            let n = file.read(&mut byte).map_err(error)?;
            self.guard.check()?;
            target.check(&p.expected_version)?;
            if version(&file.metadata().map_err(error)?) != p.expected_version {
                return Err("files_version_conflict".into());
            }
            if n != 0 {
                return Err("files_invalid_type".into());
            }
            file.seek(SeekFrom::Start(0)).map_err(error)?;
        }
        let id = format!("{:032x}", rand::random::<u128>());
        let size = m.len();
        let n = target.name.to_string_lossy().into_owned();
        self.downloads.insert(
            id.clone(),
            Download {
                file,
                target,
                version: p.expected_version,
                size,
                offset: 0,
                hasher: Sha256::new(),
                done: false,
            },
        );
        Ok(json!({"transfer_id":id,"size":size,"name":n,"chunk_size":CHUNK_BYTES}))
    }
    pub(super) fn download_chunk(&mut self, p: Value) -> Result<Value> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Req {
            transfer_id: String,
            offset: u64,
        }
        let p: Req = parse(p)?;
        transfer_id(&p.transfer_id)?;
        let d = self
            .downloads
            .get_mut(&p.transfer_id)
            .ok_or(FileError::from("files_transfer_not_found"))?;
        if p.offset != d.offset || d.done {
            return Err("files_offset_mismatch".into());
        }
        d.target.check(&d.version)?;
        if version(&d.file.metadata().map_err(error)?) != d.version {
            return Err("files_version_conflict".into());
        }
        let n = (d.size - d.offset).min(CHUNK_BYTES as u64) as usize;
        let mut bytes = vec![0u8; n];
        d.file.read_exact(&mut bytes).map_err(error)?;
        self.guard.check()?;
        d.target.check(&d.version)?;
        if version(&d.file.metadata().map_err(error)?) != d.version {
            return Err("files_version_conflict".into());
        }
        d.hasher.update(&bytes);
        d.offset += n as u64;
        d.done = d.offset == d.size;
        let mut out = json!({"data":STANDARD.encode(bytes),"offset":d.offset,"eof":d.done});
        if d.done {
            out["sha256"] = json!(format!("{:x}", d.hasher.clone().finalize()));
        }
        Ok(out)
    }
    pub(super) fn download_end(&mut self, p: Value) -> Result<Value> {
        let p: Id = parse(p)?;
        transfer_id(&p.transfer_id)?;
        self.downloads
            .remove(&p.transfer_id)
            .ok_or(FileError::from("files_transfer_not_found"))?;
        Ok(json!({"closed":true}))
    }
}
