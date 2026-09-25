//! Read-only, firmware-derived U50 candidate collector.
//!
//! This path deliberately does not expose ZWRT UBus/UCI or mutating routes.
//! The firmware proves names and code paths, not successful device responses.
use crate::model::{DatadVersion, Snapshot};
use anyhow::{Context, Result, ensure};
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde_json::{Map, Value, json};
use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::RwLock;

const CMD: &str = "model_name,wa_inner_version,network_type,network_provider_fullname,wan_active_band,battery_value,signalbar";
const MAX_RESPONSE: usize = 64 * 1024;

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
fn from_goform(model: Model, raw: &Value) -> Result<Snapshot> {
    ensure!(raw.is_object(), "U50 GoAhead returned non-object JSON");
    ensure!(
        raw.get("model_name").is_some() || raw.get("network_type").is_some(),
        "U50 GoAhead response has no expected fields; login may be required"
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
            "model_name": string_field(raw, "model_name").unwrap_or("")
        }),
    );
    if let Some(fw) = string_field(raw, "wa_inner_version") {
        fields.insert("system".into(), json!({"sw_version":fw}));
    }
    let mut net = Map::new();
    for (source, target) in [
        ("network_type", "type"),
        ("network_provider_fullname", "operator"),
        ("wan_active_band", "band"),
    ] {
        if let Some(value) = string_field(raw, source) {
            net.insert(target.into(), json!(value));
        }
    }
    if !net.is_empty() {
        fields.insert("net".into(), Value::Object(net));
    }
    // Signal and battery units cannot be established from binary strings alone.
    let mut candidate = Map::new();
    for key in ["signalbar", "battery_value"] {
        if let Some(value) = raw.get(key).filter(|v| v.is_string() || v.is_number()) {
            candidate.insert(key.into(), value.clone());
        }
    }
    if !candidate.is_empty() {
        fields.insert("u50_unverified".into(), Value::Object(candidate));
    }
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
async fn fetch(client: &reqwest::Client, url: &reqwest::Url, model: Model) -> Result<Snapshot> {
    let mut request_url = url.clone();
    request_url
        .query_pairs_mut()
        .append_pair("isTest", "false")
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
    from_goform(model, &raw)
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
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let initial = fetch(&client, &url, model).await?;
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
            if let Ok(next) = fetch(&client, &url, model).await {
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
    fn maps_only_supported_candidate_fields() {
        let raw = json!({"model_name":"U50Pro","wa_inner_version":"B02","network_type":"NR5G","battery_value":"84","sim_iccid":"private"});
        let value = from_goform(Model::U50Pro, &raw).unwrap();
        assert_eq!(value.fields["device"]["api_template_supported"], 0);
        assert_eq!(value.fields["net"]["type"], "NR5G");
        assert!(value.fields.get("sim_iccid").is_none());
        assert!(value.fields.get("battery").is_none());
    }
    #[test]
    fn rejects_untrusted_sources_and_login_html() {
        assert!(validate_goform_url("http://192.168.0.1/goform/goform_get_cmd_process").is_err());
        assert!(validate_goform_url("http://127.0.0.1/goform/goform_set_cmd_process").is_err());
        assert!(from_goform(Model::U50S, &json!({"error":"login"})).is_err());
    }
}
