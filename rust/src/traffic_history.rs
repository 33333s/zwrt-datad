//! Local daily cellular usage history for the on-demand NMS panel.
//! Only daily totals leave the device; raw UCI and per-sample data stay local.
use crate::model::Snapshot;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

const SAMPLE_SECONDS: i64 = 300;
const MAX_DAYS: usize = 400;
const MAX_FILE_BYTES: u64 = 96 * 1024;
const MAX_DAILY_BYTES: u64 = 100 * 1024 * 1024 * 1024 * 1024;
const MIN_CLOCK_SECONDS: i64 = 1_704_067_200; // 2024-01-01 UTC.

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Day {
    bytes: u64,
    last_counter: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Stored {
    schema: u8,
    last_sample_at: i64,
    days: BTreeMap<String, Day>,
}

impl Default for Stored {
    fn default() -> Self {
        Self {
            schema: 1,
            last_sample_at: 0,
            days: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub date: String,
    pub bytes: u64,
}

pub struct History {
    path: PathBuf,
    stored: Stored,
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
    let days: [u8; 12] = [
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

pub(crate) fn local_date() -> Option<String> {
    // A fixed executable and argument follow the router's own local clock and
    // timezone. Do not silently relabel local traffic as UTC if this fails.
    let output = Command::new("/bin/date").arg("+%Y-%m-%d").output().ok()?;
    if !output.status.success() || output.stdout.len() > 32 {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    let date = raw.trim();
    valid_date(date).then(|| date.to_owned())
}

impl History {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("traffic-history.json");
        let stored = fs::symlink_metadata(&path)
            .ok()
            .filter(|meta| meta.file_type().is_file() && meta.len() <= MAX_FILE_BYTES)
            .and_then(|_| fs::read(&path).ok())
            .and_then(|raw| serde_json::from_slice::<Stored>(&raw).ok())
            .filter(|value| {
                value.schema == 1
                    && value.days.len() <= MAX_DAYS
                    && value.days.keys().all(|date| valid_date(date))
                    && value.days.values().all(|day| {
                        day.bytes <= MAX_DAILY_BYTES && day.last_counter <= MAX_DAILY_BYTES
                    })
            })
            .unwrap_or_default();
        Self { path, stored }
    }

    pub fn days(&self) -> Vec<Usage> {
        self.stored
            .days
            .iter()
            .map(|(date, day)| Usage {
                date: date.clone(),
                bytes: day.bytes,
            })
            .collect()
    }

    pub fn record(&mut self, snapshot: &Snapshot) -> bool {
        let Some(traffic) = snapshot.fields.get("traffic") else {
            return false;
        };
        let (Some(rx), Some(tx)) = (
            traffic.get("day_rx_bytes").and_then(Value::as_u64),
            traffic.get("day_tx_bytes").and_then(Value::as_u64),
        ) else {
            return false;
        };
        let Some(counter) = rx.checked_add(tx) else {
            return false;
        };
        let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
            return false;
        };
        let Ok(now) = i64::try_from(now.as_secs()) else {
            return false;
        };
        if self.stored.last_sample_at != 0
            && (now < self.stored.last_sample_at
                || now - self.stored.last_sample_at < SAMPLE_SECONDS)
        {
            return false;
        }
        let Some(date) = local_date() else {
            return false;
        };
        let changed = self.apply_sample(&date, now, counter);
        if changed && let Err(error) = self.persist() {
            eprintln!("traffic history save failed: {error}");
        }
        changed
    }

    fn apply_sample(&mut self, date: &str, now: i64, counter: u64) -> bool {
        if now < MIN_CLOCK_SECONDS
            || !valid_date(date)
            || counter > MAX_DAILY_BYTES
            || now < self.stored.last_sample_at
            || self
                .stored
                .days
                .last_key_value()
                .is_some_and(|(last, _)| date < last.as_str())
            || (self.stored.days.contains_key(date)
                && now - self.stored.last_sample_at < SAMPLE_SECONDS)
        {
            return false;
        }
        if self
            .stored
            .days
            .get(date)
            .is_some_and(|day| day.last_counter == counter)
        {
            return false;
        }
        let is_new_day = !self.stored.days.contains_key(date);
        let day = self.stored.days.entry(date.to_owned()).or_insert(Day {
            bytes: 0,
            last_counter: 0,
        });
        let increment = if is_new_day {
            counter // Source is a daily total, never subtract yesterday.
        } else if counter >= day.last_counter {
            counter - day.last_counter
        } else {
            counter // Counter reset during the day.
        };
        let next = day.bytes.saturating_add(increment);
        if next > MAX_DAILY_BYTES {
            return false;
        }
        day.bytes = next;
        day.last_counter = counter;
        self.stored.last_sample_at = now;
        while self.stored.days.len() > MAX_DAYS {
            self.stored.days.pop_first();
        }
        true
    }

    fn persist(&self) -> std::io::Result<()> {
        let raw = serde_json::to_vec(&self.stored)?;
        if raw.len() as u64 > MAX_FILE_BYTES {
            return Err(std::io::Error::other("history exceeds size limit"));
        }
        let temp = self.path.with_extension("json.tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)?;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
        file.write_all(&raw)?;
        file.sync_all()?;
        fs::rename(temp, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_samples_survive_restarts_and_count_counter_resets() {
        assert!(!valid_date("12é-01-01"));
        assert!(valid_date("2024-02-29"));
        assert!(!valid_date("2025-02-29"));
        let dir = std::env::temp_dir().join(format!("datad-history-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut history = History::load(&dir);
        let start = MIN_CLOCK_SECONDS + 86_400;
        assert!(history.apply_sample("2025-01-01", start, 100));
        assert!(!history.apply_sample("2025-01-01", start + 10, 110));
        assert!(history.apply_sample("2025-01-01", start + 300, 160));
        assert!(history.apply_sample("2025-01-01", start + 600, 20));
        assert!(history.apply_sample("2025-01-01", start + 900, 70));
        assert!(!history.apply_sample("2025-01-01", start + 1200, 70));
        assert!(!history.apply_sample("2025-01-01", start + 1200, MAX_DAILY_BYTES + 1));
        assert!(history.apply_sample("2025-01-02", start + 1000, 30));
        assert_eq!(history.days()[0].bytes, 230);
        history.persist().unwrap();
        let reopened = History::load(&dir);
        assert_eq!(reopened.days()[0].bytes, 230);
        assert_eq!(reopened.days()[1].bytes, 30);

        let mut midnight = History::load(&dir);
        assert!(midnight.apply_sample("2025-01-03", start + 1400, 40));
        assert_eq!(midnight.days()[2].bytes, 40);
        assert_eq!(
            fs::metadata(dir.join("traffic-history.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn snapshot_record_is_private_and_persists_only_daily_bytes() {
        let dir =
            std::env::temp_dir().join(format!("datad-history-snapshot-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut snapshot = Snapshot {
            ts: 1,
            datad: Default::default(),
            fields: serde_json::Map::new(),
        };
        snapshot.fields.insert(
            "traffic".into(),
            serde_json::json!({"day_rx_bytes":120,"day_tx_bytes":30,"secret":"must-not-persist"}),
        );
        History::load(&dir).record(&snapshot);
        let history = History::load(&dir);
        assert_eq!(history.days().len(), 1);
        assert_eq!(history.days()[0].bytes, 150);
        let raw = fs::read_to_string(dir.join("traffic-history.json")).unwrap();
        assert!(!raw.contains("must-not-persist"));
        fs::remove_dir_all(dir).unwrap();
    }
}
