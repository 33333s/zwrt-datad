//! Fixed-file Hosts management. Content leaves the device only on explicit
//! authenticated reads or an on-demand panel; it is never periodic telemetry.
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::{self, File, Metadata, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::process::Command;

pub(crate) const MAX_CONTENT: usize = 4096;
const BACKUP: &str = "original.json";
const SERVICE: &str = "/etc/init.d/dnsmasq";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Backup {
    schema: u8,
    bytes: Vec<u8>,
    revision: String,
    mode: u32,
    uid: u32,
    gid: u32,
}

struct Image {
    bytes: Vec<u8>,
    meta: Metadata,
}

pub(crate) struct Manager {
    target: PathBuf,
    data: PathBuf,
    service: PathBuf,
    owner: u32,
}

fn revision(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn valid_revision(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn content(bytes: &[u8], allow_empty: bool) -> Result<&str, &'static str> {
    if bytes.len() > MAX_CONTENT {
        return Err("hosts_too_large");
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "hosts_invalid_utf8")?;
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\t' | '\r' | '\n'))
    {
        return Err("hosts_invalid_content");
    }
    if !allow_empty && text.trim().is_empty() {
        return Err("hosts_empty");
    }
    Ok(text)
}
fn directory(path: &Path, owner: u32, private: bool) -> Result<File, &'static str> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| "hosts_permission_denied")?;
    let meta = file.metadata().map_err(|_| "hosts_permission_denied")?;
    if !meta.is_dir()
        || meta.uid() != owner
        || meta.mode() & if private { 0o077 } else { 0o022 } != 0
    {
        return Err("hosts_permission_denied");
    }
    Ok(file)
}
fn open_at(dir: &File, name: &str, flags: i32, mode: u32) -> Result<File, &'static str> {
    let name = CString::new(name).map_err(|_| "hosts_invalid_file")?;
    // SAFETY: the directory descriptor and C string remain alive for openat.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    if fd < 0 {
        return Err("hosts_read_failed");
    }
    // SAFETY: openat returned a new descriptor whose ownership is transferred.
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn read_at(
    dir: &File,
    name: &str,
    owner: u32,
    limit: usize,
    private: bool,
) -> Result<Image, &'static str> {
    let mut file = open_at(dir, name, libc::O_RDONLY | libc::O_NONBLOCK, 0)?;
    let meta = file.metadata().map_err(|_| "hosts_read_failed")?;
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.uid() != owner
        || private && meta.mode() & 0o077 != 0
    {
        return Err("hosts_invalid_file");
    }
    if meta.len() > limit as u64 {
        return Err("hosts_too_large");
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "hosts_read_failed")?;
    if bytes.len() > limit {
        return Err("hosts_too_large");
    }
    let after = file.metadata().map_err(|_| "hosts_read_failed")?;
    if meta.len() != after.len()
        || meta.mtime() != after.mtime()
        || meta.mtime_nsec() != after.mtime_nsec()
        || meta.ctime() != after.ctime()
        || meta.ctime_nsec() != after.ctime_nsec()
    {
        return Err("hosts_read_failed");
    }
    Ok(Image { bytes, meta })
}
fn unlink(dir: &File, name: &str) {
    if let Ok(name) = CString::new(name) {
        // SAFETY: only a generated temporary name relative to a bound directory.
        unsafe {
            libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0);
        }
    }
}
fn rename(dir: &File, from: &str, to: &str) -> Result<(), &'static str> {
    let from = CString::new(from).map_err(|_| "hosts_write_failed")?;
    let to = CString::new(to).map_err(|_| "hosts_write_failed")?;
    // SAFETY: both names and the owned directory descriptor are valid.
    if unsafe { libc::renameat(dir.as_raw_fd(), from.as_ptr(), dir.as_raw_fd(), to.as_ptr()) } != 0
    {
        return Err("hosts_write_failed");
    }
    dir.sync_all().map_err(|_| "hosts_write_failed")
}

impl Manager {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            target: "/etc/hosts".into(),
            data: data_dir.join("hosts-control"),
            service: SERVICE.into(),
            owner: 0,
        }
    }
    #[cfg(test)]
    fn fake(root: &Path) -> Self {
        Self {
            target: root.join("etc/hosts"),
            data: root.join("data/hosts-control"),
            service: root.join("etc/init.d/dnsmasq"),
            owner: unsafe { libc::geteuid() },
        }
    }
    fn target_dir(&self) -> Result<File, &'static str> {
        directory(
            self.target.parent().ok_or("hosts_invalid_file")?,
            self.owner,
            false,
        )
    }
    fn current(&self, dir: &File) -> Result<Image, &'static str> {
        let image = read_at(dir, "hosts", self.owner, MAX_CONTENT, false)?;
        content(&image.bytes, true)?;
        Ok(image)
    }
    fn backup_dir(&self, create: bool) -> Result<File, &'static str> {
        let parent = self.data.parent().ok_or("hosts_backup_failed")?;
        let _parent = directory(parent, self.owner, false).map_err(|_| "hosts_backup_invalid")?;
        if create && fs::symlink_metadata(&self.data).is_err() {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&self.data)
                .map_err(|_| "hosts_backup_failed")?;
        }
        directory(&self.data, self.owner, true).map_err(|_| "hosts_backup_invalid")
    }
    fn backup(&self, dir: &File) -> Result<Backup, &'static str> {
        let image =
            read_at(dir, BACKUP, self.owner, 24576, true).map_err(|_| "hosts_backup_invalid")?;
        let backup: Backup =
            serde_json::from_slice(&image.bytes).map_err(|_| "hosts_backup_invalid")?;
        if backup.schema != 1
            || backup.uid != self.owner
            || backup.mode & !0o7777 != 0
            || !valid_revision(&backup.revision)
            || revision(&backup.bytes) != backup.revision
            || content(&backup.bytes, true).is_err()
        {
            return Err("hosts_backup_invalid");
        }
        Ok(backup)
    }
    fn service_available(&self) -> bool {
        fs::symlink_metadata(&self.service).is_ok_and(|m| {
            m.is_file()
                && m.nlink() == 1
                && m.uid() == self.owner
                && m.mode() & 0o022 == 0
                && m.mode() & 0o111 != 0
        })
    }
    fn writable(&self) -> bool {
        let path = CString::new(self.target.as_os_str().as_encoded_bytes());
        let parent = self
            .target
            .parent()
            .and_then(|p| CString::new(p.as_os_str().as_encoded_bytes()).ok());
        path.is_ok_and(|p| unsafe { libc::access(p.as_ptr(), libc::W_OK) } == 0)
            && parent
                .is_some_and(|p| unsafe { libc::access(p.as_ptr(), libc::W_OK | libc::X_OK) } == 0)
            && self.service_available()
    }
    pub fn status(&self) -> Value {
        let dnsmasq = self.service_available();
        let mut state =
            json!({"supported":true,"writable":false,"has_backup":false,"dnsmasq":dnsmasq});
        match self.target_dir().and_then(|d| self.current(&d)) {
            Ok(image) => {
                state["content"] = json!(std::str::from_utf8(&image.bytes).unwrap());
                state["revision"] = json!(revision(&image.bytes));
                state["writable"] = json!(self.writable());
                if !dnsmasq {
                    state["error"] = json!("hosts_dns_unavailable");
                }
            }
            Err(code) => state["error"] = json!(code),
        }
        if fs::symlink_metadata(&self.data).is_ok() {
            match self.backup_dir(false) {
                Ok(dir) => {
                    if fs::symlink_metadata(self.data.join(BACKUP)).is_ok() {
                        match self.backup(&dir) {
                            Ok(_) => state["has_backup"] = json!(true),
                            Err(code) => {
                                state["writable"] = json!(false);
                                state["error"] = json!(code);
                            }
                        }
                    }
                }
                Err(code) => {
                    state["writable"] = json!(false);
                    state["error"] = json!(code);
                }
            }
        }
        state
    }
    async fn reload(&self) -> Result<(), &'static str> {
        if !self.service_available() {
            return Err("hosts_dns_unavailable");
        }
        let status = tokio::time::timeout(
            Duration::from_secs(8),
            Command::new(&self.service)
                .arg("reload")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .status(),
        )
        .await
        .map_err(|_| "hosts_reload_failed")?
        .map_err(|_| "hosts_reload_failed")?;
        if status.success() {
            Ok(())
        } else {
            Err("hosts_reload_failed")
        }
    }
    fn lock(&self, dir: &File) -> Result<File, &'static str> {
        let file = open_at(dir, "write.lock", libc::O_RDWR | libc::O_CREAT, 0o600)
            .map_err(|_| "hosts_backup_failed")?;
        let m = file.metadata().map_err(|_| "hosts_backup_invalid")?;
        if !m.is_file() || m.nlink() != 1 || m.uid() != self.owner || m.mode() & 0o077 != 0 {
            return Err("hosts_backup_invalid");
        }
        file.try_lock_exclusive().map_err(|_| "hosts_busy")?;
        Ok(file)
    }
    fn ensure_backup(&self, dir: &File, image: &Image) -> Result<(), &'static str> {
        if fs::symlink_metadata(self.data.join(BACKUP)).is_ok() {
            self.backup(dir)?;
            return Ok(());
        }
        let backup = Backup {
            schema: 1,
            bytes: image.bytes.clone(),
            revision: revision(&image.bytes),
            mode: image.meta.mode() & 0o7777,
            uid: image.meta.uid(),
            gid: image.meta.gid(),
        };
        let name = format!(".backup-{:032x}", rand::random::<u128>());
        let mut file = open_at(
            dir,
            &name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )
        .map_err(|_| "hosts_backup_failed")?;
        let data = serde_json::to_vec(&backup).map_err(|_| "hosts_backup_failed")?;
        let mut published = false;
        let result = (|| {
            file.write_all(&data)
                .and_then(|_| file.sync_all())
                .map_err(|_| "hosts_backup_failed")?;
            let from = CString::new(name.as_str()).map_err(|_| "hosts_backup_failed")?;
            let to = CString::new(BACKUP).map_err(|_| "hosts_backup_failed")?;
            // SAFETY: linkat publishes the fully synced file under the fixed
            // name without replacing any existing backup or following a link.
            if unsafe {
                libc::linkat(
                    dir.as_raw_fd(),
                    from.as_ptr(),
                    dir.as_raw_fd(),
                    to.as_ptr(),
                    0,
                )
            } != 0
            {
                return Err("hosts_backup_failed");
            }
            published = true;
            unlink(dir, &name);
            dir.sync_all().map_err(|_| "hosts_backup_failed")
        })();
        if result.is_err() {
            if published
                && let (Ok(owned), Ok(entry)) = (
                    file.metadata(),
                    fs::symlink_metadata(self.data.join(BACKUP)),
                )
                && owned.ino() == entry.ino()
                && owned.dev() == entry.dev()
            {
                unlink(dir, BACKUP);
                let _ = dir.sync_all();
            }
            unlink(dir, &name);
            let _ = dir.sync_all();
        }
        result
    }
    fn replace(
        &self,
        dir: &File,
        current: &Image,
        bytes: &[u8],
        expected: &str,
        attrs: (u32, u32, u32),
    ) -> Result<(), &'static str> {
        let (mode, uid, gid) = attrs;
        let name = format!(".datad-hosts-{:032x}", rand::random::<u128>());
        let result = (|| {
            let mut file = open_at(
                dir,
                &name,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )
            .map_err(|_| "hosts_write_failed")?;
            let meta = file.metadata().map_err(|_| "hosts_write_failed")?;
            if (meta.uid() != uid || meta.gid() != gid)
                && unsafe { libc::fchown(file.as_raw_fd(), uid, gid) } != 0
            {
                return Err("hosts_permission_denied");
            }
            file.set_permissions(fs::Permissions::from_mode(mode))
                .map_err(|_| "hosts_permission_denied")?;
            file.write_all(bytes)
                .and_then(|_| file.sync_all())
                .map_err(|_| "hosts_write_failed")?;
            let latest = self.current(dir)?;
            let parent = fs::symlink_metadata(self.target.parent().unwrap())
                .map_err(|_| "hosts_revision_conflict")?;
            let bound = dir.metadata().map_err(|_| "hosts_revision_conflict")?;
            if revision(&latest.bytes) != expected
                || latest.meta.ino() != current.meta.ino()
                || latest.meta.dev() != current.meta.dev()
                || latest.meta.mode() != current.meta.mode()
                || latest.meta.uid() != current.meta.uid()
                || latest.meta.gid() != current.meta.gid()
                || parent.ino() != bound.ino()
                || parent.dev() != bound.dev()
            {
                return Err("hosts_revision_conflict");
            }
            rename(dir, &name, "hosts")
        })();
        if result.is_err() {
            unlink(dir, &name);
        }
        result
    }
    pub async fn action(&self, action: &str, params: Value) -> Result<String, &'static str> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Save {
            content: String,
            expected_revision: String,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Restore {
            expected_revision: String,
        }
        let (expected, desired) = match action {
            "hosts.save" => {
                let req: Save = serde_json::from_value(params).map_err(|_| "invalid_parameter")?;
                content(req.content.as_bytes(), false)?;
                (req.expected_revision, Some(req.content.into_bytes()))
            }
            "hosts.restore" => {
                let req: Restore =
                    serde_json::from_value(params).map_err(|_| "invalid_parameter")?;
                (req.expected_revision, None)
            }
            _ => return Err("unsupported_action"),
        };
        if !valid_revision(&expected) {
            return Err("invalid_parameter");
        }
        if !self.service_available() {
            return Err("hosts_dns_unavailable");
        }
        let dir = self.target_dir()?;
        let current = self.current(&dir)?;
        if !self.writable() {
            return Err("hosts_permission_denied");
        }
        if revision(&current.bytes) != expected {
            return Err("hosts_revision_conflict");
        }
        let backup_dir = self.backup_dir(true)?;
        let _lock = self.lock(&backup_dir)?;
        let (bytes, mode, uid, gid) = if let Some(bytes) = desired {
            self.ensure_backup(&backup_dir, &current)?;
            (
                bytes,
                current.meta.mode() & 0o7777,
                current.meta.uid(),
                current.meta.gid(),
            )
        } else {
            if fs::symlink_metadata(self.data.join(BACKUP)).is_err() {
                return Err("hosts_backup_unavailable");
            }
            let backup = self.backup(&backup_dir)?;
            (backup.bytes, backup.mode, backup.uid, backup.gid)
        };
        self.replace(&dir, &current, &bytes, &expected, (mode, uid, gid))?;
        let updated = self.current(&dir)?;
        if updated.bytes != bytes {
            return Err("hosts_readback_failed");
        }
        self.reload().await?;
        let after = self.current(&dir)?;
        if after.bytes != bytes {
            return Err("hosts_readback_failed");
        }
        Ok(revision(&after.bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    const ORIGINAL: &[u8] = b"127.0.0.1 localhost\n::1 localhost _service\n";
    struct Fixture {
        root: PathBuf,
        manager: Manager,
    }
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("datad-hosts-{:032x}", rand::random::<u128>()));
            fs::create_dir_all(root.join("etc/init.d")).unwrap();
            fs::create_dir(root.join("data")).unwrap();
            fs::set_permissions(root.join("data"), fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(root.join("etc/hosts"), ORIGINAL).unwrap();
            fs::set_permissions(root.join("etc/hosts"), fs::Permissions::from_mode(0o664)).unwrap();
            let service = root.join("etc/init.d/dnsmasq");
            fs::write(&service, format!("#!/bin/sh\n[ \"$1\" = reload ] || exit 2\n[ ! -e '{}' ] || exit 1\ntouch '{}'\n", root.join("fail-reload").display(), root.join("reloaded").display())).unwrap();
            fs::set_permissions(service, fs::Permissions::from_mode(0o755)).unwrap();
            let manager = Manager::fake(&root);
            Self { root, manager }
        }
        async fn save(&self, bytes: &str) -> Result<String, &'static str> {
            self.manager
                .action(
                    "hosts.save",
                    json!({"content":bytes,"expected_revision":self.manager.status()["revision"]}),
                )
                .await
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn save_preserves_exact_bytes_mode_and_first_backup_restore_is_read_back() {
        let f = Fixture::new();
        let text = "# draft only\r\n# UTF-8 コメント\t_service\n";
        let expected = f.save(text).await.unwrap();
        assert_eq!(expected, revision(text.as_bytes()));
        assert_eq!(fs::read(&f.manager.target).unwrap(), text.as_bytes());
        assert_eq!(
            fs::metadata(&f.manager.target).unwrap().mode() & 0o777,
            0o664
        );
        assert!(f.root.join("reloaded").exists());
        let backup = fs::read(f.manager.data.join(BACKUP)).unwrap();
        assert_eq!(
            fs::metadata(f.manager.data.join(BACKUP)).unwrap().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::metadata(&f.manager.data).unwrap().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(f.manager.data.join(BACKUP)).unwrap().nlink(),
            1
        );
        assert!(fs::read_dir(&f.manager.data).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".backup-")
        }));
        f.save("127.0.0.1 localhost\n# second\n").await.unwrap();
        assert_eq!(fs::read(f.manager.data.join(BACKUP)).unwrap(), backup);
        let tag = f.manager.status()["revision"].clone();
        assert_eq!(
            f.manager
                .action("hosts.restore", json!({"expected_revision":tag}))
                .await
                .unwrap(),
            revision(ORIGINAL)
        );
        assert_eq!(fs::read(&f.manager.target).unwrap(), ORIGINAL);
        assert_eq!(
            f.manager.status()["content"],
            std::str::from_utf8(ORIGINAL).unwrap()
        );
    }

    #[tokio::test]
    async fn stale_revision_and_second_cas_never_replace_external_edits() {
        let f = Fixture::new();
        let expected = revision(ORIGINAL);
        let dir = f.manager.target_dir().unwrap();
        let original = f.manager.current(&dir).unwrap();
        let external = b"# external edit\n";
        fs::write(&f.manager.target, external).unwrap();
        assert_eq!(
            f.manager
                .action(
                    "hosts.save",
                    json!({"content":"# wanted\n","expected_revision":expected})
                )
                .await,
            Err("hosts_revision_conflict")
        );
        assert!(!f.manager.data.exists());
        assert_eq!(
            f.manager.replace(
                &dir,
                &original,
                b"# wanted\n",
                &expected,
                (0o664, original.meta.uid(), original.meta.gid())
            ),
            Err("hosts_revision_conflict")
        );
        assert_eq!(fs::read(&f.manager.target).unwrap(), external);
        assert!(fs::read_dir(f.root.join("etc")).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".datad-hosts-")
        }));
    }

    #[tokio::test]
    async fn reload_failure_reports_failure_after_write_without_faking_rollback() {
        let f = Fixture::new();
        fs::write(f.root.join("fail-reload"), b"").unwrap();
        assert_eq!(f.save("# changed\n").await, Err("hosts_reload_failed"));
        assert_eq!(fs::read(&f.manager.target).unwrap(), b"# changed\n");
        assert_eq!(f.manager.status()["revision"], revision(b"# changed\n"));
        assert!(f.manager.status()["has_backup"].as_bool().unwrap());
        fs::remove_file(f.root.join("fail-reload")).unwrap();
        let current = f.manager.status()["revision"].clone();
        f.manager
            .action("hosts.restore", json!({"expected_revision":current}))
            .await
            .unwrap();
        assert_eq!(fs::read(&f.manager.target).unwrap(), ORIGINAL);
    }

    #[tokio::test]
    async fn an_empty_original_and_external_truncation_can_both_be_restored() {
        let f = Fixture::new();
        fs::write(&f.manager.target, b"").unwrap();
        let empty = f.manager.status();
        assert_eq!(empty["content"], "");
        assert_eq!(empty["revision"], revision(b""));
        assert_eq!(empty["writable"], true);
        assert!(empty.get("error").is_none());
        f.save("# new valid file\n").await.unwrap();
        let rev = f.manager.status()["revision"].clone();
        assert_eq!(
            f.manager
                .action("hosts.restore", json!({"expected_revision":rev}))
                .await
                .unwrap(),
            revision(b"")
        );
        assert_eq!(fs::read(&f.manager.target).unwrap(), b"");
        assert_eq!(f.save("").await, Err("hosts_empty"));

        let f = Fixture::new();
        f.save("# temporary file\n").await.unwrap();
        fs::write(&f.manager.target, b"").unwrap();
        let state = f.manager.status();
        assert_eq!(state["has_backup"], true);
        assert_eq!(state["writable"], true);
        f.manager
            .action(
                "hosts.restore",
                json!({"expected_revision":state["revision"]}),
            )
            .await
            .unwrap();
        assert_eq!(fs::read(&f.manager.target).unwrap(), ORIGINAL);
    }

    #[test]
    fn a_metadata_only_external_change_is_not_overwritten() {
        let f = Fixture::new();
        let dir = f.manager.target_dir().unwrap();
        let captured = f.manager.current(&dir).unwrap();
        fs::set_permissions(&f.manager.target, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            f.manager.replace(
                &dir,
                &captured,
                b"# wanted\n",
                &revision(ORIGINAL),
                (0o664, captured.meta.uid(), captured.meta.gid())
            ),
            Err("hosts_revision_conflict")
        );
        assert_eq!(fs::read(&f.manager.target).unwrap(), ORIGINAL);
        assert_eq!(
            fs::metadata(&f.manager.target).unwrap().mode() & 0o777,
            0o644
        );
    }

    #[tokio::test]
    async fn symlinks_hardlinks_and_bad_backups_fail_closed() {
        let f = Fixture::new();
        fs::remove_file(&f.manager.target).unwrap();
        let other = f.root.join("other");
        fs::write(&other, ORIGINAL).unwrap();
        symlink(&other, &f.manager.target).unwrap();
        assert!(f.manager.status().get("content").is_none());
        assert!(f.save("# wanted\n").await.is_err());
        assert_eq!(fs::read(&other).unwrap(), ORIGINAL);
        fs::remove_file(&f.manager.target).unwrap();
        fs::hard_link(&other, &f.manager.target).unwrap();
        assert_eq!(f.manager.status()["error"], "hosts_invalid_file");
        fs::remove_file(&f.manager.target).unwrap();
        fs::remove_file(&other).unwrap();
        fs::write(&f.manager.target, ORIGINAL).unwrap();
        let linked_dir = f.root.join("outside");
        fs::create_dir(&linked_dir).unwrap();
        symlink(&linked_dir, &f.manager.data).unwrap();
        assert_eq!(f.manager.status()["writable"], false);
        assert!(f.save("# wanted\n").await.is_err());
        assert!(fs::read_dir(&linked_dir).unwrap().next().is_none());
        fs::remove_file(&f.manager.data).unwrap();
        f.save("# valid\n").await.unwrap();
        fs::write(f.manager.data.join(BACKUP), b"broken").unwrap();
        assert_eq!(f.manager.status()["writable"], false);
        assert_eq!(f.save("# another\n").await, Err("hosts_backup_invalid"));
        assert_eq!(fs::read(f.manager.data.join(BACKUP)).unwrap(), b"broken");
        assert_eq!(fs::read(&f.manager.target).unwrap(), b"# valid\n");
    }

    #[tokio::test]
    async fn readonly_failures_never_make_up_empty_content_and_validation_is_bounded() {
        let f = Fixture::new();
        for bad in [vec![0xff], vec![b'x'; MAX_CONTENT + 1], b"bad\0".to_vec()] {
            fs::write(&f.manager.target, bad).unwrap();
            let state = f.manager.status();
            assert!(state.get("content").is_none());
            assert!(state.get("revision").is_none());
            assert_eq!(state["writable"], false);
        }
        fs::write(&f.manager.target, ORIGINAL).unwrap();
        for bad in ["", " \r\n\t", "bad\0", "bad\u{7}", "bad\u{85}"] {
            assert!(f.save(bad).await.is_err());
        }
        assert!(!f.manager.data.exists());
        fs::write(&f.manager.target, b"").unwrap();
        assert_eq!(f.manager.status()["content"], "");
        assert_eq!(f.manager.status()["writable"], true);
        fs::write(&f.manager.target, ORIGINAL).unwrap();
        fs::remove_file(&f.manager.service).unwrap();
        assert_eq!(f.manager.status()["dnsmasq"], false);
        assert_eq!(f.manager.status()["writable"], false);
        assert_eq!(f.save("# wanted\n").await, Err("hosts_dns_unavailable"));
        assert_eq!(fs::read(&f.manager.target).unwrap(), ORIGINAL);
    }

    #[tokio::test]
    async fn a_second_writer_is_locked_and_unknown_parameters_are_rejected() {
        let f = Fixture::new();
        let dir = f.manager.backup_dir(true).unwrap();
        let held = f.manager.lock(&dir).unwrap();
        let second = Manager::fake(&f.root);
        assert_eq!(
            second
                .action(
                    "hosts.save",
                    json!({"content":"# wanted\n","expected_revision":revision(ORIGINAL)})
                )
                .await,
            Err("hosts_busy")
        );
        drop(held);
        for (action, params) in [
            (
                "hosts.restore",
                json!({"expected_revision":revision(ORIGINAL),"path":"/other"}),
            ),
            (
                "hosts.save",
                json!({"content":"# wanted\n","expected_revision":null}),
            ),
        ] {
            assert_eq!(
                f.manager.action(action, params).await,
                Err("invalid_parameter")
            );
        }
        assert_eq!(fs::read(&f.manager.target).unwrap(), ORIGINAL);
    }
}
