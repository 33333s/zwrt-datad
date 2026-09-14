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
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::process::Command;

const NETDISK: &str = "https://pan.ericsfj.com/d/github%20releases/zwrt-datad";
const GITHUB: &str = "https://github.com/33333s/zwrt-datad/releases/latest/download";
const NETDISK_MANIFEST: &str = "https://pan.ericsfj.com/p/%E4%B8%AD%E5%9B%BD%E7%A7%BB%E5%8A%A8%E4%BA%91%E7%9B%982/cherry%20studio/github%20releases/zwrt-datad/update.json?sign=TZsBt-YNMzLwMcS5FX-ksLg2sK_fCNGOWxzaKN_4JOU=:0";
const NETDISK_SIGNATURE: &str = "https://pan.ericsfj.com/p/%E4%B8%AD%E5%9B%BD%E7%A7%BB%E5%8A%A8%E4%BA%91%E7%9B%982/cherry%20studio/github%20releases/zwrt-datad/update.json.sig?sign=gvS_X703akHpSbzEZjzWUvLB7yvZsmPgm8RCjKzNSCA=:0";
const NETDISK_INSTALLER: &str = "https://pan.ericsfj.com/p/%E4%B8%AD%E5%9B%BD%E7%A7%BB%E5%8A%A8%E4%BA%91%E7%9B%982/cherry%20studio/github%20releases/zwrt-datad/install-datad.sh?sign=8qh7EYrKi_zRCOnHQKNU7k2XxghIxIyiJnoalfNlUss=:0";
const NETDISK_BINARY: &str = "https://pan.ericsfj.com/p/%E4%B8%AD%E5%9B%BD%E7%A7%BB%E5%8A%A8%E4%BA%91%E7%9B%982/cherry%20studio/github%20releases/zwrt-datad/zwrt-datad-aarch64?sign=dOcZWpLnxOETF2JboS_3_191tszCrcTI7_khCWUfLqg=:0";
const PUBLIC_KEY: &str = include_str!("../../cloud/ota_public.pem");
const IDLE_FOR: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub enabled: bool,
    pub servers: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            servers: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Artifact {
    pub name: String,
    pub size: i64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: i64,
    pub version: String,
    pub tag: String,
    pub published_at: String,
    pub artifacts: BTreeMap<String, Artifact>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
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
    dir: PathBuf,
    config: Config,
    status: Status,
    candidate: Option<Candidate>,
    idle_since: Option<i64>,
    pub busy: bool,
    client: Client,
    key: VerifyingKey,
    last_auto_check: i64,
}

impl Ota {
    pub fn load(dir: &Path) -> Result<Self, String> {
        let key = parse_public_key(PUBLIC_KEY)?;
        let config = read_json::<Config>(&dir.join("ota.json"))
            .filter(|value| validate_config(value).is_ok())
            .unwrap_or_default();
        let mut status = read_json::<Status>(&dir.join("ota-state.json")).unwrap_or_default();
        status.current_version = env!("DATAD_VERSION").into();
        let client = Client::builder()
            .timeout(Duration::from_secs(45))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            dir: dir.to_owned(),
            config,
            status,
            candidate: None,
            idle_since: None,
            busy: false,
            client,
            key,
            last_auto_check: 0,
        })
    }

    pub fn config_json(&self) -> Value {
        json!({"success":true,"config":self.config,"default_servers":[NETDISK,GITHUB]})
    }

    pub fn status_json(&self) -> Value {
        json!({"success":true,"status":self.status,"enabled":self.config.enabled})
    }

    pub fn begin_update(&mut self) -> Result<(), String> {
        if self.busy {
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
    }

    pub fn update_config(&mut self, mut config: Config) -> Result<Value, String> {
        validate_config(&config)?;
        for server in &mut config.servers {
            *server = server.trim().trim_end_matches('/').to_owned();
        }
        atomic_json(&self.dir.join("ota.json"), &config, 0o600)?;
        self.config = config;
        Ok(json!({"success":true,"config":self.config}))
    }

    fn servers(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        self.config
            .servers
            .iter()
            .map(|value| value.trim().trim_end_matches('/'))
            .chain([NETDISK, GITHUB])
            .filter(|value| !value.is_empty() && seen.insert((*value).to_owned()))
            .map(str::to_owned)
            .collect()
    }

    pub async fn check(&mut self) -> Result<Candidate, String> {
        self.status.state = "checking".into();
        self.status.error.clear();
        self.status.last_check_at = now();
        self.status.progress = 0;
        self.save_status();
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
            let candidate = Candidate {
                manifest,
                base_url: base,
            };
            if !has_update(&candidate) {
                valid_but_current.get_or_insert(candidate);
                continue;
            }
            self.status.latest_version = candidate.manifest.version.clone();
            self.status.source = candidate.base_url.clone();
            self.status.signature_verified = true;
            self.status.wait_reasons.clear();
            self.status.state = "available".into();
            self.candidate = Some(candidate.clone());
            self.save_status();
            return Ok(candidate);
        }
        if let Some(candidate) = valid_but_current {
            self.status.latest_version = candidate.manifest.version.clone();
            self.status.source = candidate.base_url.clone();
            self.status.signature_verified = true;
            self.status.wait_reasons.clear();
            self.status.state = "idle".into();
            self.candidate = Some(candidate.clone());
            self.save_status();
            return Ok(candidate);
        }
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
        let raw = self
            .fetch(&source_url(&candidate.base_url, &installer.name), 16 << 20)
            .await?;
        if hex_sha256(&raw) != installer.sha256 {
            return Err("安装器 SHA-256 校验失败".into());
        }
        let installer_path = self.dir.join("ota-installer.sh");
        atomic_write(&installer_path, &raw, 0o700)?;
        let binary = candidate
            .manifest
            .artifacts
            .get("binary")
            .ok_or("缺少二进制")?;
        let wrapper = self.dir.join("ota-run.sh");
        let result = self.dir.join("ota-result.log");
        let marker = self.dir.join("ota-install-result");
        let script = format!(
            "#!/bin/sh\nsleep 2\nrm -f {}\nif DATAD_DOWNLOAD_URL={} sh {} >{} 2>&1; then value=success; else value=failed; fi\nprintf '%s\\n' \"$value\" >{}.tmp\nmv -f {}.tmp {}\n",
            shell_quote(&marker),
            shell_quote(source_url(&candidate.base_url, &binary.name)),
            shell_quote(&installer_path),
            shell_quote(&result),
            shell_quote(&marker),
            shell_quote(&marker),
            shell_quote(&marker),
        );
        atomic_write(&wrapper, script.as_bytes(), 0o700)?;
        self.status.state = "installing".into();
        self.status.progress = 100;
        self.save_status();
        Command::new("/bin/sh")
            .arg(&wrapper)
            .kill_on_drop(false)
            .spawn()
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn fail(&mut self, candidate: Option<&Candidate>, error: String) {
        self.status.state = "error".into();
        self.status.error = error;
        self.status.failure_count = self.status.failure_count.saturating_add(1);
        let delay = match self.status.failure_count {
            1 => 3600,
            2 => 6 * 3600,
            3 => 24 * 3600,
            _ => {
                self.status.state = "blocked".into();
                0
            }
        };
        self.status.next_retry_at = if delay == 0 { 0 } else { now() + delay };
        if let Some(candidate) = candidate {
            self.status.latest_version = candidate.manifest.version.clone();
            self.status.source = candidate.base_url.clone();
        }
        self.save_status();
    }

    pub fn reconcile_install_result(&mut self) {
        let marker = self.dir.join("ota-install-result");
        let Ok(value) = fs::read_to_string(&marker) else {
            return;
        };
        let _ = fs::remove_file(marker);
        if value.trim() == "success" {
            self.status.state = "succeeded".into();
            self.status.current_version = env!("DATAD_VERSION").into();
            self.status.last_success_at = now();
            self.status.error.clear();
            self.status.wait_reasons.clear();
            self.status.failure_count = 0;
            self.status.next_retry_at = 0;
        } else {
            self.status.state = "error".into();
            self.status.error = "安装器执行失败，已由安装器回滚".into();
            self.status.failure_count = self.status.failure_count.saturating_add(1);
        }
        self.save_status();
    }

    pub fn auto_candidate(&mut self) -> Option<Candidate> {
        if !self.config.enabled || self.busy || self.status.next_retry_at > now() {
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
            && self.status.next_retry_at <= now()
            && (self.candidate.is_none() || now() - self.last_auto_check >= 6 * 3600)
    }

    pub fn mark_auto_check(&mut self) {
        self.last_auto_check = now();
    }

    fn safety(&mut self, snapshot: &Value, manual: bool) -> Vec<String> {
        let mut reasons = Vec::new();
        let battery = number_at(snapshot, &["battery", "percent"])
            .or_else(|| number_at(snapshot, &["system", "battery_percent"]));
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
            let first = *self.idle_since.get_or_insert_with(now);
            if now() - first < IDLE_FOR.as_secs() as i64 {
                reasons.push("设备需连续空闲 2 分钟".into());
            }
        } else {
            self.idle_since = None;
        }
        reasons
    }

    async fn fetch(&self, url: &str, max: usize) -> Result<Vec<u8>, String> {
        let response = self
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
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        if bytes.len() > max {
            return Err("响应过大".into());
        }
        Ok(bytes.to_vec())
    }

    fn save_status(&self) {
        let _ = atomic_json(&self.dir.join("ota-state.json"), &self.status, 0o600);
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
    Ok(())
}

pub fn validate_manifest(manifest: &Manifest) -> Result<(), String> {
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
    if base == NETDISK {
        match name {
            "update.json" => return NETDISK_MANIFEST.into(),
            "update.json.sig" => return NETDISK_SIGNATURE.into(),
            "install-datad.sh" => return NETDISK_INSTALLER.into(),
            "zwrt-datad-aarch64" => return NETDISK_BINARY.into(),
            _ => {}
        }
    }
    format!("{}/{}", base.trim_end_matches('/'), name)
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
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
    fn public_key_and_defaults_are_valid() {
        parse_public_key(PUBLIC_KEY).unwrap();
        validate_config(&Config::default()).unwrap();
        assert_eq!(source_url(NETDISK, "update.json"), NETDISK_MANIFEST);
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
                    servers: vec![server.into()]
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
        let candidate = ota.check().await.unwrap();
        assert_eq!(candidate.base_url, format!("http://{address}/good"));
        assert_eq!(candidate.manifest.version, "99.0.0");
        assert!(ota.status.signature_verified);
        let _ = fs::remove_dir_all(dir);
    }
}
