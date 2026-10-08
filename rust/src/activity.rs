//! Bounded device-local execution history; never stores control parameters or messages.
//!
//! How much is recorded, and when it reaches flash, depends on the level:
//! only `detailed` writes every entry at once; the others keep entries in
//! memory and write them together on a fixed interval (and before the daemon
//! stops or the device reboots), so a quiet or busy device costs few writes.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, RwLock, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

/// Default time between writes of buffered entries.
const DEFAULT_FLUSH_SECONDS: u64 = 300;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Nothing is recorded.
    Off,
    /// Failures, and what recovery does to the network (redial, reboot, back online).
    Basic,
    /// Everything above plus successful deliveries, task runs and recovery
    /// settings. The default; buffered and written on an interval.
    #[default]
    Standard,
    /// Everything above plus each change in the recovery probe result. The
    /// only level that writes every entry immediately.
    Detailed,
}
impl Level {
    pub const NAMES: [&'static str; 4] = ["off", "basic", "standard", "detailed"];
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "off" => Some(Self::Off),
            "basic" => Some(Self::Basic),
            "standard" => Some(Self::Standard),
            "detailed" => Some(Self::Detailed),
            _ => None,
        }
    }
}
/// The lowest level at which an event is recorded.
fn rank(category: &str, action: &str, result: &str) -> Level {
    match (category, action) {
        ("recovery", "probe") => Level::Detailed,
        _ if result == "failed" => Level::Basic,
        ("recovery", "redial" | "reboot" | "online") => Level::Basic,
        _ => Level::Standard,
    }
}
/// Time between writes of buffered entries. `ZWRT_DATAD_ACTIVITY_FLUSH_SECONDS`
/// (1-3600) is for tests.
pub fn flush_interval() -> Duration {
    let seconds = std::env::var("ZWRT_DATAD_ACTIVITY_FLUSH_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| (1..=3600).contains(value))
        .unwrap_or(DEFAULT_FLUSH_SECONDS);
    Duration::from_secs(seconds)
}

type Shared = Arc<Mutex<Journal>>;
static ACTIVE: OnceLock<RwLock<Weak<Mutex<Journal>>>> = OnceLock::new();
pub fn install(shared: &Shared) {
    *ACTIVE
        .get_or_init(|| RwLock::new(Weak::new()))
        .write()
        .unwrap() = Arc::downgrade(shared);
}
/// Writes buffered entries now. Called before anything that ends the process
/// (a reboot or poweroff) so the entries leading up to it are not lost.
pub async fn flush_active() {
    let shared = ACTIVE.get().and_then(|cell| cell.read().ok()?.upgrade());
    if let Some(shared) = shared {
        shared.lock().await.flush();
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
pub fn save<T: Serialize>(path: &Path, data: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec(data).map_err(|_| "storage_failed")?;
    let tmp = path.with_extension("tmp");
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)
        .map_err(|_| "storage_failed")?;
    f.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|_| "storage_failed")?;
    f.write_all(&bytes)
        .and_then(|_| f.sync_all())
        .and_then(|_| fs::rename(tmp, path))
        .map_err(|_| "storage_failed".into())
}
pub fn read<T: serde::de::DeserializeOwned>(path: &Path, max: u64) -> Option<T> {
    let m = fs::symlink_metadata(path).ok()?;
    if !m.is_file() || m.len() > max {
        return None;
    }
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    pub timestamp: u64,
    pub device_time: String,
    pub category: String,
    pub action: String,
    pub result: String,
    pub reason: String,
    pub task: String,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Stored {
    next_id: u64,
    entries: Vec<Entry>,
    #[serde(default)]
    level: Level,
}
pub struct Journal {
    path: PathBuf,
    stored: Stored,
    pub storage_ok: bool,
    /// Entries in memory that are not on flash yet.
    dirty: bool,
}
impl Journal {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("activity.json");
        let stored = read::<Stored>(&path, 256 * 1024)
            .filter(|s| {
                s.entries.len() <= 300
                    && s.entries.iter().all(|e| {
                        e.id < s.next_id
                            && e.task.len() <= 80
                            && e.action.len() <= 80
                            && e.reason.len() <= 80
                    })
            })
            .unwrap_or_default();
        Self {
            path,
            stored,
            storage_ok: true,
            dirty: false,
        }
    }
    #[cfg(test)]
    pub fn level(&self) -> Level {
        self.stored.level
    }
    /// Takes effect at once and is saved (with any buffered entries) right away.
    pub fn set_level(&mut self, level: Level) -> Result<(), String> {
        let mut next = self.stored.clone();
        next.level = level;
        save(&self.path, &next)?;
        self.stored = next;
        self.dirty = false;
        self.storage_ok = true;
        Ok(())
    }
    /// Writes buffered entries. A failed write keeps them buffered for the
    /// next attempt.
    pub fn flush(&mut self) {
        if self.dirty {
            self.storage_ok = save(&self.path, &self.stored).is_ok();
            self.dirty = !self.storage_ok;
        }
    }
    pub fn push(&mut self, category: &str, action: &str, result: &str, reason: &str, task: &str) {
        if self.stored.level < rank(category, action, result) {
            return;
        }
        let at = now();
        // Repeated background source failures are one event per five minutes.
        if result == "failed"
            && self.stored.entries.last().is_some_and(|e| {
                e.category == category
                    && e.action == action
                    && e.result == result
                    && e.reason == reason
                    && e.task == task
                    && at.saturating_sub(e.timestamp) < 300
            })
        {
            return;
        }
        let device_time = crate::reboot_schedule::local_clock()
            .map(|c| format!("{} {}", c.date, c.time))
            .unwrap_or_default();
        let id = self.stored.next_id.max(1);
        self.stored.next_id = id.saturating_add(1);
        self.stored.entries.push(Entry {
            id,
            timestamp: at,
            device_time,
            category: category.into(),
            action: action.into(),
            result: result.into(),
            reason: reason.into(),
            task: task.chars().take(80).collect(),
        });
        if self.stored.entries.len() > 300 {
            self.stored.entries.remove(0);
        }
        self.dirty = true;
        if self.stored.level == Level::Detailed {
            self.flush();
        }
    }
    pub fn view(&self, before: Option<u64>, category: Option<&str>, failed: bool) -> Value {
        let all: Vec<_> = self
            .stored
            .entries
            .iter()
            .rev()
            .filter(|e| {
                before.is_none_or(|id| e.id < id)
                    && category.is_none_or(|c| e.category == c)
                    && (!failed || e.result == "failed")
            })
            .collect();
        let page: Vec<_> = all.iter().take(100).collect();
        json!({"entries":page,"next_before":if all.len()>100 {page.last().map(|e|e.id)}else{None},
            "max_entries":300,"storage_ok":self.storage_ok,
            "level":self.stored.level,"levels":Level::NAMES,
            "flush_interval_seconds":if self.stored.level == Level::Detailed {Value::Null} else {json!(flush_interval().as_secs())},
            "unsaved":self.dirty})
    }
    pub fn clear(&mut self) -> Result<(), String> {
        let next = Stored {
            next_id: self.stored.next_id,
            entries: vec![],
            level: self.stored.level,
        };
        save(&self.path, &next)?;
        self.stored = next;
        self.dirty = false;
        self.storage_ok = true;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_private_history_pagination_restart_and_clear() {
        let dir = std::env::temp_dir().join(format!("datad-activity-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut j = Journal::load(&dir);
        for _ in 0..305 {
            j.push("task", "device.reboot", "requested", "", "fixture");
        }
        assert!(
            Journal::load(&dir).stored.entries.is_empty(),
            "buffered at the default level"
        );
        j.flush();
        let j2 = Journal::load(&dir);
        let view = j2.view(None, None, false);
        assert_eq!(view["entries"].as_array().unwrap().len(), 100);
        assert_eq!(view["entries"][0]["id"], 305);
        assert_eq!(j2.view(Some(206), None, false)["entries"][0]["id"], 205);
        assert_eq!(j2.stored.entries.len(), 300);
        assert_eq!(
            fs::metadata(dir.join("activity.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        j.push("notification", "sms", "failed", "smtp_auth_failed", "");
        j.push("notification", "sms", "failed", "smtp_auth_failed", "");
        j.flush();
        assert_eq!(
            j.view(None, Some("notification"), true)["entries"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        j.clear().unwrap();
        assert!(Journal::load(&dir).stored.entries.is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "datad-activity-{name}-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn file_len(dir: &Path) -> Option<u64> {
        fs::metadata(dir.join("activity.json"))
            .ok()
            .map(|m| m.len())
    }

    #[test]
    fn each_level_records_only_what_it_promises() {
        let events = [
            ("notification", "sms", "success"),
            ("notification", "sms", "failed"),
            ("task", "device.reboot", "requested"),
            ("task", "sim.set_slot", "failed"),
            ("recovery", "enable", "success"),
            ("recovery", "redial", "requested"),
            ("recovery", "reboot", "failed"),
            ("recovery", "online", "success"),
            ("recovery", "probe", "failed"),
            ("recovery", "probe", "success"),
        ];
        let expected = [
            (Level::Off, 0),
            (Level::Basic, 5),
            (Level::Standard, 8),
            (Level::Detailed, 10),
        ];
        for (level, count) in expected {
            let dir = temp("levels");
            let mut j = Journal::load(&dir);
            j.set_level(level).unwrap();
            for (n, (category, action, result)) in events.iter().enumerate() {
                // Distinct reasons keep the duplicate filter out of the way.
                j.push(category, action, result, &format!("r{n}"), "");
            }
            assert_eq!(j.stored.entries.len(), count, "{level:?}");
            fs::remove_dir_all(dir).unwrap();
        }
        assert_eq!(Level::default(), Level::Standard);
        assert!(
            Level::Off < Level::Basic
                && Level::Basic < Level::Standard
                && Level::Standard < Level::Detailed
        );
        for name in Level::NAMES {
            assert_eq!(
                serde_json::to_value(Level::parse(name).unwrap()).unwrap(),
                name
            );
        }
        assert_eq!(Level::parse("verbose"), None);
    }

    #[test]
    fn only_the_detailed_level_writes_every_entry_at_once() {
        let dir = temp("write");
        let mut j = Journal::load(&dir);
        assert_eq!(j.level(), Level::Standard);
        j.push("notification", "sms", "success", "", "");
        j.push("notification", "sms", "failed", "delivery_failed", "");
        assert_eq!(file_len(&dir), None, "nothing written yet");
        assert_eq!(j.view(None, None, false)["unsaved"], true);
        assert_eq!(
            j.view(None, None, false)["entries"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        j.flush();
        let written = file_len(&dir).unwrap();
        assert_eq!(j.view(None, None, false)["unsaved"], false);
        // Nothing new, nothing written.
        j.flush();
        assert_eq!(file_len(&dir), Some(written));
        for level in [Level::Basic, Level::Standard] {
            j.set_level(level).unwrap();
            let before = fs::read(dir.join("activity.json")).unwrap();
            j.push("notification", "sms", "failed", &format!("{level:?}"), "");
            assert_eq!(
                fs::read(dir.join("activity.json")).unwrap(),
                before,
                "{level:?}"
            );
        }
        // Switching saves what was buffered, then every entry is immediate.
        let before = fs::read(dir.join("activity.json")).unwrap();
        j.set_level(Level::Detailed).unwrap();
        assert_ne!(fs::read(dir.join("activity.json")).unwrap(), before);
        let before = Journal::load(&dir).stored.entries.len();
        j.push("recovery", "probe", "failed", "", "");
        assert_eq!(Journal::load(&dir).stored.entries.len(), before + 1);
        assert_eq!(
            j.view(None, None, false)["flush_interval_seconds"],
            Value::Null
        );
        assert_eq!(j.view(None, None, false)["unsaved"], false);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_level_survives_restart_and_clearing_and_a_failed_write_is_retried() {
        let dir = temp("persist");
        let mut j = Journal::load(&dir);
        j.set_level(Level::Basic).unwrap();
        assert_eq!(Journal::load(&dir).level(), Level::Basic);
        j.push("notification", "sms", "failed", "delivery_failed", "");
        j.flush();
        j.clear().unwrap();
        assert_eq!(Journal::load(&dir).level(), Level::Basic);
        assert!(Journal::load(&dir).stored.entries.is_empty());
        // A write that fails leaves the entries buffered and is retried.
        let real = j.path.clone();
        j.path = dir.join("missing/activity.json");
        j.push("notification", "sms", "failed", "storage_failed", "");
        j.flush();
        assert!(!j.storage_ok);
        assert_eq!(j.view(None, None, false)["unsaved"], true);
        j.path = real;
        j.flush();
        assert!(j.storage_ok);
        assert_eq!(Journal::load(&dir).stored.entries.len(), 1);
        // An older file without a level keeps the default.
        fs::write(dir.join("activity.json"), br#"{"next_id":1,"entries":[]}"#).unwrap();
        assert_eq!(Journal::load(&dir).level(), Level::Standard);
        fs::remove_dir_all(dir).unwrap();
    }
}
