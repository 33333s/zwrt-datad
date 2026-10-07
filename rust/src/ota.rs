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

const NETDISK: &str = "https://pan.ericsfj.com/sd/wN2PJUK8";
const GITHUB: &str = "https://github.com/33333s/zwrt-datad/releases/latest/download";
const PUBLIC_KEY: &str = include_str!("../ota_public.pem");
const IDLE_FOR: Duration = Duration::from_secs(120);

/// Platform-specific update behaviour. The ZWRT ARM64 devices keep the defaults;
/// the ARM32 U50S has its own signed manifest, no curl/wget, a tiny data
/// volume and a systemd-managed service.
#[derive(Clone, Copy, Debug)]
pub struct Profile {
    /// Signed manifest file name (its signature is `<name>.sig`).
    pub manifest: &'static str,
    /// Minimum free bytes for the legacy generic platform. U50 checks actual paths.
    pub min_free_bytes: f64,
    /// The daemon downloads and verifies the binary itself and hands the
    /// installer a local file (`DATAD_BINARY_FILE`) instead of a URL.
    pub download_in_process: bool,
    /// Start the installer as a transient systemd unit so it survives the
    /// restart of the service that spawned it.
    pub detached_systemd: bool,
    /// Base URL of the built-in GitHub source.
    pub github: &'static str,
    /// Whether the netdisk mirror carries this platform's manifest.
    pub netdisk: bool,
}

impl Profile {
    pub const ZWRT: Profile = Profile {
        manifest: "update.json",
        min_free_bytes: 64.0 * 1024.0 * 1024.0,
        download_in_process: false,
        detached_systemd: false,
        github: GITHUB,
        netdisk: true,
    };
    pub const U50: Profile = Profile {
        manifest: "update-armv7.json",
        // U50 downloads use /tmp; install-volume space is checked separately.
        min_free_bytes: 0.0,
        download_in_process: true,
        detached_systemd: true,
        // GitHub's "latest" is the ARM64 line's newest release, which has no
        // ARMv7 assets. ARM32 releases are published as ordinary vX.Y.Z
        // releases that are never "latest", and each one also refreshes this
        // rolling channel release so devices have one stable URL.
        github: "https://github.com/33333s/zwrt-datad/releases/download/arm32-latest",
        netdisk: false,
    };
}

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

fn default_sources() -> Vec<String> {
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

/// Private download directory, removed on preparation failure. A successful
/// preparation transfers ownership to the detached wrapper.
struct DownloadStage {
    path: PathBuf,
    keep: bool,
}
impl DownloadStage {
    fn new() -> Result<Self, String> {
        use std::os::unix::fs::DirBuilderExt;
        let path = Path::new("/tmp").join(format!("zwrt-datad-ota-{:016x}", rand::random::<u64>()));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|e| e.to_string())?;
        Ok(Self { path, keep: false })
    }
    fn remove(path: &Path) {
        if path.parent() == Some(Path::new("/tmp"))
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("zwrt-datad-ota-"))
        {
            let _ = fs::remove_dir_all(path);
        }
    }
}
impl Drop for DownloadStage {
    fn drop(&mut self) {
        if !self.keep {
            Self::remove(&self.path);
        }
    }
}

#[allow(clippy::unnecessary_cast)]
fn available_bytes(path: &Path) -> Result<u64, String> {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(name.as_ptr(), &mut stat) } != 0 {
        return Err(format!("无法检查 {} 可用空间", path.display()));
    }
    let block = if stat.f_frsize > 0 {
        stat.f_frsize
    } else {
        stat.f_bsize
    } as u64;
    Ok(block.saturating_mul(stat.f_bavail as u64))
}
fn stage_space_reasons(
    tmp_free: u64,
    install_free: u64,
    binary: u64,
    installer: u64,
) -> Vec<String> {
    let mut reasons = Vec::new();
    for (name, free, need) in [
        (
            "/tmp 下载目录",
            tmp_free,
            binary.saturating_add(installer).saturating_add(512 * 1024),
        ),
        ("安装目录", install_free, binary.saturating_add(512 * 1024)),
    ] {
        if free < need {
            reasons.push(format!(
                "{name}可用空间不足，需 {} MiB",
                need.div_ceil(1024 * 1024)
            ));
        }
    }
    reasons
}

pub struct Ota {
    view: tokio::sync::watch::Sender<Value>,
    profile: Profile,
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
    #[cfg(test)]
    pub fn load(dir: &Path) -> Result<Self, String> {
        Self::load_with(dir, Profile::ZWRT)
    }

    pub fn load_with(dir: &Path, profile: Profile) -> Result<Self, String> {
        let key = parse_public_key(PUBLIC_KEY)?;
        let config = read_json::<Config>(&dir.join("ota.json"))
            .filter(|value| validate_config(value).is_ok())
            .unwrap_or_default();
        let mut status = read_json::<Status>(&dir.join("ota-state.json")).unwrap_or_default();
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
        let (view, _) = tokio::sync::watch::channel(Value::Null);
        let result = Self {
            view,
            profile,
            dir: dir.to_owned(),
            config,
            status,
            candidate: None,
            idle_since: None,
            busy: false,
            client,
            key,
            last_auto_check: 0,
        };
        result.publish();
        Ok(result)
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
        self.view.send_replace(json!({"status":self.status,"config":self.config,
            "busy": self.busy || self.status.state == "installing", "platform":self.profile.manifest,
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
        json!({"success":true,"status":self.status,"enabled":self.config.enabled})
    }

    pub fn begin_update(&mut self) -> Result<(), String> {
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
            return Err("更新任务正在运行".into());
        }
        validate_config(&config)?;
        for server in &mut config.servers {
            *server = server.trim().trim_end_matches('/').to_owned();
        }
        atomic_json(&self.dir.join("ota.json"), &config, 0o600)?;
        self.config = config;
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

    fn servers(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let enabled: HashSet<_> = self.config.sources.iter().map(String::as_str).collect();
        let mut ordered = Vec::new();
        if enabled.contains("custom") {
            ordered.extend(self.config.servers.iter().map(String::as_str));
        }
        if enabled.contains("netdisk") && self.profile.netdisk {
            ordered.push(NETDISK);
        }
        if enabled.contains("github") {
            ordered.push(self.profile.github);
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
            return Err("更新任务正在运行".into());
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
        let mut errors = Vec::new();
        let mut valid_but_current = None;
        for base in self.servers() {
            let manifest_name = self.profile.manifest;
            let raw = match self.fetch(&source_url(&base, manifest_name), 1 << 20).await {
                Ok(value) => value,
                Err(error) => {
                    errors.push(format!("{base}: {error}"));
                    continue;
                }
            };
            let encoded = match self
                .fetch(&source_url(&base, &format!("{manifest_name}.sig")), 4096)
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
        let wrapper = self.prepare_install(candidate, snapshot, manual).await?;
        self.status.state = "installing".into();
        self.status.progress = 95;
        self.status.install_started_at = now();
        self.save_status();
        let spawned = if self.profile.detached_systemd {
            // The installer restarts this very service; a child in the
            // service's cgroup would be killed with it, so run it as its own
            // transient unit.
            Command::new("systemd-run")
                .args([
                    "--no-block",
                    "--collect",
                    "--quiet",
                    "--unit=zwrt-datad-ota",
                    "/bin/sh",
                ])
                .arg(&wrapper)
                .kill_on_drop(false)
                .status()
                .await
                .map_err(|e| e.to_string())
                .and_then(|status| {
                    status
                        .success()
                        .then_some(())
                        .ok_or_else(|| format!("systemd-run exited with {status}"))
                })
        } else {
            Command::new("/bin/sh")
                .arg(&wrapper)
                .kill_on_drop(false)
                .spawn()
                .map(|_| ())
                .map_err(|e| e.to_string())
        };
        if spawned.is_err()
            && self.profile.download_in_process
            && let Some(stage) = wrapper.parent()
        {
            DownloadStage::remove(stage);
        }
        spawned
    }

    /// Everything before launching the installer: safety gates, pinned
    /// installer download, and (for in-process profiles) the verified binary.
    /// Returns the wrapper script that runs the installer.
    async fn prepare_install(
        &mut self,
        candidate: &Candidate,
        snapshot: &Value,
        manual: bool,
    ) -> Result<PathBuf, String> {
        let mut reasons = self.safety(snapshot, manual);
        if self.profile.download_in_process {
            let binary = candidate
                .manifest
                .artifacts
                .get("binary")
                .ok_or("缺少二进制")?;
            let installer = candidate
                .manifest
                .artifacts
                .get("installer")
                .ok_or("缺少安装器")?;
            reasons.extend(stage_space_reasons(
                available_bytes(Path::new("/tmp"))?,
                available_bytes(&self.dir)?,
                binary.size.max(0) as u64,
                installer.size.max(0) as u64,
            ));
        }
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
        let binary = candidate
            .manifest
            .artifacts
            .get("binary")
            .ok_or("缺少二进制")?;
        let mut temp = if self.profile.download_in_process {
            Some(DownloadStage::new()?)
        } else {
            None
        };
        let stage_dir = temp
            .as_ref()
            .map(|t| t.path.clone())
            .unwrap_or_else(|| self.dir.clone());
        self.status.state = "downloading".into();
        self.status.progress = 5;
        self.status.wait_reasons.clear();
        self.save_status();
        let raw = self
            .fetch(&source_url(&candidate.base_url, &installer.name), 16 << 20)
            .await?;
        if raw.len() as i64 != installer.size || hex_sha256(&raw) != installer.sha256 {
            return Err("安装器 SHA-256 校验失败".into());
        }
        let installer_path = stage_dir.join("ota-installer.sh");
        atomic_write(&installer_path, &raw, 0o700)?;
        let binary_url = source_url(&candidate.base_url, &binary.name);
        let install_file = self
            .dir
            .join(format!(".ota-new-{:016x}", rand::random::<u64>()));
        let input = if self.profile.download_in_process {
            // Download to tmpfs. Only the final installation copy uses the target volume.
            self.status.progress = 30;
            self.save_status();
            let data = self.fetch_binary(&binary_url, binary.size).await?;
            if i64::try_from(data.len()).ok() != Some(binary.size)
                || hex_sha256(&data) != binary.sha256
            {
                return Err("二进制大小或 SHA-256 校验失败".into());
            }
            let staged = stage_dir.join("zwrt-datad.new");
            atomic_write(&staged, &data, 0o700)?;
            // DATAD_DIR keeps the installer's binary path correct on layouts
            // where the data directory is not the /etc_rw default (the U50 Pro
            // runs everything from /cache/zwrt-datad).
            format!(
                "DATAD_DIR={} DATAD_BINARY_FILE={}",
                shell_quote(&self.dir),
                shell_quote(&install_file)
            )
        } else {
            format!("DATAD_DOWNLOAD_URL={}", shell_quote(binary_url))
        };
        // Remove an old result BEFORE publishing the installing state.
        let _ = fs::remove_file(self.dir.join("ota-install-result"));
        let wrapper = stage_dir.join("ota-run.sh");
        let result = self.dir.join("ota-result.log");
        let marker = self.dir.join("ota-install-result");
        let script = if self.profile.download_in_process {
            // Keep compatibility with signed older installers which require their input
            // inside DATAD_DIR. This local installation copy is made only after the
            // verified download is complete, immediately before the atomic swap.
            let lock = self.dir.join(".deploy-lock");
            format!(
                r#"#!/bin/sh
owned=0
cleanup() {{
  if [ "$owned" = 1 ]; then rm -f {new} {lock}/pid; rmdir {lock} 2>/dev/null || true; fi
  rm -f {stage}/zwrt-datad.new {stage}/ota-installer.sh {stage}/ota-run.sh
  rmdir {stage} 2>/dev/null || true
}}
trap cleanup EXIT
sleep 2
value=failed
if mkdir {lock}; then
  owned=1
  printf '%s\n' "$$" > {lock}/pid
  if {{ cp {stage}/zwrt-datad.new {new} && {input} sh {installer}; }} >{log} 2>&1; then value=success; fi
else
  echo 'Another deployment holds the lock; installed service unchanged' >{log}
fi
printf '%s\n' "$value" >{marker}.tmp
mv -f {marker}.tmp {marker}
"#,
                new = shell_quote(&install_file),
                lock = shell_quote(&lock),
                stage = shell_quote(&stage_dir),
                input = input,
                installer = shell_quote(&installer_path),
                log = shell_quote(&result),
                marker = shell_quote(&marker)
            )
        } else {
            format!(
                "#!/bin/sh\nsleep 2\nrm -f {}\nif {} sh {} >{} 2>&1; then value=success; else value=failed; fi\nprintf '%s\\n' \"$value\" >{}.tmp\nmv -f {}.tmp {}\n",
                shell_quote(&marker),
                input,
                shell_quote(&installer_path),
                shell_quote(&result),
                shell_quote(&marker),
                shell_quote(&marker),
                shell_quote(&marker),
            )
        };
        atomic_write(&wrapper, script.as_bytes(), 0o700)?;
        if let Some(stage) = temp.as_mut() {
            stage.keep = true;
        }
        Ok(wrapper)
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
            self.status.source = source_name(&candidate.base_url).into();
        }
        self.save_status();
    }

    pub fn reconcile_install_result(&mut self) {
        let marker = self.dir.join("ota-install-result");
        let Ok(value) = fs::read_to_string(&marker) else {
            if self.status.state == "installing"
                && self.status.install_started_at > 0
                && now() - self.status.install_started_at > 1800
            {
                self.fail(None, "安装结果未确认，请检查设备后重试".into());
            }
            return;
        };
        let _ = fs::remove_file(marker);
        if value.trim() == "success" && self.status.latest_version == env!("DATAD_VERSION") {
            self.status.state = "succeeded".into();
            self.status.current_version = env!("DATAD_VERSION").into();
            self.status.progress = 100;
            self.status.last_success_at = now();
            self.status.error.clear();
            self.status.wait_reasons.clear();
            self.status.failure_count = 0;
            self.status.next_retry_at = 0;
        } else {
            self.status.state = "error".into();
            self.status.error = "安装未完成或运行版本与目标不符，请查看设备安装日志".into();
            self.status.failure_count = self.status.failure_count.saturating_add(1);
        }
        self.save_status();
    }

    pub fn auto_candidate(&mut self) -> Option<Candidate> {
        if !self.config.enabled
            || self.busy
            || self.status.state == "installing"
            || self.status.next_retry_at > now()
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
        let min_free = self.profile.min_free_bytes;
        if !self.profile.download_in_process
            && number_at(snapshot, &["runtime", "storage", "available"])
                .is_some_and(|value| value < min_free)
        {
            reasons.push(format!(
                "可用存储不足 {} MiB",
                (min_free / (1024.0 * 1024.0)).ceil() as u64
            ));
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

    async fn fetch_binary(&mut self, url: &str, expected: i64) -> Result<Vec<u8>, String> {
        if !(1..=16 << 20).contains(&expected) {
            return Err("二进制大小超出限制".into());
        }
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
            .is_some_and(|n| n != expected as u64)
        {
            return Err("二进制大小与清单不符".into());
        }
        let mut data = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
            if data.len().saturating_add(chunk.len()) > expected as usize {
                return Err("二进制大小与清单不符".into());
            }
            data.extend_from_slice(&chunk);
            let progress = 30 + (data.len() as u64 * 60 / expected as u64) as u8;
            if progress != self.status.progress {
                self.status.progress = progress;
                self.publish();
            }
        }
        Ok(data)
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
        return Err("更新说明过长".into());
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
        value if value == Profile::U50.github => "GitHub",
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
        assert_eq!(
            source_url(NETDISK, "update.json"),
            format!("{NETDISK}/update.json")
        );
        assert_eq!(Config::default().sources, default_sources());
        assert_eq!(source_name(NETDISK), "网盘");
        assert_eq!(source_name(Profile::U50.github), "GitHub");
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

    #[tokio::test]
    async fn u50_profile_uses_its_manifest_and_stages_a_verified_binary() {
        let signing = SigningKey::from_bytes(&[11; 32]);
        let binary = b"pretend armv7 binary".to_vec();
        let installer = b"#!/bin/sh\nexit 0\n".to_vec();
        let manifest = Manifest {
            notes: String::new(),
            schema: 1,
            version: "99.0.0".into(),
            tag: "v99.0.0".into(),
            published_at: "2026-09-30T00:00:00Z".into(),
            artifacts: BTreeMap::from([
                (
                    "installer".into(),
                    Artifact {
                        name: "install-datad-armv7.sh".into(),
                        size: installer.len() as i64,
                        sha256: hex_sha256(&installer),
                    },
                ),
                (
                    "binary".into(),
                    Artifact {
                        name: "zwrt-datad-armv7".into(),
                        size: binary.len() as i64,
                        sha256: hex_sha256(&binary),
                    },
                ),
            ]),
        };
        let raw = serde_json::to_vec(&manifest).unwrap();
        let signature = STANDARD.encode(signing.sign(&raw).to_bytes());
        let (served_binary, served_installer) = (binary.clone(), installer.clone());
        let router = Router::new()
            // The ARM64 manifest must never be read by the U50 profile.
            .route("/x/update.json", get(|| async { "wrong manifest" }))
            .route(
                "/x/update-armv7.json",
                get(move || {
                    let v = raw.clone();
                    async move { v }
                }),
            )
            .route(
                "/x/update-armv7.json.sig",
                get(move || {
                    let v = signature.clone();
                    async move { v }
                }),
            )
            .route(
                "/x/install-datad-armv7.sh",
                get(move || {
                    let v = served_installer.clone();
                    async move { v }
                }),
            )
            .route(
                "/x/zwrt-datad-armv7",
                get(move || {
                    let v = served_binary.clone();
                    async move { v }
                }),
            )
            .route(
                "/tampered/update-armv7.json",
                get({
                    let v = serde_json::to_vec(&manifest).unwrap();
                    move || {
                        let v = v.clone();
                        async move { v }
                    }
                }),
            )
            .route(
                "/tampered/update-armv7.json.sig",
                get({
                    let v = STANDARD.encode(
                        signing
                            .sign(&serde_json::to_vec(&manifest).unwrap())
                            .to_bytes(),
                    );
                    move || {
                        let v = v.clone();
                        async move { v }
                    }
                }),
            )
            .route(
                "/tampered/install-datad-armv7.sh",
                get({
                    let v = installer.clone();
                    move || {
                        let v = v.clone();
                        async move { v }
                    }
                }),
            )
            .route(
                "/tampered/zwrt-datad-armv7",
                get(|| async { "pretend armv7 binarY" }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let dir =
            std::env::temp_dir().join(format!("zwrt-datad-ota-u50-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut ota = Ota::load_with(&dir, Profile::U50).unwrap();
        ota.key = signing.verifying_key();
        ota.config.sources = vec!["custom".into()];
        let roomy = json!({"runtime":{"storage":{"available":1.0e9}}});

        ota.config.servers = vec![format!("http://{address}/x")];
        let candidate = ota.check().await.unwrap();
        assert_eq!(candidate.manifest.version, "99.0.0");

        // The snapshot's root filesystem may be full; U50 checks actual /tmp + install mounts.
        assert!(
            ota.safety(&json!({"runtime":{"storage":{"available":0}}}), true)
                .is_empty()
        );
        let wrapper = ota.prepare_install(&candidate, &roomy, true).await.unwrap();
        let stage = wrapper.parent().unwrap().to_owned();
        assert_eq!(stage.parent(), Some(Path::new("/tmp")));
        assert!(!dir.join("zwrt-datad.new").exists());
        assert!(!dir.join("ota-installer.sh").exists());
        assert_eq!(fs::read(stage.join("zwrt-datad.new")).unwrap(), binary);
        assert_eq!(
            fs::metadata(&stage).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(fs::read(stage.join("ota-installer.sh")).unwrap(), installer);
        let script = fs::read_to_string(&wrapper).unwrap();
        assert!(
            script.contains("DATAD_BINARY_FILE=") && !script.contains("DATAD_DOWNLOAD_URL"),
            "{script}"
        );
        assert!(
            script.contains("DATAD_DIR="),
            "the installer must receive the data dir for non-/etc_rw layouts: {script}"
        );
        // Execute the harmless fixture wrapper: local installation copy, marker,
        // deployment lock and tmp download cleanup are exercised end to end.
        let status = Command::new("sh").arg(&wrapper).status().await.unwrap();
        assert!(status.success());
        assert_eq!(
            fs::read_to_string(dir.join("ota-install-result"))
                .unwrap()
                .trim(),
            "success"
        );
        assert!(!stage.exists());
        assert!(!dir.join(".deploy-lock").exists());
        assert!(!fs::read_dir(&dir).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".ota-new-")
        }));

        let wrapper = ota.prepare_install(&candidate, &roomy, true).await.unwrap();
        let stage = wrapper.parent().unwrap().to_owned();
        fs::create_dir(dir.join(".deploy-lock")).unwrap();
        fs::write(dir.join(".deploy-lock/pid"), "other-owner").unwrap();
        assert!(
            Command::new("sh")
                .arg(&wrapper)
                .status()
                .await
                .unwrap()
                .success()
        );
        assert_eq!(
            fs::read_to_string(dir.join("ota-install-result"))
                .unwrap()
                .trim(),
            "failed"
        );
        assert_eq!(
            fs::read_to_string(dir.join(".deploy-lock/pid")).unwrap(),
            "other-owner"
        );
        assert!(!stage.exists());
        fs::remove_dir_all(dir.join(".deploy-lock")).unwrap();

        // A binary that does not match the signed manifest is refused and not staged.
        ota.config.servers = vec![format!("http://{address}/tampered")];
        let candidate = ota.check().await.unwrap();
        let error = ota
            .prepare_install(&candidate, &roomy, true)
            .await
            .unwrap_err();
        assert!(error.contains("校验失败"), "{error}");
        assert!(!dir.join("zwrt-datad.new").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn newer_running_build_does_not_offer_an_old_saved_candidate() {
        let dir = std::env::temp_dir().join(format!("zwrt-ota-upgraded-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut ota = Ota::load_with(&dir, Profile::U50).unwrap();
        for state in ["available", "waiting_idle"] {
            ota.status.state = state.into();
            ota.status.latest_version = "0.0.1".into();
            ota.status.signature_verified = true;
            ota.status.wait_reasons = vec!["old storage warning".into()];
            ota.save_status();
            let restored = Ota::load_with(&dir, Profile::U50).unwrap();
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
        let mut ota = Ota::load_with(&dir, Profile::U50).unwrap();
        ota.status.state = "installing".into();
        ota.status.install_started_at = now();
        ota.status.latest_version = "99.0.0".into();
        ota.save_status();
        let mut restarted = Ota::load_with(&dir, Profile::U50).unwrap();
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
        restarted.status.install_started_at = now() - 1801;
        restarted.reconcile_install_result();
        assert_eq!(restarted.status.state, "error");
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn progress_snapshot_does_not_wait_for_downloader_lock_and_bounds_body() {
        let router = Router::new().route(
            "/binary",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(150)).await;
                vec![1u8; 4096]
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let dir = std::env::temp_dir().join(format!("zwrt-ota-progress-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut ota = Ota::load_with(&dir, Profile::U50).unwrap();
        ota.begin_update().unwrap();
        let watch = ota.subscribe();
        let manager = std::sync::Arc::new(tokio::sync::Mutex::new(ota));
        let task = manager.clone();
        let worker = tokio::spawn(async move {
            let mut ota = task.lock().await;
            let url = format!("http://{address}/binary");
            assert!(ota.fetch_binary(&url, 1024).await.is_err());
            assert_eq!(ota.fetch_binary(&url, 4096).await.unwrap().len(), 4096);
            ota.finish_update();
        });
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(manager.try_lock().is_err());
        assert_eq!(watch.borrow()["busy"], true);
        worker.await.unwrap();
        assert_eq!(watch.borrow()["status"]["progress"], 90);
        assert_eq!(watch.borrow()["busy"], false);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn tmp_and_install_space_are_checked_independently() {
        let mib = 1024 * 1024;
        assert!(stage_space_reasons(20 * mib, 6 * mib, 5 * mib, 1024).is_empty());
        assert!(stage_space_reasons(mib, 20 * mib, 5 * mib, 1024)[0].starts_with("/tmp 下载目录"));
        assert!(stage_space_reasons(20 * mib, mib, 5 * mib, 1024)[0].starts_with("安装目录"));
    }

    #[test]
    fn u50_profile_uses_its_own_channel_and_skips_the_arm64_mirror() {
        let dir =
            std::env::temp_dir().join(format!("zwrt-datad-ota-servers-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let zwrt = Ota::load(&dir).unwrap();
        assert_eq!(zwrt.servers(), vec![NETDISK.to_owned(), GITHUB.to_owned()]);
        let u50 = Ota::load_with(&dir, Profile::U50).unwrap();
        assert_eq!(
            u50.servers(),
            vec!["https://github.com/33333s/zwrt-datad/releases/download/arm32-latest".to_owned()]
        );
        let _ = fs::remove_dir_all(dir);
    }
}
