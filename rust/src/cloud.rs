use crate::model::Snapshot;
use futures_util::{SinkExt, StreamExt};
use reqwest::Url;
use rumqttc::{
    AsyncClient, Event, Incoming, LastWill, MqttOptions, Outgoing, PublishOptions, QoS,
    TlsConfiguration, Transport,
};
use rustls::{
    ClientConfig, RootCertStore,
    pki_types::{CertificateDer, pem::PemObject},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Once},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{Mutex as AsyncMutex, watch},
    task::JoinSet,
};
use tokio_tungstenite::{
    Connector, connect_async_tls_with_config,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

pub fn init_crypto() {
    static PROVIDER: Once = Once::new();
    PROVIDER.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

const DEFAULT_PLATFORM_URL: &str = "https://nms.ericsfj.com";
const DEFAULT_BROKER: &str = "wss://nms.ericsfj.com/mqtt";
const DEFAULT_MEMBER_ORIGIN: &str = "https://a.ericsfj.com:16001";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    pub remote_origins: Vec<String>,
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
    pub remote_panel_control_enabled: bool,
    pub remote_webshell_enabled: bool,
    pub services: Vec<Service>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            broker: DEFAULT_BROKER.into(),
            platform_url: DEFAULT_PLATFORM_URL.into(),
            remote_origins: Vec::new(),
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
            remote_panel_control_enabled: false,
            remote_webshell_enabled: false,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteFeatures {
    pub remote_panel_control_enabled: bool,
    pub remote_webshell_enabled: bool,
    pub services: Vec<Service>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuickConnect {
    pub username: String,
    pub password: String,
}

#[derive(Clone, Debug, Default, Serialize)]
struct Status {
    state: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    error: String,
    #[serde(skip_serializing_if = "is_zero")]
    last_report_at: i64,
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

pub struct Cloud {
    file: PathBuf,
    config: Config,
    status: Arc<Mutex<Status>>,
    tx: watch::Sender<Option<Config>>,
}

impl Cloud {
    pub fn load(data_dir: &Path) -> Self {
        let file = data_dir.join("cloud.json");
        let (config, runtime, status) = match fs::read(&file) {
            Ok(data) => match serde_json::from_slice::<Config>(&data) {
                Ok(config) if validate(&config).is_ok() => {
                    let state = if config.enabled {
                        "starting"
                    } else {
                        "disabled"
                    };
                    (
                        config.clone(),
                        Some(config),
                        Status {
                            state: state.into(),
                            ..Default::default()
                        },
                    )
                }
                _ => (
                    Config::default(),
                    None,
                    Status {
                        state: "error".into(),
                        error: "云端配置无效，请重新保存".into(),
                        ..Default::default()
                    },
                ),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let config = Config::default();
                (
                    config.clone(),
                    Some(config),
                    Status {
                        state: "disabled".into(),
                        ..Default::default()
                    },
                )
            }
            Err(_) => (
                Config::default(),
                None,
                Status {
                    state: "error".into(),
                    error: "无法读取云端配置".into(),
                    ..Default::default()
                },
            ),
        };
        let (tx, _) = watch::channel(runtime);
        Self {
            file,
            config,
            status: Arc::new(Mutex::new(status)),
            tx,
        }
    }

    pub fn start(&self, state: watch::Receiver<Snapshot>, app: crate::server::App) {
        let config = self.tx.subscribe();
        let status = self.status.clone();
        tokio::spawn(async move { supervisor(config, state, status, Some(app)).await });
    }

    pub fn public_config(&self) -> Value {
        let mut config = self.config.clone();
        let configured = !config.password.is_empty();
        config.password.clear();
        json!({"config":config,"password_configured":configured})
    }

    /// Only fields needed by the authorized NMS-hosted UFI settings panel.
    /// MQTT password, custom CA PEM and other private runtime data never leave
    /// the device through this view.
    pub fn panel_config(&self) -> Value {
        json!({
            "state":self.status.lock().unwrap().state,
            "enabled":self.config.enabled,
            "remote_enabled":self.config.remote_enabled,
            "remote_panel_control_enabled":self.config.remote_panel_control_enabled,
            "remote_webshell_enabled":self.config.remote_webshell_enabled,
            "platform_url":self.config.platform_url,
            "broker":self.config.broker,
            "username":self.config.username,
            "password_configured":!self.config.password.is_empty(),
            "custom_ca_configured":!self.config.ca_pem.is_empty(),
            "vendor":self.config.vendor,
            "model":self.config.model,
            "identity_type":self.config.identity_type,
            "identity":self.config.identity,
            "platform":self.config.platform,
            "report_interval_seconds":self.config.report_interval_seconds,
            "services":self.config.services,
        })
    }

    /// Persist only remote feature switches and reviewed local services. The
    /// caller sends its control acknowledgement before activating the new
    /// supervisor configuration, which may close the current panel session.
    pub fn save_remote_features(&mut self, input: RemoteFeatures) -> Result<(Value, bool), String> {
        if !self.config.enabled || !self.config.remote_enabled {
            return Err("远程管理未启用".into());
        }
        if input
            .services
            .iter()
            .any(|service| service.port == 0 || service.name.chars().any(char::is_control))
        {
            return Err("远程后台名称或端口无效".into());
        }
        if self.config.remote_panel_control_enabled == input.remote_panel_control_enabled
            && self.config.remote_webshell_enabled == input.remote_webshell_enabled
            && self.config.services == input.services
        {
            return Ok((self.panel_config(), false));
        }
        let mut next = self.config.clone();
        next.remote_panel_control_enabled = input.remote_panel_control_enabled;
        next.remote_webshell_enabled = input.remote_webshell_enabled;
        next.services = input.services;
        validate(&next)?;
        atomic_json(&self.file, &next)?;
        self.config = next;
        set_status(&self.status, "reconfiguring", "");
        Ok((self.panel_config(), true))
    }

    pub fn activate_saved_features(&self) {
        let _ = self.tx.send(Some(self.config.clone()));
    }

    pub fn status(&self) -> Value {
        serde_json::to_value(self.status.lock().unwrap().clone())
            .unwrap_or_else(|_| json!({"state":"error"}))
    }

    pub fn update(&mut self, mut update: Update) -> Result<Value, String> {
        if update.config.password.is_empty() && !update.clear_password {
            update.config.password = self.config.password.clone();
        }
        if update.clear_password {
            update.config.password.clear();
        }
        validate(&update.config)?;
        atomic_json(&self.file, &update.config)?;
        self.config = update.config;
        set_status(&self.status, "reconfiguring", "");
        let _ = self.tx.send(Some(self.config.clone()));
        Ok(self.public_config())
    }

    pub fn quick_connect(
        &mut self,
        input: QuickConnect,
        snapshot: &Snapshot,
    ) -> Result<Value, String> {
        if self.config.enabled && !self.config.username.is_empty() {
            return Err("已配置云端连接；如需更换账户，请使用高级配置".into());
        }
        if !pending_username(&input.username)
            || input.password.is_empty()
            || input.password.len() > 256
        {
            return Err("请填写 NMS 签发的 MQTT 用户名和密码".into());
        }
        let device = snapshot.fields.get("device").unwrap_or(&Value::Null);
        let info = snapshot
            .fields
            .get("uci_device_info")
            .unwrap_or(&Value::Null);
        let model = device
            .get("api_template")
            .and_then(Value::as_str)
            .filter(|value| *value != "legacy_compat" && topic(value))
            .or_else(|| {
                device
                    .get("model_name")
                    .and_then(Value::as_str)
                    .filter(|value| topic(value))
            })
            .ok_or("设备型号尚未就绪")?;
        let stable_id = [("modem_msn", info), ("serial_number", info), ("imei", info)]
            .into_iter()
            .find_map(|(key, source)| {
                source
                    .get(key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty() && topic(value))
                    .map(|value| (key, value))
            })
            .or_else(|| {
                snapshot
                    .fields
                    .get("system")
                    .and_then(|system| system.get("imei"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| {
                        (14..=16).contains(&value.len())
                            && value.bytes().all(|c| c.is_ascii_digit())
                    })
                    .map(|value| ("imei", value))
            })
            .ok_or("设备稳定标识尚未就绪")?;
        let mut config = self.config.clone();
        config.broker = DEFAULT_BROKER.into();
        config.platform_url = DEFAULT_PLATFORM_URL.into();
        config.remote_origins.clear();
        config.ca_pem.clear();
        config.vendor = device
            .get("vendor")
            .and_then(Value::as_str)
            .filter(|value| topic(value))
            .unwrap_or("ZTE")
            .to_owned();
        config.model = model.to_owned();
        config.identity_type = "uuid".into();
        if config.identity.is_empty() {
            config.identity =
                deterministic_uuid(&format!("ufi-device:{}:{}", stable_id.0, stable_id.1));
        }
        config.platform = if matches!(model, "MU5250" | "MU5252" | "MC7523" | "MC8532B") {
            "qualcomm"
        } else {
            "generic"
        }
        .into();
        config.username = input.username;
        config.password = input.password;
        config.enabled = true;
        config.remote_enabled = true;
        config.remote_panel_control_enabled = false;
        config.remote_webshell_enabled = false;
        self.update(Update {
            config,
            clear_password: false,
        })
    }
}

fn pending_username(username: &str) -> bool {
    username.len() == 36
        && username.starts_with("enr_")
        && username[4..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn deterministic_uuid(seed: &str) -> String {
    let hash = Sha256::digest(seed.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{}-{}-{}-{}-{}",
        hex(&bytes[..4]),
        hex(&bytes[4..6]),
        hex(&bytes[6..8]),
        hex(&bytes[8..10]),
        hex(&bytes[10..16])
    )
}

async fn supervisor(
    mut config_rx: watch::Receiver<Option<Config>>,
    state_rx: watch::Receiver<Snapshot>,
    status: Arc<Mutex<Status>>,
    app: Option<crate::server::App>,
) {
    loop {
        let config = config_rx.borrow_and_update().clone();
        let Some(config) = config else {
            if config_rx.changed().await.is_err() {
                return;
            }
            continue;
        };
        if !config.enabled {
            set_status(&status, "disabled", "");
            if config_rx.changed().await.is_err() {
                return;
            }
            continue;
        }
        let mut delay = Duration::from_secs(1);
        loop {
            set_status(&status, "connecting", "");
            match session(
                &config,
                state_rx.clone(),
                config_rx.clone(),
                status.clone(),
                app.clone(),
            )
            .await
            {
                SessionEnd::Reconfigure => break,
                SessionEnd::Stopped => return,
                SessionEnd::Failed => {
                    set_status(
                        &status,
                        "retrying",
                        "云端连接中断或认证失败，请检查地址、证书与设备凭据",
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {},
                        changed = config_rx.changed() => {
                            if changed.is_err() { return; }
                            break;
                        }
                    }
                    delay = (delay * 2).min(Duration::from_secs(30));
                }
            }
        }
    }
}

enum SessionEnd {
    Reconfigure,
    Stopped,
    Failed,
}

const MQTT_ADDRESS_ERROR: &str = "MQTT 地址格式为 ssl://主机:端口 或 wss://主机[:端口]/mqtt";

fn mqtt_url(address: &str) -> Result<Url, String> {
    if address.trim() != address || address.chars().any(char::is_control) {
        return Err(MQTT_ADDRESS_ERROR.into());
    }
    let url = Url::parse(address).map_err(|_| MQTT_ADDRESS_ERROR)?;
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port() == Some(0)
    {
        return Err(MQTT_ADDRESS_ERROR.into());
    }
    match url.scheme() {
        "ssl" if url.port().is_some() && matches!(url.path(), "" | "/") => Ok(url),
        "wss" => Ok(url),
        _ => Err(MQTT_ADDRESS_ERROR.into()),
    }
}

fn mqtt_options(config: &Config, client_id: &str) -> Result<MqttOptions, String> {
    let url = mqtt_url(&config.broker)?;
    if url.scheme() == "wss" {
        // Preserve the endpoint path and verify the broker hostname with the
        // same embedded trust roots / optional CA used by remote WSS sessions.
        return MqttOptions::websocket_with_tls_config(
            client_id,
            url.as_str(),
            TlsConfiguration::Rustls(websocket_tls(config)?),
        )
        .map_err(|_| MQTT_ADDRESS_ERROR.into());
    }
    let mut options = MqttOptions::new(
        client_id,
        (
            url.host_str().ok_or(MQTT_ADDRESS_ERROR)?,
            url.port().ok_or(MQTT_ADDRESS_ERROR)?,
        ),
    );
    let transport = if config.ca_pem.is_empty() {
        Transport::try_tls_with_default_config().map_err(|_| "无法加载 MQTT TLS 根证书")?
    } else {
        Transport::tls(config.ca_pem.as_bytes().to_vec(), None, None)
    };
    options.set_transport(transport);
    Ok(options)
}

async fn session(
    config: &Config,
    mut state_rx: watch::Receiver<Snapshot>,
    mut config_rx: watch::Receiver<Option<Config>>,
    status: Arc<Mutex<Status>>,
    app: Option<crate::server::App>,
) -> SessionEnd {
    let pending = pending_username(&config.username);
    let enrollment_root = format!("onboard/{}", config.username);
    let id_hash = Sha256::digest(root(config).as_bytes());
    let client_id = format!("datad-{}", hex(&id_hash[..12]));
    let Ok(mut options) = mqtt_options(config, &client_id) else {
        return SessionEnd::Failed;
    };
    options.set_credentials(config.username.clone(), config.password.clone());
    options.set_keep_alive(30);
    options.set_clean_session(true);
    options.set_last_will(LastWill::new(
        if pending {
            format!("{enrollment_root}/status")
        } else {
            format!("{}/status", root(config))
        },
        envelope(json!({"online":false})).to_string(),
        QoS::AtLeastOnce,
        false,
    ));
    let (client, mut eventloop) = AsyncClient::builder(options).capacity(32).build();
    if !pending
        && client
            .subscribe(
                format!("{}/command/request", root(config)),
                QoS::AtLeastOnce,
            )
            .await
            .is_err()
    {
        return SessionEnd::Failed;
    }
    let bridge = BridgeManager::new(config.clone(), app.clone());
    let webshell_available = app
        .as_ref()
        .is_some_and(|app| app.cloud_webshell_available());
    let mut updates = JoinSet::new();
    let mut connected = false;
    let mut enrolled = !pending;
    let mut ticker = tokio::time::interval(Duration::from_secs(u64::from(
        config.report_interval_seconds,
    )));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Presence must remain faster than NMS's 30-second offline deadline,
    // independently of the user-selected telemetry reporting interval.
    let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            changed = config_rx.changed() => {
                bridge.shutdown(); updates.detach_all();
                if connected && enrolled {
                    let _ = publish_and_flush(
                        &client,
                        &mut eventloop,
                        format!("{}/status", root(config)),
                        json!({"online":false}),
                    ).await;
                }
                return if changed.is_ok() { SessionEnd::Reconfigure } else { SessionEnd::Stopped };
            }
            result = updates.join_next(), if !updates.is_empty() => {
                if let Some(Ok(result)) = result {
                    let _ = publish(&client, format!("{}/command/result", root(config)), result).await;
                }
            },
            event = eventloop.poll() => match event {
                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                    connected = true;
                    enrolled = !pending;
                    ticker.reset();
                    heartbeat.reset();
                    set_status(&status, if pending {"enrolling"} else {"connected"}, "");
                    if pending {
                        if client.subscribe(format!("{enrollment_root}/result"), QoS::AtLeastOnce).await.is_err() {
                            bridge.shutdown(); updates.detach_all(); return SessionEnd::Failed;
                        }
                        continue;
                    }
                    let snapshot = state_rx.borrow().clone();
                    if let Some(app) = &app
                        && let Some(result) = app.cloud_update_result().await {
                            let _ = publish(&client, format!("{}/command/result", root(config)), result).await;
                    }
                    if report(&client, config, &snapshot, &status, webshell_available, app.is_some()).await.is_err() {
                        bridge.shutdown(); updates.detach_all();
                        return SessionEnd::Failed;
                    }
                }
                Ok(Event::Incoming(Incoming::SubAck(_))) if pending && connected && !enrolled => {
                    if publish(&client, format!("{enrollment_root}/hello"), bootstrap_report(config)).await.is_err() {
                        bridge.shutdown(); updates.detach_all(); return SessionEnd::Failed;
                    }
                }
                Ok(Event::Incoming(Incoming::Publish(message))) if connected => {
                    if pending && !enrolled && message.topic == format!("{enrollment_root}/result") && !message.retain && message.payload.len() <= 1024 {
                        let ack: Value=serde_json::from_slice(&message.payload).unwrap_or(Value::Null);
                        if ack.get("protocol_version").and_then(Value::as_i64)==Some(1)
                            && ack.get("state").and_then(Value::as_str)==Some("bound")
                            && ack.get("topic_root").and_then(Value::as_str)==Some(root(config).as_str()) {
                            enrolled=true;
                            if client.subscribe(format!("{}/command/request",root(config)),QoS::AtLeastOnce).await.is_err() {
                                bridge.shutdown();updates.detach_all();return SessionEnd::Failed;
                            }
                            set_status(&status,"connected","");
                            let snapshot=state_rx.borrow().clone();
                            if report(&client,config,&snapshot,&status,webshell_available,app.is_some()).await.is_err() {
                                bridge.shutdown();updates.detach_all();return SessionEnd::Failed;
                            }
                        } else {
                            set_status(&status,"retrying","NMS 拒绝设备接入，请检查凭据及设备绑定状态");
                            bridge.shutdown();updates.detach_all();return SessionEnd::Failed;
                        }
                        continue;
                    }
                    if !enrolled { continue; }
                    if message.topic != format!("{}/command/request", root(config)) || message.retain || message.payload.len() > 8192 { continue; }
                    if let Ok(command) = serde_json::from_slice::<crate::cloud_update::Command>(&message.payload) {
                        if let Some(app) = &app {
                            let app = app.clone();
                            let config = config.clone();
                            if updates.is_empty() {
                                updates.spawn(async move { app.cloud_update(command, config).await });
                            }
                        }
                        continue;
                    }
                    if let Ok(command) = serde_json::from_slice::<RemoteCommand>(&message.payload)
                            && let Some(result) = bridge.receive(command).await {
                                let _ = publish(&client, format!("{}/command/result", root(config)), result).await;
                            }
                }
                Ok(_) => {},
                Err(_) => { bridge.shutdown(); updates.detach_all(); return SessionEnd::Failed; }
            },
            _ = heartbeat.tick(), if connected => {
                let (topic,body)=if pending && !enrolled {(format!("{enrollment_root}/hello"),bootstrap_report(config))}
                    else {(format!("{}/status",root(config)),json!({"online":true}))};
                if publish(&client, topic, body).await.is_err() {
                    bridge.shutdown(); updates.detach_all();
                    return SessionEnd::Failed;
                }
            },
            _ = ticker.tick(), if connected && enrolled => {
                if let Some(app) = &app
                    && let Some(result) = app.cloud_update_result().await {
                        let _ = publish(&client, format!("{}/command/result", root(config)), result).await;
                }
                let snapshot = state_rx.borrow_and_update().clone();
                if report(&client, config, &snapshot, &status, webshell_available, app.is_some()).await.is_err() {
                    bridge.shutdown(); updates.detach_all();
                    return SessionEnd::Failed;
                }
            }
        }
    }
}

async fn publish_and_flush(
    client: &AsyncClient,
    eventloop: &mut rumqttc::EventLoop,
    topic: String,
    payload: Value,
) -> Result<(), String> {
    publish(client, topic, payload).await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut pending = None;
        loop {
            match eventloop.poll().await.map_err(|error| error.to_string())? {
                Event::Outgoing(Outgoing::Publish(packet_id)) if packet_id != 0 => {
                    pending = Some(packet_id);
                }
                Event::Incoming(Incoming::PubAck(ack)) if pending == Some(ack.pkid) => {
                    return Ok(());
                }
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| "MQTT offline status acknowledgement timed out".to_owned())?
}

async fn report(
    client: &AsyncClient,
    config: &Config,
    snapshot: &Snapshot,
    status: &Arc<Mutex<Status>>,
    webshell_available: bool,
    panel_available: bool,
) -> Result<(), String> {
    let state = serde_json::to_value(snapshot).unwrap_or(Value::Null);
    let mut capabilities = vec!["datad.update", "datad.remote", "datad.remote_origins"];
    if webshell_available {
        capabilities.push("datad.webshell");
    }
    if panel_available {
        capabilities.push("datad.panel");
        if config.remote_enabled && config.remote_panel_control_enabled {
            capabilities.push("datad.panel.control");
        }
    }
    let firmware = state
        .get("system")
        .and_then(|value| value.get("sw_version").or_else(|| value.get("fw")))
        .and_then(Value::as_str)
        .unwrap_or_default();
    publish(client, format!("{}/telemetry/device", root(config)), json!({
        "vendor":config.vendor,"model":config.model,"device_id":config.identity,
        "id_type":config.identity_type,"platform":config.platform,
        "agent_version":env!("DATAD_VERSION"),"firmware_version":firmware,
        "capabilities":capabilities,"remote_services":config.services,"remote_enabled":config.remote_enabled,
        "remote_origins":reported_remote_origins(config),
        "remote_panel_control_enabled":config.remote_enabled && config.remote_panel_control_enabled && panel_available,
        "remote_webshell_enabled":config.remote_enabled && config.remote_webshell_enabled && webshell_available
    })).await?;
    publish(
        client,
        format!("{}/status", root(config)),
        json!({"online":true}),
    )
    .await?;
    publish(
        client,
        format!("{}/telemetry/system", root(config)),
        system_telemetry(&state),
    )
    .await?;
    publish(
        client,
        format!("{}/telemetry/network", root(config)),
        json!({"upstream":{"ipv4":upstream_addresses(&state,"wan4","ipv4"),"ipv6":upstream_addresses(&state,"wan6","ipv6")},"cell":serving_cell(&state)}),
    )
    .await?;
    let mut guard = status.lock().unwrap();
    guard.state = "connected".into();
    guard.error.clear();
    guard.last_report_at = now();
    Ok(())
}

async fn publish(client: &AsyncClient, topic: String, payload: Value) -> Result<(), String> {
    client
        .publish(
            topic,
            envelope(payload).to_string(),
            PublishOptions::at_least_once(),
        )
        .await
        .map_err(|error| error.to_string())
}

fn envelope(mut payload: Value) -> Value {
    if let Some(object) = payload.as_object_mut() {
        object.insert("protocol_version".into(), json!(1));
        object.insert("timestamp".into(), json!(now()));
    }
    payload
}

/// WAN addresses from `/state.interfaces.wan4|wan6.<family>` (ubus
/// `network.interface.*` `ipv4-address`/`ipv6-address` entries). Only valid
/// addresses of the requested family are reported, capped like the panel view.
fn upstream_addresses(state: &Value, interface: &str, family: &str) -> Vec<Value> {
    state
        .pointer(&format!("/interfaces/{interface}/{family}"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let address: std::net::IpAddr = entry.get("address")?.as_str()?.parse().ok()?;
            (address.is_ipv4() == (family == "ipv4")).then(|| json!(address.to_string()))
        })
        .take(4)
        .collect()
}

/// Serving-cell identity for NMS cell-based positioning. Zero-valued OEM
/// fields are omitted instead of being sent as a misleading location. A modem
/// can retain a prior cell value after a RAT switch, so consumers must also
/// inspect the reported RAT and timestamp. MNC is the exception: 0 is a real
/// network code (for example 460-00), so it follows a known MCC.
fn serving_cell(state: &Value) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(rat) = state
        .pointer("/net/type")
        .and_then(Value::as_str)
        .filter(|rat| !rat.is_empty())
    {
        out.insert("rat".into(), json!(rat));
    }
    let net_i64 = |key: &str| {
        state
            .pointer(&format!("/net/{key}"))
            .and_then(Value::as_i64)
    };
    if let Some(mcc) = net_i64("mcc").filter(|mcc| *mcc > 0) {
        out.insert("mcc".into(), json!(mcc));
        if let Some(mnc) = net_i64("mnc").filter(|mnc| *mnc >= 0) {
            out.insert("mnc".into(), json!(mnc));
        }
    }
    for key in [
        "lte_tac",
        "lte_cell_id",
        "lte_pci",
        "nr_tac",
        "nr_cell_id",
        "nr_pci",
    ] {
        if let Some(value) = net_i64(key).filter(|value| *value > 0) {
            out.insert(key.into(), json!(value));
        }
    }
    Value::Object(out)
}

fn system_telemetry(state: &Value) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(value) =
        number_at(state, &["system", "cpu_usage"]).filter(|value| (0.0..=100.0).contains(value))
    {
        out.insert("cpu".into(), json!({"usage_percent":value}));
    }
    if let Some(value) =
        number_at(state, &["system", "mem_used_pct"]).filter(|value| (0.0..=100.0).contains(value))
    {
        out.insert("memory".into(), json!({"usage_percent":value}));
    }
    if let Some(value) =
        number_at(state, &["thermal", "cpu_celsius"]).filter(|value| *value > 0.0 && *value < 150.0)
    {
        out.insert(
            "temperature".into(),
            json!({"available":true,"cpu_celsius":value}),
        );
    }
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default();
    out.insert("boot_id".into(), json!(boot.trim()));
    let uptime = fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|value| value.split_whitespace().next()?.parse::<f64>().ok())
        .unwrap_or_default();
    out.insert("uptime".into(), json!(uptime));
    Value::Object(out)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteCommand {
    protocol_version: i64,
    request_id: String,
    action: String,
    remote_url: String,
    token: String,
    target_service: String,
    target_port: u16,
    #[serde(default)]
    target_ports: Vec<u16>,
    ttl_seconds: u64,
}

#[derive(Default)]
struct BridgeState {
    active: HashSet<String>,
    seen: HashMap<String, Duration>,
}

impl BridgeState {
    fn already_seen(&mut self, request: &str) -> bool {
        let now = crate::elapsed::now();
        self.seen.retain(|_, expires| *expires > now);
        self.seen.contains_key(request)
    }

    fn remember(&mut self, request: String) {
        self.seen.insert(
            request,
            crate::elapsed::now().saturating_add(Duration::from_secs(12 * 3600)),
        );
    }
}

#[derive(Clone)]
struct BridgeManager {
    config: Config,
    app: Option<crate::server::App>,
    state: Arc<AsyncMutex<BridgeState>>,
    shutdown: watch::Sender<bool>,
}

impl BridgeManager {
    fn new(config: Config, app: Option<crate::server::App>) -> Self {
        let (shutdown, _) = watch::channel(false);
        Self {
            config,
            app,
            state: Arc::new(AsyncMutex::new(BridgeState::default())),
            shutdown,
        }
    }

    fn shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    async fn receive(&self, command: RemoteCommand) -> Option<Value> {
        let reject = |code: String| json!({"request_id":command.request_id,"status":"rejected","error":{"code":code}});
        if let Err(error) = validate_remote(&self.config, &command) {
            return Some(reject(error));
        }
        let mut state = self.state.lock().await;
        if state.already_seen(&command.request_id) {
            return None;
        }
        if state.active.len() >= 4 || state.seen.len() >= 128 {
            return Some(reject("session_limit".into()));
        }
        let shell_permit = if command.target_service == "webshell" {
            let Some(app) = &self.app else {
                return Some(reject("webshell_disabled".into()));
            };
            match app.cloud_webshell_slot() {
                Ok(permit) => Some(permit),
                Err(error) => return Some(reject(error.into())),
            }
        } else {
            None
        };
        if matches!(
            command.target_service.as_str(),
            "datad_panel" | "datad_panel_control"
        ) && self.app.is_none()
        {
            return Some(reject("panel_unavailable".into()));
        }
        state.active.insert(command.request_id.clone());
        state.remember(command.request_id.clone());
        drop(state);
        let manager = self.clone();
        tokio::spawn(async move { manager.run_bridge(command, shell_permit).await });
        None
    }

    async fn run_bridge(
        &self,
        command: RemoteCommand,
        shell_permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) {
        if let Some(permit) = shell_permit {
            if let Some(app) = &self.app {
                crate::cloud_shell::run(
                    app.cloud_webshell(),
                    &self.config,
                    &command.remote_url,
                    &command.token,
                    Duration::from_secs(command.ttl_seconds),
                    self.shutdown.subscribe(),
                    permit,
                )
                .await;
            }
            self.state.lock().await.active.remove(&command.request_id);
            return;
        }
        if matches!(
            command.target_service.as_str(),
            "datad_panel" | "datad_panel_control"
        ) {
            if let Some(app) = &self.app {
                crate::cloud_panel::run(
                    crate::cloud_panel::PanelFeeds {
                        state: app.cloud_panel_state(),
                        history: app.cloud_panel_history(),
                        schedule: Some(app.cloud_panel_schedule()),
                        speedtest: Some(app.cloud_panel_speedtest()),
                        cloud_app: Some(app.clone()),
                    },
                    command.target_service == "datad_panel_control",
                    &self.config,
                    &command.remote_url,
                    &command.token,
                    Duration::from_secs(command.ttl_seconds),
                    self.shutdown.subscribe(),
                )
                .await;
            }
            self.state.lock().await.active.remove(&command.request_id);
            return;
        }
        let mut workers = JoinSet::new();
        for _ in 0..4 {
            let config = self.config.clone();
            let command = command.clone();
            let shutdown = self.shutdown.subscribe();
            workers.spawn(async move {
                loop {
                    if bridge_pipe(&config, &command, shutdown.clone()).await {
                        return true;
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            });
        }
        let _ = tokio::time::timeout(Duration::from_secs(command.ttl_seconds), async {
            while let Some(result) = workers.join_next().await {
                if result.unwrap_or(true) {
                    break;
                }
            }
        })
        .await;
        workers.abort_all();
        self.state.lock().await.active.remove(&command.request_id);
    }
}

fn validate_remote(config: &Config, command: &RemoteCommand) -> Result<(), String> {
    if !config.enabled || !config.remote_enabled {
        return Err("remote_disabled".into());
    }
    if command.protocol_version != 1
        || command.action != "remote.open"
        || !topic(&command.request_id)
        || !(1..=43200).contains(&command.ttl_seconds)
        || command.token.len() != 64
    {
        return Err("invalid_request".into());
    }
    if command.remote_url.len() > 2048
        || command
            .remote_url
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err("invalid_remote_url".into());
    }
    let remote = Url::parse(&command.remote_url).map_err(|_| "invalid_remote_url")?;
    https_origin(&config.platform_url).map_err(|_| "invalid_remote_url")?;
    let approved = effective_remote_origins(config).iter().any(|origin| {
        remote.host_str() == origin.host_str()
            && remote.port_or_known_default() == origin.port_or_known_default()
    });
    if remote.scheme() != "wss"
        || !approved
        || !remote.username().is_empty()
        || remote.password().is_some()
        || remote.query().is_some()
        || remote.fragment().is_some()
        || remote.path() != format!("/api/remote/device/{}", command.request_id)
    {
        return Err("invalid_remote_url".into());
    }
    if command.target_ports.len() > 1
        || command
            .target_ports
            .first()
            .is_some_and(|port| *port != command.target_port)
    {
        return Err("multiple_ports_unsupported".into());
    }
    if command.target_service == "webshell" {
        if !config.remote_webshell_enabled {
            return Err("webshell_disabled".into());
        }
        if command.target_port != 0
            || !command.target_ports.is_empty()
            || command.ttl_seconds > 1800
            || !command.token.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("invalid_webshell_request".into());
        }
        return Ok(());
    }
    if matches!(
        command.target_service.as_str(),
        "datad_panel" | "datad_panel_control"
    ) {
        if command.target_service == "datad_panel_control" && !config.remote_panel_control_enabled {
            return Err("panel_control_disabled".into());
        }
        if command.target_port != 0
            || !command.target_ports.is_empty()
            || command.ttl_seconds > 3600
            || !command.token.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("invalid_panel_request".into());
        }
        return Ok(());
    }
    let allowed = config.services.iter().any(|service| {
        service.port == command.target_port
            && ((command.target_service == "terminal" && service.kind == "terminal")
                || (matches!(command.target_service.as_str(), "router_web" | "web")
                    && service.kind == "web"))
    });
    if !allowed {
        return Err("port_not_allowed".into());
    }
    Ok(())
}

async fn bridge_pipe(
    config: &Config,
    command: &RemoteCommand,
    mut shutdown: watch::Receiver<bool>,
) -> bool {
    if *shutdown.borrow() {
        return true;
    }
    let Ok(mut request) = command.remote_url.clone().into_client_request() else {
        return true;
    };
    let Ok(auth) = HeaderValue::from_str(&format!("Bearer {}", command.token)) else {
        return true;
    };
    let Ok(port) = HeaderValue::from_str(&command.target_port.to_string()) else {
        return true;
    };
    request.headers_mut().insert("authorization", auth);
    request.headers_mut().insert("x-nms-target-port", port);
    let connector = match websocket_tls(config) {
        Ok(value) => Connector::Rustls(value),
        Err(_) => return true,
    };
    let websocket = tokio::select! {
        value = connect_async_tls_with_config(request, None, false, Some(connector)) => {
            match value {
                Ok((socket, _)) => socket,
                Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                    // Expired/revoked sessions must release their slot, not retry for 12 hours.
                    return matches!(response.status().as_u16(), 401 | 403 | 404 | 410);
                }
                Err(_) => return false,
            }
        },
        _ = shutdown.changed() => return true,
    };
    let tcp = match TcpStream::connect(("127.0.0.1", command.target_port)).await {
        Ok(value) => value,
        Err(_) => return true,
    };
    let (mut ws_write, mut ws_read) = websocket.split();
    let (mut tcp_read, mut tcp_write) = tcp.into_split();
    let mut buffer = vec![0u8; 32 * 1024];
    loop {
        tokio::select! {
            _ = shutdown.changed() => return true,
            message = ws_read.next() => match message {
                Some(Ok(Message::Binary(data))) if data.len() <= 1024 * 1024 => {
                    if tcp_write.write_all(&data).await.is_err() { return false; }
                }
                Some(Ok(Message::Ping(data))) => {
                    if ws_write.send(Message::Pong(data)).await.is_err() { return false; }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return false,
                _ => return true,
            },
            read = tcp_read.read(&mut buffer) => match read {
                Ok(0) | Err(_) => return false,
                Ok(size) => {
                    if ws_write
                        .send(Message::Binary(buffer[..size].to_vec().into()))
                        .await
                        .is_err()
                    {
                        return false;
                    }
                },
            }
        }
    }
}

pub(crate) fn websocket_tls(config: &Config) -> Result<Arc<ClientConfig>, String> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if !config.ca_pem.is_empty() {
        let mut added = 0usize;
        for certificate in CertificateDer::pem_slice_iter(config.ca_pem.as_bytes()) {
            roots
                .add(certificate.map_err(|_| "CA 证书无效")?)
                .map_err(|_| "CA 证书无效")?;
            added += 1;
        }
        if added == 0 {
            return Err("CA 证书无效".into());
        }
    }
    Ok(Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

fn root(config: &Config) -> String {
    format!(
        "devices/{}/{}/{}",
        config.vendor, config.model, config.identity
    )
}

fn bootstrap_proof(config: &Config) -> String {
    let key = Sha256::digest(config.password.as_bytes());
    let mut inner_pad = [0x36_u8; 64];
    let mut outer_pad = [0x5c_u8; 64];
    for index in 0..key.len() {
        inner_pad[index] ^= key[index];
        outer_pad[index] ^= key[index];
    }
    let message = format!(
        "nms-onboard-v1\n{}\n{}\n{}\n{}\n{}\n{}",
        config.username,
        config.vendor,
        config.model,
        config.identity_type,
        config.identity,
        config.platform
    );
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message.as_bytes());
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner.finalize());
    hex(&outer.finalize())
}

fn bootstrap_report(config: &Config) -> Value {
    json!({"protocol_version":1,"vendor":config.vendor,"model":config.model,
        "device_id":config.identity,"id_type":config.identity_type,"platform":config.platform,
        "proof":bootstrap_proof(config)})
}

fn topic(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_. -".contains(&byte))
}

fn https_origin(raw: &str) -> Result<Url, String> {
    let invalid = || "NMS 地址必须是 HTTPS 站点地址".to_owned();
    if raw.len() > 512
        || raw
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(invalid());
    }
    let url = Url::parse(raw).map_err(|_| invalid())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || url.host_str().is_some_and(|host| host.contains('*'))
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port() == Some(0)
    {
        return Err(invalid());
    }
    Ok(url)
}

fn uses_builtin_network(config: &Config) -> bool {
    config.broker == DEFAULT_BROKER
        && https_origin(&config.platform_url)
            .ok()
            .is_some_and(|origin| origin.origin().ascii_serialization() == DEFAULT_PLATFORM_URL)
}

fn effective_remote_origins(config: &Config) -> Vec<Url> {
    let mut result: Vec<Url> = Vec::new();
    let built_in = std::iter::once(DEFAULT_MEMBER_ORIGIN).filter(|_| uses_builtin_network(config));
    for raw in std::iter::once(config.platform_url.as_str())
        .chain(config.remote_origins.iter().map(String::as_str))
        .chain(built_in)
    {
        if let Ok(origin) = https_origin(raw)
            && !result
                .iter()
                .any(|existing| existing.origin() == origin.origin())
        {
            result.push(origin);
        }
    }
    result
}

fn reported_remote_origins(config: &Config) -> Vec<String> {
    effective_remote_origins(config)
        .iter()
        .map(|origin| origin.origin().ascii_serialization())
        .collect()
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
            || matches!(service.port, 9460 | 9461)
            || !ports.insert(service.port)
            || !matches!(service.kind.as_str(), "web" | "terminal")
        {
            return Err("后台名称、类型或端口无效，不能使用 datad 管理端口".into());
        }
    }
    if !config.broker.is_empty() {
        mqtt_url(&config.broker)?;
    }
    if config.remote_origins.len() > 4 {
        return Err("最多配置 4 个备用远程地址".into());
    }
    let mut origins = HashSet::new();
    if !config.platform_url.is_empty() {
        origins.insert(
            https_origin(&config.platform_url)?
                .origin()
                .ascii_serialization(),
        );
    }
    for raw in &config.remote_origins {
        if !origins.insert(https_origin(raw)?.origin().ascii_serialization()) {
            return Err("远程地址不能重复".into());
        }
    }
    if !config.ca_pem.is_empty() {
        websocket_tls(config)?;
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

fn set_status(status: &Arc<Mutex<Status>>, state: &str, error: &str) {
    let mut value = status.lock().unwrap();
    value.state = state.into();
    value.error = error.into();
}

fn number_at(value: &Value, path: &[&str]) -> Option<f64> {
    path.iter()
        .try_fold(value, |current, key| current.get(*key))
        .and_then(Value::as_f64)
}

fn atomic_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| "保存配置失败")?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|_| "保存配置失败")?;
    }
    let temp = path.with_extension(format!("tmp-{}-{}", std::process::id(), now()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|_| "保存配置失败")?;
    let data = serde_json::to_vec_pretty(value).map_err(|_| "保存配置失败")?;
    file.write_all(&data)
        .and_then(|_| file.sync_all())
        .map_err(|_| "保存配置失败")?;
    drop(file);
    fs::rename(temp, path).map_err(|_| "保存配置失败".into())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn hex(value: &[u8]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::generate_simple_self_signed;
    use rustls::{ServerConfig, pki_types::PrivateKeyDer};
    use tokio::{
        io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::{mpsc, oneshot},
    };
    use tokio_rustls::TlsAcceptor;
    use tokio_tungstenite::accept_hdr_async;

    #[test]
    fn tunnel_dedup_uses_boot_elapsed_ttl_across_wall_steps_and_suspend() {
        for wall_step in [-8 * 3600, 8 * 3600] {
            crate::elapsed::with_clock(Duration::from_secs(100), 1_780_000_000, || {
                let mut state = BridgeState::default();
                state.remember("request".into());
                crate::elapsed::advance(Duration::ZERO, wall_step);
                assert!(state.already_seen("request"));
                crate::elapsed::advance(Duration::from_secs(12 * 3600 - 1), 0);
                assert!(state.already_seen("request"));
                crate::elapsed::advance(Duration::from_secs(1), 0);
                assert!(!state.already_seen("request"));
                assert!(state.seen.is_empty());
            });
        }
    }

    #[test]
    fn pending_credentials_derive_stable_device_identity_and_proof() {
        let username = format!("enr_{}", "a".repeat(32));
        let config = Config {
            username: username.clone(),
            password: "sample-secret".into(),
            model: "MU5250".into(),
            identity: "11111111-1111-4111-8111-111111111111".into(),
            ..Default::default()
        };
        assert!(pending_username(&username));
        assert_eq!(
            bootstrap_proof(&config),
            "ca7356594d85a66c7dd411ae8afaf01705a986c6d521dd6e905ebdd9348a1647"
        );
        assert_eq!(
            deterministic_uuid("ufi-device:modem_msn:MSN-fixture"),
            "7d77fceb-0592-529a-8d12-2b54068aaaff"
        );

        let dir = std::env::temp_dir().join(format!(
            "datad-quick-connect-{}-{}",
            std::process::id(),
            now()
        ));
        fs::create_dir_all(&dir).unwrap();
        let mut fields = serde_json::Map::new();
        fields.insert(
            "device".into(),
            json!({"api_template":"MU5250","vendor":"ZTE"}),
        );
        fields.insert("uci_device_info".into(), json!({"modem_msn":"MSN-fixture"}));
        let snapshot = Snapshot {
            ts: 1,
            datad: crate::model::DatadVersion::default(),
            fields,
        };
        let mut cloud = Cloud::load(&dir);
        cloud.config.broker = "wss://legacy.example/mqtt".into();
        cloud.config.platform_url = "https://legacy.example".into();
        cloud.config.remote_panel_control_enabled = true;
        cloud.config.remote_webshell_enabled = true;
        let public = cloud
            .quick_connect(
                QuickConnect {
                    username,
                    password: "sample-secret".into(),
                },
                &snapshot,
            )
            .unwrap();
        assert_eq!(public["config"]["model"], "MU5250");
        assert_eq!(public["config"]["broker"], DEFAULT_BROKER);
        assert_eq!(public["config"]["platform_url"], DEFAULT_PLATFORM_URL);
        assert_eq!(
            public["config"]["identity"],
            "7d77fceb-0592-529a-8d12-2b54068aaaff"
        );
        assert_eq!(public["config"]["remote_enabled"], true);
        assert_eq!(public["config"]["remote_panel_control_enabled"], false);
        assert_eq!(public["config"]["remote_webshell_enabled"], false);
        assert_eq!(public["password_configured"], true);
        assert!(!public.to_string().contains("sample-secret"));
        assert!(
            cloud
                .quick_connect(
                    QuickConnect {
                        username: "enr_".to_owned() + &"b".repeat(32),
                        password: "other".into()
                    },
                    &snapshot
                )
                .is_err()
        );
        fs::remove_dir_all(dir).unwrap();

        let fallback_dir = std::env::temp_dir().join(format!(
            "datad-quick-connect-fallback-{}-{}",
            std::process::id(),
            now()
        ));
        fs::create_dir_all(&fallback_dir).unwrap();
        let mut fallback = snapshot.clone();
        fallback.fields.insert(
            "uci_device_info".into(),
            json!({"serial_number":"SERIAL-fixture"}),
        );
        let mut cloud = Cloud::load(&fallback_dir);
        cloud
            .quick_connect(
                QuickConnect {
                    username: format!("enr_{}", "b".repeat(32)),
                    password: "sample-secret".into(),
                },
                &fallback,
            )
            .unwrap();
        assert_eq!(
            cloud.public_config()["config"]["identity"],
            deterministic_uuid("ufi-device:serial_number:SERIAL-fixture")
        );
        fs::remove_dir_all(fallback_dir).unwrap();
    }

    #[test]
    fn mqtt_addresses_require_encryption_and_keep_wss_paths() {
        init_crypto();
        for address in [
            "ssl://broker.example:8883",
            "wss://broker.example/mqtt",
            "wss://broker.example:8443/custom/mqtt",
            "wss://[::1]:8443/mqtt",
        ] {
            let config = Config {
                broker: address.into(),
                ..Default::default()
            };
            validate(&config).unwrap();
            let options = mqtt_options(&config, "fixture").unwrap();
            if address.starts_with("wss://") {
                assert!(matches!(options.transport(), Transport::Wss(_)));
                assert_eq!(
                    options.broker().websocket_url(),
                    Some(mqtt_url(address).unwrap().as_str())
                );
            } else {
                assert!(matches!(options.transport(), Transport::Tls(_)));
            }
        }
        for address in [
            "ws://broker.example/mqtt",
            "mqtt://broker.example:1883",
            "https://broker.example/mqtt",
            "ssl://broker.example",
            "ssl://broker.example:8883/mqtt",
            "wss://user:secret@broker.example/mqtt",
            "wss://broker.example/mqtt?token=secret",
            "wss://broker.example/mqtt#fragment",
            "wss://broker.example:0/mqtt",
            "wss://broker.example:65536/mqtt",
            "wss://broker.example\n/mqtt",
            " wss://broker.example/mqtt",
        ] {
            assert!(mqtt_url(address).is_err(), "accepted {address}");
        }
        assert_eq!(
            mqtt_url("wss://broker.example/mqtt")
                .unwrap()
                .port_or_known_default(),
            Some(443)
        );
    }

    // WebSocket messages need not align with MQTT packets. The fixture broker
    // buffers complete MQTT packets independently from the client's framing.
    fn take_mqtt_packet(bytes: &mut Vec<u8>) -> Option<(u8, Vec<u8>)> {
        let mut size = 0usize;
        let mut multiplier = 1usize;
        for offset in 1..=4 {
            let value = *bytes.get(offset)?;
            size += usize::from(value & 127) * multiplier;
            if value & 128 == 0 {
                let end = offset + 1 + size;
                if bytes.len() < end {
                    return None;
                }
                let header = bytes[0];
                let payload = bytes[offset + 1..end].to_vec();
                bytes.drain(..end);
                return Some((header, payload));
            }
            multiplier *= 128;
        }
        panic!("invalid fixture MQTT remaining length");
    }

    fn mqtt_publish(topic: &str, payload: &Value) -> Vec<u8> {
        let body = payload.to_string();
        let length = topic.len() + body.len() + 2;
        let mut packet = vec![0x30];
        let mut remaining = length;
        loop {
            let mut byte = (remaining % 128) as u8;
            remaining /= 128;
            if remaining > 0 {
                byte |= 128;
            }
            packet.push(byte);
            if remaining == 0 {
                break;
            }
        }
        packet.extend_from_slice(&(topic.len() as u16).to_be_bytes());
        packet.extend_from_slice(topic.as_bytes());
        packet.extend_from_slice(body.as_bytes());
        packet
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)]
    async fn pending_mqtt_reports_identity_before_telemetry() {
        let (acceptor, ca_pem) = tls_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let username = format!("enr_{}", "a".repeat(32));
        let identity = "11111111-1111-4111-8111-111111111111";
        let root = format!("devices/ZTE/MU5250/{identity}");
        let (reports_tx, mut reports_rx) = mpsc::channel(8);
        let broker_username = username.clone();
        let broker_root = root.clone();
        let broker = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let stream = acceptor.accept(tcp).await.unwrap();
            let mut ws = accept_hdr_async(stream, |request: &tokio_tungstenite::tungstenite::handshake::server::Request, mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                assert_eq!(request.uri().path(), "/mqtt");
                response.headers_mut().insert("sec-websocket-protocol", HeaderValue::from_static("mqtt"));
                Ok(response)
            }).await.unwrap();
            let mut pending = Vec::new();
            let mut bound = false;
            'messages: while let Some(Ok(message)) = ws.next().await {
                if let Message::Binary(data) = message {
                    pending.extend_from_slice(&data);
                }
                while let Some((header, payload)) = take_mqtt_packet(&mut pending) {
                    match header >> 4 {
                        1 => {
                            assert!(
                                payload
                                    .windows(broker_username.len())
                                    .any(|bytes| bytes == broker_username.as_bytes())
                            );
                            ws.send(Message::Binary(vec![0x20, 2, 0, 0].into()))
                                .await
                                .unwrap();
                        }
                        8 => {
                            let topic = String::from_utf8_lossy(&payload[4..]);
                            assert!(
                                topic.starts_with(if bound { &broker_root } else { "onboard/" }),
                                "unexpected subscription {topic:?}, bound={bound}"
                            );
                            ws.send(Message::Binary(
                                vec![0x90, 3, payload[0], payload[1], 1].into(),
                            ))
                            .await
                            .unwrap();
                        }
                        3 => {
                            let topic_len =
                                usize::from(u16::from_be_bytes([payload[0], payload[1]]));
                            let topic = std::str::from_utf8(&payload[2..2 + topic_len]).unwrap();
                            let packet_id = 2 + topic_len;
                            let value: Value =
                                serde_json::from_slice(&payload[packet_id + 2..]).unwrap();
                            if !bound {
                                assert_eq!(topic, format!("onboard/{broker_username}/hello"));
                                assert_eq!(value["model"], "MU5250");
                                assert_eq!(value["device_id"], identity);
                                assert_eq!(
                                    value["proof"],
                                    "ca7356594d85a66c7dd411ae8afaf01705a986c6d521dd6e905ebdd9348a1647"
                                );
                                bound = true;
                                ws.send(Message::Binary(
                                    vec![0x40, 2, payload[packet_id], payload[packet_id + 1]]
                                        .into(),
                                ))
                                .await
                                .unwrap();
                                ws.send(Message::Binary(mqtt_publish(&format!("onboard/{broker_username}/result"), &json!({"protocol_version":1,"state":"bound","topic_root":broker_root})).into())).await.unwrap();
                            } else {
                                assert!(topic.starts_with(&broker_root));
                                reports_tx.send(topic.to_owned()).await.unwrap();
                                ws.send(Message::Binary(
                                    vec![0x40, 2, payload[packet_id], payload[packet_id + 1]]
                                        .into(),
                                ))
                                .await
                                .unwrap();
                                if topic.ends_with("/telemetry/device") {
                                    break 'messages;
                                }
                            }
                        }
                        12 => ws
                            .send(Message::Binary(vec![0xd0, 0].into()))
                            .await
                            .unwrap(),
                        _ => {}
                    }
                }
            }
        });
        let config = Config {
            enabled: true,
            broker: format!("wss://{address}/mqtt"),
            username,
            password: "sample-secret".into(),
            ca_pem,
            model: "MU5250".into(),
            identity: identity.into(),
            ..Default::default()
        };
        let (_config_tx, config_rx) = watch::channel(Some(config));
        let (_state_tx, state_rx) = watch::channel(Snapshot {
            ts: now(),
            datad: Default::default(),
            fields: serde_json::Map::new(),
        });
        let status = Arc::new(Mutex::new(Status::default()));
        let task = tokio::spawn(supervisor(config_rx, state_rx, status, None));
        let topic = tokio::time::timeout(Duration::from_secs(10), reports_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(topic.starts_with(&root));
        task.abort();
        broker.await.unwrap();
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)] // Tungstenite callback requires its HTTP error response type.
    async fn mqtt_wss_authenticates_reports_and_reconnects() {
        let (acceptor, ca_pem) = tls_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (reports_tx, mut reports_rx) = mpsc::channel(32);
        let broker = tokio::spawn(async move {
            for connection in 0..2 {
                let (tcp, _) = listener.accept().await.unwrap();
                let stream = acceptor.accept(tcp).await.unwrap();
                let mut ws = accept_hdr_async(stream, |request: &tokio_tungstenite::tungstenite::handshake::server::Request, mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    assert_eq!(request.uri().path(), "/custom/mqtt");
                    assert_eq!(request.headers()["sec-websocket-protocol"], "mqtt");
                    assert!(!request.headers().contains_key("authorization"));
                    response.headers_mut().insert("sec-websocket-protocol", HeaderValue::from_static("mqtt"));
                    Ok(response)
                }).await.unwrap();
                let mut pending = Vec::new();
                let mut got_system = false;
                let mut got_device = false;
                let mut got_online = false;
                'messages: while let Some(Ok(message)) = ws.next().await {
                    if let Message::Binary(data) = message {
                        pending.extend_from_slice(&data);
                    }
                    while let Some((header, payload)) = take_mqtt_packet(&mut pending) {
                        let response = match header >> 4 {
                            1 => {
                                assert!(payload.windows(12).any(|w| w == b"fixture-user"));
                                assert!(payload.windows(16).any(|w| w == b"fixture-password"));
                                vec![0x20, 2, 0, 0]
                            }
                            8 => vec![0x90, 3, payload[0], payload[1], 1],
                            3 => {
                                let topic_len =
                                    usize::from(u16::from_be_bytes([payload[0], payload[1]]));
                                let mut offset = topic_len + 2;
                                let ack = if (header >> 1) & 3 == 1 {
                                    let ack = vec![0x40, 2, payload[offset], payload[offset + 1]];
                                    offset += 2;
                                    ack
                                } else {
                                    vec![]
                                };
                                if let Ok(value) =
                                    serde_json::from_slice::<Value>(&payload[offset..])
                                {
                                    assert!(!value.to_string().contains("do-not-upload"));
                                    got_online |= value["online"] == true;
                                    got_device |= value["model"] == "WSS-fixture";
                                    got_system |= value.get("boot_id").is_some();
                                    reports_tx.send((connection, value)).await.unwrap();
                                }
                                ack
                            }
                            12 => vec![0xd0, 0],
                            _ => vec![],
                        };
                        if !response.is_empty() {
                            ws.send(Message::Binary(response.into())).await.unwrap();
                        }
                        if got_online && got_device && got_system {
                            ws.close(None).await.unwrap();
                            break 'messages;
                        }
                    }
                }
            }
        });
        let config = Config {
            enabled: true,
            broker: format!("wss://{address}/custom/mqtt"),
            username: "fixture-user".into(),
            password: "fixture-password".into(),
            ca_pem,
            model: "WSS-fixture".into(),
            identity: "fixture-device".into(),
            ..Default::default()
        };
        let (_config_tx, config_rx) = watch::channel(Some(config));
        let (_state_tx, state_rx) = watch::channel(Snapshot {
            ts: now(),
            datad: Default::default(),
            fields: serde_json::Map::from_iter([("password".into(), json!("do-not-upload"))]),
        });
        let status = Arc::new(Mutex::new(Status::default()));
        let task = tokio::spawn(supervisor(config_rx, state_rx, status, None));
        tokio::time::timeout(Duration::from_secs(15), async {
            let mut connections = HashSet::new();
            while connections.len() < 2 {
                let (connection, value) = reports_rx.recv().await.unwrap();
                if value["online"] == true {
                    connections.insert(connection);
                }
            }
        })
        .await
        .unwrap();
        task.abort();
        broker.abort();
    }

    #[tokio::test]
    async fn mqtt_wss_rejects_untrusted_certificates_and_hostname_mismatch() {
        init_crypto();
        for trust_wrong_hostname in [false, true] {
            let names = if trust_wrong_hostname {
                vec!["wrong.example".into()]
            } else {
                vec!["127.0.0.1".into()]
            };
            let certified = generate_simple_self_signed(names).unwrap();
            let key = PrivateKeyDer::Pkcs8(certified.key_pair.serialize_der().into());
            let server = ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![certified.cert.der().clone()], key)
                .unwrap();
            let acceptor = TlsAcceptor::from(Arc::new(server));
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (tcp, _) = listener.accept().await.unwrap();
                assert!(acceptor.accept(tcp).await.is_err());
            });
            let config = Config {
                broker: format!("wss://{address}/mqtt"),
                ca_pem: if trust_wrong_hostname {
                    certified.cert.pem()
                } else {
                    String::new()
                },
                ..Default::default()
            };
            let (_state_tx, state_rx) = watch::channel(Snapshot {
                ts: now(),
                datad: Default::default(),
                fields: Default::default(),
            });
            let (_config_tx, config_rx) = watch::channel(Some(config.clone()));
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                session(
                    &config,
                    state_rx,
                    config_rx,
                    Arc::new(Mutex::new(Status::default())),
                    None,
                ),
            )
            .await
            .unwrap();
            assert!(matches!(result, SessionEnd::Failed));
            tokio::time::timeout(Duration::from_secs(5), server)
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[test]
    fn defaults_validate() {
        validate(&Config::default()).unwrap()
    }

    #[test]
    fn remote_feature_updates_keep_cloud_credentials_and_defer_reconnect() {
        let dir = std::env::temp_dir().join(format!(
            "datad-cloud-panel-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut cloud = Cloud::load(&dir);
        let _receiver = cloud.tx.subscribe();
        let config = Config {
            enabled: true,
            remote_enabled: true,
            username: "dev_fixture".into(),
            password: "must-not-leak".into(),
            model: "MU5250".into(),
            identity: "fixture-id".into(),
            platform_url: "https://custom.example".into(),
            broker: "wss://custom.example/mqtt".into(),
            ..Config::default()
        };
        cloud
            .update(Update {
                config,
                clear_password: false,
            })
            .unwrap();
        let before = cloud.config.clone();
        let first = cloud.panel_config();
        assert!(first["password_configured"].as_bool().unwrap());
        assert!(!first.to_string().contains("must-not-leak"));
        assert!(first.get("ca_pem").is_none());
        let service = Service {
            name: "UFI Test".into(),
            port: 2333,
            kind: "web".into(),
        };
        let (view, changed) = cloud
            .save_remote_features(RemoteFeatures {
                remote_panel_control_enabled: true,
                remote_webshell_enabled: true,
                services: vec![service.clone()],
            })
            .unwrap();
        assert!(changed && view["remote_webshell_enabled"] == true);
        assert_eq!(cloud.config.password, before.password);
        assert_eq!(cloud.config.broker, before.broker);
        assert_eq!(cloud.config.platform_url, before.platform_url);
        assert_eq!(cloud.config.username, before.username);
        assert_eq!(cloud.config.identity, before.identity);
        assert_eq!(cloud.config.services, vec![service]);
        assert!(!cloud.tx.borrow().as_ref().unwrap().remote_webshell_enabled);
        cloud.activate_saved_features();
        assert!(cloud.tx.borrow().as_ref().unwrap().remote_webshell_enabled);
        let raw = std::fs::read(&cloud.file).unwrap();
        assert!(String::from_utf8_lossy(&raw).contains("must-not-leak"));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&cloud.file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            !cloud
                .save_remote_features(RemoteFeatures {
                    remote_panel_control_enabled: true,
                    remote_webshell_enabled: true,
                    services: cloud.config.services.clone()
                })
                .unwrap()
                .1
        );
        assert!(
            cloud
                .save_remote_features(RemoteFeatures {
                    remote_panel_control_enabled: true,
                    remote_webshell_enabled: true,
                    services: vec![Service {
                        name: "Bad".into(),
                        port: 9460,
                        kind: "web".into()
                    }]
                })
                .is_err()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn built_in_routes_remain_disabled_until_credentials_and_remote_access_are_enabled() {
        let mut config = Config::default();
        assert_eq!(config.platform_url, DEFAULT_PLATFORM_URL);
        assert_eq!(config.broker, DEFAULT_BROKER);
        assert!(!config.enabled && !config.remote_enabled);
        assert!(config.username.is_empty() && config.password.is_empty());
        assert!(config.remote_origins.is_empty());
        assert_eq!(
            reported_remote_origins(&config),
            vec![DEFAULT_PLATFORM_URL, DEFAULT_MEMBER_ORIGIN]
        );

        let command = RemoteCommand {
            protocol_version: 1,
            request_id: "member-default".into(),
            action: "remote.open".into(),
            remote_url: "wss://a.ericsfj.com:16001/api/remote/device/member-default".into(),
            token: "a".repeat(64),
            target_service: "router_web".into(),
            target_port: 80,
            target_ports: vec![],
            ttl_seconds: 900,
        };
        assert_eq!(
            validate_remote(&config, &command).unwrap_err(),
            "remote_disabled"
        );
        config.enabled = true;
        config.remote_enabled = true;
        validate_remote(&config, &command).unwrap();

        config.broker = "wss://custom.example/mqtt".into();
        assert!(validate_remote(&config, &command).is_err());
        assert_eq!(reported_remote_origins(&config), vec![DEFAULT_PLATFORM_URL]);
        config.remote_origins = vec![DEFAULT_MEMBER_ORIGIN.into()];
        validate_remote(&config, &command).unwrap();
        config.remote_origins.clear();
        config.broker = DEFAULT_BROKER.into();
        config.platform_url = "https://custom.example".into();
        assert!(validate_remote(&config, &command).is_err());
        assert_eq!(
            reported_remote_origins(&config),
            vec!["https://custom.example"]
        );
    }

    #[test]
    fn existing_custom_cloud_config_is_not_replaced_by_built_in_defaults() {
        let config: Config = serde_json::from_str(
            r#"{"enabled":false,"platform_url":"https://custom.example","broker":"wss://custom.example/mqtt","remote_origins":[]}"#,
        )
        .unwrap();
        assert_eq!(config.platform_url, "https://custom.example");
        assert_eq!(config.broker, "wss://custom.example/mqtt");
        assert!(config.remote_origins.is_empty());
        assert_eq!(
            reported_remote_origins(&config),
            vec!["https://custom.example"]
        );
        validate(&config).unwrap();
    }

    #[test]
    fn cloud_load_prefills_new_device_but_preserves_saved_custom_file_and_ui_values() {
        let dir = std::env::temp_dir().join(format!(
            "datad-cloud-defaults-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir(&dir).unwrap();
        let fresh = Cloud::load(&dir);
        let fresh_view = fresh.public_config();
        assert_eq!(fresh_view["config"]["platform_url"], DEFAULT_PLATFORM_URL);
        assert_eq!(fresh_view["config"]["broker"], DEFAULT_BROKER);
        assert_eq!(fresh_view["config"]["enabled"], false);
        assert_eq!(fresh_view["password_configured"], false);
        assert!(!dir.join("cloud.json").exists());

        let saved = br#"{"enabled":false,"platform_url":"https://custom.example","broker":"wss://custom.example/mqtt","remote_origins":[]}"#;
        fs::write(dir.join("cloud.json"), saved).unwrap();
        let existing = Cloud::load(&dir);
        let view = existing.public_config();
        assert_eq!(view["config"]["platform_url"], "https://custom.example");
        assert_eq!(view["config"]["broker"], "wss://custom.example/mqtt");
        assert_eq!(view["config"]["remote_origins"], json!([]));
        assert_eq!(fs::read(dir.join("cloud.json")).unwrap(), saved);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_management_ports() {
        let mut config = Config::default();
        config.services[0].port = 9460;
        assert!(validate(&config).is_err());
    }

    #[test]
    fn telemetry_does_not_copy_raw_state() {
        let value = system_telemetry(&json!({
            "system":{"cpu_usage":23,"mem_used_pct":42},
            "thermal":{"cpu_celsius":51},
            "password":"do-not-upload"
        }));
        let encoded = value.to_string();
        assert!(!encoded.contains("do-not-upload"));
        assert_eq!(value["cpu"]["usage_percent"].as_f64(), Some(23.0));
        assert_eq!(value["memory"]["usage_percent"].as_f64(), Some(42.0));
        assert_eq!(value["temperature"]["cpu_celsius"].as_f64(), Some(51.0));
    }

    #[test]
    fn network_telemetry_reads_wan_addresses_from_state_interfaces() {
        let state = json!({
            "interfaces":{
                "wan4":{"ipv4":[{"address":"10.20.30.40","mask":8},{"address":"not-an-ip"},{"address":"2001:db8::1"}]},
                "wan6":{"ipv6":[{"address":"2001:db8::7","mask":64},{"address":"10.0.0.1"}]},
                "lan":{"ipv4":[{"address":"192.168.0.1","mask":24}]}
            },
            "net":{"interfaces":{"ipv4":{"ipv4":[{"address":"203.0.113.9"}]}}}
        });
        assert_eq!(
            upstream_addresses(&state, "wan4", "ipv4"),
            vec![json!("10.20.30.40")]
        );
        assert_eq!(
            upstream_addresses(&state, "wan6", "ipv6"),
            vec![json!("2001:db8::7")]
        );
        assert!(upstream_addresses(&json!({}), "wan4", "ipv4").is_empty());
    }

    #[test]
    fn network_telemetry_reports_only_known_serving_cell_identity() {
        let value = serving_cell(&json!({
            "net":{"type":"ENDC","mcc":460,"mnc":0,"lte_tac":40302,"lte_cell_id":23621663,
                "lte_pci":123,"nr_tac":0,"nr_cell_id":0,"nr_pci":321,"imsi":"do-not-upload"}
        }));
        assert_eq!(
            value,
            json!({"rat":"ENDC","mcc":460,"mnc":0,"lte_tac":40302,"lte_cell_id":23621663,"lte_pci":123,"nr_pci":321})
        );
        assert_eq!(serving_cell(&json!({})), json!({}));
        // Without a registered PLMN an MNC of 0 is only the collector default.
        assert_eq!(serving_cell(&json!({"net":{"mcc":0,"mnc":0}})), json!({}));
    }

    #[test]
    fn remote_validation_is_strict() {
        let config = Config {
            enabled: true,
            remote_enabled: true,
            platform_url: "https://nms.example.com".into(),
            services: vec![Service {
                name: "UFI".into(),
                port: 2333,
                kind: "web".into(),
            }],
            ..Default::default()
        };
        let mut command = RemoteCommand {
            protocol_version: 1,
            request_id: "session-123".into(),
            action: "remote.open".into(),
            remote_url: "wss://nms.example.com/api/remote/device/session-123".into(),
            token: "a".repeat(64),
            target_service: "router_web".into(),
            target_port: 2333,
            target_ports: vec![],
            ttl_seconds: 900,
        };
        validate_remote(&config, &command).unwrap();
        command.target_ports = vec![2333, 80];
        assert_eq!(
            validate_remote(&config, &command).unwrap_err(),
            "multiple_ports_unsupported"
        );
    }

    #[test]
    fn panel_is_an_on_demand_zero_port_session_not_a_management_proxy() {
        let mut config = Config {
            enabled: true,
            remote_enabled: true,
            platform_url: "https://nms.example.com".into(),
            ..Default::default()
        };
        let mut command = RemoteCommand {
            protocol_version: 1,
            request_id: "panel-session".into(),
            action: "remote.open".into(),
            remote_url: "wss://nms.example.com/api/remote/device/panel-session".into(),
            token: "a".repeat(64),
            target_service: "datad_panel".into(),
            target_port: 0,
            target_ports: vec![],
            ttl_seconds: 3600,
        };
        validate_remote(&config, &command).unwrap();
        command.target_port = 9460;
        assert_eq!(
            validate_remote(&config, &command).unwrap_err(),
            "invalid_panel_request"
        );
        command.target_port = 0;
        command.target_ports = vec![0];
        assert!(validate_remote(&config, &command).is_err());
        command.target_ports.clear();
        command.ttl_seconds = 3601;
        assert!(validate_remote(&config, &command).is_err());
        command.ttl_seconds = 60;
        command.token = "z".repeat(64);
        assert!(validate_remote(&config, &command).is_err());
        command.token = "a".repeat(64);
        command.target_service = "datad_panel_control".into();
        assert_eq!(
            validate_remote(&config, &command).unwrap_err(),
            "panel_control_disabled"
        );
        config.remote_panel_control_enabled = true;
        validate_remote(&config, &command).unwrap();
        command.target_port = 9460;
        assert_eq!(
            validate_remote(&config, &command).unwrap_err(),
            "invalid_panel_request"
        );
        command.target_port = 0;
        config.remote_enabled = false;
        assert_eq!(
            validate_remote(&config, &command).unwrap_err(),
            "remote_disabled"
        );
    }

    #[test]
    fn alternate_remote_origins_require_explicit_https_hosts_and_ports() {
        let mut config = Config {
            enabled: true,
            remote_enabled: true,
            platform_url: "https://nms.example.com".into(),
            ..Default::default()
        };
        let mut command = RemoteCommand {
            protocol_version: 1,
            request_id: "route-fixture".into(),
            action: "remote.open".into(),
            remote_url: "wss://relay.example.com:16001/api/remote/device/route-fixture".into(),
            token: "a".repeat(64),
            target_service: "router_web".into(),
            target_port: 80,
            target_ports: vec![],
            ttl_seconds: 900,
        };
        assert!(validate_remote(&config, &command).is_err());
        config.remote_origins = vec!["https://relay.example.com:16001".into()];
        validate_remote(&config, &command).unwrap();
        command.remote_url = "wss://nms.example.com/api/remote/device/route-fixture".into();
        validate_remote(&config, &command).unwrap();
        for address in [
            "wss://relay.example.com/api/remote/device/route-fixture",
            "wss://relay.example.com:16002/api/remote/device/route-fixture",
            "wss://relay.example.com.evil.test:16001/api/remote/device/route-fixture",
            "ws://relay.example.com:16001/api/remote/device/route-fixture",
            "wss://user:secret@relay.example.com:16001/api/remote/device/route-fixture",
            "wss://relay.example.com:16001/api/remote/device/other-session",
            "wss://relay.example.com:16001/api/remote/device/route-fixture?token=x",
            "wss://relay.example.com:16001/api/remote/device/route-fixture#x",
            " wss://relay.example.com:16001/api/remote/device/route-fixture",
        ] {
            command.remote_url = address.into();
            assert!(
                validate_remote(&config, &command).is_err(),
                "accepted {address}"
            );
        }
        assert_eq!(
            reported_remote_origins(&config),
            vec!["https://nms.example.com", "https://relay.example.com:16001"]
        );
        config.remote_origins.clear();
        assert_eq!(
            reported_remote_origins(&config),
            vec!["https://nms.example.com"]
        );
        assert!(
            serde_json::from_str::<Config>("{}")
                .unwrap()
                .remote_origins
                .is_empty()
        );
    }

    #[test]
    fn origin_configuration_is_bounded_and_does_not_contain_credentials() {
        for raw in [
            "http://relay.example.com",
            "https://*.example.com",
            "https://user:secret@relay.example.com",
            "https://relay.example.com/path",
            "https://relay.example.com?",
            "https://relay.example.com#",
            "https://relay.example.com:0",
            "https://relay.example.com:65536",
            "https://relay.example.com\n",
            " https://relay.example.com",
        ] {
            let config = Config {
                remote_origins: vec![raw.into()],
                ..Default::default()
            };
            assert!(validate(&config).is_err(), "accepted {raw}");
        }
        let config = Config {
            platform_url: "https://nms.example.com".into(),
            remote_origins: vec!["https://NMS.example.com:443/".into()],
            ..Default::default()
        };
        assert!(validate(&config).is_err());
        let config = Config {
            remote_origins: (1..=5)
                .map(|i| format!("https://relay{i}.example.com"))
                .collect(),
            ..Default::default()
        };
        assert!(validate(&config).is_err());
        let config = Config {
            remote_origins: vec!["https://[::1]:9443".into()],
            ..Default::default()
        };
        validate(&config).unwrap();
    }

    #[tokio::test]
    async fn native_webshell_requires_opt_in_and_local_engine() {
        assert!(!Config::default().remote_webshell_enabled);
        assert!(
            !serde_json::from_str::<Config>("{}")
                .unwrap()
                .remote_webshell_enabled
        );
        let mut config = Config {
            enabled: true,
            remote_enabled: true,
            platform_url: "https://nms.example.com".into(),
            ..Default::default()
        };
        let mut command = RemoteCommand {
            protocol_version: 1,
            request_id: "native-session".into(),
            action: "remote.open".into(),
            remote_url: "wss://nms.example.com/api/remote/device/native-session".into(),
            token: "a".repeat(64),
            target_service: "webshell".into(),
            target_port: 0,
            target_ports: vec![],
            ttl_seconds: 1800,
        };
        assert_eq!(
            validate_remote(&config, &command).unwrap_err(),
            "webshell_disabled"
        );
        config.remote_webshell_enabled = true;
        validate_remote(&config, &command).unwrap();
        command.target_port = 22;
        assert!(validate_remote(&config, &command).is_err());
        command.target_port = 0;
        command.target_ports = vec![0];
        assert!(validate_remote(&config, &command).is_err());
        command.target_ports.clear();
        command.ttl_seconds = 1801;
        assert!(validate_remote(&config, &command).is_err());
        command.ttl_seconds = 1800;
        command.token = "!".repeat(64);
        assert!(validate_remote(&config, &command).is_err());
        command.token = "a".repeat(64);
        command.remote_url = "wss://other.example.com/api/remote/device/native-session".into();
        assert!(validate_remote(&config, &command).is_err());
        command.remote_url = "wss://nms.example.com/api/remote/device/native-session".into();
        let manager = BridgeManager::new(config, None);
        assert_eq!(
            manager.receive(command).await.unwrap()["error"]["code"],
            "webshell_disabled"
        );
        manager.shutdown();
        assert!(
            *manager.shutdown.subscribe().borrow(),
            "shutdown lost before subscription"
        );
    }

    fn tls_fixture() -> (TlsAcceptor, String) {
        init_crypto();
        let certified =
            generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
        let certificate = certified.cert.der().clone();
        let key = PrivateKeyDer::Pkcs8(certified.key_pair.serialize_der().into());
        let server = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate], key)
            .unwrap();
        (TlsAcceptor::from(Arc::new(server)), certified.cert.pem())
    }

    async fn mqtt_packet(reader: &mut (impl AsyncRead + Unpin)) -> (u8, Vec<u8>) {
        let header = reader.read_u8().await.unwrap();
        let mut size = 0usize;
        let mut multiplier = 1usize;
        loop {
            let value = reader.read_u8().await.unwrap();
            size += usize::from(value & 127) * multiplier;
            if value & 128 == 0 {
                break;
            }
            multiplier *= 128;
        }
        let mut payload = vec![0u8; size];
        reader.read_exact(&mut payload).await.unwrap();
        (header, payload)
    }

    #[tokio::test]
    async fn supervisor_consumes_configuration_changes_before_starting_a_session() {
        let (acceptor, ca_pem) = tls_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (online_tx, online_rx) = oneshot::channel();
        let broker = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(tcp).await.unwrap();
            let mut online_tx = Some(online_tx);
            loop {
                let (header, payload) = mqtt_packet(&mut stream).await;
                match header >> 4 {
                    1 => stream.write_all(&[0x20, 2, 0, 0]).await.unwrap(),
                    8 => stream
                        .write_all(&[0x90, 3, payload[0], payload[1], 1])
                        .await
                        .unwrap(),
                    3 => {
                        let length = usize::from(u16::from_be_bytes([payload[0], payload[1]]));
                        let mut offset = length + 2;
                        if (header >> 1) & 3 == 1 {
                            stream
                                .write_all(&[0x40, 2, payload[offset], payload[offset + 1]])
                                .await
                                .unwrap();
                            offset += 2;
                        }
                        if let Ok(value) = serde_json::from_slice::<Value>(&payload[offset..])
                            && value["online"] == true
                            && let Some(sender) = online_tx.take()
                        {
                            let _ = sender.send(());
                        }
                    }
                    12 => stream.write_all(&[0xd0, 0]).await.unwrap(),
                    _ => {}
                }
            }
        });
        let (config_tx, config_rx) = watch::channel(Some(Config::default()));
        let (_state_tx, state_rx) = watch::channel(Snapshot {
            ts: now(),
            datad: Default::default(),
            fields: Default::default(),
        });
        let status = Arc::new(Mutex::new(Status::default()));
        let task = tokio::spawn(supervisor(config_rx, state_rx, status.clone(), None));
        tokio::task::yield_now().await;
        config_tx
            .send(Some(Config {
                enabled: true,
                broker: format!("ssl://{address}"),
                ca_pem,
                username: "fixture".into(),
                password: "secret".into(),
                identity: "fixture-device".into(),
                ..Default::default()
            }))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), online_rx)
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(status.lock().unwrap().state, "connected");
        task.abort();
        broker.abort();
    }

    #[tokio::test]
    async fn mqtt_tls_reports_selected_state_and_reconfigures() {
        let (acceptor, ca_pem) = tls_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (reports_tx, mut reports_rx) = mpsc::channel::<Value>(16);
        let broker = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(tcp).await.unwrap();
            loop {
                let (header, payload) = mqtt_packet(&mut stream).await;
                match header >> 4 {
                    1 => stream.write_all(&[0x20, 2, 0, 0]).await.unwrap(),
                    8 => {
                        stream
                            .write_all(&[0x90, 3, payload[0], payload[1], 1])
                            .await
                            .unwrap();
                    }
                    3 => {
                        let topic_len = usize::from(u16::from_be_bytes([payload[0], payload[1]]));
                        let mut offset = 2 + topic_len;
                        if (header >> 1) & 3 == 1 {
                            stream
                                .write_all(&[0x40, 2, payload[offset], payload[offset + 1]])
                                .await
                                .unwrap();
                            offset += 2;
                        }
                        if let Ok(value) = serde_json::from_slice(&payload[offset..]) {
                            reports_tx.send(value).await.unwrap();
                        }
                    }
                    12 => stream.write_all(&[0xd0, 0]).await.unwrap(),
                    14 => return,
                    _ => {}
                }
            }
        });
        let config = Config {
            enabled: true,
            broker: format!("ssl://{address}"),
            username: "fixture".into(),
            password: "secret".into(),
            ca_pem,
            vendor: "ZTE".into(),
            model: "MU5252".into(),
            identity: "fixture-device".into(),
            report_interval_seconds: 3600,
            ..Default::default()
        };
        let mut fields = serde_json::Map::new();
        fields.insert(
            "system".into(),
            json!({"sw_version":"test","cpu_usage":23,"mem_used_pct":42}),
        );
        fields.insert("thermal".into(), json!({"cpu_celsius":51}));
        fields.insert("password".into(), json!("do-not-upload"));
        let snapshot = Snapshot {
            ts: now(),
            datad: Default::default(),
            fields,
        };
        let (_state_tx, state_rx) = watch::channel(snapshot);
        let (config_tx, config_rx) = watch::channel(Some(config.clone()));
        let status = Arc::new(Mutex::new(Status::default()));
        let session_task = tokio::spawn({
            let status = status.clone();
            async move { session(&config, state_rx, config_rx, status, None).await }
        });
        let mut found = HashSet::new();
        while found.len() < 3 {
            let report = tokio::time::timeout(Duration::from_secs(8), reports_rx.recv())
                .await
                .unwrap()
                .unwrap();
            let encoded = report.to_string();
            assert!(!encoded.contains("do-not-upload"));
            assert_eq!(report["protocol_version"], 1);
            if report["model"] == "MU5252" {
                assert_eq!(report["firmware_version"], "test");
                found.insert("device");
            }
            if report["online"] == true {
                found.insert("online");
            }
            if report.get("boot_id").is_some() {
                assert_eq!(report["cpu"]["usage_percent"].as_f64(), Some(23.0));
                assert_eq!(report["memory"]["usage_percent"].as_f64(), Some(42.0));
                assert_eq!(report["temperature"]["cpu_celsius"].as_f64(), Some(51.0));
                found.insert("system");
            }
        }
        // Long telemetry intervals must still emit an independent presence heartbeat.
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let report = reports_rx.recv().await.unwrap();
                assert!(
                    report.get("model").is_none(),
                    "unexpected full telemetry report"
                );
                if report["online"] == true {
                    break;
                }
            }
        })
        .await
        .unwrap();
        config_tx.send(Some(Config::default())).unwrap();
        let offline = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let value = reports_rx.recv().await.unwrap();
                if value["online"] == false {
                    return value;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(offline["protocol_version"], 1);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(5), session_task)
                .await
                .unwrap()
                .unwrap(),
            SessionEnd::Reconfigure
        ));
        broker.abort();
    }

    #[tokio::test]
    async fn closed_remote_sessions_stop_but_gateway_failures_retry() {
        for code in [401, 403, 404, 410, 502] {
            let (acceptor, ca_pem) = tls_fixture();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut stream = acceptor.accept(tcp).await.unwrap();
                let mut request = [0u8; 4096];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                stream.write_all(format!("HTTP/1.1 {code} Rejected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            });
            let config = Config {
                ca_pem,
                ..Default::default()
            };
            let command = RemoteCommand {
                protocol_version: 1,
                request_id: "closed-session".into(),
                action: "remote.open".into(),
                remote_url: format!("wss://{address}/api/remote/device/closed-session"),
                token: "a".repeat(64),
                target_service: "router_web".into(),
                target_port: 2333,
                target_ports: vec![],
                ttl_seconds: 30,
            };
            let (_tx, rx) = watch::channel(false);
            let stop =
                tokio::time::timeout(Duration::from_secs(5), bridge_pipe(&config, &command, rx))
                    .await
                    .unwrap();
            assert_eq!(stop, code != 502, "HTTP {code}");
            server.await.unwrap();
        }
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)]
    async fn websocket_bridge_roundtrip_and_close() {
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        let echo_task = tokio::spawn(async move {
            let (mut stream, _) = echo.accept().await.unwrap();
            let mut buffer = [0u8; 1024];
            loop {
                let size = stream.read(&mut buffer).await.unwrap();
                if size == 0 {
                    return;
                }
                stream.write_all(&buffer[..size]).await.unwrap();
            }
        });
        let (acceptor, ca_pem) = tls_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (roundtrip_tx, roundtrip_rx) = oneshot::channel();
        let websocket_server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let tls = acceptor.accept(tcp).await.unwrap();
            let mut socket = accept_hdr_async(
                tls,
                |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                    assert_eq!(
                        request.headers()["authorization"],
                        format!("Bearer {}", "a".repeat(64))
                    );
                    assert_eq!(
                        request.headers()["x-nms-target-port"],
                        echo_port.to_string()
                    );
                    Ok(response)
                },
            )
            .await
            .unwrap();
            socket
                .send(Message::Binary(b"ufi-test".to_vec().into()))
                .await
                .unwrap();
            let message = socket.next().await.unwrap().unwrap();
            roundtrip_tx.send(message.into_data()).unwrap();
        });
        let config = Config {
            enabled: true,
            remote_enabled: true,
            platform_url: format!("https://{address}"),
            ca_pem,
            services: vec![Service {
                name: "fixture".into(),
                port: echo_port,
                kind: "web".into(),
            }],
            ..Default::default()
        };
        let command = RemoteCommand {
            protocol_version: 1,
            request_id: "session-123".into(),
            action: "remote.open".into(),
            remote_url: format!("wss://{address}/api/remote/device/session-123"),
            token: "a".repeat(64),
            target_service: "router_web".into(),
            target_port: echo_port,
            target_ports: vec![],
            ttl_seconds: 30,
        };
        validate_remote(&config, &command).unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let pipe = tokio::spawn(async move { bridge_pipe(&config, &command, shutdown_rx).await });
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(7), roundtrip_rx)
                .await
                .unwrap()
                .unwrap(),
            b"ufi-test".as_slice()
        );
        websocket_server.await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), pipe)
            .await
            .unwrap()
            .unwrap();
        echo_task.abort();
    }
}
