//! Short, serialized root writes for confirmed U50 Hosts operations.
//! Never relax the root mount for ordinary file sessions or EFS.
use std::{fs, sync::Arc};
use tokio::sync::{Mutex, MutexGuard};

static GATE: Mutex<()> = Mutex::const_new(());
type Flags = libc::c_ulong;
trait Mount: Send + Sync {
    fn flags(&self) -> Result<Flags, &'static str>;
    fn allowed(&self) -> bool;
    fn remount(&self, flags: Flags) -> Result<(), &'static str>;
}
fn flags(options: &str) -> Result<Flags, &'static str> {
    let mut value = 0;
    let mut mode = false;
    for option in options.split(',') {
        value |= match option {
            "ro" => {
                mode = true;
                libc::MS_RDONLY
            }
            "rw" => {
                mode = true;
                0
            }
            "nosuid" => libc::MS_NOSUID,
            "nodev" => libc::MS_NODEV,
            "noexec" => libc::MS_NOEXEC,
            "sync" => libc::MS_SYNCHRONOUS,
            "dirsync" => libc::MS_DIRSYNC,
            "noatime" => libc::MS_NOATIME,
            "nodiratime" => libc::MS_NODIRATIME,
            "relatime" => libc::MS_RELATIME,
            "strictatime" => libc::MS_STRICTATIME,
            "lazytime" => libc::MS_LAZYTIME,
            _ => 0,
        };
    }
    if mode {
        Ok(value)
    } else {
        Err("root_mount_unknown")
    }
}
struct Device;
impl Mount for Device {
    fn flags(&self) -> Result<Flags, &'static str> {
        let mounts = fs::read_to_string("/proc/mounts").map_err(|_| "root_mount_unknown")?;
        let row = mounts
            .lines()
            .find_map(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                (fields.len() >= 4
                    && fields[1] == "/"
                    && fields[2] == "ubifs"
                    && fields[0] == "ubi0:rootfs")
                    .then(|| fields[3])
            })
            .ok_or("root_mount_unknown")?;
        flags(row)
    }
    fn allowed(&self) -> bool {
        (unsafe { libc::geteuid() == 0 })
            && fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|s| {
                    s.lines()
                        .find(|line| line.starts_with("CapEff:"))
                        .map(str::to_owned)
                })
                .and_then(|s| u64::from_str_radix(s.split_whitespace().nth(1)?, 16).ok())
                .is_some_and(|caps| caps & (1 << 21) != 0)
            && fs::read_to_string("/sys/class/ubi/ubi0/ro_mode").is_ok_and(|s| s.trim() == "0")
            && self.flags().is_ok()
    }
    fn remount(&self, flags: Flags) -> Result<(), &'static str> {
        // Fixed mountpoint only. MS_REMOUNT retains the existing UBIFS source
        // and superblock options; preserve its per-mount protection flags.
        if unsafe {
            libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REMOUNT | flags,
                std::ptr::null(),
            )
        } != 0
        {
            return Err("root_remount_failed");
        }
        if self.flags()? & libc::MS_RDONLY != flags & libc::MS_RDONLY {
            return Err("root_remount_failed");
        }
        Ok(())
    }
}
pub fn can_open() -> bool {
    crate::u50_ctl::get().is_some() && Device.allowed()
}
pub struct WriteGuard {
    mount: Arc<dyn Mount>,
    original: Option<Flags>,
    _gate: Option<MutexGuard<'static, ()>>,
}
impl WriteGuard {
    pub async fn open() -> Result<Self, &'static str> {
        if crate::u50_ctl::get().is_none() {
            return Ok(Self {
                mount: Arc::new(Device),
                original: None,
                _gate: None,
            });
        }
        Self::acquire(Arc::new(Device)).await
    }
    async fn acquire(mount: Arc<dyn Mount>) -> Result<Self, &'static str> {
        let gate = GATE.lock().await;
        if !mount.allowed() {
            return Err("root_write_unavailable");
        }
        let old = mount.flags()?;
        let mut guard = Self {
            mount,
            original: None,
            _gate: Some(gate),
        };
        if old & libc::MS_RDONLY != 0 {
            // Arm restoration BEFORE the syscall, including failed readback.
            guard.original = Some(old);
            guard.mount.remount(old & !libc::MS_RDONLY)?;
        }
        Ok(guard)
    }
    pub fn finish(&mut self) -> Result<(), &'static str> {
        if let Some(original) = self.original {
            self.mount
                .remount(original)
                .map_err(|_| "root_restore_failed")?;
            self.original = None;
        }
        Ok(())
    }
}
impl Drop for WriteGuard {
    fn drop(&mut self) {
        if self.finish().is_err() {
            eprintln!("U50 root mount restoration failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake(std::sync::Mutex<(Flags, Vec<Flags>, bool)>);
    impl Mount for Fake {
        fn flags(&self) -> Result<Flags, &'static str> {
            Ok(self.0.lock().unwrap().0)
        }
        fn allowed(&self) -> bool {
            true
        }
        fn remount(&self, flags: Flags) -> Result<(), &'static str> {
            let mut state = self.0.lock().unwrap();
            state.1.push(flags);
            if state.2 {
                state.2 = false;
                return Err("injected_failure");
            }
            state.0 = flags;
            Ok(())
        }
    }
    #[tokio::test]
    async fn restores_read_only_and_other_flags_on_finish_and_early_drop() {
        let original = flags("ro,nosuid,nodev,noexec,relatime").unwrap();
        let mount = Arc::new(Fake(std::sync::Mutex::new((original, vec![], false))));
        {
            let _guard = WriteGuard::acquire(mount.clone()).await.unwrap();
            assert_eq!(mount.flags().unwrap(), original & !libc::MS_RDONLY);
        }
        assert_eq!(mount.flags().unwrap(), original);
        let mut guard = WriteGuard::acquire(mount.clone()).await.unwrap();
        mount.0.lock().unwrap().2 = true;
        assert_eq!(guard.finish().unwrap_err(), "root_restore_failed");
        drop(guard);
        assert_eq!(mount.flags().unwrap(), original, "Drop retries restoration");
    }
    #[tokio::test]
    async fn preexisting_writable_mount_is_not_changed() {
        let mount = Arc::new(Fake(std::sync::Mutex::new((
            flags("rw,relatime").unwrap(),
            vec![],
            false,
        ))));
        {
            let _guard = WriteGuard::acquire(mount.clone()).await.unwrap();
        }
        assert!(mount.0.lock().unwrap().1.is_empty());
        assert!(flags("relatime").is_err());
    }
}
