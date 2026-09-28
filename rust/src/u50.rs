//! Read-only, firmware-derived U50 candidate collector.
//!
//! This path deliberately does not expose ZWRT UBus/UCI or mutating routes.
//! The firmware proves names and code paths, not successful device responses.
use crate::{
    command,
    model::{DatadVersion, Snapshot},
    u50_oem::Bridge,
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    convert::Infallible,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{RwLock, Semaphore, watch};
use tokio_stream::{StreamExt, wrappers::WatchStream};
use tower_http::limit::RequestBodyLimitLayer;

const CMD: &str = "model_name,wa_inner_version,network_type,network_provider_fullname,network_provider,battery_value,battery_temp,battery_status,battery_vol_percent,battery_charging,signalbar,simcard_status,realtime_tx_thrpt,realtime_rx_thrpt,realtime_tx_bytes,realtime_rx_bytes,monthly_tx_bytes,monthly_rx_bytes,wifi_onoff_state,wifi_access_sta_num,modem_main_state,pin_status,simcard_active_slot,lte_rsrp,lte_rsrq,lte_snr";
const MAX_RESPONSE: usize = 64 * 1024;
const CFG_KEYS: &[&str] = &[
    "model_name",
    "integrate_version",
    "lan_ipaddr",
    "lan_netmask",
    "wan_ipaddr",
    "wan_gateway",
    "ppp_status",
    "network_type",
    "network_provider_fullname",
    "signalbar",
    "battery_vol_percent",
    "battery_temp",
    "battery_charging",
    "battery_value",
    "realtime_tx_thrpt",
    "realtime_rx_thrpt",
    "realtime_tx_bytes",
    "realtime_rx_bytes",
    "monthly_tx_bytes",
    "monthly_rx_bytes",
    "wifi_onoff_state",
    "wifi_access_sta_num",
    "modem_main_state",
    "pin_status",
    "simcard_active_slot",
    "lte_rsrp",
    "lte_rsrq",
    "lte_snr",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Model {
    U50Pro,
    U50S,
}
impl Model {
    pub fn parse(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "u50pro" => Ok(Self::U50Pro),
            "u50s" => Ok(Self::U50S),
            _ => anyhow::bail!("--u50-model must be u50pro or u50s"),
        }
    }
    fn template(self) -> &'static str {
        match self {
            Self::U50Pro => "U50PRO",
            Self::U50S => "U50S",
        }
    }
}

fn validate_goform_url(value: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value).context("invalid U50 GoAhead URL")?;
    ensure!(
        url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "localhost")),
        "U50 GoAhead URL must use loopback HTTP"
    );
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "U50 GoAhead URL must not contain credentials or query"
    );
    ensure!(
        url.path() == "/goform/goform_get_cmd_process",
        "invalid U50 GoAhead path"
    );
    Ok(url)
}

fn string_field<'a>(raw: &'a Value, key: &str) -> Option<&'a str> {
    raw.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
}
fn cfg_number(values: &BTreeMap<String, String>, key: &str, min: i64, max: i64) -> Option<i64> {
    values
        .get(key)?
        .parse::<i64>()
        .ok()
        .filter(|v| (min..=max).contains(v))
}

fn source_number(
    cfg: &BTreeMap<String, String>,
    goform: Option<&Value>,
    key: &str,
    min: i64,
    max: i64,
) -> Option<i64> {
    goform
        .and_then(|raw| raw.get(key))
        .and_then(|v| match v {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => s.parse().ok(),
            _ => None,
        })
        .filter(|v| (min..=max).contains(v))
        .or_else(|| cfg_number(cfg, key, min, max))
}

fn cfg_bin() -> String {
    std::env::var("ZWRT_DATAD_U50_CFG_BIN").unwrap_or_else(|_| "/usr/bin/cfg".into())
}

async fn cfg_values() -> Result<BTreeMap<String, String>> {
    let program = cfg_bin();
    let mut values = BTreeMap::new();
    for key in CFG_KEYS {
        let Ok(raw) = command::run(&program, ["get", key], Duration::from_secs(2)).await else {
            continue;
        };
        ensure!(raw.len() <= 512, "U50 cfg output too large for {key}");
        let value = String::from_utf8(raw).context("U50 cfg returned invalid UTF-8")?;
        let value = value.trim();
        if !value.is_empty() && !value.chars().any(char::is_control) {
            values.insert((*key).into(), value.into());
        }
    }
    ensure!(
        values.contains_key("model_name"),
        "U50 cfg model_name unavailable"
    );
    Ok(values)
}

fn from_sources(
    model: Model,
    cfg: &BTreeMap<String, String>,
    goform: Option<&Value>,
) -> Result<Snapshot> {
    let model_name = cfg
        .get("model_name")
        .context("U50 cfg model_name unavailable")?;
    let observed: String = model_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let other = match model {
        Model::U50Pro => "U50S",
        Model::U50S => "U50PRO",
    };
    ensure!(
        !observed.contains(other),
        "U50 cfg model_name conflicts with --u50-model"
    );
    let mut fields = Map::new();
    fields.insert(
        "device".into(),
        json!({
            "profile": model.template().to_ascii_lowercase(),
            "profile_source": "explicit_candidate",
            "api_template": model.template(),
            "api_template_label": model.template(),
            "api_template_supported": 0,
            "full_ubus": 0,
            "model_name": model_name
        }),
    );
    if let Some(fw) = cfg
        .get("integrate_version")
        .map(String::as_str)
        .or_else(|| goform.and_then(|raw| string_field(raw, "wa_inner_version")))
    {
        fields.insert("system".into(), json!({"sw_version":fw}));
    }
    let mut vendor = Map::new();
    for key in ["lan_ipaddr", "lan_netmask", "wan_ipaddr", "wan_gateway"] {
        if let Some(value) = cfg
            .get(key)
            .filter(|value| value.parse::<Ipv4Addr>().is_ok())
        {
            vendor.insert(key.into(), json!(value));
        }
    }
    if let Some(value) = cfg.get("ppp_status") {
        vendor.insert("ppp_status_raw".into(), json!(value));
    }
    if !vendor.is_empty() {
        fields.insert("u50_cfg".into(), Value::Object(vendor));
    }
    if let Some(ip) = cfg
        .get("lan_ipaddr")
        .filter(|value| value.parse::<IpAddr>().is_ok())
    {
        fields.insert("dhcp".into(), json!({"ip":ip}));
    }
    let mut net = Map::new();
    for (source, target) in [
        ("network_type", "type"),
        ("network_provider_fullname", "operator"),
    ] {
        if let Some(value) = cfg.get(source) {
            net.insert(target.into(), json!(value));
        }
    }
    if let Some(raw) = goform {
        for (source, target) in [
            ("network_type", "type"),
            ("network_provider_fullname", "operator"),
        ] {
            if let Some(value) = string_field(raw, source) {
                net.insert(target.into(), json!(value));
            }
        }
        if !net.contains_key("operator")
            && let Some(value) = string_field(raw, "network_provider")
        {
            net.insert("operator".into(), json!(value));
        }
        let mut candidate = Map::new();
        for key in [
            "signalbar",
            "battery_value",
            "battery_temp",
            "battery_status",
            "simcard_status",
        ] {
            if let Some(value) = raw.get(key).filter(|v| v.is_string() || v.is_number()) {
                candidate.insert(key.into(), value.clone());
            }
        }
        if !candidate.is_empty() {
            fields.insert("u50_unverified".into(), Value::Object(candidate));
        }
    }
    for (source, target, min, max) in [
        ("signalbar", "bars", 0, 5),
        ("lte_rsrp", "lte_rsrp", -160, -20),
        ("lte_rsrq", "lte_rsrq", -50, 0),
        ("lte_snr", "lte_snr", -30, 60),
    ] {
        if let Some(value) = source_number(cfg, goform, source, min, max) {
            net.insert(target.into(), json!(value));
        }
    }
    if let Some(value) = cfg.get("ppp_status") {
        net.insert("wan_status".into(), json!(value));
    }
    net.insert("HSR".into(), json!(false));
    fields.insert("net".into(), Value::Object(net));

    let mut battery = Map::new();
    if let Some(value) = source_number(cfg, goform, "battery_vol_percent", 0, 100) {
        battery.insert("percent".into(), json!(value));
    }
    if let Some(value) = source_number(cfg, goform, "battery_temp", -40, 120) {
        battery.insert("temp".into(), json!(value));
    }
    if let Some(value) = cfg.get("battery_charging") {
        battery.insert("charging_raw".into(), json!(value));
    }
    if !battery.is_empty() {
        fields.insert("battery".into(), Value::Object(battery));
    }

    let mut traffic = Map::new();
    for (source, target) in [
        ("realtime_rx_thrpt", "rx_speed"),
        ("realtime_tx_thrpt", "tx_speed"),
        ("realtime_rx_bytes", "rx_bytes"),
        ("realtime_tx_bytes", "tx_bytes"),
        ("monthly_rx_bytes", "month_rx_bytes"),
        ("monthly_tx_bytes", "month_tx_bytes"),
    ] {
        if let Some(value) = source_number(cfg, goform, source, 0, i64::MAX) {
            traffic.insert(target.into(), json!(value));
        }
    }
    if !traffic.is_empty() {
        fields.insert("traffic".into(), Value::Object(traffic));
    }

    if let Some(value) = cfg
        .get("wifi_onoff_state")
        .filter(|v| *v == "0" || *v == "1")
    {
        fields.insert("wlan".into(), json!({"enabled":i64::from(value == "1")}));
    }
    if let Some(value) = cfg_number(cfg, "wifi_access_sta_num", 0, 1024) {
        fields.insert("clients".into(), json!({"total":value,"wifi":value}));
    }
    let mut sim = Map::new();
    if let Some(value) = cfg.get("modem_main_state") {
        sim.insert("modem_state".into(), json!(value));
    }
    if let Some(value) = cfg.get("pin_status") {
        sim.insert("pin_status_raw".into(), json!(value));
    }
    if let Some(value) = cfg_number(cfg, "simcard_active_slot", 0, 4) {
        sim.insert("current_slot".into(), json!(value));
    }
    if !sim.is_empty() {
        fields.insert("sim".into(), Value::Object(sim));
    }
    fields.insert(
        "u50_sources".into(),
        json!({"cfg":"ok", "goform":if goform.is_some() {"ok"} else {"unavailable"}}),
    );
    Ok(Snapshot {
        ts: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
        datad: DatadVersion::default(),
        fields,
    })
}

#[derive(Clone)]
struct App {
    snapshot: Arc<RwLock<Snapshot>>,
    tx: watch::Sender<Snapshot>,
    sse_slots: Arc<Semaphore>,
    last_ok: Arc<RwLock<Instant>>,
    max_stale: Duration,
    oem: Option<Bridge>,
}
async fn health(State(app): State<App>) -> (StatusCode, &'static str) {
    if app.last_ok.read().await.elapsed() > app.max_stale {
        (StatusCode::SERVICE_UNAVAILABLE, "probe stale\n")
    } else {
        (StatusCode::OK, "ok\n")
    }
}
async fn version() -> Json<DatadVersion> {
    Json(Default::default())
}
async fn snapshot(State(app): State<App>) -> Json<Snapshot> {
    Json(app.snapshot.read().await.clone())
}
async fn capabilities(State(app): State<App>) -> Json<Value> {
    let controls: Vec<&str> = if app.oem.is_some() {
        vec!["u50.oem.goform"]
    } else {
        vec![]
    };
    Json(json!({
        "schema_version":1,
        "protocol":1,
        "events":["state"],
        "transport":["http","sse"],
        "control":controls,
        "controls":controls,
        "oem_actions":if app.oem.is_some() {crate::u50_oem_ids::IDS} else {&[]},
        "discovery":[],
        "passthrough":[],
        "template_status":"firmware_candidate"
    }))
}
#[derive(Deserialize)]
struct LoginRequest {
    password: String,
}
#[derive(Deserialize)]
struct ReadQuery {
    cmd: String,
}
#[derive(Deserialize)]
struct ControlRequest {
    action: String,
    goform_id: String,
    #[serde(default)]
    params: Map<String, Value>,
    #[serde(default)]
    confirm: bool,
}
fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}
fn api_error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"ok":false,"error":message}))).into_response()
}
async fn oem_login(State(app): State<App>, Json(body): Json<LoginRequest>) -> Response {
    let Some(oem) = app.oem.as_ref() else {
        return api_error(StatusCode::NOT_FOUND, "OEM writes disabled");
    };
    match oem.login(body.password).await {
        Ok(value) => Json(value).into_response(),
        Err(error) if error == "login rate limited" => {
            api_error(StatusCode::TOO_MANY_REQUESTS, &error)
        }
        Err(error) if error == "invalid password" => api_error(StatusCode::BAD_REQUEST, &error),
        Err(error) if error == "OEM login rejected" => api_error(StatusCode::UNAUTHORIZED, &error),
        Err(error) => api_error(StatusCode::BAD_GATEWAY, &error),
    }
}
async fn oem_read(
    State(app): State<App>,
    headers: HeaderMap,
    Query(query): Query<ReadQuery>,
) -> Response {
    let Some(token) = bearer(&headers) else {
        return api_error(StatusCode::UNAUTHORIZED, "Bearer token required");
    };
    let Some(oem) = app.oem.as_ref() else {
        return api_error(StatusCode::NOT_FOUND, "OEM writes disabled");
    };
    match oem.read(token, &query.cmd).await {
        Ok(value) => Json(value).into_response(),
        Err(error) if error.contains("session") || error.contains("login required") => {
            api_error(StatusCode::UNAUTHORIZED, &error)
        }
        Err(error) if error.starts_with("OEM ") => api_error(StatusCode::BAD_GATEWAY, &error),
        Err(error) => api_error(StatusCode::BAD_REQUEST, &error),
    }
}
async fn oem_control(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<ControlRequest>,
) -> Response {
    let Some(token) = bearer(&headers) else {
        return api_error(StatusCode::UNAUTHORIZED, "Bearer token required");
    };
    let Some(oem) = app.oem.as_ref() else {
        return api_error(StatusCode::NOT_FOUND, "OEM writes disabled");
    };
    if body.action != "u50.oem.goform" {
        return api_error(StatusCode::BAD_REQUEST, "unsupported action");
    }
    match oem
        .write(token, &body.goform_id, &body.params, body.confirm)
        .await
    {
        Ok(value) => Json(value).into_response(),
        Err(error) if error.contains("session") || error.contains("login required") => {
            api_error(StatusCode::UNAUTHORIZED, &error)
        }
        Err(error) if error.starts_with("OEM ") => api_error(StatusCode::BAD_GATEWAY, &error),
        Err(error) => api_error(StatusCode::BAD_REQUEST, &error),
    }
}
async fn oem_logout(State(app): State<App>, headers: HeaderMap) -> Response {
    let Some(token) = bearer(&headers) else {
        return api_error(StatusCode::UNAUTHORIZED, "Bearer token required");
    };
    let Some(oem) = app.oem.as_ref() else {
        return api_error(StatusCode::NOT_FOUND, "OEM writes disabled");
    };
    match oem.logout(token).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => api_error(StatusCode::UNAUTHORIZED, &error),
    }
}

async fn events(State(app): State<App>) -> Response {
    let Ok(permit) = app.sse_slots.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok":false,"error":"sse_client_limit"})),
        )
            .into_response();
    };
    let stream = WatchStream::new(app.tx.subscribe()).map(move |snapshot| {
        let _keep_permit_alive = &permit;
        Ok::<_, Infallible>(Event::default().event("state").json_data(snapshot).unwrap())
    });
    Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::new())
        .into_response()
}
async fn fetch_goform(
    client: &reqwest::Client,
    url: &reqwest::Url,
    cfg: &BTreeMap<String, String>,
) -> Result<Value> {
    let mut request_url = url.clone();
    request_url
        .query_pairs_mut()
        .append_pair("cmd", CMD)
        .append_pair("multi_data", "1");
    let mut request = client.get(request_url);
    // The OEM GoAhead returns empty fields for a loopback Host even when it
    // serves the same socket. Keep the TCP destination local and use its
    // validated LAN address as the original WebUI's Host/Referer.
    if let Some(ip) = cfg
        .get("lan_ipaddr")
        .filter(|v| v.parse::<Ipv4Addr>().is_ok())
    {
        request = request
            .header(reqwest::header::HOST, ip.as_str())
            .header(reqwest::header::REFERER, format!("http://{ip}/"));
    }
    let response = request
        .send()
        .await
        .context("U50 GoAhead request failed")?
        .error_for_status()
        .context("U50 GoAhead HTTP error")?;
    ensure!(
        response
            .content_length()
            .is_none_or(|size| size <= MAX_RESPONSE as u64),
        "U50 GoAhead response too large"
    );
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.context("U50 GoAhead body failed")? {
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= MAX_RESPONSE,
            "U50 GoAhead response too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    let raw: Value = serde_json::from_slice(&bytes).context("U50 GoAhead response is not JSON")?;
    ensure!(raw.is_object(), "U50 GoAhead returned non-object JSON");
    ensure!(
        raw.get("model_name").is_some() || raw.get("network_type").is_some(),
        "U50 GoAhead response has no expected fields; login may be required"
    );
    Ok(raw)
}

async fn collect(client: &reqwest::Client, url: &reqwest::Url, model: Model) -> Result<Snapshot> {
    let cfg = cfg_values().await?;
    let goform = fetch_goform(client, url, &cfg).await.ok();
    from_sources(model, &cfg, goform.as_ref())
}

pub async fn run(
    model: Model,
    goform_url: &str,
    bind: SocketAddr,
    once: bool,
    interval: Duration,
    enable_writes: bool,
) -> Result<()> {
    ensure!(
        bind.ip().is_loopback(),
        "U50 candidate server is loopback-only"
    );
    let url = validate_goform_url(goform_url)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let initial = collect(&client, &url, model).await?;
    if once {
        println!("{}", serde_json::to_string(&initial)?);
        return Ok(());
    }
    ensure!(
        !enable_writes || model == Model::U50S,
        "OEM writes are only mapped for U50S"
    );
    let oem = if enable_writes {
        let host = initial
            .fields
            .get("u50_cfg")
            .and_then(|v| v.get("lan_ipaddr"))
            .and_then(Value::as_str)
            .context("U50S LAN address unavailable for OEM writes")?;
        Some(Bridge::new(client.clone(), url.clone(), host.into()).map_err(anyhow::Error::msg)?)
    } else {
        None
    };
    let (tx, _) = watch::channel(initial.clone());
    let app = App {
        snapshot: Arc::new(RwLock::new(initial)),
        tx,
        sse_slots: Arc::new(Semaphore::new(16)),
        last_ok: Arc::new(RwLock::new(Instant::now())),
        max_stale: interval.saturating_mul(3),
        oem,
    };
    let state = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            if let Ok(next) = collect(&client, &url, model).await {
                let mut current = state.snapshot.write().await;
                let mut comparable = next.clone();
                comparable.ts = current.ts;
                if comparable != *current {
                    *current = next.clone();
                    let _ = state.tx.send(next);
                } else {
                    current.ts = next.ts;
                }
                *state.last_ok.write().await = Instant::now();
            }
        }
    });
    let mut router = Router::new()
        .route("/healthz", get(health))
        .route("/version", get(version))
        .route("/state", get(snapshot))
        .route("/events", get(events))
        .route("/capabilities", get(capabilities));
    if app.oem.is_some() {
        router = router
            .route("/auth/login", post(oem_login))
            .route("/auth/logout", post(oem_logout))
            .route("/oem/read", get(oem_read))
            .route("/control", post(oem_control));
    }
    let router = router
        .layer(RequestBodyLimitLayer::new(16 * 1024))
        .with_state(app);
    axum::serve(tokio::net::TcpListener::bind(bind).await?, router).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maps_firmware_cfg_even_when_goform_requires_login() {
        let cfg = BTreeMap::from([
            ("model_name".into(), "U50Pro".into()),
            ("integrate_version".into(), "B02".into()),
            ("lan_ipaddr".into(), "192.168.0.1".into()),
            ("network_type".into(), "LTE".into()),
            ("signalbar".into(), "5".into()),
            ("lte_rsrp".into(), "-83".into()),
            ("battery_vol_percent".into(), "66".into()),
            ("battery_temp".into(), "34".into()),
            ("realtime_rx_thrpt".into(), "1814".into()),
            ("monthly_tx_bytes".into(), "211028951".into()),
            ("wifi_onoff_state".into(), "1".into()),
            ("wifi_access_sta_num".into(), "2".into()),
            ("simcard_active_slot".into(), "1".into()),
        ]);
        let value = from_sources(Model::U50Pro, &cfg, None).unwrap();
        assert_eq!(value.fields["device"]["api_template_supported"], 0);
        assert_eq!(value.fields["system"]["sw_version"], "B02");
        assert_eq!(value.fields["dhcp"]["ip"], "192.168.0.1");
        assert_eq!(value.fields["net"]["type"], "LTE");
        assert_eq!(value.fields["net"]["bars"], 5);
        assert_eq!(value.fields["net"]["lte_rsrp"], -83);
        assert_eq!(value.fields["net"]["HSR"], false);
        assert_eq!(value.fields["battery"]["percent"], 66);
        assert_eq!(value.fields["battery"]["temp"], 34);
        assert_eq!(value.fields["traffic"]["rx_speed"], 1814);
        assert_eq!(value.fields["traffic"]["month_tx_bytes"], 211028951);
        assert_eq!(value.fields["wlan"]["enabled"], 1);
        assert_eq!(value.fields["clients"]["total"], 2);
        assert_eq!(value.fields["sim"]["current_slot"], 1);
        assert_eq!(value.fields["u50_sources"]["goform"], "unavailable");
    }
    #[test]
    fn maps_only_readable_goform_fields() {
        let cfg = BTreeMap::from([("model_name".into(), "U50S".into())]);
        let raw = json!({"network_type":"NR5G","battery_value":"84","sim_iccid":"private"});
        let value = from_sources(Model::U50S, &cfg, Some(&raw)).unwrap();
        assert_eq!(value.fields["net"]["type"], "NR5G");
        assert!(value.fields.get("sim_iccid").is_none());
        assert!(value.fields.get("battery").is_none());
    }
    #[test]
    fn rejects_untrusted_sources_and_missing_cfg_identity() {
        assert!(validate_goform_url("http://192.168.0.1/goform/goform_get_cmd_process").is_err());
        assert!(validate_goform_url("http://127.0.0.1/goform/goform_set_cmd_process").is_err());
        assert!(from_sources(Model::U50S, &BTreeMap::new(), None).is_err());
        let wrong = BTreeMap::from([("model_name".into(), "ZTE U50 Pro".into())]);
        assert!(from_sources(Model::U50S, &wrong, None).is_err());
    }
}
