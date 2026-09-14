use crate::neighbor;
use fs2::FileExt;
use serde_json::{Value, json};
use std::{
    collections::hash_map::DefaultHasher,
    fs::{self, File, OpenOptions},
    hash::{Hash, Hasher},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    process::{Child, Command},
    time::{Duration, timeout},
};

const CAPTURE_LIMIT: u64 = 32 * 1024 * 1024;

pub struct Manager {
    enabled: bool,
    base: PathBuf,
    _config: PathBuf,
    diag: PathBuf,
    lock: Option<File>,
    run: Option<PathBuf>,
    child: Option<Child>,
    generation: u64,
    context: String,
    latest: Value,
    sampled_ms: Option<i64>,
    fingerprint: u64,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
fn disabled() -> Value {
    json!({"status":"disabled","enabled":false,"collector_running":false,"cells":[],"reason":"disabled_by_default","frames":0,"malformed":0,"partial":false,"discarded":0,"ambiguous_measurements":0,"capture_bytes":0,"generation":0,"sampled_at":Value::Null,"age_ms":Value::Null,"source":""})
}

impl Manager {
    pub fn new(force: bool) -> Self {
        let base = std::env::var("ZWRT_DATAD_NEIGHBOR_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp/zwrt-datad-neighbor"));
        let config = std::env::var("ZWRT_DATAD_NEIGHBOR_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/data/zwrt-datad/neighbor.json"));
        let diag = std::env::var("ZWRT_DATAD_DIAG_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/usr/bin/diag_mdlog"));
        if fs::read(&config)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v.get("enabled").and_then(Value::as_bool))
            .unwrap_or(false)
        {
            let _ = write_disabled(&config);
        }
        let mut manager = Self {
            enabled: false,
            base,
            _config: config,
            diag,
            lock: None,
            run: None,
            child: None,
            generation: 0,
            context: String::new(),
            latest: disabled(),
            sampled_ms: None,
            fingerprint: 0,
        };
        if force {
            manager.enabled = true;
            manager.latest["enabled"] = json!(true);
            manager.latest["status"] = json!("starting");
            manager.latest["reason"] = json!("none");
        }
        manager
    }
    pub fn status(&self) -> Value {
        let mut out = self.latest.clone();
        out["enabled"] = json!(self.enabled);
        out["collector_running"] = json!(self.child.as_ref().and_then(Child::id).is_some());
        if let Some(sampled) = self.sampled_ms {
            out["sampled_at"] = json!(sampled / 1000);
            out["age_ms"] = json!(now_ms().saturating_sub(sampled));
        }
        out
    }
    pub async fn set_enabled(&mut self, enabled: bool) -> Result<Value, String> {
        if enabled == self.enabled {
            return Ok(self.status());
        }
        if enabled {
            self.enabled = true;
            self.latest = disabled();
            self.latest["enabled"] = json!(true);
            self.latest["status"] = json!("starting");
            self.latest["reason"] = json!("none");
            self.start().await?;
        } else {
            self.shutdown().await;
            self.latest = disabled();
        }
        Ok(self.status())
    }
    async fn start(&mut self) -> Result<(), String> {
        if !self.diag.is_file() {
            self.latest["status"] = json!("dependency_missing");
            self.latest["reason"] = json!("diag_mdlog_missing");
            return Ok(());
        }
        fs::create_dir_all(&self.base).map_err(|e| e.to_string())?;
        fs::set_permissions(&self.base, fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        let lock_path = self.base.join("owner.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(lock_path)
            .map_err(|e| e.to_string())?;
        if lock.try_lock_exclusive().is_err() {
            self.latest["status"] = json!("blocked");
            self.latest["reason"] = json!("another_neighbor_instance");
            return Ok(());
        }
        self.lock = Some(lock);
        for entry in fs::read_dir(&self.base)
            .map_err(|e| e.to_string())?
            .flatten()
        {
            if entry.file_name().to_string_lossy().starts_with("capture.") {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
        self.generation += 1;
        let run = self.base.join(format!(
            "capture.{}.{}",
            std::process::id(),
            self.generation
        ));
        let ring = run.join("ring");
        fs::create_dir_all(&ring).map_err(|e| e.to_string())?;
        fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
        fs::set_permissions(&ring, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
        let mask = run.join("qtrace.cfg");
        let mut bytes = Vec::new();
        for token in include_str!("../../src/neighbor/qtrace_mask.h")
            .split(|c: char| c == ',' || c.is_whitespace() || c == '{' || c == '}' || c == ';')
        {
            if let Some(hex) = token.trim().strip_prefix("0x") {
                if hex.len() == 2 {
                    if let Ok(v) = u8::from_str_radix(hex, 16) {
                        bytes.push(v)
                    }
                }
            }
        }
        fs::write(&mask, bytes).map_err(|e| e.to_string())?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(run.join("diag.log"))
            .map_err(|e| e.to_string())?;
        let err = log.try_clone().map_err(|e| e.to_string())?;
        let child = Command::new(&self.diag)
            .args([
                "-f",
                mask.to_string_lossy().as_ref(),
                "-o",
                ring.to_string_lossy().as_ref(),
                "-s",
                "4",
                "-n",
                "4",
                "-c",
                "-d",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(err))
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| e.to_string())?;
        self.run = Some(run);
        self.fingerprint = 0;
        self.child = Some(child);
        self.latest["status"] = json!("collecting");
        self.latest["reason"] = json!("none");
        self.latest["generation"] = json!(self.generation);
        Ok(())
    }
    async fn restart(&mut self) -> Result<(), String> {
        self.stop_child().await;
        if let Some(run) = self.run.take() {
            let _ = fs::remove_dir_all(run);
        }
        self.lock.take();
        self.start().await
    }
    pub async fn tick(&mut self, net: &Value) {
        if !self.enabled {
            return;
        }
        if self.child.is_none() && self.lock.is_none() {
            let _ = self.start().await;
            return;
        }
        let context = context(net);
        if !context.is_empty() && context != self.context {
            if !self.context.is_empty() {
                let _ = self.restart().await;
            }
            self.context = context;
        }
        if let Some(child) = self.child.as_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                self.latest["status"] = json!("error");
                self.latest["reason"] = json!("collector_exited");
                self.latest["exit_code"] = json!(status.code());
                self.child = None;
                return;
            }
        }
        let Some(run) = &self.run else { return };
        let ring = run.join("ring");
        let mut files = Vec::new();
        let mut bytes = 0;
        scan(&ring, &mut files, &mut bytes, 0);
        self.latest["capture_bytes"] = json!(bytes);
        if bytes > CAPTURE_LIMIT {
            self.latest["status"] = json!("error");
            self.latest["reason"] = json!("capture_limit");
            self.stop_child().await;
            return;
        }
        if files.is_empty() {
            return;
        }
        let fingerprint = capture_fingerprint(&files);
        if fingerprint == self.fingerprint {
            return;
        }
        self.fingerprint = fingerprint;
        match neighbor::parse_files(&files, now_ms()) {
            Ok(parsed) => {
                let frames = parsed.get("frames").cloned().unwrap_or(json!(0));
                let cells = filter_cells(
                    parsed
                        .get("cells")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default(),
                    net,
                );
                let reason = if cells.is_empty() {
                    "no_supported_reports"
                } else {
                    "none"
                };
                self.sampled_ms = Some(now_ms());
                self.latest = json!({"status":if cells.is_empty(){"empty"}else{"ready"},"enabled":true,"collector_running":true,"cells":cells,"reason":reason,"frames":frames,"malformed":parsed.get("malformed").cloned().unwrap_or(json!(0)),"partial":parsed.get("partial").cloned().unwrap_or(json!(false)),"discarded":parsed.get("discarded").cloned().unwrap_or(json!(0)),"ambiguous_measurements":parsed.get("ambiguous").cloned().unwrap_or(json!(0)),"capture_bytes":bytes,"generation":self.generation,"sampled_at":now_ms()/1000,"age_ms":0,"source":"qtrace"});
            }
            Err(_) => {
                self.latest["status"] = json!("error");
                self.latest["reason"] = json!("capture_read_error");
            }
        }
    }
    async fn stop_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            if let Some(pid) = child.id() {
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
            }
            if timeout(Duration::from_secs(2), child.wait()).await.is_err() {
                let _ = child.kill().await;
            }
        }
    }
    pub async fn shutdown(&mut self) {
        self.enabled = false;
        self.stop_child().await;
        if let Some(run) = self.run.take() {
            let _ = fs::remove_dir_all(run);
        }
        self.lock.take();
    }
}
impl Drop for Manager {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
        if let Some(run) = self.run.take() {
            let _ = fs::remove_dir_all(run);
        }
    }
}

fn write_disabled(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut f = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(b"{\"enabled\":false}\n")?;
    f.sync_all()?;
    drop(f);
    fs::rename(tmp, path)
}
fn scan(path: &Path, files: &mut Vec<PathBuf>, bytes: &mut u64, depth: u8) {
    if depth > 3 || *bytes > CAPTURE_LIMIT {
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let Ok(meta) = p.symlink_metadata() else {
            continue;
        };
        if meta.file_type().is_symlink() {
            *bytes = CAPTURE_LIMIT + 1;
            return;
        }
        if meta.is_dir() {
            scan(&p, files, bytes, depth + 1)
        } else if meta.is_file() {
            *bytes = bytes.saturating_add(meta.len());
            if p.extension().and_then(|v| v.to_str()) == Some("qmdl") && files.len() < 32 {
                files.push(p)
            }
        }
    }
}
fn capture_fingerprint(files: &[PathBuf]) -> u64 {
    let mut sorted = files.to_vec();
    sorted.sort();
    let mut hasher = DefaultHasher::new();
    for path in sorted {
        path.hash(&mut hasher);
        if let Ok(meta) = path.metadata() {
            meta.len().hash(&mut hasher);
            meta.modified()
                .ok()
                .and_then(|v| v.duration_since(UNIX_EPOCH).ok())
                .map(|v| v.as_nanos())
                .unwrap_or_default()
                .hash(&mut hasher);
        }
    }
    hasher.finish()
}
fn known_identity(v: &Value) -> Option<u64> {
    let n = v.as_u64().or_else(|| v.as_str()?.parse().ok())?;
    (n > 0 && n < u32::MAX as u64).then_some(n)
}
fn context(net: &Value) -> String {
    format!(
        "{}:{}:{}:{}:{}:{}:{}",
        net.get("type").and_then(Value::as_str).unwrap_or_default(),
        net.get("nr_channel")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        net.get("nr_pci")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        known_identity(&net["nr_cell_id"]).unwrap_or_default(),
        net.get("lte_channel")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        net.get("lte_pci")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        known_identity(&net["lte_cell_id"]).unwrap_or_default()
    )
}
fn ca_pcis(text: &str, nr: bool) -> Vec<u64> {
    text.split(';')
        .filter_map(|row| {
            let p: Vec<_> = row.split(',').collect();
            (if nr { p.first() } else { p.get(1) }).and_then(|v| v.parse().ok())
        })
        .collect()
}
fn filter_cells(cells: Vec<Value>, net: &Value) -> Vec<Value> {
    let network_type = net["type"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let nr_channel = net["nr_channel"].as_i64().unwrap_or_default();
    let lte_channel = net["lte_channel"].as_i64().unwrap_or_default();
    let mut nr = ca_pcis(net["nrca"].as_str().unwrap_or_default(), true);
    nr.push(net["nr_pci"].as_u64().unwrap_or_default());
    let mut lte = ca_pcis(net["lteca"].as_str().unwrap_or_default(), false);
    lte.push(net["lte_pci"].as_u64().unwrap_or_default());
    cells
        .into_iter()
        .filter_map(|mut cell| {
            let rat = cell["rat"].as_str()?;
            if network_type == "LTE" && rat != "LTE" {
                return None;
            }
            if network_type.contains("SA") && !network_type.contains("NSA") && rat != "NR" {
                return None;
            }
            let pci = cell["pci"].as_u64()?;
            let arfcn = cell["arfcn"].as_i64().unwrap_or_default();
            if (rat == "NR" && arfcn == nr_channel && nr.contains(&pci))
                || (rat == "LTE" && arfcn == lte_channel && lte.contains(&pci))
            {
                return None;
            }
            let serving = if rat == "NR" { nr_channel } else { lte_channel };
            cell["frequency_relation"] = json!(if serving > 0 && arfcn == serving {
                "intra"
            } else {
                "inter"
            });
            cell["frequency_evidence"] = json!(if arfcn > 0 { "explicit" } else { "unknown" });
            Some(cell)
        })
        .collect()
}
