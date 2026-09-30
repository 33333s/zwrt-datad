//! Optional on-device signaling capture for the U50 runtime.
//!
//! A small glibc ARM worker (embedded at build time when
//! `DATAD_DIAG_WORKER` was provided, see `u50_diag_worker.c`) is extracted to
//! the data directory and supervised by this module. The worker dlopens the
//! vendor `libdiag.so`, registers a DCI client, subscribes to the NR/LTE RRC
//! OTA log codes and writes a bounded status JSON once per second. Crash
//! isolation, respawn backoff and a supervisor heartbeat keep the collector
//! self-limiting; every failure degrades to an honest reason in the state.
use serde_json::{Map, Value, json};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const WORKER_NAME: &str = "diag-worker";
const STATUS_NAME: &str = "diag-status.json";
const HEARTBEAT_NAME: &str = "diag-heartbeat";
/// The worker writes once per second; anything older is stale.
const STATUS_MAX_AGE: Duration = Duration::from_secs(5);
/// Respawn backoff bounds after worker exits.
const RESPAWN_MIN: Duration = Duration::from_secs(1);
const RESPAWN_MAX: Duration = Duration::from_secs(60);

fn worker_blob() -> Option<&'static [u8]> {
    (option_env!("DATAD_DIAG_WORKER_EMBEDDED").unwrap_or("0") == "1")
        .then(|| include_bytes!(concat!(env!("OUT_DIR"), "/diag-worker")).as_slice())
}

struct Supervisor {
    data_dir: PathBuf,
    child: Option<Child>,
    last_spawn: Option<std::time::Instant>,
    backoff: Duration,
    restarts: u32,
}

static RUN: OnceLock<Mutex<Supervisor>> = OnceLock::new();
static ENABLED: OnceLock<bool> = OnceLock::new();

/// Enable the collector for this process (U50 runtime `--u50-signaling`).
pub fn enable() {
    let _ = ENABLED.set(true);
}

fn supervisor() -> &'static Mutex<Supervisor> {
    RUN.get_or_init(|| {
        Mutex::new(Supervisor {
            data_dir: PathBuf::new(),
            child: None,
            last_spawn: None,
            backoff: RESPAWN_MIN,
            restarts: 0,
        })
    })
}

/// Bind the collector to a data directory (called once from `u50::run`).
pub fn configure(data_dir: &Path) {
    let mut run = supervisor().lock().unwrap_or_else(|p| p.into_inner());
    if run.data_dir != data_dir {
        run.data_dir = data_dir.to_path_buf();
    }
}

/// The `signaling` state block; refreshed on every state sample.
pub fn block() -> Value {
    if !ENABLED.get().copied().unwrap_or(false) {
        return json!({"available": false, "enabled": false,
                       "reason": "disabled"});
    }
    let mut run = supervisor().lock().unwrap_or_else(|p| p.into_inner());
    let mut out = Map::new();
    out.insert("enabled".into(), json!(true));
    if run.data_dir.as_os_str().is_empty() {
        out.insert("available".into(), json!(false));
        out.insert("reason".into(), json!("not_configured"));
        return Value::Object(out);
    }
    let Some(blob) = worker_blob() else {
        out.insert("available".into(), json!(false));
        out.insert("reason".into(), json!("worker_not_built"));
        return Value::Object(out);
    };
    let worker_path = run.data_dir.join(WORKER_NAME);
    let status_path = run.data_dir.join(STATUS_NAME);
    let heartbeat_path = run.data_dir.join(HEARTBEAT_NAME);

    if let Err(reason) = materialize_worker(&worker_path, blob) {
        out.insert("available".into(), json!(false));
        out.insert("reason".into(), json!(reason));
        return Value::Object(out);
    }

    ensure_running(&mut run, &worker_path, &status_path, &heartbeat_path);
    touch_heartbeat(&heartbeat_path);

    match read_status(&status_path, SystemTime::now()) {
        Some(status) => {
            let worker_state = status
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned();
            let reason = status.get("reason").cloned().unwrap_or_else(|| json!(""));
            out.insert("available".into(), json!(worker_state == "running"));
            out.insert("worker".into(), json!(worker_state));
            out.insert("reason".into(), reason);
            out.insert("restarts".into(), json!(run.restarts));
            for (key, value) in status {
                if key != "state" && key != "reason" {
                    out.insert(key, value);
                }
            }
        }
        None => {
            out.insert("available".into(), json!(false));
            out.insert("reason".into(), json!("status_stale"));
            out.insert("restarts".into(), json!(run.restarts));
        }
    }
    Value::Object(out)
}

/// Write the worker executable unless the on-disk copy already matches.
/// 0700: the worker speaks for this daemon only.
fn materialize_worker(path: &Path, blob: &[u8]) -> Result<(), &'static str> {
    match fs::read(path) {
        Ok(existing) if existing == blob => return Ok(()),
        _ => {}
    }
    let tmp = path.with_extension("new");
    fs::write(&tmp, blob).map_err(|_| "worker_write_failed")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o700))
            .map_err(|_| "worker_perm_failed")?;
    }
    fs::rename(&tmp, path).map_err(|_| "worker_rename_failed")
}

fn ensure_running(run: &mut Supervisor, worker: &Path, status: &Path, heartbeat: &Path) {
    if let Some(child) = run.child.as_mut()
        && child.try_wait().map(|w| w.is_none()).unwrap_or(false)
    {
        return; // still alive
    }
    let exited = run.child.is_some();
    run.child = None;
    if exited {
        run.restarts = run.restarts.saturating_add(1);
    }
    let since = run.last_spawn.map(|at| at.elapsed());
    if let Some(elapsed) = since
        && elapsed < run.backoff
    {
        return; // backoff window
    }
    let _ = fs::remove_file(status);
    match Command::new(worker)
        .arg(status)
        .arg(heartbeat)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => {
            run.last_spawn = Some(std::time::Instant::now());
            run.child = Some(child);
        }
        Err(_) => {
            run.last_spawn = Some(std::time::Instant::now());
        }
    }
    run.backoff = (run.backoff * 2).min(RESPAWN_MAX);
}

fn touch_heartbeat(path: &Path) {
    if let Ok(mut f) = fs::File::options().create(true).append(true).open(path) {
        let _ = f.write_all(b"h");
    }
    // mtime is what the worker checks; append updates it
}

/// Read and age-check the status file; keeps the worker's bounded fields.
fn read_status(path: &Path, now: SystemTime) -> Option<Map<String, Value>> {
    let text = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(text.trim()).ok()?;
    let object = value.as_object()?;
    let ts = object.get("ts")?.as_i64()?;
    let age = now
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs()
        .saturating_sub(ts.unsigned_abs());
    if Duration::from_secs(age) > STATUS_MAX_AGE {
        return None;
    }
    // Bounded copy: never trust the file with unbounded content.
    const KEEP: &[&str] = &[
        "state",
        "reason",
        "total",
        "dropped",
        "rate",
        "cell",
        "codes",
        "pdu_types",
        "five_qi",
        "ambr",
        "nas_last",
        "nas_ts",
    ];
    let mut out = Map::new();
    for key in KEEP {
        if let Some(v) = object.get(*key) {
            out.insert((*key).into(), bounded(v, 0));
        }
    }
    Some(out)
}

/// Recursively bound any decoded value (depth + array length) defensively.
fn bounded(value: &Value, depth: usize) -> Value {
    match value {
        Value::Object(map) if depth < 4 => {
            let mut out = Map::new();
            for (k, v) in map.iter().take(64) {
                out.insert(k.clone(), bounded(v, depth + 1));
            }
            Value::Object(out)
        }
        Value::Array(items) if depth < 4 => Value::Array(
            items
                .iter()
                .take(64)
                .map(|v| bounded(v, depth + 1))
                .collect(),
        ),
        Value::String(s) if s.len() <= 128 => value.clone(),
        Value::String(_) => Value::String(String::new()),
        _ if value.is_number() || value.is_boolean() || value.is_null() => value.clone(),
        _ => Value::Null,
    }
}

/// Stop the worker when the daemon shuts down: SIGTERM first so it can
/// deregister its DCI client cleanly, then reap.
pub fn shutdown() {
    if let Ok(mut run) = supervisor().lock()
        && let Some(mut child) = run.child.take()
    {
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }
        std::thread::sleep(Duration::from_millis(200));
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_status_file(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join(STATUS_NAME);
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn status_parse_keeps_bounded_fields_and_freshness() {
        let dir = std::env::temp_dir().join(format!("u50sig-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let body = format!(
            r#"{{"state":"running","reason":"","client_id":7,"ts":{now},"total":25,"dropped":0,
            "rate":3.3,"cell":{{"pci":393,"arfcn":627264}},"codes":{{"0xb821":25}},
            "pdu_types":{{"pcch":13}},"secret":"should-not-appear"}}"#
        );
        let p = write_status_file(&dir, &body);
        let st = read_status(&p, SystemTime::now()).expect("parse");
        assert_eq!(st["state"], json!("running"));
        assert_eq!(st["cell"]["pci"], json!(393));
        assert_eq!(st["codes"]["0xb821"], json!(25));
        assert!(st.get("secret").is_none());
        assert!(st.get("client_id").is_none());
        // stale
        let old = body.replace(&format!(r#""ts":{now}"#), r#""ts":1000"#);
        let p = write_status_file(&dir, &old);
        assert!(read_status(&p, SystemTime::now()).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bounded_defends_depth_and_size() {
        let deep = json!({"a": {"b": {"c": {"d": {"e": {"f": 1}}}}}});
        let out = bounded(&deep, 0);
        assert!(out["a"]["b"]["c"]["d"].get("e").is_none());
        let long = json!("x".repeat(500));
        assert_eq!(bounded(&long, 0), json!(""));
        assert_eq!(bounded(&json!([1, 2, 3]), 0), json!([1, 2, 3]));
    }

    #[test]
    fn disabled_block_is_honest() {
        let value = block();
        assert_eq!(value["enabled"], json!(false));
        assert_eq!(value["reason"], json!("disabled"));
    }

    #[test]
    fn worker_blob_is_absent_or_elf32_arm() {
        if let Some(blob) = worker_blob() {
            assert_eq!(&blob[..6], b"\x7fELF\x01\x01");
            assert_eq!(&blob[18..20], &[40, 0]);
        }
        // Without DATAD_DIAG_WORKER at build time the embedding is empty and
        // the collector must degrade instead of spawning something.
    }

    #[test]
    fn materialize_rewrites_only_on_change() {
        let dir = std::env::temp_dir().join(format!("u50sigw-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(WORKER_NAME);
        materialize_worker(&p, b"hello").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"hello");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&p).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        materialize_worker(&p, b"hello").unwrap(); // unchanged: no rewrite path
        assert_eq!(fs::read(&p).unwrap(), b"hello");
        let _ = fs::remove_dir_all(&dir);
    }
}
