use crate::{
    auth::{self, Sessions},
    cloud::{Cloud, Update as CloudUpdate},
    model::{DatadVersion, Snapshot, UbusCall},
    neighbor_manager::Manager as NeighborManager,
    ota::{self, Config as OtaConfig, Ota},
    state,
};
use anyhow::Result;
use axum::{
    Json, Router,
    extract::rejection::JsonRejection,
    extract::{ConnectInfo, Request},
    extract::{Query, State},
    http::{HeaderMap, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{get, post},
};
use futures_util::Stream;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    convert::Infallible,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Mutex, RwLock, watch},
};
use tokio_stream::{StreamExt, wrappers::WatchStream};
use tower_http::limit::RequestBodyLimitLayer;

#[derive(Clone)]
pub struct App {
    inner: Arc<Inner>,
}
struct Inner {
    snapshot: RwLock<Snapshot>,
    tx: watch::Sender<Snapshot>,
    interval_ms: AtomicU64,
    _data_dir: PathBuf,
    token: Option<String>,
    sessions: Mutex<Sessions>,
    cloud: RwLock<Cloud>,
    ota: Mutex<Ota>,
    neighbor: Mutex<NeighborManager>,
}

impl App {
    pub async fn new(
        data_dir: PathBuf,
        interval: Duration,
        token: Option<String>,
        neighbor_enabled: bool,
    ) -> Result<Self> {
        let mut initial = state::collect(interval.as_millis() as u64).await;
        let mut neighbor = NeighborManager::new(neighbor_enabled);
        neighbor
            .tick(initial.fields.get("net").unwrap_or(&Value::Null))
            .await;
        initial.fields.insert("neighbor".into(), neighbor.status());
        let (tx, _) = watch::channel(initial.clone());
        let app = Self {
            inner: Arc::new(Inner {
                snapshot: RwLock::new(initial),
                tx,
                interval_ms: AtomicU64::new(interval.as_millis() as u64),
                cloud: RwLock::new(Cloud::load(&data_dir)),
                ota: Mutex::new(Ota::load(&data_dir).map_err(anyhow::Error::msg)?),
                neighbor: Mutex::new(neighbor),
                _data_dir: data_dir,
                token,
                sessions: Mutex::new(Sessions::default()),
            }),
        };
        app.spawn_sampler();
        app.spawn_ota();
        Ok(app)
    }
    pub async fn snapshot(&self) -> Snapshot {
        self.inner.snapshot.read().await.clone()
    }
    fn spawn_sampler(&self) {
        let app = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(
                    app.inner.interval_ms.load(Ordering::Relaxed),
                ))
                .await;
                app.refresh_snapshot().await;
            }
        });
    }
    fn spawn_ota(&self) {
        if std::env::var("ZWRT_DATAD_OTA_DISABLE_AUTO").as_deref() == Ok("1") {
            return;
        }
        let app = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(90)).await;
            loop {
                let snapshot = serde_json::to_value(app.snapshot().await).unwrap_or(Value::Null);
                let mut manager = app.inner.ota.lock().await;
                manager.reconcile_install_result();
                if manager.should_auto_check() && manager.begin_update().is_ok() {
                    manager.mark_auto_check();
                    let _ = manager.check().await;
                    manager.finish_update();
                }
                if let Some(candidate) = manager.auto_candidate() {
                    if manager.begin_update().is_ok() {
                        if let Err(error) = manager.install(&candidate, &snapshot, false).await {
                            if !error.starts_with("等待安装条件:") {
                                manager.fail(Some(&candidate), error);
                            }
                        }
                        manager.finish_update();
                    }
                }
                drop(manager);
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
    }
    async fn refresh_snapshot(&self) {
        let mut next = state::collect(self.inner.interval_ms.load(Ordering::Relaxed)).await;
        let mut neighbor = self.inner.neighbor.lock().await;
        neighbor
            .tick(next.fields.get("net").unwrap_or(&Value::Null))
            .await;
        next.fields.insert("neighbor".into(), neighbor.status());
        drop(neighbor);
        let mut old = self.inner.snapshot.write().await;
        let mut comparable = next.clone();
        comparable.ts = old.ts;
        if comparable != *old {
            *old = next.clone();
            let _ = self.inner.tx.send(next);
        } else {
            old.ts = next.ts;
        }
    }
    pub async fn serve(
        self,
        addr: SocketAddr,
        require_auth: bool,
        open_auth_routes: bool,
    ) -> Result<()> {
        let mut router = Router::new()
            .route("/", get(index))
            .route("/healthz", get(health))
            .route("/version", get(version))
            .route("/state", get(snapshot))
            .route("/events", get(events))
            .route("/capabilities", get(capabilities))
            .route("/ubus", get(ubus_list))
            .route("/ubus/list", get(ubus_list))
            .route("/ubus/call", post(ubus_call))
            .route("/control", post(control));
        if !require_auth {
            router = router
                .route(
                    "/cloud/config",
                    get(cloud_config_get).post(cloud_config_post),
                )
                .route("/cloud/status", get(cloud_status));
            router = router
                .route("/ota/config", get(ota_config_get).post(ota_config_post))
                .route("/ota/status", get(ota_status))
                .route("/ota/check", post(ota_check))
                .route("/ota/update", post(ota_update));
        }
        if open_auth_routes {
            router = router
                .route("/auth/login", post(auth_login))
                .route("/auth/exchange", post(auth_exchange));
        }
        if require_auth {
            router = router.layer(middleware::from_fn_with_state(self.clone(), authenticate));
        }
        let router = router
            .layer(RequestBodyLimitLayer::new(1024 * 1024))
            .with_state(self.clone());
        let listener = TcpListener::bind(addr).await?;
        let result = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown())
        .await;
        self.inner.neighbor.lock().await.shutdown().await;
        result?;
        Ok(())
    }
}

async fn authenticate(State(app): State<App>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if matches!(path, "/" | "/healthz" | "/auth/login" | "/auth/exchange")
        || app.inner.token.is_none()
    {
        return next.run(request).await;
    }
    let wanted = app.inner.token.as_deref().unwrap_or_default();
    let headers = request.headers();
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let legacy = headers.get("x-auth-token").and_then(|v| v.to_str().ok());
    let query = request.uri().query().and_then(|q| {
        q.split('&')
            .find_map(|part| part.strip_prefix("access_token="))
    });
    let presented = [bearer, legacy, query].into_iter().flatten().next();
    let static_valid =
        presented.is_some_and(|value| constant_time_eq(value.as_bytes(), wanted.as_bytes()));
    let session_valid = if static_valid {
        false
    } else if let Some(value) = presented {
        app.inner.sessions.lock().await.validate(value)
    } else {
        false
    };
    if static_valid || session_valid {
        next.run(request).await
    } else {
        (StatusCode::UNAUTHORIZED, Json(json!({"ok":false,"error":{"code":"unauthorized","message":"authentication required"}}))).into_response()
    }
}

async fn auth_login(State(app): State<App>, headers: HeaderMap) -> Response {
    let Some((username, password)) = basic_credentials(&headers) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok":false,"error":"missing_credentials"})),
        )
            .into_response();
    };
    if !auth::verify_password(&username, &password).await {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"ok":false,"error":"invalid_credentials"})),
        )
            .into_response();
    }
    match app.inner.sessions.lock().await.issue() {
        Ok((token, expires_at)) => {
            (StatusCode::OK, Json(auth::token_reply(token, expires_at))).into_response()
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok":false,"error":"token_issue_failed"})),
        )
            .into_response(),
    }
}

async fn auth_exchange(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let token = headers
        .get("x-web-token")
        .or_else(|| headers.get("x-zte-webtoken"))
        .and_then(|value| value.to_str().ok());
    let Some(token) = token.filter(|value| !value.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok":false,"error":"missing_webtoken"})),
        )
            .into_response();
    };
    let mode = headers
        .get("x-z-mode")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or_default();
    let tag = headers
        .get("x-z-tag")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("zwrt-datad");
    if !auth::verify_webtoken(token, mode, &peer.ip().to_string(), tag).await {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"ok":false,"error":"invalid_webtoken"})),
        )
            .into_response();
    }
    match app.inner.sessions.lock().await.issue() {
        Ok((token, expires_at)) => {
            (StatusCode::OK, Json(auth::token_reply(token, expires_at))).into_response()
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok":false,"error":"token_issue_failed"})),
        )
            .into_response(),
    }
}

fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let encoded = headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Basic ")?;
    if encoded.len() > 1024 {
        return None;
    }
    let decoded = decode_base64(encoded)?;
    if decoded.len() > 512 || decoded.contains(&0) {
        return None;
    }
    let separator = decoded.iter().position(|b| *b == b':')?;
    if separator == 0 {
        return None;
    }
    let username = String::from_utf8(decoded[..separator].to_vec()).ok()?;
    let password = String::from_utf8(decoded[separator + 1..].to_vec()).ok()?;
    if username.len() >= 257 || password.len() >= 257 {
        return None;
    }
    Some((username, password))
}

fn decode_base64(value: &str) -> Option<Vec<u8>> {
    let mut acc = 0u32;
    let mut bits = 0u8;
    let mut out = Vec::new();
    for byte in value.bytes().filter(|b| !b.is_ascii_whitespace()) {
        if byte == b'=' {
            break;
        }
        let digit = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(digit);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
            acc &= if bits == 0 { 0 } else { (1 << bits) - 1 };
        }
    }
    Some(out)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

async fn index() -> &'static str {
    "zwrt-datad Rust rewrite\n"
}
async fn health() -> &'static str {
    "ok\n"
}
async fn version() -> Json<DatadVersion> {
    Json(Default::default())
}
async fn snapshot(State(app): State<App>) -> Json<Snapshot> {
    Json(app.snapshot().await)
}
async fn capabilities() -> Json<Value> {
    Json(json!({
        "protocol":1,
        "events":["state"],
        "controls":[
            "device.login_info",
            "device.session_status",
            "wifi.status",
            "wifi.dual_band_status",
            "wifi.txpower.status",
            "sleep.status",
            "usb.status",
            "power.direct_supply.status",
            "apn.list",
            "client.access",
            "neighbor.status",
            "neighbor.set",
            "state.refresh",
            "state.set_interval",
            "qos.reload"
        ],
        "rewrite":"rust"
    }))
}

async fn events(State(app): State<App>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = WatchStream::new(app.inner.tx.subscribe())
        .map(|v| Ok(Event::default().event("state").json_data(v).unwrap()));
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::new())
}
#[derive(Deserialize)]
struct ListQuery {
    verbose: Option<u8>,
}
async fn ubus_list(Query(q): Query<ListQuery>) -> Response {
    result(state::ubus_list(q.verbose == Some(1)).await)
}
async fn ubus_call(Json(req): Json<UbusCall>) -> Response {
    let service = req.service.clone();
    let method = req.method.clone();
    match state::ubus(&req.service, &req.method, req.args).await {
        Ok(value) => (
            StatusCode::OK,
            Json(json!({"ok":true,"service":service,"method":method,"result":value})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"ok":false,"error":{"code":"device_call_failed","message":e}})),
        )
            .into_response(),
    }
}
async fn control(
    State(app): State<App>,
    method: Method,
    payload: Result<Json<Value>, JsonRejection>,
) -> Response {
    let _ = method;
    let Json(body) = match payload {
        Ok(body) => body,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok":false,"error":{"code":"invalid_request","message":"request body must be valid JSON"}})),
            )
                .into_response();
        }
    };
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if action.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok":false,"error":{"code":"invalid_request","message":"missing action"}})),
        )
            .into_response();
    }
    if body.get("params").is_some_and(|params| !params.is_object()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok":false,"action":action,"error":{"code":"invalid_request","message":"params must be a JSON object"}})),
        )
            .into_response();
    }
    if action == "neighbor.status" {
        return (StatusCode::OK,Json(json!({"ok":true,"action":action,"result":app.inner.neighbor.lock().await.status()}))).into_response();
    }
    if action == "neighbor.set" {
        let enabled = body
            .get("params")
            .and_then(|v| v.get("enabled"))
            .and_then(Value::as_bool);
        let Some(enabled) = enabled else {
            return (StatusCode::BAD_REQUEST,Json(json!({"ok":false,"action":action,"error":{"code":"invalid_parameter","message":"enabled must be boolean"}}))).into_response();
        };
        return match app.inner.neighbor.lock().await.set_enabled(enabled).await {Ok(value)=>(StatusCode::OK,Json(json!({"ok":true,"action":action,"result":value}))).into_response(),Err(error)=>(StatusCode::BAD_GATEWAY,Json(json!({"ok":false,"action":action,"error":{"code":"device_call_failed","message":error}}))).into_response()};
    }
    if action == "device.login_info" {
        return readonly_ubus(action, "zwrt_web", "web_login_info", json!({})).await;
    }
    if action == "device.session_status" {
        return control_ok(action, json!({"logged_in":false}));
    }
    if action == "wifi.dual_band_status" {
        return match state::ubus("zwrt_router.api", "router_get_wifi_isolate", json!({})).await {
            Ok(value) => {
                let enabled = value
                    .get("wifimain24_wifimain5_enable")
                    .and_then(Value::as_i64)
                    .unwrap_or_default()
                    != 0;
                control_ok(
                    action,
                    json!({
                        "WiFiDualBandSupported":"1",
                        "WiFiDualBandEnabled":if enabled { "1" } else { "0" },
                        "BandSteeringSwitch":if enabled { "1" } else { "0" }
                    }),
                )
            }
            Err(error) => control_failed(action, error),
        };
    }
    if action == "wifi.status" {
        let mut result = serde_json::Map::new();
        for section in ["main_2g", "main_5g"] {
            let mut item = serde_json::Map::new();
            for field in ["ssid", "key", "encryption", "disabled"] {
                item.insert(
                    field.into(),
                    json!(state::uci_read(&format!("wireless.{section}.{field}")).await),
                );
            }
            result.insert(section.into(), Value::Object(item));
        }
        return control_ok(action, Value::Object(result));
    }
    if action == "wifi.txpower.status" {
        let model = state::uci_read("zwrt_common_info.common_config.model_name").await;
        let hardware = state::uci_read("zwrt_common_info.common_config.hardware_version").await;
        if model != "MU5252" && !hardware.starts_with("MU5252_") {
            return (StatusCode::BAD_REQUEST,Json(json!({"ok":false,"action":action,"error":{"code":"invalid_parameter","message":"wifi power control is only supported on MU5252"}}))).into_response();
        }
        let mut result = Map::new();
        for (band, section, factory_limit) in [("2g", "wifi0", 19), ("5g", "wifi1", 18)] {
            let mut values = Vec::new();
            for option in ["disabled", "txpowerpercent", "txpower", "max_power"] {
                let raw = state::uci_read(&format!("wireless.{section}.{option}")).await;
                let Ok(value) = raw.parse::<i64>() else {
                    return control_failed(
                        action,
                        format!("failed to read {band} wifi power configuration"),
                    );
                };
                values.push(value);
            }
            result.insert(band.into(), json!({"enabled":values[0]==0,"percent":values[1],"txpower_dbm":values[2],"limit_dbm":values[3],"factory_limit_dbm":factory_limit}));
        }
        return control_ok(action, Value::Object(result));
    }
    if action == "sleep.status" {
        return control_ok(
            action,
            json!({
                "idle_seconds":state::uci_read("zwrt_sleep.ztmp_time.SysIdTime").await,
                "enabled":state::uci_read("zwrt_sleep.ztmp_switch.sleepSwitch").await,
                "wakeup":state::uci_read("zwrt_sleep.ztmp_switch.wakeupSwitch").await,
                "status":state::uci_read("zwrt_sleep.ztmp_status.sleepStatus").await,
            }),
        );
    }
    if action == "usb.status" {
        let typec = state::ubus("zwrt_bsp.typec", "list", json!({})).await;
        let usb = state::ubus("zwrt_bsp.usb", "list", json!({})).await;
        return match (typec, usb) {
            (Ok(typec), Ok(usb)) => control_ok(action, json!({"typec":typec,"usb":usb})),
            (Err(error), _) | (_, Err(error)) => control_failed(action, error),
        };
    }
    if action == "power.direct_supply.status" {
        return match state::ubus("zwrt_bsp.charger", "list", json!({})).await {
            Ok(value) => {
                let result = match value
                    .get("direct_power_supply_mode")
                    .and_then(Value::as_str)
                {
                    Some("enable") => json!({"supported":true,"enabled":true,"mode":"enable"}),
                    Some("disable") => json!({"supported":true,"enabled":false,"mode":"disable"}),
                    Some(_) => json!({"supported":true,"enabled":Value::Null,"mode":Value::Null}),
                    None => json!({"supported":false,"enabled":Value::Null,"mode":Value::Null}),
                };
                control_ok(action, result)
            }
            Err(error) => control_failed(action, error),
        };
    }
    if action == "apn.list" {
        let values = tokio::join!(
            state::ubus("zwrt_apn_object", "get_apn_mode", json!({})),
            state::ubus("zwrt_apn_object", "getAutoApnList", json!({})),
            state::ubus("zwrt_apn_object", "getManuApnList", json!({})),
            state::ubus("zwrt_apn_object", "get_enabled_manu_apn_id", json!({}))
        );
        return match values {
            (Ok(mode), Ok(automatic), Ok(manual), Ok(enabled)) => control_ok(
                action,
                json!({"mode":mode,"automatic":automatic,"manual":manual,"enabled":enabled}),
            ),
            (Err(error), _, _, _)
            | (_, Err(error), _, _)
            | (_, _, Err(error), _)
            | (_, _, _, Err(error)) => control_failed(action, error),
        };
    }
    if action == "client.access" {
        let values = tokio::join!(
            state::ubus(
                "uci",
                "get",
                json!({"config":"wireless","section":"main_2g"})
            ),
            state::ubus(
                "zwrt_router.api",
                "router_lan_access_list",
                json!({"start_id":1,"end_id":64})
            ),
            state::ubus(
                "zwrt_router.api",
                "router_wireless_access_list",
                json!({"start_id":1,"end_id":64})
            )
        );
        return match values {
            (Ok(policy), Ok(lan), Ok(wifi)) => {
                control_ok(action, json!({"policy":policy,"lan":lan,"wifi":wifi}))
            }
            (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                control_failed(action, error)
            }
        };
    }
    if action == "state.refresh" {
        let refresh = app.clone();
        tokio::spawn(async move { refresh.refresh_snapshot().await });
        return control_ok(action, json!({"queued":true}));
    }
    if action == "state.set_interval" {
        let milliseconds = body
            .get("params")
            .and_then(|value| value.get("milliseconds"))
            .and_then(Value::as_u64);
        let Some(milliseconds) = milliseconds.filter(|value| (500..=5000).contains(value)) else {
            return (StatusCode::BAD_REQUEST,Json(json!({"ok":false,"action":action,"error":{"code":"invalid_parameter","message":"milliseconds must be between 500 and 5000"}}))).into_response();
        };
        app.inner.interval_ms.store(milliseconds, Ordering::Relaxed);
        let refresh = app.clone();
        tokio::spawn(async move { refresh.refresh_snapshot().await });
        return control_ok(action, json!({"sample_interval_ms":milliseconds}));
    }
    if action == "qos.reload" {
        let refresh = app.clone();
        tokio::spawn(async move { refresh.refresh_snapshot().await });
        return control_ok(action, json!({"queued":true}));
    }
    (StatusCode::NOT_IMPLEMENTED, Json(json!({"ok":false,"action":body.get("action"),"error":{"code":"rust_port_in_progress","message":"control adapter has not been migrated"}}))).into_response()
}

fn control_ok(action: &str, value: Value) -> Response {
    (
        StatusCode::OK,
        Json(json!({"ok":true,"action":action,"result":value})),
    )
        .into_response()
}

fn control_failed(action: &str, error: String) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({"ok":false,"action":action,"error":{"code":"device_call_failed","message":error}})),
    )
        .into_response()
}

async fn readonly_ubus(action: &str, service: &str, method: &str, args: Value) -> Response {
    match state::ubus(service, method, args).await {
        Ok(value) => control_ok(action, value),
        Err(error) => control_failed(action, error),
    }
}
async fn cloud_config_get(State(app): State<App>) -> Json<Value> {
    Json(app.inner.cloud.read().await.public_config())
}
async fn cloud_status(State(app): State<App>) -> Json<Value> {
    Json(app.inner.cloud.read().await.status())
}
async fn cloud_config_post(State(app): State<App>, Json(update): Json<CloudUpdate>) -> Response {
    match app.inner.cloud.write().await.update(update) {
        Ok(value) => (StatusCode::OK, Json(value)).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response(),
    }
}
async fn ota_config_get(State(app): State<App>) -> Json<Value> {
    Json(app.inner.ota.lock().await.config_json())
}
async fn ota_status(State(app): State<App>) -> Json<Value> {
    Json(app.inner.ota.lock().await.status_json())
}
async fn ota_config_post(
    State(app): State<App>,
    payload: Result<Json<OtaConfig>, JsonRejection>,
) -> Response {
    let Json(config) = match payload {
        Ok(value) => value,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"success":false,"error":"配置格式无效"})),
            )
                .into_response();
        }
    };
    match app.inner.ota.lock().await.update_config(config) {
        Ok(value) => (StatusCode::OK, Json(value)).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"success":false,"error":error})),
        )
            .into_response(),
    }
}
async fn ota_check(State(app): State<App>) -> Response {
    let mut manager = app.inner.ota.lock().await;
    match manager.check().await {
        Ok(candidate) => (
            StatusCode::OK,
            Json(json!({"success":true,"has_update":ota::has_update(&candidate),"manifest":candidate.manifest,"source":candidate.base_url})),
        )
            .into_response(),
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"success":false,"error":error})),
        )
            .into_response(),
    }
}
async fn ota_update(State(app): State<App>) -> Response {
    {
        let mut manager = app.inner.ota.lock().await;
        if let Err(error) = manager.begin_update() {
            return (
                StatusCode::CONFLICT,
                Json(json!({"success":false,"error":error})),
            )
                .into_response();
        }
    }
    let task = app.clone();
    tokio::spawn(async move {
        let snapshot = serde_json::to_value(task.snapshot().await).unwrap_or(Value::Null);
        let mut manager = task.inner.ota.lock().await;
        match manager.check().await {
            Ok(candidate) if !ota::has_update(&candidate) => {
                manager.fail(Some(&candidate), "当前已是最新版本".into());
            }
            Ok(candidate) => match manager.install(&candidate, &snapshot, true).await {
                Ok(()) => {}
                Err(error) => {
                    manager.fail(Some(&candidate), error);
                }
            },
            Err(error) => manager.fail(None, error),
        }
        manager.finish_update();
    });
    (
        StatusCode::ACCEPTED,
        Json(json!({"success":true,"status":"started"})),
    )
        .into_response()
}
fn result(value: Result<Value, String>) -> Response {
    match value {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"ok":false,"error":e}))).into_response(),
    }
}
async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn version_shape() {
        let v = serde_json::to_value(DatadVersion::default()).unwrap();
        assert_eq!(v["name"], "zwrt-datad");
    }
}
