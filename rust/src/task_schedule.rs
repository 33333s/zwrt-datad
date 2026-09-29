//! Bounded device-local tasks for the NMS-hosted UFI panel. The original UFI
//! accepts arbitrary JSON actions; this scheduler deliberately accepts only
//! existing reviewed datad controls and never executes shell input.
use crate::reboot_schedule::{self, Clock};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    fs::{self, DirBuilder, OpenOptions, Permissions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const FILE_NAME: &str = "scheduled-tasks.json";
const MAX_FILE_BYTES: u64 = 32 * 1024;
const MAX_TASKS: usize = 16;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TaskInput {
    pub id: String,
    pub time: String,
    pub repeat_daily: bool,
    pub action: String,
    pub params: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Task {
    #[serde(flatten)]
    input: TaskInput,
    last_attempt_date: String,
    has_triggered: bool,
    last_result: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    schema: u8,
    tasks: Vec<Task>,
}

impl Default for Stored {
    fn default() -> Self {
        Self {
            schema: 1,
            tasks: Vec::new(),
        }
    }
}

fn one_of(value: &Value, values: &[&str]) -> bool {
    value.as_str().is_some_and(|name| values.contains(&name))
}

fn exact_object<'a>(params: &'a Value, key: &str) -> Option<&'a Value> {
    let object = params.as_object()?;
    (object.len() == 1).then(|| object.get(key)).flatten()
}

fn action_valid(action: &str, params: &Value) -> bool {
    match action {
        "device.reboot"
        | "device.poweroff"
        | "cellular.connect"
        | "cellular.disconnect"
        | "cell.unlock_all" => params.as_object().is_some_and(Map::is_empty),
        "cellular.set" => exact_object(params, "roaming")
            .and_then(Value::as_i64)
            .is_some_and(|value| value == 0 || value == 1),
        "network.set_mode" => exact_object(params, "mode").is_some_and(|value| {
            one_of(
                value,
                &[
                    "WL_AND_5G",
                    "LTE_AND_5G",
                    "Only_5G",
                    "WCDMA_AND_LTE",
                    "Only_LTE",
                    "Only_WCDMA",
                ],
            )
        }),
        "nfc.set" => exact_object(params, "enabled")
            .and_then(Value::as_bool)
            .is_some(),
        "sim.set_slot" => exact_object(params, "slot")
            .and_then(Value::as_i64)
            .is_some_and(|value| value == 1 || value == 2),
        "wifi.set_module" => exact_object(params, "enabled")
            .and_then(Value::as_i64)
            .is_some_and(|value| value == 0 || value == 1),
        "wifi.set_chip" => params.as_object().is_some_and(|fields| {
            fields.len() == 2
                && fields
                    .get("chip")
                    .and_then(Value::as_str)
                    .is_some_and(|chip| matches!(chip, "chip1" | "chip2"))
                && fields.get("guest_enabled").and_then(Value::as_i64) == Some(0)
        }),
        _ => false,
    }
}

fn input_valid(input: &TaskInput) -> bool {
    !input.id.trim().is_empty()
        && input.id.trim() == input.id
        && input.id.len() <= 80
        && !input.id.chars().any(char::is_control)
        && reboot_schedule::valid_time(&input.time)
        && action_valid(&input.action, &input.params)
}

fn stored_valid(stored: &Stored) -> bool {
    if stored.schema != 1 || stored.tasks.len() > MAX_TASKS {
        return false;
    }
    let mut seen = std::collections::HashSet::new();
    stored.tasks.iter().all(|task| {
        input_valid(&task.input)
            && seen.insert(&task.input.id)
            && (task.last_attempt_date.is_empty()
                || reboot_schedule::valid_date(&task.last_attempt_date))
            && matches!(
                task.last_result.as_str(),
                "" | "requested" | "ok" | "failed"
            )
    })
}

pub struct TaskSchedule {
    path: PathBuf,
    stored: Stored,
    error: Option<&'static str>,
}

impl TaskSchedule {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join(FILE_NAME);
        let mut error = None;
        let stored = if path.exists() {
            match fs::symlink_metadata(&path)
                .ok()
                .filter(|metadata| metadata.is_file() && metadata.len() <= MAX_FILE_BYTES)
                .and_then(|_| fs::read(&path).ok())
                .and_then(|raw| serde_json::from_slice::<Stored>(&raw).ok())
                .filter(stored_valid)
            {
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
            error,
        }
    }

    pub fn status(&self) -> Value {
        let clock = reboot_schedule::local_clock();
        json!({
            "supported":true,
            "device_time":clock.as_ref().map(|clock| format!("{} {}",clock.date,clock.time)),
            "timezone_offset":clock.as_ref().map(|clock| clock.offset.as_str()),
            "tasks":self.stored.tasks,
            "error":self.error,
        })
    }

    fn save(&self, next: &Stored) -> Result<(), String> {
        let dir = self.path.parent().ok_or("task_storage_failed")?;
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|_| "task_storage_failed")?;
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "task_storage_failed")?
            .as_nanos();
        let temporary = dir.join(format!("{FILE_NAME}.tmp.{}.{}", std::process::id(), suffix));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(|_| "task_storage_failed")?;
            let mut raw = serde_json::to_vec(next).map_err(|_| "task_storage_failed")?;
            raw.push(b'\n');
            if raw.len() as u64 > MAX_FILE_BYTES {
                return Err("task_storage_failed");
            }
            file.write_all(&raw).map_err(|_| "task_storage_failed")?;
            file.set_permissions(Permissions::from_mode(0o600))
                .map_err(|_| "task_storage_failed")?;
            file.sync_all().map_err(|_| "task_storage_failed")?;
            fs::rename(&temporary, &self.path).map_err(|_| "task_storage_failed")
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result.map_err(str::to_string)
    }

    pub fn upsert(&mut self, input: TaskInput) -> Result<Value, String> {
        if !input_valid(&input) {
            return Err("invalid_task".into());
        }
        if reboot_schedule::local_clock().is_none() {
            return Err("device_clock_unavailable".into());
        }
        let mut next = self.stored.clone();
        if let Some(task) = next.tasks.iter_mut().find(|task| task.input.id == input.id) {
            if task.input == input && self.error.is_none() {
                return Ok(self.status());
            }
            task.input = input;
            task.last_attempt_date.clear();
            task.has_triggered = false;
            task.last_result.clear();
        } else {
            if next.tasks.len() == MAX_TASKS {
                return Err("task_limit".into());
            }
            next.tasks.push(Task {
                input,
                last_attempt_date: String::new(),
                has_triggered: false,
                last_result: String::new(),
            });
        }
        self.save(&next)?;
        self.stored = next;
        self.error = None;
        Ok(self.status())
    }

    pub fn remove(&mut self, id: &str) -> Result<Value, String> {
        if id.is_empty() || id.len() > 80 {
            return Err("invalid_task".into());
        }
        let mut next = self.stored.clone();
        next.tasks.retain(|task| task.input.id != id);
        if next.tasks.len() == self.stored.tasks.len() {
            return Err("task_not_found".into());
        }
        self.save(&next)?;
        self.stored = next;
        self.error = None;
        Ok(self.status())
    }

    /// Record an attempt before a device action. A reboot or crash cannot
    /// cause the same one-shot task to run again on daemon restart.
    pub fn observe(&mut self, clock: Option<&Clock>) -> Option<TaskInput> {
        let clock = clock?;
        if self.error.is_some() {
            return None;
        }
        let index = self.stored.tasks.iter().position(|task| {
            task.input.time == clock.time
                && task.last_attempt_date != clock.date
                && (task.input.repeat_daily || !task.has_triggered)
        })?;
        let mut next = self.stored.clone();
        let task = &mut next.tasks[index];
        task.last_attempt_date = clock.date.clone();
        task.last_result = "requested".into();
        if !task.input.repeat_daily {
            task.has_triggered = true;
        }
        let input = task.input.clone();
        if self.save(&next).is_err() {
            self.error = Some("task_storage_failed");
            return None;
        }
        self.stored = next;
        Some(input)
    }

    pub fn finish(&mut self, id: &str, date: &str, success: bool) {
        let mut next = self.stored.clone();
        let Some(task) = next
            .tasks
            .iter_mut()
            .find(|task| task.input.id == id && task.last_attempt_date == date)
        else {
            return;
        };
        task.last_result = if success { "ok" } else { "failed" }.into();
        if self.save(&next).is_ok() {
            self.stored = next;
        } else {
            self.error = Some("task_storage_failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (PathBuf, TaskSchedule) {
        let dir = std::env::temp_dir().join(format!(
            "datad-tasks-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        (dir.clone(), TaskSchedule::load(&dir))
    }

    fn input(action: &str, params: Value) -> TaskInput {
        TaskInput {
            id: "早间任务".into(),
            time: "08:30".into(),
            repeat_daily: false,
            action: action.into(),
            params,
        }
    }

    #[test]
    fn only_reviewed_actions_are_accepted() {
        for (action, params) in [
            ("device.reboot", json!({})),
            ("cellular.set", json!({"roaming":0})),
            ("network.set_mode", json!({"mode":"Only_LTE"})),
            ("nfc.set", json!({"enabled":false})),
            ("sim.set_slot", json!({"slot":2})),
            ("wifi.set_module", json!({"enabled":1})),
            ("wifi.set_chip", json!({"chip":"chip2","guest_enabled":0})),
        ] {
            assert!(input_valid(&input(action, params)), "{action}");
        }
        for (action, params) in [
            ("ubus.call", json!({"service":"zwrt_web"})),
            ("sms.send_raw", json!({})),
            ("device.reboot", json!({"shell":"reboot"})),
            ("network.set_mode", json!({"mode":"anything"})),
            ("wifi.set_module", json!({"enabled":2})),
        ] {
            assert!(!input_valid(&input(action, params)), "{action}");
        }
    }

    #[test]
    fn one_shot_is_persisted_before_execution_and_survives_restart() {
        let (dir, mut schedule) = fixture();
        schedule.upsert(input("device.reboot", json!({}))).unwrap();
        let clock = Clock {
            date: "2026-09-29".into(),
            time: "08:30".into(),
            offset: "+0800".into(),
        };
        assert!(schedule.observe(Some(&clock)).is_some());
        assert!(schedule.observe(Some(&clock)).is_none());
        let mut reloaded = TaskSchedule::load(&dir);
        assert!(reloaded.observe(Some(&clock)).is_none());
        assert_eq!(reloaded.status()["tasks"][0]["last_result"], "requested");
        assert_eq!(
            fs::metadata(dir.join(FILE_NAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        reloaded.finish("早间任务", "2026-09-29", true);
        assert_eq!(reloaded.status()["tasks"][0]["last_result"], "ok");
        reloaded.remove("早间任务").unwrap();
        assert_eq!(reloaded.status()["tasks"].as_array().unwrap().len(), 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn daily_repeats_on_next_local_date_only() {
        let (dir, mut schedule) = fixture();
        let mut task = input("nfc.set", json!({"enabled":true}));
        task.repeat_daily = true;
        schedule.upsert(task).unwrap();
        let mut clock = Clock {
            date: "2026-09-29".into(),
            time: "08:30".into(),
            offset: "+0800".into(),
        };
        assert!(schedule.observe(Some(&clock)).is_some());
        assert!(schedule.observe(Some(&clock)).is_none());
        clock.date = "2026-09-30".into();
        assert!(schedule.observe(Some(&clock)).is_some());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_or_unreviewed_saved_tasks_fail_closed() {
        let (dir, _) = fixture();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(FILE_NAME), br#"{"schema":1,"tasks":[{"id":"bad","time":"08:30","repeat_daily":true,"action":"ubus.call","params":{},"last_attempt_date":"","has_triggered":false,"last_result":""}]}"#).unwrap();
        let mut schedule = TaskSchedule::load(&dir);
        assert_eq!(schedule.status()["error"], "invalid_config");
        let clock = Clock {
            date: "2026-09-29".into(),
            time: "08:30".into(),
            offset: "+0800".into(),
        };
        assert!(schedule.observe(Some(&clock)).is_none());
        schedule
            .upsert(input("nfc.set", json!({"enabled":true})))
            .unwrap();
        assert_eq!(schedule.status()["error"], Value::Null);
        fs::remove_dir_all(dir).unwrap();
    }
}
