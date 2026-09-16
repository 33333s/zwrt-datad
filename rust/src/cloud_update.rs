//! NMS only requests an existing, signed datad update; it cannot supply executables or URLs.
use crate::{cloud::Config, ota, server::App};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{fs, path::Path};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub protocol_version: u8,
    pub request_id: String,
    pub action: String,
    pub identity: String,
    pub boot_id: String,
    pub expires_uptime: f64,
    #[serde(default)]
    pub target_version: String,
    #[serde(default)]
    pub binary_sha256: String,
}

fn uptime() -> f64 {
    fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0)
}
fn validate(c: &Command, config: &Config, boot: &str, now: f64) -> Result<(), String> {
    if !config.enabled
        || c.protocol_version != 1
        || c.identity != config.identity
        || c.request_id.len() < 8
        || c.request_id.len() > 80
        || !c
            .request_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || !matches!(
            c.action.as_str(),
            "datad.update.check" | "datad.update.install"
        )
    {
        return Err("invalid_update_request".into());
    }
    if boot.is_empty()
        || c.boot_id != boot
        || !c.expires_uptime.is_finite()
        || now <= 0.0
        || c.expires_uptime <= now
        || c.expires_uptime > now + 300.0
    {
        return Err("expired_update_request".into());
    }
    if c.action == "datad.update.install"
        && (c.target_version.is_empty()
            || c.target_version.len() > 40
            || c.binary_sha256.len() != 64
            || !c.binary_sha256.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err("missing_verified_candidate".into());
    }
    Ok(())
}
fn write_record(path: &Path, value: &Value) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let temp = path.with_extension("tmp");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|e| e.to_string())?;
    file.write_all(value.to_string().as_bytes())
        .map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    fs::rename(temp, path).map_err(|e| e.to_string())
}
impl App {
    pub async fn cloud_update_result(&self) -> Option<Value> {
        let path = self.inner._data_dir.join("cloud-update.json");
        let mut result: Value = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
        if result["status"] == "installing" {
            let version = env!("DATAD_VERSION");
            if result["data"]["target_version"] == version {
                let expected = result["data"]["binary_sha256"].as_str()?;
                use sha2::{Digest, Sha256};
                let bytes = fs::read(std::env::current_exe().ok()?).ok()?;
                let actual = format!("{:x}", Sha256::digest(&bytes));
                result["status"] = json!(if actual == expected {
                    "succeeded"
                } else {
                    "failed"
                });
                result["data"]["current_version"] = json!(version);
                result["data"]["installed_sha256"] = json!(actual);
                let _ = write_record(&path, &result);
            } else if let Ok(marker) =
                fs::read_to_string(self.inner._data_dir.join("ota-install-result"))
                && marker.trim() == "failed"
            {
                result["status"] = json!("failed");
                result["error"] = json!({"code":"installer_failed"});
                let _ = write_record(&path, &result);
            }
        }
        Some(result)
    }

    pub async fn cloud_update(&self, command: Command, config: Config) -> Value {
        let mut result =
            json!({"request_id":command.request_id,"action":command.action,"status":"failed"});
        let mut persist = false;
        let run = async {
            let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default();
            validate(&command, &config, boot.trim(), uptime())?;
            let path = self.inner._data_dir.join("cloud-update.json");
            if let Some(previous) = self.cloud_update_result().await {
                if previous["request_id"] == command.request_id { return Ok(previous); }
                if previous["status"] == "installing" { return Err("update_in_progress".into()); }
            }
            let mut manager = self.inner.ota.try_lock().map_err(|_| "update_in_progress".to_string())?;
            manager.begin_update()?;
            persist = true;
            let operation: Result<Value, String> = async {
                let candidate = manager.check().await?;
                let binary = candidate.manifest.artifacts.get("binary").ok_or("missing_binary")?;
                let data = json!({"current_version":env!("DATAD_VERSION"),"target_version":candidate.manifest.version,
                    "binary_sha256":binary.sha256,"size":binary.size,"has_update":ota::has_update(&candidate),"signature_verified":true});
                if command.action == "datad.update.check" {
                    return Ok(json!({"request_id":command.request_id,"action":command.action,"status":"checked","data":data}));
                }
                if command.target_version != candidate.manifest.version || command.binary_sha256 != binary.sha256 {
                    return Err("candidate_changed_check_again".into());
                }
                if !ota::has_update(&candidate) { return Err("already_current".into()); }
                let installing = json!({"request_id":command.request_id,"action":command.action,"status":"installing","data":data});
                // Persist before starting the installer, which restarts this process.
                let _ = fs::remove_file(self.inner._data_dir.join("ota-install-result"));
                write_record(&path, &installing)?;
                let snapshot = serde_json::to_value(self.snapshot().await).unwrap_or(Value::Null);
                manager.install(&candidate, &snapshot, true).await?;
                Ok(installing)
            }.await;
            if let Err(error) = &operation { manager.fail(None, error.clone()); }
            manager.finish_update();
            operation
        }.await;
        match run {
            Ok(value) => result = value,
            Err(error) => result["error"] = json!({"code":error}),
        }
        // A rejected concurrent command must not overwrite an active installation record.
        if persist {
            let _ = write_record(&self.inner._data_dir.join("cloud-update.json"), &result);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requires_current_boot_identity_freshness_and_pinned_install() {
        let config = Config {
            enabled: true,
            identity: "device".into(),
            ..Config::default()
        };
        let mut c = Command {
            protocol_version: 1,
            request_id: "datad-123456".into(),
            action: "datad.update.check".into(),
            identity: "device".into(),
            boot_id: "boot".into(),
            expires_uptime: 200.0,
            target_version: String::new(),
            binary_sha256: String::new(),
        };
        assert!(validate(&c, &config, "boot", 100.0).is_ok());
        assert!(validate(&c, &config, "other", 100.0).is_err());
        assert!(validate(&c, &config, "boot", 201.0).is_err());
        c.action = "datad.update.install".into();
        assert!(validate(&c, &config, "boot", 100.0).is_err());
        c.target_version = "0.10.5".into();
        c.binary_sha256 = "a".repeat(64);
        assert!(validate(&c, &config, "boot", 100.0).is_ok());
        c.identity = "another".into();
        assert!(validate(&c, &config, "boot", 100.0).is_err());
    }
}
