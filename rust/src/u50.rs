//! Read-only, firmware-derived U50 candidate collector.
//!
//! This path deliberately does not expose ZWRT UBus/UCI or mutating routes.
//! The firmware proves names and code paths, not successful device responses.
use crate::{
    command,
    model::{DatadVersion, Snapshot},
};
use anyhow::{Context, Result, ensure};
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::RwLock;

const CMD: &str = "model_name,wa_inner_version,network_type,network_provider_fullname,network_provider,battery_value,battery_temp,battery_status,signalbar,simcard_status";
const MAX_RESPONSE: usize = 64 * 1024;
const CFG_KEYS: &[&str] = &[
    "model_name",
    "integrate_version",
    "lan_ipaddr",
    "lan_netmask",
    "wan_ipaddr",
    "wan_gateway",
    "ppp_status",
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
    if let Some(raw) = goform {
        let mut net = Map::new();
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
        if !net.is_empty() {
            fields.insert("net".into(), Value::Object(net));
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
    last_ok: Arc<RwLock<Instant>>,
    max_stale: Duration,
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
async fn capabilities() -> Json<Value> {
    Json(
        json!({"schema_version":1,"protocol":1,"transport":["http"],"control":[],"controls":[],"discovery":[],"passthrough":[],"template_status":"firmware_candidate"}),
    )
}
async fn fetch_goform(client: &reqwest::Client, url: &reqwest::Url) -> Result<Value> {
    let mut request_url = url.clone();
    request_url
        .query_pairs_mut()
        .append_pair("cmd", CMD)
        .append_pair("multi_data", "1");
    let response = client
        .get(request_url)
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
    let goform = fetch_goform(client, url).await.ok();
    from_sources(model, &cfg, goform.as_ref())
}

pub async fn run(
    model: Model,
    goform_url: &str,
    bind: SocketAddr,
    once: bool,
    interval: Duration,
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
    let app = App {
        snapshot: Arc::new(RwLock::new(initial)),
        last_ok: Arc::new(RwLock::new(Instant::now())),
        max_stale: interval.saturating_mul(3),
    };
    let state = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            if let Ok(next) = collect(&client, &url, model).await {
                *state.snapshot.write().await = next;
                *state.last_ok.write().await = Instant::now();
            }
        }
    });
    let router = Router::new()
        .route("/healthz", get(health))
        .route("/version", get(version))
        .route("/state", get(snapshot))
        .route("/capabilities", get(capabilities))
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
        ]);
        let value = from_sources(Model::U50Pro, &cfg, None).unwrap();
        assert_eq!(value.fields["device"]["api_template_supported"], 0);
        assert_eq!(value.fields["system"]["sw_version"], "B02");
        assert_eq!(value.fields["dhcp"]["ip"], "192.168.0.1");
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
