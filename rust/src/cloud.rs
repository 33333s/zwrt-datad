use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub name: String,
    pub port: u16,
    pub kind: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub enabled: bool,
    pub broker: String,
    pub platform_url: String,
    pub username: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub password: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ca_pem: String,
    pub vendor: String,
    pub model: String,
    pub identity_type: String,
    pub identity: String,
    pub platform: String,
    pub report_interval_seconds: u16,
    pub remote_enabled: bool,
    pub services: Vec<Service>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            broker: String::new(),
            platform_url: String::new(),
            username: String::new(),
            password: String::new(),
            ca_pem: String::new(),
            vendor: "ZTE".into(),
            model: String::new(),
            identity_type: "uuid".into(),
            identity: String::new(),
            platform: "qualcomm".into(),
            report_interval_seconds: 30,
            remote_enabled: false,
            services: vec![
                Service {
                    name: "设备后台".into(),
                    port: 80,
                    kind: "web".into(),
                },
                Service {
                    name: "UFI".into(),
                    port: 2333,
                    kind: "web".into(),
                },
                Service {
                    name: "WebSSH".into(),
                    port: 8899,
                    kind: "terminal".into(),
                },
            ],
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    #[serde(flatten)]
    pub config: Config,
    #[serde(default)]
    pub clear_password: bool,
}

#[derive(Debug)]
pub struct Cloud {
    file: PathBuf,
    config: Config,
    state: String,
    error: Option<String>,
}

impl Cloud {
    pub fn load(data_dir: &Path) -> Self {
        let file = data_dir.join("cloud.json");
        match fs::read(&file) {
            Ok(data) => match serde_json::from_slice::<Config>(&data).and_then(|config| {
                validate(&config).map_err(serde::de::Error::custom)?;
                Ok(config)
            }) {
                Ok(config) => Self {
                    file,
                    state: if config.enabled {
                        "starting".into()
                    } else {
                        "disabled".into()
                    },
                    config,
                    error: None,
                },
                Err(_) => Self {
                    file,
                    config: Config::default(),
                    state: "error".into(),
                    error: Some("云端配置无效，请重新保存".into()),
                },
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self {
                file,
                config: Config::default(),
                state: "disabled".into(),
                error: None,
            },
            Err(_) => Self {
                file,
                config: Config::default(),
                state: "error".into(),
                error: Some("无法读取云端配置".into()),
            },
        }
    }

    pub fn public_config(&self) -> Value {
        let mut config = self.config.clone();
        let configured = !config.password.is_empty();
        config.password.clear();
        json!({"config":config,"password_configured":configured})
    }
    pub fn status(&self) -> Value {
        let mut out = json!({"state":self.state});
        if let Some(error) = &self.error {
            out["error"] = json!(error)
        }
        out
    }
    pub fn update(&mut self, mut update: Update) -> Result<Value, String> {
        if update.config.password.is_empty() && !update.clear_password {
            update.config.password = self.config.password.clone()
        }
        if update.clear_password {
            update.config.password.clear()
        }
        validate(&update.config)?;
        let data = serde_json::to_vec_pretty(&update.config).map_err(|e| e.to_string())?;
        atomic_write(&self.file, &data).map_err(|_| "保存配置失败".to_string())?;
        self.config = update.config;
        self.state = if self.config.enabled {
            "starting"
        } else {
            "disabled"
        }
        .into();
        self.error = None;
        Ok(self.public_config())
    }
}

fn topic(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_. -".contains(&b))
}
fn validate(config: &Config) -> Result<(), String> {
    if !(10..=3600).contains(&config.report_interval_seconds) {
        return Err("上报间隔必须为 10–3600 秒".into());
    }
    if config.services.len() > 8 {
        return Err("最多配置 8 个后台".into());
    }
    let mut ports = HashSet::new();
    for service in &config.services {
        if service.name.trim().is_empty()
            || service.name.len() > 80
            || service.port == 9460
            || service.port == 9461
            || !ports.insert(service.port)
            || !matches!(service.kind.as_str(), "web" | "terminal")
        {
            return Err("后台名称、类型或端口无效，不能使用 datad 管理端口".into());
        }
    }
    if !config.broker.is_empty() {
        let value = config
            .broker
            .strip_prefix("ssl://")
            .ok_or("MQTT 地址格式为 ssl://主机:端口")?;
        let (host, port) = value
            .rsplit_once(':')
            .ok_or("MQTT 地址格式为 ssl://主机:端口")?;
        if host.is_empty() || port.parse::<u16>().is_err() || value.contains(['/', '?', '#', '@']) {
            return Err("MQTT 地址格式为 ssl://主机:端口".into());
        }
    }
    if !config.platform_url.is_empty() {
        let value = config
            .platform_url
            .strip_prefix("https://")
            .ok_or("NMS 地址必须是 HTTPS 站点地址")?;
        if value.is_empty()
            || value.contains(['?', '#', '@'])
            || value.trim_end_matches('/').contains('/')
        {
            return Err("NMS 地址必须是 HTTPS 站点地址".into());
        }
    }
    if !config.ca_pem.is_empty()
        && (!config.ca_pem.contains("-----BEGIN CERTIFICATE-----")
            || !config.ca_pem.contains("-----END CERTIFICATE-----"))
    {
        return Err("CA 证书无效".into());
    }
    if config.enabled {
        if config.broker.is_empty() || config.username.is_empty() || config.password.is_empty() {
            return Err("请填写 MQTT 地址和设备凭据".into());
        }
        if !topic(&config.vendor) || !topic(&config.model) || !topic(&config.identity) {
            return Err("厂商、型号和设备标识不能为空或包含主题特殊字符".into());
        }
        if !matches!(config.identity_type.as_str(), "uuid" | "sn") {
            return Err("设备标识类型无效".into());
        }
        if !matches!(
            config.platform.as_str(),
            "generic" | "qualcomm" | "mediatek" | "quecopen"
        ) {
            return Err("固件平台无效".into());
        }
        if config.remote_enabled && (config.platform_url.is_empty() || config.services.is_empty()) {
            return Err("远程访问需要 NMS 地址和至少一个后台".into());
        }
    }
    Ok(())
}
fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(data)?;
    file.sync_all()?;
    drop(file);
    fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_validate() {
        validate(&Config::default()).unwrap()
    }
    #[test]
    fn rejects_management_ports() {
        let mut c = Config::default();
        c.services[0].port = 9460;
        assert!(validate(&c).is_err())
    }
}
