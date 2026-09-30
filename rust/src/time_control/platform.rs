//! Fixed system resources, private ownership journal, and conflict-aware restore.
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::process::Command;

const PATHS: [&str; 3] = ["etc/localtime.datad", "etc/localtime", "tmp/TZ"];
const SERVICE: &str = "etc/init.d/zte_topsw_ntp";
const OPTIONS: [&str; 2] = ["system.@system[0].timezone", "system.@system[0].zonename"];
pub(super) trait Clock: Send + Sync {
    fn wall(&self) -> Option<i128>;
    fn set(&self, epoch_ns: i128) -> Result<(), &'static str>;
    fn writable(&self) -> bool;
    #[cfg(test)]
    fn fixture_sample(&self) -> Option<Result<i128, &'static str>> {
        None
    }
}
struct KernelClock;
impl Clock for KernelClock {
    fn wall(&self) -> Option<i128> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|v| v.as_nanos() as i128)
    }
    fn writable(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|s| {
                    s.lines()
                        .find(|l| l.starts_with("CapEff:"))
                        .map(str::to_owned)
                })
                .and_then(|v| u64::from_str_radix(v.split_whitespace().nth(1)?, 16).ok())
                .is_some_and(|v| v & (1 << 25) != 0)
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }
    fn set(&self, epoch_ns: i128) -> Result<(), &'static str> {
        if !self.writable() {
            return Err("clock_permission_denied");
        }
        #[cfg(target_os = "linux")]
        {
            let tv = libc::timespec {
                tv_sec: (epoch_ns / 1_000_000_000)
                    .try_into()
                    .map_err(|_| "invalid_time")?,
                tv_nsec: (epoch_ns % 1_000_000_000)
                    .try_into()
                    .map_err(|_| "invalid_time")?,
            };
            // Only called under the serialized transition gate after a validated sample.
            if unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &tv) } != 0 {
                return Err("clock_apply_failed");
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = epoch_ns;
            Err("clock_unsupported")
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Image {
    Missing,
    File { bytes: Vec<u8>, mode: u32 },
    Link { target: PathBuf },
}
fn image(path: &Path) -> Result<Image, &'static str> {
    let meta = match fs::symlink_metadata(path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Image::Missing),
        Err(_) => return Err("timezone_read_failed"),
    };
    if meta.file_type().is_symlink() {
        let target = fs::read_link(path).map_err(|_| "timezone_read_failed")?;
        if target.as_os_str().len() > 1024 {
            return Err("timezone_invalid_file");
        };
        return Ok(Image::Link { target });
    }
    if !meta.is_file() || meta.len() > 32768 {
        return Err("timezone_invalid_file");
    }
    Ok(Image::File {
        bytes: fs::read(path).map_err(|_| "timezone_read_failed")?,
        mode: meta.permissions().mode() & 0o777,
    })
}

pub(super) fn atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<(), &'static str> {
    let parent = path.parent().ok_or("storage_failed")?;
    let temp = parent.join(format!(".datad-time-{}.new", super::new_tag()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temp)
            .map_err(|_| "storage_failed")?;
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|_| "storage_failed")?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| "storage_failed")?;
        fs::rename(&temp, path).map_err(|_| "storage_failed")?;
        fs::File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|_| "storage_failed")
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
fn put(path: &Path, value: &Image) -> Result<(), &'static str> {
    match value {
        Image::Missing => {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err("timezone_restore_failed"),
            };
            Ok(())
        }
        Image::File { bytes, mode } => atomic(path, bytes, *mode),
        Image::Link { target } => {
            let temporary = path.with_file_name(format!(".datad-time-{}.link", super::new_tag()));
            symlink(target, &temporary).map_err(|_| "timezone_apply_failed")?;
            let result = fs::rename(&temporary, path).map_err(|_| "timezone_apply_failed");
            if result.is_err() {
                let _ = fs::remove_file(temporary);
            }
            result
        }
    }
}
fn zone() -> Vec<u8> {
    // TZif v1, a single modern fixed UTC+8 type, no transitions or DST.
    let mut bytes = vec![0u8; 44];
    bytes[0..4].copy_from_slice(b"TZif");
    bytes[39] = 1;
    bytes[43] = 4;
    bytes.extend_from_slice(&28800i32.to_be_bytes());
    bytes.extend_from_slice(&[0, 0]);
    bytes.extend_from_slice(b"CST\0");
    bytes
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceBefore {
    enabled: bool,
    running: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Journal {
    schema: u8,
    #[serde(default)]
    complete: bool,
    before: Vec<Image>,
    applied: Vec<Image>,
    service: Option<ServiceBefore>,
    service_owned: bool,
    uci: Option<[Option<String>; 2]>,
    uci_owned: bool,
}
impl Journal {
    pub fn oem_owned(&self) -> bool {
        self.service_owned
    }
    pub fn service_disabled_before(&self) -> Option<bool> {
        self.service.as_ref().map(|s| !s.enabled && !s.running)
    }
}
pub(super) struct Platform {
    root: PathBuf,
    pub clock: Arc<dyn Clock>,
    journal: PathBuf,
}
impl Platform {
    pub fn new(dir: &Path) -> Self {
        Self {
            root: "/".into(),
            clock: Arc::new(KernelClock),
            journal: dir.join("time-ownership.json"),
        }
    }
    pub fn write_supported(&self) -> bool {
        if !self.clock.writable() {
            return false;
        };
        for parent in ["etc", "tmp"] {
            let Ok(path) = std::ffi::CString::new(self.path(parent).as_os_str().as_encoded_bytes())
            else {
                return false;
            };
            if unsafe { libc::access(path.as_ptr(), libc::W_OK) } != 0 {
                return false;
            };
            let mut status = std::mem::MaybeUninit::<libc::statvfs>::uninit();
            if unsafe { libc::statvfs(path.as_ptr(), status.as_mut_ptr()) } != 0 {
                return false;
            };
            if unsafe { status.assume_init() }.f_flag & libc::ST_RDONLY != 0 {
                return false;
            };
        }
        true
    }
    #[cfg(test)]
    pub fn fake(dir: &Path, root: PathBuf, clock: Arc<dyn Clock>) -> Self {
        Self {
            root,
            clock,
            journal: dir.join("time-ownership.json"),
        }
    }
    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }
    async fn run(&self, program: &str, args: &[&str]) -> Result<bool, &'static str> {
        let mut command = Command::new(self.path(program));
        command
            .args(args)
            .env("LC_ALL", "C")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let status = tokio::time::timeout(Duration::from_secs(3), command.status())
            .await
            .map_err(|_| "system_command_timeout")?
            .map_err(|_| "system_command_failed")?;
        match status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err("system_command_failed"),
        }
    }
    async fn uci_get(&self, key: &str) -> Result<Option<String>, &'static str> {
        let output = tokio::time::timeout(
            Duration::from_secs(2),
            Command::new(self.path("sbin/uci"))
                .args(["-q", "get", key])
                .env("LC_ALL", "C")
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| "timezone_read_failed")?
        .map_err(|_| "timezone_read_failed")?;
        if output.status.code() == Some(1) {
            return Ok(None);
        };
        if !output.status.success() || output.stdout.len() > 128 {
            return Err("timezone_read_failed");
        };
        let text = String::from_utf8(output.stdout).map_err(|_| "timezone_read_failed")?;
        let text = text.trim();
        if text.chars().any(char::is_control) {
            return Err("timezone_read_failed");
        };
        Ok(Some(text.into()))
    }
    async fn uci_values(&self) -> Result<Option<[Option<String>; 2]>, &'static str> {
        if !self.path("sbin/uci").exists() {
            return Ok(None);
        };
        Ok(Some([
            self.uci_get(OPTIONS[0]).await?,
            self.uci_get(OPTIONS[1]).await?,
        ]))
    }
    async fn uci_pending(&self) -> Result<bool, &'static str> {
        if !self.path("sbin/uci").exists() {
            return Ok(false);
        };
        let output = tokio::time::timeout(
            Duration::from_secs(2),
            Command::new(self.path("sbin/uci"))
                .args(["-q", "changes", "system"])
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| "timezone_read_failed")?
        .map_err(|_| "timezone_read_failed")?;
        if !output.status.success() || output.stdout.len() > 8192 {
            return Err("timezone_read_failed");
        };
        Ok(output.stdout.iter().any(|b| !b.is_ascii_whitespace()))
    }
    async fn uci_set(&self, values: &[Option<String>; 2]) -> Result<(), &'static str> {
        for (key, value) in OPTIONS.iter().zip(values) {
            let ok = if let Some(value) = value {
                self.run("sbin/uci", &["set", &format!("{key}={value}")])
                    .await?
            } else {
                self.run("sbin/uci", &["-q", "delete", key]).await?
            };
            if !ok && value.is_some() {
                return Err("timezone_apply_failed");
            };
        }
        if !self.run("sbin/uci", &["commit", "system"]).await? {
            return Err("timezone_apply_failed");
        };
        if self.uci_values().await?.as_ref() != Some(values) {
            return Err("timezone_readback_failed");
        };
        Ok(())
    }
    pub fn oem_supported(&self) -> bool {
        fs::symlink_metadata(self.path(SERVICE)).is_ok_and(|m| {
            m.is_file()
                && m.uid()
                    == if self.root == Path::new("/") {
                        0
                    } else {
                        unsafe { libc::geteuid() }
                    }
                && m.permissions().mode() & 0o022 == 0
                && m.permissions().mode() & 0o111 != 0
        })
    }
    async fn service_state(&self) -> Result<ServiceBefore, &'static str> {
        Ok(ServiceBefore {
            enabled: self.run(SERVICE, &["enabled"]).await?,
            running: self.run(SERVICE, &["status"]).await?,
        })
    }
    pub fn load_journal(&self) -> Result<Option<Journal>, &'static str> {
        let meta = match fs::symlink_metadata(&self.journal) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("ownership_read_failed"),
        };
        if !meta.is_file()
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.len() > 512 * 1024
            || meta.permissions().mode() & 0o077 != 0
        {
            return Err("ownership_invalid");
        };
        let journal: Journal =
            serde_json::from_slice(&fs::read(&self.journal).map_err(|_| "ownership_read_failed")?)
                .map_err(|_| "ownership_invalid")?;
        if journal.schema != 1 || journal.before.len() != 3 || journal.applied.len() != 3 {
            return Err("ownership_invalid");
        };
        Ok(Some(journal))
    }
    fn save(&self, journal: &Journal) -> Result<(), &'static str> {
        atomic(
            &self.journal,
            &serde_json::to_vec(journal).map_err(|_| "storage_failed")?,
            0o600,
        )
    }
    pub async fn prepare(&self, calibration: bool) -> Result<Journal, &'static str> {
        if self.uci_pending().await? {
            return Err("restore_conflict");
        };
        if let Some(mut existing) = self.load_journal()? {
            if !existing.complete {
                self.restore(&mut existing).await?;
            } else {
                self.verify_owned(&existing).await?;
                if calibration && existing.service.is_none() && self.oem_supported() {
                    existing.service = Some(self.service_state().await?);
                }
                existing.complete = false;
                self.save(&existing)?;
                return Ok(existing);
            }
        };
        let before = PATHS
            .iter()
            .map(|p| image(&self.path(p)))
            .collect::<Result<Vec<_>, _>>()?;
        let service = if calibration && self.oem_supported() {
            Some(self.service_state().await?)
        } else {
            None
        };
        let uci = self.uci_values().await?;
        let journal = Journal {
            schema: 1,
            complete: false,
            applied: before.clone(),
            before,
            service,
            service_owned: false,
            uci,
            uci_owned: false,
        };
        self.save(&journal)?;
        Ok(journal)
    }
    pub fn seal(&self, journal: &mut Journal) -> Result<(), &'static str> {
        journal.complete = true;
        self.save(journal)
    }
    async fn verify_owned(&self, journal: &Journal) -> Result<(), &'static str> {
        for (p, expected) in PATHS.iter().zip(&journal.applied) {
            if image(&self.path(p))? != *expected {
                return Err("restore_conflict");
            };
        }
        if journal.uci_owned
            && self.uci_values().await?
                != Some([Some("CST-8".into()), Some("Asia/Shanghai".into())])
        {
            return Err("restore_conflict");
        };
        if journal.service_owned {
            let current = self.service_state().await?;
            if current.enabled || current.running {
                return Err("restore_conflict");
            };
        }
        Ok(())
    }
    pub async fn take_over(&self, journal: &mut Journal) -> Result<(), &'static str> {
        if journal.service.is_none() || journal.service_owned {
            return Ok(());
        };
        // Ownership is journaled before mutation, including partial failure.
        journal.service_owned = true;
        self.save(journal)?;
        if !self.run(SERVICE, &["disable"]).await? || !self.run(SERVICE, &["stop"]).await? {
            return Err("oem_apply_failed");
        };
        let state = self.service_state().await?;
        if state.enabled || state.running {
            return Err("oem_readback_failed");
        };
        Ok(())
    }
    pub async fn timezone(&self, journal: &mut Journal) -> Result<(), &'static str> {
        let values = [
            Image::File {
                bytes: zone(),
                mode: 0o644,
            },
            Image::Link {
                target: self.path(PATHS[0]),
            },
            Image::File {
                bytes: b"CST-8\n".to_vec(),
                mode: 0o644,
            },
        ];
        for (index, value) in values.into_iter().enumerate() {
            if image(&self.path(PATHS[index]))? != journal.applied[index] {
                return Err("restore_conflict");
            };
            journal.applied[index] = value.clone();
            self.save(journal)?;
            put(&self.path(PATHS[index]), &value)?;
        }
        if journal.uci.is_some() {
            journal.uci_owned = true;
            self.save(journal)?;
            self.uci_set(&[Some("CST-8".into()), Some("Asia/Shanghai".into())])
                .await?;
        }
        self.verify_owned(journal).await
    }
    pub async fn restore(&self, journal: &mut Journal) -> Result<(), &'static str> {
        // Refuse all destructive restores when a resource has an external owner.
        for (index, p) in PATHS.iter().enumerate() {
            let current = image(&self.path(p))?;
            if current != journal.applied[index] && current != journal.before[index] {
                return Err("restore_conflict");
            };
        }
        if journal.uci_owned {
            let current = self.uci_values().await?.ok_or("restore_conflict")?;
            let before = journal.uci.as_ref().ok_or("ownership_invalid")?;
            for index in 0..2 {
                let wanted = if index == 0 { "CST-8" } else { "Asia/Shanghai" };
                if current[index] != before[index] && current[index].as_deref() != Some(wanted) {
                    return Err("restore_conflict");
                };
            }
        }
        if let Some(before) = &journal.service
            && journal.service_owned
        {
            let current = self.service_state().await?;
            if (current.enabled || current.running)
                && (current.enabled != before.enabled || current.running != before.running)
            {
                return Err("restore_conflict");
            };
        }
        for index in (0..3).rev() {
            put(&self.path(PATHS[index]), &journal.before[index])?;
            journal.applied[index] = journal.before[index].clone();
            self.save(journal)?;
        }
        if let Some(before) = &journal.uci
            && journal.uci_owned
        {
            self.uci_set(before).await?;
            journal.uci_owned = false;
            self.save(journal)?;
        }
        if let Some(before) = &journal.service
            && journal.service_owned
        {
            let now = self.service_state().await?;
            // Already restored states are idempotent; externally changed mixed states conflict.
            if (now.enabled || now.running)
                && (now.enabled != before.enabled || now.running != before.running)
            {
                return Err("restore_conflict");
            };
            if !self
                .run(
                    SERVICE,
                    &[if before.enabled { "enable" } else { "disable" }],
                )
                .await?
                || !self
                    .run(SERVICE, &[if before.running { "start" } else { "stop" }])
                    .await?
            {
                return Err("oem_restore_failed");
            };
            let after = self.service_state().await?;
            if after.enabled != before.enabled || after.running != before.running {
                return Err("oem_restore_failed");
            };
            journal.service_owned = false;
            self.save(journal)?;
        }
        fs::remove_file(&self.journal).map_err(|_| "storage_failed")?;
        Ok(())
    }
    pub fn rtc_supported(&self) -> bool {
        self.path("dev/rtc0").exists()
    }
    pub async fn rtc(&self) -> Option<bool> {
        if !self.rtc_supported() {
            return None;
        };
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            #[derive(Clone, Copy)]
            #[repr(C)]
            struct RtcTime {
                sec: i32,
                min: i32,
                hour: i32,
                day: i32,
                month: i32,
                year: i32,
                wday: i32,
                yday: i32,
                isdst: i32,
            }
            let result = (|| {
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(self.path("dev/rtc0"))
                    .ok()?;
                let epoch: libc::time_t = (self.clock.wall()? / 1_000_000_000).try_into().ok()?;
                let mut t = std::mem::MaybeUninit::<libc::tm>::uninit();
                if unsafe { libc::gmtime_r(&epoch, t.as_mut_ptr()) }.is_null() {
                    return None;
                };
                let t = unsafe { t.assume_init() };
                let wanted = RtcTime {
                    sec: t.tm_sec,
                    min: t.tm_min,
                    hour: t.tm_hour,
                    day: t.tm_mday,
                    month: t.tm_mon,
                    year: t.tm_year,
                    wday: t.tm_wday,
                    yday: t.tm_yday,
                    isdst: 0,
                };
                // Linux RTC_SET_TIME/RTC_RD_TIME encode the fixed nine-int UAPI structure.
                let set = 0x4000_0000u32
                    | (std::mem::size_of::<RtcTime>() as u32) << 16
                    | (b'p' as u32) << 8
                    | 0x0a;
                let get = 0x8000_0000u32
                    | (std::mem::size_of::<RtcTime>() as u32) << 16
                    | (b'p' as u32) << 8
                    | 0x09;
                if unsafe { libc::ioctl(file.as_raw_fd(), set as _, &wanted) } != 0 {
                    return Some(false);
                };
                let mut actual = wanted;
                if unsafe { libc::ioctl(file.as_raw_fd(), get as _, &mut actual) } != 0 {
                    return Some(false);
                };
                let mut tm = t;
                tm.tm_sec = actual.sec;
                tm.tm_min = actual.min;
                tm.tm_hour = actual.hour;
                tm.tm_mday = actual.day;
                tm.tm_mon = actual.month;
                tm.tm_year = actual.year;
                Some((unsafe { libc::timegm(&mut tm) } as i128 - epoch as i128).abs() <= 2)
            })();
            Some(result.unwrap_or(false))
        }
        #[cfg(not(target_os = "linux"))]
        {
            Some(false)
        }
    }
    pub async fn ownership_valid(&self) -> Result<(), &'static str> {
        if let Some(j) = self.load_journal()? {
            self.verify_owned(&j).await?
        };
        Ok(())
    }
    pub fn journal_present(&self) -> bool {
        self.journal.exists()
    }
}
