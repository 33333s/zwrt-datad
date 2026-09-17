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
    let id_hash = Sha256::digest(root(config).as_bytes());
    let client_id = format!("datad-{}", hex(&id_hash[..12]));
    let Ok(mut options) = mqtt_options(config, &client_id) else {
        return SessionEnd::Failed;
    };
    options.set_credentials(config.username.clone(), config.password.clone());
    options.set_keep_alive(30);
    options.set_clean_session(true);
    options.set_last_will(LastWill::new(
        format!("{}/status", root(config)),
        envelope(json!({"online":false})).to_string(),
        QoS::AtLeastOnce,
        false,
    ));
    let (client, mut eventloop) = AsyncClient::builder(options).capacity(32).build();
    if client
        .subscribe(
            format!("{}/command/request", root(config)),
            QoS::AtLeastOnce,
        )
        .await
        .is_err()
    {
        return SessionEnd::Failed;
    }
    let bridge = BridgeManager::new(config.clone());
    let mut updates = JoinSet::new();
    let mut connected = false;
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
                if connected {
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
                    ticker.reset();
                    heartbeat.reset();
                    set_status(&status, "connected", "");
                    let snapshot = state_rx.borrow().clone();
                    if let Some(app) = &app
                        && let Some(result) = app.cloud_update_result().await {
                            let _ = publish(&client, format!("{}/command/result", root(config)), result).await;
                    }
                    if report(&client, config, &snapshot, &status).await.is_err() {
                        bridge.shutdown(); updates.detach_all();
                        return SessionEnd::Failed;
                    }
                }
                Ok(Event::Incoming(Incoming::Publish(message))) if connected => {
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
                if publish(&client, format!("{}/status", root(config)), json!({"online":true})).await.is_err() {
                    bridge.shutdown(); updates.detach_all();
                    return SessionEnd::Failed;
                }
            },
            _ = ticker.tick(), if connected => {
                if let Some(app) = &app
                    && let Some(result) = app.cloud_update_result().await {
                        let _ = publish(&client, format!("{}/command/result", root(config)), result).await;
                }
                let snapshot = state_rx.borrow_and_update().clone();
                if report(&client, config, &snapshot, &status).await.is_err() {
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
) -> Result<(), String> {
    let state = serde_json::to_value(snapshot).unwrap_or(Value::Null);
    let firmware = state
        .get("system")
        .and_then(|value| value.get("sw_version").or_else(|| value.get("fw")))
        .and_then(Value::as_str)
        .unwrap_or_default();
    publish(client, format!("{}/telemetry/device", root(config)), json!({
        "vendor":config.vendor,"model":config.model,"device_id":config.identity,
        "id_type":config.identity_type,"platform":config.platform,
        "agent_version":env!("DATAD_VERSION"),"firmware_version":firmware,
        "capabilities":["datad.update","datad.remote"],"remote_services":config.services,"remote_enabled":config.remote_enabled
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
    let addresses = |family: &str| -> Vec<Value> {
        state
            .pointer(&format!("/net/interfaces/{family}/{family}"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.get("address").and_then(Value::as_str))
            .filter(|address| address.parse::<std::net::IpAddr>().is_ok())
            .map(|address| json!(address))
            .collect()
    };
    publish(
        client,
        format!("{}/telemetry/network", root(config)),
        json!({"upstream":{"ipv4":addresses("ipv4"),"ipv6":addresses("ipv6")}}),
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
    seen: HashMap<String, i64>,
}

#[derive(Clone)]
struct BridgeManager {
    config: Config,
    state: Arc<AsyncMutex<BridgeState>>,
    shutdown: watch::Sender<bool>,
}

impl BridgeManager {
    fn new(config: Config) -> Self {
        let (shutdown, _) = watch::channel(false);
        Self {
            config,
            state: Arc::new(AsyncMutex::new(BridgeState::default())),
            shutdown,
        }
    }

    fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    async fn receive(&self, command: RemoteCommand) -> Option<Value> {
        let reject = |code: String| json!({"request_id":command.request_id,"status":"rejected","error":{"code":code}});
        if let Err(error) = validate_remote(&self.config, &command) {
            return Some(reject(error));
        }
        let mut state = self.state.lock().await;
        state.seen.retain(|_, expires| *expires > now());
        if state.seen.contains_key(&command.request_id) {
            return None;
        }
        if state.active.len() >= 4 || state.seen.len() >= 128 {
            return Some(reject("session_limit".into()));
        }
        state.active.insert(command.request_id.clone());
        state
            .seen
            .insert(command.request_id.clone(), now() + 12 * 3600);
        drop(state);
        let manager = self.clone();
        tokio::spawn(async move { manager.run_bridge(command).await });
        None
    }

    async fn run_bridge(&self, command: RemoteCommand) {
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
    let remote = Url::parse(&command.remote_url).map_err(|_| "invalid_remote_url")?;
    let base = Url::parse(&config.platform_url).map_err(|_| "invalid_remote_url")?;
    if remote.scheme() != "wss"
        || remote.host_str() != base.host_str()
        || remote.port_or_known_default() != base.port_or_known_default()
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

fn websocket_tls(config: &Config) -> Result<Arc<ClientConfig>, String> {
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

fn topic(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_. -".contains(&byte))
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
    if !config.platform_url.is_empty() {
        let url = Url::parse(&config.platform_url).map_err(|_| "NMS 地址必须是 HTTPS 站点地址")?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || !matches!(url.path(), "" | "/")
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("NMS 地址必须是 HTTPS 站点地址".into());
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
