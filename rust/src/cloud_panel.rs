//! On-demand state stream for an authenticated NMS panel session. No local
//! HTTP port, datad bearer token, or UFI process is exposed to the cloud.
use crate::{
    cloud::{Config, websocket_tls},
    control::{self, Outcome},
    cooling,
    model::Snapshot,
    state, wifi,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{collections::HashSet, time::Duration};
use tokio::sync::watch;
use tokio_tungstenite::{
    Connector, connect_async_tls_with_config,
    tungstenite::{
        Message, client::IntoClientRequest, http::HeaderValue, protocol::WebSocketConfig,
    },
};

pub(crate) const PROTOCOL: &str = "nms-datad-panel-v1";
pub(crate) const CONTROL_PROTOCOL: &str = "nms-datad-panel-v2";
const MAX_STATE_BYTES: usize = 192 * 1024;
const MAX_CONTROL_BYTES: usize = 8 * 1024;
const MAX_CONTROLS_PER_SESSION: usize = 128;
const EXPOSED_BLOCKS: &[&str] = &[
    "net",
    "neighbor",
    "battery",
    "power",
    "sms",
    "traffic",
    "wlan",
    "nfc",
    "sim",
    "modems",
    "aggregation",
    "multiwan",
    "cooling",
    "dhcp",
    "device",
    "system",
    "thermal",
    "usb",
    "runtime",
    "qos",
    "clients",
    "sample_interval_ms",
];

fn panel_snapshot(snapshot: &Snapshot) -> Value {
    let mut view = Map::new();
    view.insert("ts".into(), json!(snapshot.ts));
    view.insert("datad".into(), json!(snapshot.datad));
    for name in EXPOSED_BLOCKS {
        if let Some(value) = snapshot.fields.get(*name) {
            view.insert((*name).to_owned(), value.clone());
        }
    }
    Value::Object(view)
}

fn state_message(snapshot: &Snapshot) -> Option<Message> {
    state_message_version(snapshot, 1)
}

fn state_message_version(snapshot: &Snapshot, version: u8) -> Option<Message> {
    state_message_with_config(snapshot, version, None, None, None)
}

fn state_message_with_config(
    snapshot: &Snapshot,
    version: u8,
    wifi_config: Option<&Value>,
    apn_config: Option<&Value>,
    cooling_config: Option<&Value>,
) -> Option<Message> {
    let mut view = panel_snapshot(snapshot);
    if let Some(wifi_config) = wifi_config {
        view.as_object_mut()?
            .insert("wifi_config".into(), wifi_config.clone());
    }
    if let Some(apn_config) = apn_config {
        view.as_object_mut()?
            .insert("apn_config".into(), apn_config.clone());
    }
    if let Some(cooling_config) = cooling_config {
        view.as_object_mut()?
            .insert("cooling_config".into(), cooling_config.clone());
    }
    let raw = serde_json::to_vec(&json!({
        "type": "state", "protocol_version": version, "snapshot": view
    }))
    .ok()?;
    if raw.len() > MAX_STATE_BYTES {
        return None;
    }
    Some(Message::Text(String::from_utf8(raw).ok()?.into()))
}

async fn wifi_panel_config() -> Option<Value> {
    let status = wifi::wireless_config_status().await.ok()?;
    let countries: Vec<String> = status
        .get("countries")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_ascii_uppercase)
        .filter(|country| {
            country == "00"
                || (country.len() == 2 && country.bytes().all(|b| b.is_ascii_alphabetic()))
        })
        .take(256)
        .collect();
    let mut bands = Map::new();
    for (band, section) in [("2g", "main_2g"), ("5g", "main_5g")] {
        let radio = status.get("radios")?.get(band)?;
        let radio_name = if band == "2g" { "wifi0" } else { "wifi1" };
        let prefix = format!("wireless.{section}");
        let supported = !state::uci_read(&format!("{prefix}.device"))
            .await
            .is_empty();
        let ssid: String = state::uci_read(&format!("{prefix}.ssid"))
            .await
            .chars()
            .take(32)
            .collect();
        let encryption = state::uci_read(&format!("{prefix}.encryption")).await;
        let channels: Vec<i64> = radio
            .get("supported_channels")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_i64)
            .filter(|value| (0..=255).contains(value))
            .take(128)
            .collect();
        let country = radio.get("country").and_then(Value::as_str).unwrap_or("");
        bands.insert(band.into(),json!({
            "ssid":ssid,
            "supported":supported,
            "encryption":if encryption.len() <= 64 { encryption } else { String::new() },
            "hidden":state::uci_read(&format!("{prefix}.hidden")).await == "1",
            "pmf":state::uci_read(&format!("{prefix}.pmf")).await.chars().take(16).collect::<String>(),
            "enabled":state::uci_read(&format!("{prefix}.disabled")).await != "1",
            "maxassoc":state::uci_read(&format!("wireless.{radio_name}.maxassoc")).await.parse::<u32>().ok().filter(|value| *value <= 1024),
            "country":if country.len()==2 { country } else { "" },
            "channel":radio.get("channel").and_then(Value::as_str).unwrap_or("0"),
            "htmode":radio.get("htmode").and_then(Value::as_str).unwrap_or("").chars().take(20).collect::<String>(),
            "channels":channels
        }));
    }
    let dual_band = wifi::dual_band_status()
        .await
        .ok()
        .and_then(|value| value.get("enabled").and_then(Value::as_bool));
    Some(json!({"countries":countries,"bands":bands,"dual_band":dual_band}))
}

fn panel_apn_profiles(reply: &Value) -> Vec<Value> {
    reply.get("apnListArray").and_then(Value::as_array)
        .into_iter().flatten().take(64).filter_map(|item| {
            let id=item.get("profileId")?.as_str()?;
            if id.is_empty() || id.len()>64 || !id.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"_-.".contains(&byte)) {return None;}
            let limited=|name:&str,max:usize|->String{
                item.get(name).and_then(Value::as_str).unwrap_or("").chars().take(max).collect()
            };
            Some(json!({
                "id":id,"name":limited("profilename",64),"apn":limited("wanapn",128),
                "pdp_type":item.get("pdpType").and_then(Value::as_i64).filter(|v|(0..=3).contains(v)),
                "auth_mode":item.get("pppAuthMode").and_then(Value::as_i64).filter(|v|(0..=3).contains(v)),
                "enabled":item.get("isEnable").and_then(Value::as_bool)==Some(true)
            }))
        }).collect()
}

async fn apn_panel_config() -> Option<Value> {
    let (mode, automatic, manual, enabled) = tokio::join!(
        state::ubus("zwrt_apn_object", "get_apn_mode", json!({})),
        state::ubus("zwrt_apn_object", "getAutoApnList", json!({})),
        state::ubus("zwrt_apn_object", "getManuApnList", json!({})),
        state::ubus("zwrt_apn_object", "get_enabled_manu_apn_id", json!({}))
    );
    let (Ok(mode), Ok(automatic), Ok(manual), Ok(enabled)) = (mode, automatic, manual, enabled)
    else {
        return None;
    };
    let mode = mode
        .get("apn_mode")
        .and_then(Value::as_i64)
        .filter(|v| *v == 0 || *v == 1)?;
    let enabled_id = enabled
        .get("profileId")
        .and_then(Value::as_str)
        .filter(|v| v.len() <= 64)
        .unwrap_or("");
    Some(json!({"mode":mode,"enabled_id":enabled_id,
        "automatic":panel_apn_profiles(&automatic),"manual":panel_apn_profiles(&manual)}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlRequest {
    #[serde(rename = "type")]
    kind: String,
    protocol_version: u8,
    request_id: String,
    action: String,
    params: Value,
    confirmed: bool,
}

fn parse_control(raw: &str) -> Option<ControlRequest> {
    let request: ControlRequest = serde_json::from_str(raw).ok()?;
    if request.kind != "control"
        || request.protocol_version != 2
        || request.request_id.len() != 32
        || !request
            .request_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || request.action.is_empty()
        || request.action.len() > 64
        || !request.params.is_object()
    {
        return None;
    }
    Some(request)
}

fn needs_confirmation(action: &str) -> bool {
    matches!(
        action,
        "device.reboot"
            | "device.poweroff"
            | "cellular.disconnect"
            | "cellular.set"
            | "network.set_mode"
            | "band.set_lte"
            | "band.set_nr_sa"
            | "band.set_nr_nsa"
            | "cell.lock_lte"
            | "cell.lock_nr"
            | "cell.unlock_all"
            | "sim.set_slot"
            | "wifi.set_module"
            | "wifi.configure"
            | "wireless.config"
            | "lan.set"
            | "apn.set_mode"
            | "apn.add"
            | "apn.modify"
            | "apn.enable"
            | "apn.delete"
            | "multiwan.interface.set"
            | "multiwan.member.set"
            | "multiwan.policy.set"
            | "multiwan.rule.set"
            | "cooling.fan.set_enabled"
            | "cooling.fan.set_mode"
            | "cooling.fan.set_curve"
            | "cooling.liquid.set_enabled"
            | "cooling.liquid.set_mode"
            | "aggregation.set"
            | "sms.delete"
            | "sms.send_raw"
            | "client.kick"
            | "client.block"
    )
}

fn valid_remote_band_list(action: &str, params: &Value) -> bool {
    if !matches!(
        action,
        "band.set_lte" | "band.set_nr_sa" | "band.set_nr_nsa"
    ) {
        return true;
    }
    let Some(value) = params.get("bands").and_then(Value::as_str) else {
        return false;
    };
    let parts: Vec<_> = value.split(',').collect();
    let mut seen = HashSet::new();
    !parts.is_empty()
        && parts.len() <= 128
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && part
                    .parse::<u16>()
                    .is_ok_and(|band| (1..=1024).contains(&band) && seen.insert(band))
        })
}

async fn control_result(request: ControlRequest) -> Message {
    let code =
        if request.action == "usb.set" || !control::ACTIONS.contains(&request.action.as_str()) {
            Some("unsupported_action")
        } else if needs_confirmation(&request.action) && !request.confirmed {
            Some("confirmation_required")
        } else if !valid_remote_band_list(&request.action, &request.params) {
            Some("invalid_parameter")
        } else {
            match tokio::time::timeout(
                Duration::from_secs(20),
                control::execute(&request.action, &request.params),
            )
            .await
            {
                Ok(Outcome::Ok(_)) => None,
                Ok(Outcome::Invalid(_)) => Some("invalid_parameter"),
                Ok(Outcome::Failed(_)) => Some("device_call_failed"),
                Ok(Outcome::NotHandled) => Some("unsupported_action"),
                Err(_) => Some("device_call_timeout"),
            }
        };
    Message::Text(
        json!({"type":"control_result","protocol_version":2,"request_id":request.request_id,"ok":code.is_none(),"code":code})
            .to_string()
            .into(),
    )
}

pub(crate) async fn run(
    state_rx: watch::Receiver<Snapshot>,
    control_mode: bool,
    config: &Config,
    url: &str,
    token: &str,
    ttl: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    if *shutdown.borrow() || ttl.is_zero() {
        return;
    }
    let protocol = if control_mode {
        CONTROL_PROTOCOL
    } else {
        PROTOCOL
    };
    let deadline = tokio::time::Instant::now() + ttl;
    let Ok(mut request) = url.into_client_request() else {
        return;
    };
    let Ok(auth) = HeaderValue::from_str(&format!("Bearer {token}")) else {
        return;
    };
    request.headers_mut().insert("authorization", auth);
    request
        .headers_mut()
        .insert("x-nms-target-port", HeaderValue::from_static("0"));
    request
        .headers_mut()
        .insert("sec-websocket-protocol", HeaderValue::from_static(protocol));
    let Ok(tls) = websocket_tls(config) else {
        return;
    };
    let limits = WebSocketConfig::default()
        .max_message_size(Some(if control_mode {
            MAX_CONTROL_BYTES
        } else {
            4 * 1024
        }))
        .max_frame_size(Some(if control_mode {
            MAX_CONTROL_BYTES
        } else {
            4 * 1024
        }))
        .write_buffer_size(0)
        .max_write_buffer_size(2 * MAX_STATE_BYTES);
    let connect =
        connect_async_tls_with_config(request, Some(limits), false, Some(Connector::Rustls(tls)));
    let (mut socket, response) = tokio::select! {
        result = tokio::time::timeout(ttl.min(Duration::from_secs(10)), connect) => {
            match result { Ok(Ok(value)) => value, _ => return }
        },
        _ = shutdown.changed() => return,
    };
    if response
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        != Some(protocol)
    {
        return;
    }
    if socket
        .send(Message::Text(
            json!({"type":"ready","protocol_version":if control_mode {2} else {1}})
                .to_string()
                .into(),
        ))
        .await
        .is_err()
    {
        return;
    }
    let mut last = state_rx.borrow().clone();
    let Some(first) = (if control_mode {
        state_message_version(&last, 2)
    } else {
        state_message(&last)
    }) else {
        return;
    };
    if socket.send(first).await.is_err() {
        return;
    }
    // Extra configuration is fetched only while a remote panel is open. It
    // contains reviewed UI fields, never Wi-Fi keys or raw vendor responses.
    let (wifi_result, apn_result, cooling_result) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(6), wifi_panel_config()),
        tokio::time::timeout(Duration::from_secs(6), apn_panel_config()),
        tokio::time::timeout(Duration::from_secs(6), cooling::panel_config())
    );
    let mut wifi_config = wifi_result.ok().flatten();
    let mut apn_config = apn_result.ok().flatten();
    let mut cooling_config = cooling_result.ok().flatten();
    let mut config_pending =
        wifi_config.is_some() || apn_config.is_some() || cooling_config.is_some();
    let mut sample = tokio::time::interval(Duration::from_secs(1));
    sample.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ping = tokio::time::interval(Duration::from_secs(15));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    sample.tick().await;
    ping.tick().await;
    let mut used_ids = HashSet::new();
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return,
            _ = shutdown.changed() => return,
            _ = sample.tick() => {
                let next = state_rx.borrow().clone();
                if next != last || config_pending {
                    let Some(message) = state_message_with_config(&next, if control_mode {2} else {1}, wifi_config.as_ref(), apn_config.as_ref(), cooling_config.as_ref()) else { return; };
                    if socket.send(message).await.is_err() { return; }
                    last = next;
                    config_pending = false;
                }
            },
            _ = ping.tick() => {
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() { return; }
            },
            incoming = socket.next() => match incoming {
                Some(Ok(Message::Ping(data))) => {
                    if socket.send(Message::Pong(data)).await.is_err() { return; }
                },
                Some(Ok(Message::Pong(_))) => {},
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(Message::Text(raw))) if control_mode => {
                    let Some(request) = parse_control(&raw) else { return; };
                    if used_ids.len() >= MAX_CONTROLS_PER_SESSION || !used_ids.insert(request.request_id.clone()) { return; }
                    let refresh_wifi = matches!(request.action.as_str(), "wifi.configure" | "wifi.set_dual_band" | "wireless.config");
                    let refresh_apn = request.action.starts_with("apn.");
                    let refresh_cooling = request.action.starts_with("cooling.");
                    if socket.send(control_result(request).await).await.is_err() { return; }
                    if refresh_wifi || refresh_apn || refresh_cooling {
                        if refresh_wifi {wifi_config = tokio::time::timeout(Duration::from_secs(6), wifi_panel_config()).await.ok().flatten();}
                        if refresh_apn {apn_config = tokio::time::timeout(Duration::from_secs(6), apn_panel_config()).await.ok().flatten();}
                        if refresh_cooling {cooling_config = tokio::time::timeout(Duration::from_secs(6), cooling::panel_config()).await.ok().flatten();}
                        if let Some(message) = state_message_with_config(&last, 2, wifi_config.as_ref(), apn_config.as_ref(), cooling_config.as_ref())
                            && socket.send(message).await.is_err() { return; }
                    }
                },
                // V1 remains strictly read-only. V2 refuses binary and unknown frames.
                Some(Ok(_)) => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DatadVersion;

    #[test]
    fn only_reviewed_state_blocks_leave_the_device() {
        let mut fields = Map::new();
        fields.insert("net".into(), json!({"type":"SA"}));
        fields.insert("future_secret".into(), json!({"token":"must-not-leak"}));
        fields.insert(
            "interfaces".into(),
            json!({"cellular":{"password":"must-not-leak"}}),
        );
        let snapshot = Snapshot {
            ts: 7,
            datad: DatadVersion::default(),
            fields,
        };
        let message = state_message(&snapshot).unwrap();
        let Message::Text(raw) = message else {
            panic!("expected a text frame")
        };
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["snapshot"]["net"]["type"], "SA");
        assert!(parsed["snapshot"].get("future_secret").is_none());
        assert!(parsed["snapshot"].get("interfaces").is_none());
        assert_eq!(parsed["protocol_version"], 1);
    }

    #[test]
    fn apn_panel_profiles_never_copy_account_secrets() {
        let raw = json!({"apnListArray":[
            {"profileId":"profile-1","profilename":"Carrier","wanapn":"internet",
             "pdpType":3,"pppAuthMode":2,"isEnable":true,
             "username":"must-not-leak","password":"must-not-leak"},
            {"profileId":"../bad","profilename":"Bad","password":"must-not-leak"}
        ]});
        let filtered = panel_apn_profiles(&raw);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["id"], "profile-1");
        assert_eq!(filtered[0]["apn"], "internet");
        assert!(
            !serde_json::to_string(&filtered)
                .unwrap()
                .contains("must-not-leak")
        );
        assert!(filtered[0].get("username").is_none());
        assert!(filtered[0].get("password").is_none());
    }

    #[test]
    fn oversized_state_is_rejected_without_truncation() {
        let mut fields = Map::new();
        fields.insert("sms".into(), json!({"text":"x".repeat(MAX_STATE_BYTES)}));
        let snapshot = Snapshot {
            ts: 1,
            datad: DatadVersion::default(),
            fields,
        };
        assert!(state_message(&snapshot).is_none());
    }

    #[tokio::test]
    async fn control_frames_require_v2_request_ids_objects_and_confirmation() {
        let id = "a".repeat(32);
        let allowed = format!(
            r#"{{"type":"control","protocol_version":2,"request_id":"{id}","action":"device.reboot","params":{{}},"confirmed":false}}"#
        );
        let request = parse_control(&allowed).unwrap();
        assert!(needs_confirmation(&request.action));
        assert!(control::ACTIONS.contains(&"wireless.config"));
        assert!(needs_confirmation("wireless.config"));
        assert!(needs_confirmation("cellular.set"));
        assert!(needs_confirmation("cooling.fan.set_curve"));
        assert!(needs_confirmation("cooling.liquid.set_mode"));
        for invalid in ["", "0", "1,,3", "1,1", "1025", "1;reboot"] {
            assert!(!valid_remote_band_list(
                "band.set_lte",
                &json!({"bands":invalid})
            ));
        }
        assert!(valid_remote_band_list(
            "band.set_nr_sa",
            &json!({"bands":"28,78"})
        ));
        let Message::Text(reply) = control_result(request).await else {
            panic!("expected a text result")
        };
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["request_id"], id);
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["code"], "confirmation_required");

        for invalid in [
            allowed.replace("\"protocol_version\":2", "\"protocol_version\":1"),
            allowed.replace("\"params\":{}", "\"params\":[]"),
            allowed.replace(&id, "not-a-request-id"),
            allowed.replace("\"type\":\"control\"", "\"type\":\"state\""),
            allowed.replace("\"confirmed\":false", "\"confirmed\":false,\"extra\":true"),
        ] {
            assert!(parse_control(&invalid).is_none());
        }
        let unsupported = allowed.replace("device.reboot", "ubus.call");
        let Message::Text(reply) = control_result(parse_control(&unsupported).unwrap()).await
        else {
            panic!("expected an unsupported-action result")
        };
        assert_eq!(
            serde_json::from_str::<Value>(&reply).unwrap()["code"],
            "unsupported_action"
        );
        let skipped_adb = allowed.replace("device.reboot", "usb.set");
        let Message::Text(reply) = control_result(parse_control(&skipped_adb).unwrap()).await
        else {
            panic!("expected USB mode to be excluded from the remote panel")
        };
        assert_eq!(
            serde_json::from_str::<Value>(&reply).unwrap()["code"],
            "unsupported_action"
        );
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)] // Tungstenite handshake callback has a large error type.
    async fn authenticated_wss_streams_changed_state_and_rejects_input() {
        use rustls::{ServerConfig, pki_types::PrivateKeyDer};
        use std::sync::Arc;
        use tokio::net::TcpListener;
        use tokio_rustls::TlsAcceptor;
        use tokio_tungstenite::accept_hdr_async;

        let _ = rustls::crypto::ring::default_provider().install_default();
        let certificate = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
        let config = Config {
            ca_pem: certificate.cert.pem(),
            ..Default::default()
        };
        let tls = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.cert.der().clone()],
                PrivateKeyDer::Pkcs8(certificate.key_pair.serialize_der().into()),
            )
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(tls));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "wss://{}/api/remote/device/panel-fixture",
            listener.local_addr().unwrap()
        );
        let mut fields = Map::new();
        fields.insert("net".into(), json!({"type":"LTE"}));
        let (sender, receiver) = watch::channel(Snapshot {
            ts: 1,
            datad: DatadVersion::default(),
            fields,
        });
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let task = tokio::spawn(async move {
            run(
                receiver,
                false,
                &config,
                &url,
                &"a".repeat(64),
                Duration::from_secs(5),
                shutdown,
            )
            .await;
        });
        let (tcp, _) = listener.accept().await.unwrap();
        let tls = acceptor.accept(tcp).await.unwrap();
        let mut ws = accept_hdr_async(tls, |request: &tokio_tungstenite::tungstenite::handshake::server::Request, mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
            assert_eq!(request.headers()["authorization"],format!("Bearer {}","a".repeat(64)));
            assert_eq!(request.headers()["x-nms-target-port"],"0");
            assert_eq!(request.headers()["sec-websocket-protocol"],PROTOCOL);
            response.headers_mut().insert("sec-websocket-protocol",HeaderValue::from_static(PROTOCOL));
            Ok(response)
        }).await.unwrap();
        let ready = ws.next().await.unwrap().unwrap();
        assert!(matches!(ready,Message::Text(ref body) if body.contains("\"ready\"")));
        let first = ws.next().await.unwrap().unwrap();
        assert!(matches!(first,Message::Text(ref body) if body.contains("\"LTE\"")));
        sender.send_modify(|snapshot| {
            snapshot.ts = 2;
            snapshot.fields.insert("net".into(), json!({"type":"SA"}));
        });
        let second = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(second,Message::Text(ref body) if body.contains("\"SA\"")));
        ws.send(Message::Text(r#"{"type":"control"}"#.into()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)] // Tungstenite handshake callback has a large error type.
    async fn opted_in_v2_wss_rejects_unknown_actions_and_replayed_requests() {
        use rustls::{ServerConfig, pki_types::PrivateKeyDer};
        use std::sync::Arc;
        use tokio::net::TcpListener;
        use tokio_rustls::TlsAcceptor;
        use tokio_tungstenite::accept_hdr_async;

        let _ = rustls::crypto::ring::default_provider().install_default();
        let certificate = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
        let config = Config {
            ca_pem: certificate.cert.pem(),
            remote_panel_control_enabled: true,
            ..Default::default()
        };
        let tls = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.cert.der().clone()],
                PrivateKeyDer::Pkcs8(certificate.key_pair.serialize_der().into()),
            )
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(tls));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "wss://{}/api/remote/device/control-fixture",
            listener.local_addr().unwrap()
        );
        let (_sender, receiver) = watch::channel(Snapshot {
            ts: 1,
            datad: DatadVersion::default(),
            fields: Map::from_iter([("net".into(), json!({"type":"SA"}))]),
        });
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let task = tokio::spawn(async move {
            run(
                receiver,
                true,
                &config,
                &url,
                &"a".repeat(64),
                Duration::from_secs(5),
                shutdown,
            )
            .await;
        });
        let (tcp, _) = listener.accept().await.unwrap();
        let tls = acceptor.accept(tcp).await.unwrap();
        let mut ws = accept_hdr_async(tls, |request: &tokio_tungstenite::tungstenite::handshake::server::Request, mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
            assert_eq!(request.headers()["sec-websocket-protocol"],CONTROL_PROTOCOL);
            assert_eq!(request.headers()["x-nms-target-port"],"0");
            response.headers_mut().insert("sec-websocket-protocol",HeaderValue::from_static(CONTROL_PROTOCOL));
            Ok(response)
        }).await.unwrap();
        let ready: Value =
            serde_json::from_str(&ws.next().await.unwrap().unwrap().into_text().unwrap()).unwrap();
        let first: Value =
            serde_json::from_str(&ws.next().await.unwrap().unwrap().into_text().unwrap()).unwrap();
        assert_eq!(ready["protocol_version"], 2);
        assert_eq!(first["protocol_version"], 2);
        assert_eq!(first["snapshot"]["net"]["type"], "SA");

        let unknown = json!({"type":"control","protocol_version":2,"request_id":"a".repeat(32),"action":"ubus.call","params":{"password":"must-not-leak"},"confirmed":true}).to_string();
        ws.send(Message::Text(unknown.into())).await.unwrap();
        let reply = ws.next().await.unwrap().unwrap().into_text().unwrap();
        assert!(!reply.contains("must-not-leak"));
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["code"], "unsupported_action");

        let dangerous = json!({"type":"control","protocol_version":2,"request_id":"b".repeat(32),"action":"device.poweroff","params":{},"confirmed":false}).to_string();
        ws.send(Message::Text(dangerous.clone().into()))
            .await
            .unwrap();
        let reply: Value =
            serde_json::from_str(&ws.next().await.unwrap().unwrap().into_text().unwrap()).unwrap();
        assert_eq!(reply["code"], "confirmation_required");
        ws.send(Message::Text(dangerous.into())).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
    }
}
