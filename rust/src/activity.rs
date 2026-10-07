//! Bounded device-local execution history; never stores control parameters or messages.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

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
}
pub struct Journal {
    path: PathBuf,
    stored: Stored,
    pub storage_ok: bool,
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
        }
    }
    pub fn push(&mut self, category: &str, action: &str, result: &str, reason: &str, task: &str) {
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
        self.storage_ok = save(&self.path, &self.stored).is_ok();
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
            "max_entries":300,"storage_ok":self.storage_ok})
    }
    pub fn clear(&mut self) -> Result<(), String> {
        let next = Stored {
            next_id: self.stored.next_id,
            entries: vec![],
        };
        save(&self.path, &next)?;
        self.stored = next;
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
}
