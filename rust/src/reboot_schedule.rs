//! Device-local daily reboot plan for the NMS-hosted UFI panel.
//! The firmware's weekly/interval window plan is a different feature. Never
//! silently reinterpret it as an exact daily HH:MM reboot.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, DirBuilder, OpenOptions, Permissions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

const FILE_NAME: &str = "reboot-schedule.json";
const MAX_FILE_BYTES: u64 = 4096;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    schema: u8,
    enabled: bool,
    time: String,
    last_attempt_date: String,
}

impl Default for Stored {
    fn default() -> Self {
        Self {
            schema: 1,
            enabled: false,
            time: "00:00".into(),
            last_attempt_date: String::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Clock {
    pub(crate) date: String,
    pub(crate) time: String,
    pub(crate) offset: String,
}

pub(crate) fn valid_time(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 5
        && bytes[2] == b':'
        && [0, 1, 3, 4].into_iter().all(|i| bytes[i].is_ascii_digit())
        && value[0..2].parse::<u8>().is_ok_and(|hour| hour < 24)
        && value[3..5].parse::<u8>().is_ok_and(|minute| minute < 60)
}

pub(crate) fn valid_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || ![0..4, 5..7, 8..10]
            .into_iter()
            .all(|part| bytes[part].iter().all(u8::is_ascii_digit))
    {
        return false;
    }
    let (Ok(year), Ok(month), Ok(day)) = (
        value[0..4].parse::<u16>(),
        value[5..7].parse::<u8>(),
        value[8..10].parse::<u8>(),
    ) else {
        return false;
    };
    if year < 2024 || !(1..=12).contains(&month) {
        return false;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    (1..=days[usize::from(month - 1)]).contains(&day)
}

fn parse_clock(value: &str) -> Option<Clock> {
    let mut parts = value.split_whitespace();
    let (date, time, offset) = (parts.next()?, parts.next()?, parts.next()?);
    let offset_bytes = offset.as_bytes();
    if parts.next().is_some()
        || !valid_date(date)
        || !valid_time(time)
        || offset_bytes.len() != 5
        || !matches!(offset_bytes[0], b'+' | b'-')
        || !offset_bytes[1..].iter().all(u8::is_ascii_digit)
        || offset[1..3].parse::<u8>().is_ok_and(|hours| hours > 14)
        || offset[3..5].parse::<u8>().is_ok_and(|minutes| minutes > 59)
        || (&offset[1..3] == "14" && &offset[3..5] != "00")
    {
        return None;
    }
    Some(Clock {
        date: date.into(),
        time: time.into(),
        offset: offset.into(),
    })
}

pub fn local_clock() -> Option<Clock> {
    let output = Command::new("/bin/date")
        .arg("+%Y-%m-%d %H:%M %z")
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.len() > 64 {
        return None;
    }
    parse_clock(String::from_utf8(output.stdout).ok()?.trim())
}

pub async fn oem_conflict() -> bool {
    if crate::u50_ctl::get().is_some() {
        // The U50S firmware has no ZWRT reboot-schedule setting to collide with.
        return false;
    }
    crate::state::uci_read("zwrt_zte_mc.reboot_schedule.reboot_schedule_enable").await == "1"
}

pub struct Schedule {
    path: PathBuf,
    stored: Stored,
    clock: Option<Clock>,
    conflict: bool,
    error: Option<&'static str>,
}

impl Schedule {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join(FILE_NAME);
        let mut error = None;
        let stored = if path.exists() {
            match fs::symlink_metadata(&path)
                .ok()
                .filter(|meta| meta.is_file() && meta.len() <= MAX_FILE_BYTES)
                .and_then(|_| fs::read(&path).ok())
                .and_then(|raw| serde_json::from_slice::<Stored>(&raw).ok())
                .filter(|value| {
                    value.schema == 1
                        && valid_time(&value.time)
                        && (value.last_attempt_date.is_empty()
                            || valid_date(&value.last_attempt_date))
                }) {
                Some(value) => value,
                None => {
                    error = Some("invalid_config");
                    Stored::default()
                }
            }
        } else {
            Stored::default()
        };
        Self {
            path,
            stored,
            clock: local_clock(),
            conflict: false,
            error,
        }
    }

    pub fn status(&self) -> Value {
        json!({
            "supported":true,
            "enabled":self.stored.enabled,
            "time":self.stored.time,
            "device_time":self.clock.as_ref().map(|clock| format!("{} {}",clock.date,clock.time)),
            "timezone_offset":self.clock.as_ref().map(|clock| clock.offset.as_str()),
            "oem_conflict":self.conflict,
            "last_attempt_date":if self.stored.last_attempt_date.is_empty(){None}else{Some(self.stored.last_attempt_date.as_str())},
            "error":self.error,
        })
    }

    fn save(&self, next: &Stored) -> Result<(), String> {
        let dir = self.path.parent().ok_or("invalid_schedule_path")?;
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|_| "schedule_storage_failed")?;
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "schedule_storage_failed")?
            .as_nanos();
        let temporary = dir.join(format!("{FILE_NAME}.tmp.{}.{}", std::process::id(), suffix));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(|_| "schedule_storage_failed")?;
            let mut raw = serde_json::to_vec(next).map_err(|_| "schedule_storage_failed")?;
            raw.push(b'\n');
            file.write_all(&raw)
                .map_err(|_| "schedule_storage_failed")?;
            file.set_permissions(Permissions::from_mode(0o600))
                .map_err(|_| "schedule_storage_failed")?;
            file.sync_all().map_err(|_| "schedule_storage_failed")?;
            fs::rename(&temporary, &self.path).map_err(|_| "schedule_storage_failed")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    pub fn set(&mut self, enabled: bool, time: &str, conflict: bool) -> Result<Value, String> {
        if !valid_time(time) {
            return Err("invalid_time".into());
        }
        self.conflict = conflict;
        if enabled && conflict {
            return Err("oem_schedule_enabled".into());
        }
        if enabled && self.clock.is_none() {
            return Err("device_clock_unavailable".into());
        }
        if self.stored.enabled != enabled || self.stored.time != time || self.error.is_some() {
            let mut next = self.stored.clone();
            next.enabled = enabled;
            next.time = time.into();
            self.save(&next)?;
            self.stored = next;
            self.error = None;
        }
        Ok(self.status())
    }

    /// Persist the date before returning a reboot request, so a daemon restart
    /// in the same minute cannot schedule another reboot.
    pub fn observe(&mut self, clock: Option<Clock>, conflict: bool) -> bool {
        self.set_environment(clock, conflict);
        let Some(clock) = &self.clock else {
            return false;
        };
        if !self.stored.enabled
            || conflict
            || self.error.is_some()
            || clock.time != self.stored.time
            || self.stored.last_attempt_date == clock.date
        {
            return false;
        }
        let mut next = self.stored.clone();
        next.last_attempt_date = clock.date.clone();
        if self.save(&next).is_err() {
            self.error = Some("schedule_storage_failed");
            return false;
        }
        self.stored = next;
        true
    }

    pub fn reboot_failed(&mut self) {
        self.error = Some("device_reboot_failed");
    }

    pub fn set_environment(&mut self, clock: Option<Clock>, conflict: bool) {
        self.clock = clock;
        self.conflict = conflict;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_and_time_are_strict() {
        assert!(parse_clock("2026-09-29 02:03 +0000").is_some());
        for value in ["2:03", "24:00", "00:60", "02:03;reboot", " 02:03"] {
            assert!(!valid_time(value));
        }
        assert!(parse_clock("2026-09-29 02:03 UTC").is_none());
        assert!(parse_clock("2026-02-31 02:03 +0000").is_none());
        assert!(parse_clock("2026-09-29 02:03 +1560").is_none());
        assert!(parse_clock("2026-09-29 02:03 +1430").is_none());
    }

    #[test]
    fn schedule_is_disabled_by_default_and_runs_once_per_local_day() {
        let dir = std::env::temp_dir().join(format!("datad-reboot-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut schedule = Schedule::load(&dir);
        assert_eq!(schedule.status()["enabled"], false);
        schedule.clock = parse_clock("2026-09-29 02:02 +0000");
        assert!(schedule.set(true, "02:03", true).is_err());
        schedule.set(true, "02:03", false).unwrap();
        assert!(!schedule.observe(parse_clock("2026-09-29 02:02 +0000"), false));
        assert!(!schedule.observe(parse_clock("2026-09-29 02:03 +0000"), true));
        assert!(schedule.observe(parse_clock("2026-09-29 02:03 +0000"), false));
        assert!(!schedule.observe(parse_clock("2026-09-29 02:03 +0000"), false));
        let mut loaded = Schedule::load(&dir);
        assert!(!loaded.observe(parse_clock("2026-09-29 02:03 +0000"), false));
        assert!(loaded.observe(parse_clock("2026-09-30 02:03 +0000"), false));
        assert_eq!(
            fs::metadata(dir.join(FILE_NAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_saved_schedule_fails_closed() {
        let dir = std::env::temp_dir().join(format!("datad-reboot-corrupt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(FILE_NAME),
            br#"{"schema":1,"enabled":true,"time":"99:99","last_attempt_date":""}"#,
        )
        .unwrap();
        let mut schedule = Schedule::load(&dir);
        assert_eq!(schedule.status()["enabled"], false);
        assert_eq!(schedule.status()["error"], "invalid_config");
        assert!(!schedule.observe(parse_clock("2026-09-29 02:03 +0000"), false));
        fs::remove_dir_all(dir).unwrap();
    }
}
