use crate::{elapsed, time_control, update_status};
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::process::Command;

const NETDISK: &str = "https://pan.ericsfj.com/sd/wN2PJUK8";
const GITHUB: &str = "https://github.com/33333s/zwrt-datad/releases/latest/download";
const PUBLIC_KEY: &str = include_str!("../ota_public.pem");
const IDLE_FOR: Duration = Duration::from_secs(120);
const AUTO_CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
const MAX_IDLE_OBSERVATION_GAP: Duration = Duration::from_secs(90);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub enabled: bool,
    pub servers: Vec<String>,
    #[serde(default = "default_sources")]
    pub sources: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            servers: Vec::new(),
            sources: default_sources(),
        }
    }
}

fn publish_config(config: &Config) {
    update_status::set_config(
        config.enabled,
        config.sources.iter().any(|source| source == "custom"),
        &config.servers,
    );
}

/// Order matters: sources are tried first to last. Servers the user configured
/// (`custom`) come first, then GitHub, with the netdisk mirror as the fallback.
fn default_sources() -> Vec<String> {
    ["custom", "github", "netdisk"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// Before 0.10.62 the default put the netdisk ahead of GitHub. A stored list that
/// is exactly that old default was never a choice, so it follows the new default;
/// any other stored order is the user's and is kept.
fn legacy_default_sources() -> Vec<String> {
    ["custom", "netdisk", "github"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Artifact {
    pub name: String,
    pub size: i64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub notes: String,
    pub schema: i64,
    pub version: String,
    pub tag: String,
    pub published_at: String,
    pub artifacts: BTreeMap<String, Artifact>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    #[serde(default)]
    pub install_started_at: i64,
    pub state: String,
    pub current_version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub latest_version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    pub progress: u8,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub last_check_at: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub last_success_at: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub next_retry_at: i64,
    pub failure_count: u8,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wait_reasons: Vec<String>,
    pub signature_verified: bool,
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

impl Default for Status {
    fn default() -> Self {
        Self {
            install_started_at: 0,
            state: "idle".into(),
            current_version: env!("DATAD_VERSION").into(),
            latest_version: String::new(),
            source: String::new(),
            progress: 0,
            error: String::new(),
            last_check_at: 0,
            last_success_at: 0,
            next_retry_at: 0,
            failure_count: 0,
            wait_reasons: Vec::new(),
            signature_verified: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub manifest: Manifest,
    pub base_url: String,
}

pub struct Ota {
    view: tokio::sync::watch::Sender<Value>,
    install_deadline: Option<Duration>,
    dir: PathBuf,
    config: Config,
    status: Status,
    candidate: Option<Candidate>,
    idle_since: Option<Duration>,
    idle_observed_at: Option<Duration>,
    clock_generation: u64,
    pub busy: bool,
    client: Client,
    key: VerifyingKey,
    last_auto_check: Option<Duration>,
    retry_deadline: Option<Duration>,
}

#[derive(Default, Serialize, Deserialize)]
struct StoredStatus {
    #[serde(flatten)]
    status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retry: Option<RetryTimer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    install_wait: Option<RetryTimer>,
}

#[derive(Serialize, Deserialize)]
struct RetryTimer {
    boot_id: String,
    deadline_ms: u64,
}

fn retry_delay(failures: u8) -> Duration {
    Duration::from_secs(match failures {
        1 => 3600,
        2 => 6 * 3600,
        3 => 24 * 3600,
        _ => 0,
    })
}

impl Ota {
    pub fn load(dir: &Path) -> Result<Self, String> {
        let key = parse_public_key(PUBLIC_KEY)?;
        let mut config = read_json::<Config>(&dir.join("ota.json"))
            .filter(|value| validate_config(value).is_ok())
            .unwrap_or_default();
        if config.sources == legacy_default_sources() {
            config.sources = default_sources();
        }
        let stored = read_json::<StoredStatus>(&dir.join("ota-state.json")).unwrap_or_default();
        let mut status = stored.status;
        status.current_version = env!("DATAD_VERSION").into();
        status.source = source_name(&status.source).into();
        if matches!(status.state.as_str(), "checking" | "downloading") {
            status.state = "error".into();
            status.error = "上次更新检查或下载被中断，请重新检查".into();
            status.signature_verified = false;
            status.latest_version.clear();
            status.source.clear();
            status.wait_reasons.clear();
            status.progress = 0;
        }
        // An ADB/manual upgrade can supersede a previously checked candidate.
        // Keep an in-flight install's target intact until its result is reconciled.
        if matches!(status.state.as_str(), "available" | "waiting_idle")
            && !status.latest_version.is_empty()
            && !newer(&status.latest_version, env!("DATAD_VERSION"))
        {
            status.state = "idle".into();
            status.latest_version = env!("DATAD_VERSION").into();
            status.signature_verified = false;
            status.source.clear();
            status.wait_reasons.clear();
            status.error.clear();
            status.progress = 0;
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(45))
            .build()
            .map_err(|e| e.to_string())?;
        let delay = retry_delay(status.failure_count);
        let retry_deadline = if delay.is_zero() || status.state == "succeeded" {
            None
        } else if let Some(timer) = stored.retry
            && elapsed::boot_id().as_deref() == Some(timer.boot_id.as_str())
            && Duration::from_millis(timer.deadline_ms)
                <= elapsed::now().saturating_add(Duration::from_secs(24 * 3600))
        {
            // Daemon restarts in the same boot preserve even an expired deadline.
            Some(Duration::from_millis(timer.deadline_ms))
        } else {
            // An old release/another boot has no trustworthy elapsed deadline.
            // Rearm the bounded cooldown; never infer elapsed age from RTC/wall time.
            Some(elapsed::now().saturating_add(delay))
        };
        let install_deadline = (status.state == "installing").then(|| {
            stored
                .install_wait
                .filter(|timer| {
                    elapsed::boot_id().as_deref() == Some(timer.boot_id.as_str())
                        && Duration::from_millis(timer.deadline_ms)
                            <= elapsed::now().saturating_add(Duration::from_secs(1800))
                })
                .map(|timer| Duration::from_millis(timer.deadline_ms))
                .unwrap_or_else(|| elapsed::now().saturating_add(Duration::from_secs(1800)))
        });
        let (view, _) = tokio::sync::watch::channel(Value::Null);
        let manager = Self {
            view,
            install_deadline,
            dir: dir.to_owned(),
            config,
            status,
            candidate: None,
            idle_since: None,
            idle_observed_at: None,
            clock_generation: time_control::clock_generation(),
            busy: false,
            client,
            key,
            last_auto_check: None,
            retry_deadline,
        };
        publish_config(&manager.config);
        manager.publish();
        if manager.retry_deadline.is_some() || manager.install_deadline.is_some() {
            manager.save_status();
        }
        Ok(manager)
    }

    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Value> {
        self.view.subscribe()
    }

    fn candidate_id(candidate: &Candidate) -> String {
        hex_sha256(
            &serde_json::to_vec(
                &json!({"manifest":candidate.manifest,"source":candidate.base_url}),
            )
            .expect("serialize candidate"),
        )
    }

    fn publish(&self) {
        let candidate = self
            .candidate
            .as_ref()
            .filter(|c| has_update(c) && self.status.signature_verified);
        self.view.send_replace(json!({"status":self.display_status(),"config":self.config,
            "busy": self.busy || self.status.state == "installing", "platform":"update.json",
            "candidate":candidate.map(|c| json!({"id":Self::candidate_id(c),"manifest":c.manifest}))}));
    }

    pub fn approved_candidate(&self, id: &str) -> Result<Candidate, String> {
        self.candidate
            .as_ref()
            .filter(|c| {
                has_update(c) && self.status.signature_verified && Self::candidate_id(c) == id
            })
            .cloned()
            .ok_or_else(|| "更新信息已失效，请重新检查更新".into())
    }

    pub fn config_json(&self) -> Value {
        json!({"success":true,"config":self.config,"default_servers":["网盘","GitHub"]})
    }

    pub fn status_json(&self) -> Value {
        json!({"success":true,"status":self.display_status(),"enabled":self.config.enabled})
    }

    fn display_status(&self) -> Status {
        let mut status = self.status.clone();
        let remaining = self
            .retry_deadline
            .map(|deadline| deadline.saturating_sub(elapsed::now()))
            .unwrap_or_default();
        status.next_retry_at = if remaining.is_zero() {
            0
        } else {
            now().saturating_add(
                remaining
                    .as_secs()
                    .saturating_add(u64::from(remaining.subsec_nanos() != 0))
                    .min(i64::MAX as u64) as i64,
            )
        };
        status
    }

    fn retry_ready(&self) -> bool {
        self.retry_deadline
            .is_none_or(|deadline| elapsed::now() >= deadline)
    }

    pub fn begin_update(&mut self) -> Result<(), String> {
        if time_control::clock_change_in_progress() {
            return Err("系统时间正在切换，请稍后重试".into());
        }
        if self.busy || self.status.state == "installing" {
            return Err("更新任务正在运行".into());
        }
        self.busy = true;
        self.status.state = "checking".into();
        self.status.error.clear();
        self.save_status();
        Ok(())
    }

    pub fn finish_update(&mut self) {
        self.busy = false;
        self.publish();
    }

    pub fn update_config(&mut self, mut config: Config) -> Result<Value, String> {
        if self.busy || self.status.state == "installing" {
            return Err("update_in_progress".into());
        }
        validate_config(&config)?;
        for server in &mut config.servers {
            *server = server.trim().trim_end_matches('/').to_owned();
        }
        atomic_json(&self.dir.join("ota.json"), &config, 0o600)?;
        self.config = config;
        publish_config(&self.config);
        self.candidate = None;
        self.status.signature_verified = false;
        self.status.latest_version.clear();
        self.status.source.clear();
        self.status.wait_reasons.clear();
        self.status.progress = 0;
        self.status.state = "idle".into();
        self.status.error.clear();
        self.save_status();

        Ok(json!({"success":true,"config":self.config}))
    }

    /// Changes only the automatic-update switch (servers and sources stay) and
    /// confirms the stored configuration before reporting the new state.
    pub fn set_auto_update(&mut self, enabled: bool) -> Result<bool, String> {
        let mut config = self.config.clone();
        config.enabled = enabled;
        self.update_config(config)?;
        let stored = read_json::<Config>(&self.dir.join("ota.json"))
            .ok_or_else(|| "ota_config_readback_failed".to_string())?;
        if stored.enabled != enabled || self.config.enabled != enabled {
            return Err("ota_config_readback_failed".into());
        }
        Ok(enabled)
    }

    fn servers(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut ordered = Vec::new();
        // Sources are tried in the configured order.
        for source in &self.config.sources {
            match source.as_str() {
                "custom" => ordered.extend(self.config.servers.iter().map(String::as_str)),
                "netdisk" => ordered.push(NETDISK),
                "github" => ordered.push(GITHUB),
                _ => {}
            }
        }
        ordered
            .into_iter()
            .map(|value| value.trim().trim_end_matches('/'))
            .filter(|value| !value.is_empty() && seen.insert((*value).to_owned()))
            .map(str::to_owned)
            .collect()
    }

    pub async fn check(&mut self) -> Result<Candidate, String> {
        if self.status.state == "installing" {
            return Err("update_in_progress".into());
        }
        if time_control::clock_change_in_progress() {
            return Err("系统时间正在切换，请稍后重试".into());
        }
        self.status.state = "checking".into();
        self.status.error.clear();
        self.candidate = None;
        self.status.signature_verified = false;
        self.status.latest_version.clear();
        self.status.source.clear();
        self.status.wait_reasons.clear();
        self.status.last_check_at = now();
        self.status.progress = 0;
        self.save_status();
        update_status::update("checking");
        let mut signature_failed = false;
        let mut verified = false;
        let mut errors = Vec::new();
        let mut valid_but_current = None;
        for base in self.servers() {
            let raw = match self.fetch(&source_url(&base, "update.json"), 1 << 20).await {
                Ok(value) => value,
                Err(error) => {
                    errors.push(format!("{base}: {error}"));
                    continue;
                }
            };
            let encoded = match self
                .fetch(&source_url(&base, "update.json.sig"), 4096)
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    errors.push(format!("{base}: {error}"));
                    continue;
                }
            };
            if verify(&self.key, &raw, &encoded).is_err() {
                signature_failed = true;
                errors.push(format!("{base}: 签名校验失败"));
                continue;
            }
            let manifest: Manifest = match serde_json::from_slice(&raw) {
                Ok(value) if validate_manifest(&value).is_ok() => value,
                _ => {
                    errors.push(format!("{base}: 清单无效"));
                    continue;
                }
            };
            if !verified {
                verified = true;
                update_status::update("signature_verified");
            }
            let candidate = Candidate {
                manifest,
                base_url: base,
            };
            if !has_update(&candidate) {
                if valid_but_current
                    .as_ref()
                    .is_none_or(|current: &Candidate| {
                        newer(&candidate.manifest.version, &current.manifest.version)
                    })
                {
                    valid_but_current = Some(candidate);
                }
                continue;
            }
            self.status.latest_version = candidate.manifest.version.clone();
            self.status.source = source_name(&candidate.base_url).into();
            self.status.signature_verified = true;
            self.status.wait_reasons.clear();
            self.status.state = "available".into();
            self.candidate = Some(candidate.clone());
            self.save_status();
            return Ok(candidate);
        }
        if let Some(candidate) = valid_but_current {
            self.status.latest_version =
                if newer(env!("DATAD_VERSION"), &candidate.manifest.version) {
                    env!("DATAD_VERSION").into()
                } else {
                    candidate.manifest.version.clone()
                };
            self.status.source = source_name(&candidate.base_url).into();
            self.status.signature_verified = true;
            self.status.wait_reasons.clear();
            self.status.state = "idle".into();
            self.candidate = Some(candidate.clone());
            self.save_status();
            update_status::update("idle");
            return Ok(candidate);
        }
        update_status::update(if signature_failed {
            "signature_failed"
        } else {
            "failed"
        });
        let error = format!("所有更新服务器均不可用或签名无效: {}", errors.join("; "));
        self.status.state = "error".into();
        self.status.error = error.clone();
        self.status.signature_verified = false;
        self.save_status();
        Err(error)
    }

    pub async fn install(
        &mut self,
        candidate: &Candidate,
        snapshot: &Value,
        manual: bool,
    ) -> Result<(), String> {
        let reasons = self.safety(snapshot, manual);
        if !reasons.is_empty() {
            self.status.state = "waiting_idle".into();
            self.status.wait_reasons = reasons.clone();
            self.save_status();
            return Err(format!("等待安装条件: {}", reasons.join("、")));
        }
        let installer = candidate
            .manifest
            .artifacts
            .get("installer")
            .ok_or("缺少安装器")?;
        self.status.state = "downloading".into();
        self.status.progress = 5;
        self.status.wait_reasons.clear();
        self.save_status();
        update_status::update("downloading");
        let raw = match self
            .fetch(&source_url(&candidate.base_url, &installer.name), 16 << 20)
            .await
        {
            Ok(raw) => raw,
            Err(error) => {
                update_status::update("download_failed");
                return Err(error);
            }
        };
        if hex_sha256(&raw) != installer.sha256 {
            update_status::update("download_failed");
            return Err("安装器 SHA-256 校验失败".into());
        }
        let installer_path = self.dir.join("ota-installer.sh");
        if let Err(error) = atomic_write(&installer_path, &raw, 0o700) {
            update_status::update("install_failed");
            return Err(error);
        }
        let binary = candidate
            .manifest
            .artifacts
            .get("binary")
            .ok_or("缺少二进制")?;
        let wrapper = self.dir.join("ota-run.sh");
        let result = self.dir.join("ota-result.log");
        let marker = self.dir.join("ota-install-result");
        let script = format!(
            "#!/bin/sh\nsleep 2\nrm -f {}\nif {}DATAD_DOWNLOAD_URL={} sh {} >{} 2>&1; then value=success; else value=failed; fi\nprintf '%s\\n' \"$value\" >{}.tmp\nmv -f {}.tmp {}\n",
            shell_quote(&marker),
            install_dir_env(),
            shell_quote(source_url(&candidate.base_url, &binary.name)),
            shell_quote(&installer_path),
            shell_quote(&result),
            shell_quote(&marker),
            shell_quote(&marker),
            shell_quote(&marker),
        );
        if let Err(error) = atomic_write(&wrapper, script.as_bytes(), 0o700) {
            update_status::update("install_failed");
            return Err(error);
        }
        match fs::remove_file(&marker) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        self.status.latest_version = candidate.manifest.version.clone();
        self.status.state = "installing".into();
        self.status.progress = 95;
        self.status.install_started_at = now();
        self.install_deadline = Some(elapsed::now().saturating_add(Duration::from_secs(1800)));
        self.save_status();
        update_status::update("installing");
        if let Err(error) = Command::new("/bin/sh")
            .arg(&wrapper)
            .kill_on_drop(false)
            .spawn()
        {
            update_status::update("install_failed");
            return Err(error.to_string());
        }
        update_status::update("restarting");
        Ok(())
    }

    pub fn fail(&mut self, candidate: Option<&Candidate>, error: String) {
        self.status.state = "error".into();
        self.status.error = error;
        self.status.failure_count = self.status.failure_count.saturating_add(1);
        let delay = retry_delay(self.status.failure_count);
        if delay.is_zero() {
            self.status.state = "blocked".into();
        }
        self.retry_deadline = (!delay.is_zero()).then(|| elapsed::now().saturating_add(delay));
        self.status.next_retry_at = self.display_status().next_retry_at;
        if let Some(candidate) = candidate {
            self.status.latest_version = candidate.manifest.version.clone();
            self.status.source = source_name(&candidate.base_url).into();
        }
        self.save_status();
    }

    pub fn reconcile_install_result(&mut self) {
        let marker = self.dir.join("ota-install-result");
        let Ok(value) = fs::read_to_string(&marker) else {
            if self.status.state == "installing"
                && self
                    .install_deadline
                    .is_some_and(|deadline| elapsed::now() >= deadline)
            {
                self.fail(
                    None,
                    "Installation result not confirmed; check device logs".into(),
                );
            }
            return;
        };
        let _ = fs::remove_file(marker);
        if value.trim() == "success" && self.status.latest_version == env!("DATAD_VERSION") {
            update_status::update("succeeded");
            self.status.state = "succeeded".into();
            self.status.current_version = env!("DATAD_VERSION").into();
            self.status.progress = 100;
            self.install_deadline = None;
            self.status.last_success_at = now();
            self.status.error.clear();
            self.status.wait_reasons.clear();
            self.status.failure_count = 0;
            self.status.next_retry_at = 0;
            self.retry_deadline = None;
        } else {
            update_status::update("install_failed");
            self.status.state = "error".into();
            self.status.error = "安装器执行失败，已由安装器回滚".into();
            self.status.failure_count = self.status.failure_count.saturating_add(1);
        }
        self.save_status();
    }

    pub fn auto_candidate(&mut self) -> Option<Candidate> {
        if !self.config.enabled
            || self.busy
            || self.status.state == "installing"
            || !self.retry_ready()
            || time_control::clock_change_in_progress()
        {
            return None;
        }
        if self.status.state == "blocked"
            && self
                .candidate
                .as_ref()
                .is_some_and(|value| value.manifest.version == self.status.latest_version)
        {
            return None;
        }
        self.candidate
            .clone()
            .filter(|value| newer(&value.manifest.version, env!("DATAD_VERSION")))
    }

    pub fn should_auto_check(&self) -> bool {
        self.config.enabled
            && !self.busy
            && self.status.state != "installing"
            && self.retry_ready()
            && !time_control::clock_change_in_progress()
            && (self.candidate.is_none()
                || self
                    .last_auto_check
                    .is_none_or(|last| elapsed::now().saturating_sub(last) >= AUTO_CHECK_EVERY))
    }

    pub fn mark_auto_check(&mut self) {
        self.last_auto_check = Some(elapsed::now());
    }

    fn safety(&mut self, snapshot: &Value, manual: bool) -> Vec<String> {
        let mut reasons = Vec::new();
        self.observe_clock_generation(time_control::clock_generation());
        if time_control::clock_change_in_progress() {
            self.idle_since = None;
            self.idle_observed_at = None;
            reasons.push("系统时间正在切换".into());
        }
        // A device without a battery (CPEs) still reports a placeholder block
        // (online 0, capacity 0); that is not a flat battery.
        let battery_absent = number_at(snapshot, &["battery", "online"]) == Some(0.0);
        let battery = number_at(snapshot, &["battery", "percent"])
            .or_else(|| number_at(snapshot, &["system", "battery_percent"]))
            .filter(|_| !battery_absent);
        if battery.is_some_and(|value| value <= 10.0) {
            reasons.push("电量必须高于 10%".into());
        }
        if number_at(snapshot, &["runtime", "storage", "available"])
            .is_some_and(|value| value < 64.0 * 1024.0 * 1024.0)
        {
            reasons.push("可用存储不足 64 MiB".into());
        }
        if manual {
            return reasons;
        }
        if bool_at(snapshot, &["neighbor", "enabled"])
            || bool_at(snapshot, &["neighbor", "collector_running"])
        {
            reasons.push("请先关闭邻区采集".into());
        }
        let cpu = number_at(snapshot, &["runtime", "cpu_usage_tenths"])
            .map(|value| value / 10.0)
            .or_else(|| number_at(snapshot, &["system", "cpu_usage"]));
        if cpu.is_some_and(|value| value >= 60.0) {
            reasons.push("CPU 占用需低于 60%".into());
        }
        let rate = number_at(snapshot, &["runtime", "throughput", "rx_bps"]).unwrap_or(0.0)
            + number_at(snapshot, &["runtime", "throughput", "tx_bps"]).unwrap_or(0.0);
        if rate >= 1024.0 * 1024.0 {
            reasons.push("上下行需低于 1 MiB/s".into());
        }
        if reasons.is_empty() {
            let observed = elapsed::now();
            if self
                .idle_observed_at
                .is_some_and(|last| observed.saturating_sub(last) > MAX_IDLE_OBSERVATION_GAP)
            {
                // No samples during suspend/process stalls: they cannot prove idle.
                self.idle_since = None;
            }
            self.idle_observed_at = Some(observed);
            let first = *self.idle_since.get_or_insert(observed);
            if observed.saturating_sub(first) < IDLE_FOR {
                reasons.push("设备需连续空闲 2 分钟".into());
            }
        } else {
            self.idle_since = None;
            self.idle_observed_at = None;
        }
        reasons
    }

    fn observe_clock_generation(&mut self, generation: u64) {
        if self.clock_generation != generation {
            self.clock_generation = generation;
            self.idle_since = None;
            self.idle_observed_at = None;
        }
    }

    async fn fetch(&self, url: &str, max: usize) -> Result<Vec<u8>, String> {
        let mut response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status().as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|value| value > max as u64)
        {
            return Err("响应过大".into());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
            if bytes.len().saturating_add(chunk.len()) > max {
                return Err("响应过大".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    fn save_status(&self) {
        self.publish();
        let install_wait = self.install_deadline.and_then(|deadline| {
            Some(RetryTimer {
                boot_id: elapsed::boot_id()?,
                deadline_ms: u64::try_from(deadline.as_millis()).ok()?,
            })
        });
        let retry = self.retry_deadline.and_then(|deadline| {
            Some(RetryTimer {
                boot_id: elapsed::boot_id()?,
                deadline_ms: u64::try_from(deadline.as_millis()).ok()?,
            })
        });
        let _ = atomic_json(
            &self.dir.join("ota-state.json"),
            &StoredStatus {
                status: self.display_status(),
                retry,
                install_wait,
            },
            0o600,
        );
    }
}

pub fn has_update(candidate: &Candidate) -> bool {
    newer(&candidate.manifest.version, env!("DATAD_VERSION"))
}

pub fn validate_config(config: &Config) -> Result<(), String> {
    if config.servers.len() > 8 {
        return Err("自定义更新服务器最多 8 个".into());
    }
    let mut seen = HashSet::new();
    for (index, raw) in config.servers.iter().enumerate() {
        let value = raw.trim().trim_end_matches('/');
        let valid = Url::parse(value).ok().is_some_and(|url| {
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        });
        if !valid {
            return Err(format!("第 {} 个更新服务器必须是 HTTPS 地址", index + 1));
        }
        if !seen.insert(value.to_owned()) {
            return Err("更新服务器地址重复".into());
        }
    }
    if config.sources.is_empty() {
        return Err("至少选择一个更新来源".into());
    }
    let mut sources = HashSet::new();
    for source in &config.sources {
        if !matches!(source.as_str(), "custom" | "netdisk" | "github") {
            return Err("更新来源无效".into());
        }
        if !sources.insert(source) {
            return Err("更新来源重复".into());
        }
    }
    Ok(())
}

pub fn validate_manifest(manifest: &Manifest) -> Result<(), String> {
    if manifest.notes.len() > 32768 {
        return Err("Release notes exceed limit".into());
    }
    let version_ok = manifest.version.split('.').count() == 3
        && manifest
            .version
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|value| value.is_ascii_digit()));
    if manifest.schema != 1 || !version_ok || manifest.tag != format!("v{}", manifest.version) {
        return Err("版本清单无效".into());
    }
    for key in ["installer", "binary"] {
        let artifact = manifest.artifacts.get(key).ok_or("产物清单无效")?;
        if artifact.size <= 0
            || artifact.sha256.len() != 64
            || !artifact
                .sha256
                .bytes()
                .all(|value| value.is_ascii_hexdigit() && !value.is_ascii_uppercase())
            || artifact.name.is_empty()
            || !artifact
                .name
                .bytes()
                .all(|value| value.is_ascii_alphanumeric() || b"._-".contains(&value))
        {
            return Err("产物清单无效".into());
        }
    }
    Ok(())
}

fn parse_public_key(pem: &str) -> Result<VerifyingKey, String> {
    let encoded: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    let der = STANDARD.decode(encoded).map_err(|_| "OTA 公钥无效")?;
    const PREFIX: &[u8] = &[
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    if der.len() != PREFIX.len() + 32 || !der.starts_with(PREFIX) {
        return Err("OTA 公钥无效".into());
    }
    let bytes: [u8; 32] = der[PREFIX.len()..].try_into().map_err(|_| "OTA 公钥无效")?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| "OTA 公钥无效".into())
}

fn verify(key: &VerifyingKey, message: &[u8], encoded: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(encoded).map_err(|_| "签名校验失败")?;
    let raw = STANDARD.decode(text.trim()).map_err(|_| "签名校验失败")?;
    let signature = Signature::from_slice(&raw).map_err(|_| "签名校验失败")?;
    key.verify(message, &signature)
        .map_err(|_| "签名校验失败".into())
}

fn source_url(base: &str, name: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), name)
}

pub fn source_name(source: &str) -> &'static str {
    match source.trim().trim_end_matches('/') {
        "" => "",
        "网盘" => "网盘",
        "GitHub" => "GitHub",
        "自定义服务器" => "自定义服务器",
        NETDISK => "网盘",
        GITHUB => "GitHub",
        _ => "自定义服务器",
    }
}

fn newer(left: &str, right: &str) -> bool {
    let values = |value: &str| -> Vec<u64> {
        value
            .split('.')
            .take(3)
            .map(|part| part.parse().unwrap_or(0))
            .collect()
    };
    values(left) > values(right)
}

fn now() -> i64 {
    elapsed::wall_epoch()
}

fn number_at(value: &Value, path: &[&str]) -> Option<f64> {
    path.iter()
        .try_fold(value, |current, key| current.get(*key))
        .and_then(Value::as_f64)
}

fn bool_at(value: &Value, path: &[&str]) -> bool {
    path.iter()
        .try_fold(value, |current, key| current.get(*key))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn hex_sha256(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|value| format!("{value:02x}"))
        .collect()
}

/// Tell the installer to update the directory this daemon runs from, so an
/// install outside `/data/zwrt-datad` is upgraded in place. Empty for the
/// standard location (the installer's default).
fn install_dir_env() -> String {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.canonicalize().ok())
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let standard = fs::canonicalize("/data/zwrt-datad").ok();
    match dir {
        Some(dir) if Some(&dir) != standard.as_ref() => {
            format!("DATAD_INSTALL_DIR={} ", shell_quote(dir))
        }
        _ => String::new(),
    }
}

fn shell_quote(value: impl AsRef<Path>) -> String {
    format!(
        "'{}'",
        value.as_ref().to_string_lossy().replace('\'', "'\\''")
    )
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let data = fs::read(path).ok()?;
    if data.len() > 1024 * 1024 {
        return None;
    }
    serde_json::from_slice(&data).ok()
}

fn atomic_json(path: &Path, value: &impl Serialize, mode: u32) -> Result<(), String> {
    let mut data = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    data.push(b'\n');
    atomic_write(path, &data, mode)
}

fn atomic_write(path: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(mode)
        .open(&tmp)
        .map_err(|e| e.to_string())?;
    file.write_all(data).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    fs::rename(&tmp, path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::get};
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn installation_timeout_uses_boot_elapsed_time_across_wall_clock_steps() {
        elapsed::with_clock(Duration::from_secs(100), 1_780_000_000, || {
            let dir = std::env::temp_dir()
                .join(format!("datad-ota-install-clock-{}", rand::random::<u64>()));
            let mut ota = Ota::load(&dir).unwrap();
            ota.status.state = "installing".into();
            ota.status.latest_version = "99.0.0".into();
            ota.install_deadline = Some(elapsed::now() + Duration::from_secs(1800));
            ota.save_status();
            elapsed::advance(Duration::from_secs(60), 86400);
            let mut reopened = Ota::load(&dir).unwrap();
            reopened.reconcile_install_result();
            assert_eq!(reopened.status.state, "installing");
            elapsed::advance(Duration::from_secs(1740), -172800);
            reopened.reconcile_install_result();
            assert_eq!(reopened.status.state, "error");
            fs::remove_dir_all(dir).unwrap();
        });
    }

    #[test]
    fn newer_running_build_does_not_offer_an_old_saved_candidate() {
        let dir = std::env::temp_dir().join(format!("zwrt-ota-upgraded-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut ota = Ota::load(&dir).unwrap();
        for state in ["available", "waiting_idle"] {
            ota.status.state = state.into();
            ota.status.latest_version = "0.0.1".into();
            ota.status.signature_verified = true;
            ota.status.wait_reasons = vec!["old storage warning".into()];
            ota.save_status();
            let restored = Ota::load(&dir).unwrap();
            assert_eq!(restored.status.state, "idle");
            assert_eq!(restored.status.latest_version, env!("DATAD_VERSION"));
            assert!(!restored.status.signature_verified);
            assert!(restored.status.wait_reasons.is_empty());
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn installing_survives_restart_and_result_must_match_running_version() {
        let dir = std::env::temp_dir().join(format!("zwrt-ota-result-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut ota = Ota::load(&dir).unwrap();
        ota.status.state = "installing".into();
        ota.status.install_started_at = now();
        ota.status.latest_version = "99.0.0".into();
        ota.save_status();
        let mut restarted = Ota::load(&dir).unwrap();
        assert!(restarted.subscribe().borrow()["busy"].as_bool().unwrap());
        assert!(restarted.begin_update().is_err());
        assert!(restarted.update_config(Config::default()).is_err());
        assert!(!restarted.should_auto_check());
        fs::write(dir.join("ota-install-result"), "success").unwrap();
        restarted.reconcile_install_result();
        assert_eq!(restarted.status.state, "error");
        restarted.status.latest_version = env!("DATAD_VERSION").into();
        fs::write(dir.join("ota-install-result"), "success").unwrap();
        restarted.reconcile_install_result();
        assert_eq!(restarted.status.state, "succeeded");
        assert_eq!(restarted.status.progress, 100);
        restarted.status.state = "installing".into();
        restarted.install_deadline = Some(elapsed::now());
        restarted.reconcile_install_result();
        assert_eq!(restarted.status.state, "error");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn public_key_and_defaults_are_valid() {
        parse_public_key(PUBLIC_KEY).unwrap();
        validate_config(&Config::default()).unwrap();
        assert_eq!(
            source_url(NETDISK, "update.json"),
            format!("{NETDISK}/update.json")
        );
        assert_eq!(Config::default().sources, default_sources());
        assert_eq!(source_name(NETDISK), "网盘");
        assert!(newer("9.1.0", "9.0.99"));
    }

    #[test]
    fn rejects_unsafe_servers() {
        for server in [
            "http://updates.example",
            "https://u:p@updates.example",
            "https://updates.example/?token=x",
        ] {
            assert!(
                validate_config(&Config {
                    enabled: true,
                    servers: vec![server.into()],
                    ..Default::default()
                })
                .is_err()
            );
        }
    }

    #[test]
    fn manual_safety_only_checks_battery_and_storage() {
        let dir = std::env::temp_dir().join(format!("zwrt-datad-ota-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut ota = Ota::load(&dir).unwrap();
        let reasons = ota.safety(&json!({"battery":{"percent":10},"runtime":{"storage":{"available":1},"cpu_usage_tenths":900,"throughput":{"rx_bps":9999999,"tx_bps":0}},"neighbor":{"enabled":true}}), true);
        assert_eq!(reasons, ["电量必须高于 10%", "可用存储不足 64 MiB"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_battery_is_not_a_low_battery() {
        let dir = std::env::temp_dir().join(format!("zwrt-datad-ota-nobat-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut ota = Ota::load(&dir).unwrap();
        let storage = json!({"available":134217728});
        let with = |battery: Value| json!({"battery":battery,"runtime":{"storage":storage}});
        // CPE placeholder: no battery fitted.
        assert!(
            ota.safety(&with(json!({"percent":0,"online":0})), true)
                .is_empty()
        );
        // A real low battery still blocks, including a battery that reads 0 %.
        for battery in [
            json!({"percent":10,"online":1}),
            json!({"percent":0,"online":1}),
            json!({"percent":5}),
        ] {
            assert_eq!(ota.safety(&with(battery), true), ["电量必须高于 10%"]);
        }
        assert!(
            ota.safety(&with(json!({"percent":80,"online":1})), true)
                .is_empty()
        );
        let _ = fs::remove_dir_all(dir);
    }

    fn timer_candidate() -> Candidate {
        Candidate {
            manifest: Manifest {
                notes: String::new(),
                schema: 1,
                version: "99.0.0".into(),
                tag: "v99.0.0".into(),
                published_at: String::new(),
                artifacts: BTreeMap::new(),
            },
            base_url: "https://updates.example".into(),
        }
    }

    #[test]
    fn ota_period_and_retry_do_not_follow_eight_hour_wall_steps() {
        let dir =
            std::env::temp_dir().join(format!("datad-ota-elapsed-period-{}", std::process::id()));
        for step in [-8 * 3600, 8 * 3600] {
            let _ = fs::remove_dir_all(&dir);
            elapsed::with_clock(Duration::from_secs(100), 1_780_000_000, || {
                let mut ota = Ota::load(&dir).unwrap();
                ota.candidate = Some(timer_candidate());
                ota.mark_auto_check();
                ota.fail(None, "fixture".into());
                elapsed::advance(Duration::ZERO, step);
                assert!(ota.auto_candidate().is_none());
                assert!(!ota.should_auto_check());
                assert_eq!(
                    ota.status_json()["status"]["next_retry_at"],
                    1_780_000_000 + step + 3600
                );
                elapsed::advance(Duration::from_secs(3600 - 1), 0);
                assert!(ota.auto_candidate().is_none());
                elapsed::advance(Duration::from_secs(1), 0);
                assert!(ota.auto_candidate().is_some());
                assert!(!ota.should_auto_check());
                elapsed::advance(Duration::from_secs(5 * 3600), 0);
                assert!(ota.should_auto_check());
            });
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ota_idle_requires_observations_not_wall_steps_or_suspend() {
        let dir =
            std::env::temp_dir().join(format!("datad-ota-elapsed-idle-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        elapsed::with_clock(Duration::from_secs(100), 1_780_000_000, || {
            let mut ota = Ota::load(&dir).unwrap();
            let snapshot = json!({"battery":{"percent":80},"runtime":{"storage":{"available":134217728},"cpu_usage_tenths":0,"throughput":{"rx_bps":0,"tx_bps":0}},"neighbor":{"enabled":false}});
            assert!(!ota.safety(&snapshot, false).is_empty());
            elapsed::advance(Duration::ZERO, 8 * 3600);
            assert!(!ota.safety(&snapshot, false).is_empty());
            elapsed::advance(Duration::from_secs(60), -8 * 3600);
            assert!(!ota.safety(&snapshot, false).is_empty());
            elapsed::advance(Duration::from_secs(60), 0);
            assert!(ota.safety(&snapshot, false).is_empty());
            // A sleeping/stalled system has not supplied fresh idle observations.
            elapsed::advance(Duration::from_secs(8 * 3600), 0);
            assert!(!ota.safety(&snapshot, false).is_empty());
            elapsed::advance(Duration::from_secs(60), 0);
            assert!(!ota.safety(&snapshot, false).is_empty());
            elapsed::advance(Duration::from_secs(60), 0);
            assert!(ota.safety(&snapshot, false).is_empty());
            ota.observe_clock_generation(ota.clock_generation + 1);
            assert!(ota.idle_since.is_none());
            assert!(ota.idle_observed_at.is_none());
        });
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn retry_restores_same_boot_deadline_without_trusting_rtc() {
        let dir =
            std::env::temp_dir().join(format!("datad-ota-elapsed-restart-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        elapsed::with_clock(Duration::from_secs(100), 1_780_000_000, || {
            let mut ota = Ota::load(&dir).unwrap();
            ota.fail(None, "fixture".into());
            elapsed::advance(Duration::from_secs(900), -8 * 3600);
            drop(ota);
            let ota = Ota::load(&dir).unwrap();
            assert!(!ota.retry_ready());
            assert_eq!(
                ota.status_json()["status"]["next_retry_at"],
                elapsed::wall_epoch() + 2700
            );
            assert!(ota.status_json()["status"].get("retry").is_none());
            elapsed::advance(Duration::from_secs(2700), 8 * 3600);
            drop(ota);
            assert!(Ota::load(&dir).unwrap().retry_ready());
        });
        // A reboot resets BOOTTIME. A bounded fresh cooldown is safer than
        // interpreting the previous kernel's deadline or an arbitrary RTC value.
        elapsed::with_clock(Duration::ZERO, 1_780_000_000 + 8 * 3600, || {
            elapsed::set_boot_id("another-test-boot");
            let ota = Ota::load(&dir).unwrap();
            assert!(!ota.retry_ready());
            elapsed::advance(Duration::from_secs(3600), -8 * 3600);
            assert!(ota.retry_ready());
        });
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn legacy_retry_state_rearms_a_bounded_elapsed_cooldown() {
        let dir =
            std::env::temp_dir().join(format!("datad-ota-elapsed-legacy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let status = Status {
            state: "error".into(),
            failure_count: 1,
            next_retry_at: 1_780_000_000 + 8 * 3600,
            ..Default::default()
        };
        fs::write(
            dir.join("ota-state.json"),
            serde_json::to_vec(&status).unwrap(),
        )
        .unwrap();
        elapsed::with_clock(Duration::ZERO, 1_780_000_000, || {
            let ota = Ota::load(&dir).unwrap();
            assert!(!ota.retry_ready());
            elapsed::advance(Duration::from_secs(3600), -8 * 3600);
            assert!(ota.retry_ready());
        });
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn verifies_ed25519_manifest_bytes() {
        let signing = SigningKey::from_bytes(&[7; 32]);
        let raw = br#"{"schema":1}"#;
        let encoded = STANDARD.encode(signing.sign(raw).to_bytes());
        verify(&signing.verifying_key(), raw, encoded.as_bytes()).unwrap();
        assert!(verify(&signing.verifying_key(), b"tampered", encoded.as_bytes()).is_err());
    }

    #[tokio::test]
    async fn check_uses_order_and_rejects_bad_signature() {
        let signing = SigningKey::from_bytes(&[9; 32]);
        let manifest = Manifest {
            notes: String::new(),
            schema: 1,
            version: "99.0.0".into(),
            tag: "v99.0.0".into(),
            published_at: "2026-09-14T00:00:00Z".into(),
            artifacts: BTreeMap::from([
                (
                    "installer".into(),
                    Artifact {
                        name: "install-datad.sh".into(),
                        size: 1,
                        sha256: "a".repeat(64),
                    },
                ),
                (
                    "binary".into(),
                    Artifact {
                        name: "zwrt-datad-aarch64".into(),
                        size: 1,
                        sha256: "b".repeat(64),
                    },
                ),
            ]),
        };
        let raw = serde_json::to_vec(&manifest).unwrap();
        let signature = STANDARD.encode(signing.sign(&raw).to_bytes());
        let mut stale_manifest = manifest.clone();
        stale_manifest.version = "0.0.0".into();
        stale_manifest.tag = "v0.0.0".into();
        let stale_raw = serde_json::to_vec(&stale_manifest).unwrap();
        let stale_signature = STANDARD.encode(signing.sign(&stale_raw).to_bytes());
        let good_raw = raw.clone();
        let good_signature = signature.clone();
        let router = Router::new()
            .route(
                "/stale/update.json",
                get(move || {
                    let value = stale_raw.clone();
                    async move { value }
                }),
            )
            .route(
                "/stale/update.json.sig",
                get(move || {
                    let value = stale_signature.clone();
                    async move { value }
                }),
            )
            .route("/bad/update.json", get(|| async { "tampered" }))
            .route(
                "/bad/update.json.sig",
                get(move || {
                    let value = signature.clone();
                    async move { value }
                }),
            )
            .route(
                "/good/update.json",
                get(move || {
                    let value = good_raw.clone();
                    async move { value }
                }),
            )
            .route(
                "/good/update.json.sig",
                get(move || {
                    let value = good_signature.clone();
                    async move { value }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let dir =
            std::env::temp_dir().join(format!("zwrt-datad-ota-http-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut ota = Ota::load(&dir).unwrap();
        ota.key = signing.verifying_key();
        ota.config.servers = vec![
            format!("http://{address}/stale"),
            format!("http://{address}/bad"),
            format!("http://{address}/good"),
        ];
        ota.config.sources = vec!["custom".into()];
        let candidate = ota.check().await.unwrap();
        assert_eq!(candidate.base_url, format!("http://{address}/good"));
        assert_eq!(candidate.manifest.version, "99.0.0");
        assert!(ota.status.signature_verified);
        let id = Ota::candidate_id(&candidate);
        assert!(ota.approved_candidate(&id).is_ok());
        let mut changed = candidate.clone();
        changed.manifest.artifacts.get_mut("binary").unwrap().sha256 = "c".repeat(64);
        assert_ne!(Ota::candidate_id(&changed), id);
        assert!(
            ota.approved_candidate(&Ota::candidate_id(&changed))
                .is_err()
        );
        changed = candidate.clone();
        changed.base_url.push_str("/other");
        assert_ne!(Ota::candidate_id(&changed), id);
        let watch = ota.subscribe();
        assert_eq!(watch.borrow()["candidate"]["id"], id);
        ota.config.servers = vec![format!("http://{address}/bad")];
        assert!(ota.check().await.is_err());
        assert!(ota.approved_candidate(&id).is_err());
        assert!(watch.borrow()["candidate"].is_null());
        assert_eq!(watch.borrow()["status"]["signature_verified"], false);
        assert_eq!(watch.borrow()["status"]["latest_version"], Value::Null);
        assert_eq!(watch.borrow()["status"]["wait_reasons"], Value::Null);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn update_sources_default_to_github_then_netdisk_and_follow_the_stored_order() {
        let dir = std::env::temp_dir().join(format!(
            "zwrt-datad-ota-order-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let order = |dir: &Path| Ota::load(dir).unwrap().servers();
        // Nothing stored: GitHub first, the netdisk mirror as the fallback.
        assert_eq!(order(&dir), vec![GITHUB.to_owned(), NETDISK.to_owned()]);
        // The user's own servers come before both.
        fs::write(
            dir.join("ota.json"),
            r#"{"enabled":true,"servers":["https://mirror.example/updates/"],"sources":["custom","github","netdisk"]}"#,
        )
        .unwrap();
        assert_eq!(
            order(&dir),
            vec![
                "https://mirror.example/updates".to_owned(),
                GITHUB.to_owned(),
                NETDISK.to_owned()
            ]
        );
        // The pre-0.10.62 default (netdisk first) was never a choice and follows the new default.
        fs::write(
            dir.join("ota.json"),
            r#"{"enabled":true,"servers":[],"sources":["custom","netdisk","github"]}"#,
        )
        .unwrap();
        assert_eq!(order(&dir), vec![GITHUB.to_owned(), NETDISK.to_owned()]);
        // Any other stored order or subset is the user's and is kept.
        fs::write(
            dir.join("ota.json"),
            r#"{"enabled":true,"servers":[],"sources":["netdisk","github"]}"#,
        )
        .unwrap();
        assert_eq!(order(&dir), vec![NETDISK.to_owned(), GITHUB.to_owned()]);
        fs::write(
            dir.join("ota.json"),
            r#"{"enabled":true,"servers":["https://mirror.example/u"],"sources":["custom"]}"#,
        )
        .unwrap();
        assert_eq!(order(&dir), vec!["https://mirror.example/u".to_owned()]);
        let _ = fs::remove_dir_all(dir);
    }
}
