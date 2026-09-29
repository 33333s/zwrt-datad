//! On-demand state stream for an authenticated NMS panel session. No local
//! HTTP port, datad bearer token, or UFI process is exposed to the cloud.
use crate::{
    cloud::{Config, RemoteFeatures, websocket_tls},
    control::{self, Outcome},
    cooling,
    model::Snapshot,
    reboot_schedule::{self, Schedule},
    server::App,
    sms_forward::Update as SmsForwardUpdate,
    speedtest::SpeedTest,
    state,
    task_schedule::TaskInput,
    traffic_history::Usage,
    wifi,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{collections::HashSet, net::IpAddr, sync::Arc, time::Duration};
use tokio::sync::{Mutex, watch};
use tokio_tungstenite::{
    Connector, connect_async_tls_with_config,
    tungstenite::{
        Message, client::IntoClientRequest, http::HeaderValue, protocol::WebSocketConfig,
    },
};

pub(crate) const PROTOCOL: &str = "nms-datad-panel-v1";
pub(crate) const CONTROL_PROTOCOL: &str = "nms-datad-panel-v2";
pub(crate) struct PanelFeeds {
    pub state: watch::Receiver<Snapshot>,
    pub history: watch::Receiver<Vec<Usage>>,
    pub schedule: Option<Arc<Mutex<Schedule>>>,
    pub speedtest: Option<Arc<Mutex<SpeedTest>>>,
    pub cloud_app: Option<App>,
}
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
    "speedtest",
    "reboot_schedule",
    "clients",
    "sample_interval_ms",
];

fn reviewed_device_info(source: &Value) -> Value {
    let mut out = Map::new();
    for key in [
        "iccid",
        "imei",
        "imsi",
        "msisdn",
        "mac_address",
        "modem_msn",
    ] {
        if let Some(value) = source.get(key).and_then(Value::as_str)
            && !value.is_empty()
            && value.len() <= 128
            && !value.chars().any(char::is_control)
        {
            out.insert(key.into(), json!(value));
        }
    }
    Value::Object(out)
}

fn reviewed_interface_addresses(source: &Value, interface: &str, family: &str) -> Vec<Value> {
    source
        .get(interface)
        .and_then(|value| value.get(family))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let address: IpAddr = entry.get("address")?.as_str()?.parse().ok()?;
            (address.is_ipv4() == (family == "ipv4"))
                .then(|| json!({"address":address.to_string()}))
        })
        .take(4)
        .collect()
}

fn panel_snapshot(snapshot: &Snapshot) -> Value {
    let mut view = Map::new();
    view.insert("ts".into(), json!(snapshot.ts));
    view.insert("datad".into(), json!(snapshot.datad));
    for name in EXPOSED_BLOCKS {
        if let Some(value) = snapshot.fields.get(*name) {
            view.insert((*name).to_owned(), value.clone());
        }
    }
    if let Some(source) = snapshot.fields.get("uci_device_info") {
        view.insert("uci_device_info".into(), reviewed_device_info(source));
    }
    if let Some(source) = snapshot.fields.get("interfaces") {
        view.insert(
            "interfaces".into(),
            json!({
                "wan4":{"ipv4":reviewed_interface_addresses(source,"wan4","ipv4")},
                "wan6":{"ipv6":reviewed_interface_addresses(source,"wan6","ipv6")}
            }),
        );
    }
    Value::Object(view)
}

#[cfg(test)]
fn state_message(snapshot: &Snapshot) -> Option<Message> {
    state_message_version(snapshot, 1)
}

#[cfg(test)]
fn state_message_version(snapshot: &Snapshot, version: u8) -> Option<Message> {
    state_message_with_config(
        snapshot,
        version,
        None,
        None,
        None,
        None,
        PanelManagement::default(),
    )
}

#[derive(Default)]
struct PanelManagement<'a> {
    cloud: Option<&'a Value>,
    tasks: Option<&'a Value>,
    sms_forward: Option<&'a Value>,
}

fn state_message_with_config(
    snapshot: &Snapshot,
    version: u8,
    wifi_config: Option<&Value>,
    apn_config: Option<&Value>,
    cooling_config: Option<&Value>,
    history: Option<&[Usage]>,
    management: PanelManagement<'_>,
) -> Option<Message> {
    let mut view = panel_snapshot(snapshot);
    if let Some(history) = history {
        view.as_object_mut()?
            .insert("traffic_history".into(), json!(history));
    }
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
    if let Some(cloud_management) = management.cloud {
        view.as_object_mut()?
            .insert("cloud_management".into(), cloud_management.clone());
    }
    if let Some(scheduled_tasks) = management.tasks {
        view.as_object_mut()?
            .insert("scheduled_tasks".into(), scheduled_tasks.clone());
    }
    if let Some(sms_forward) = management.sms_forward {
        view.as_object_mut()?
            .insert("sms_forward".into(), sms_forward.clone());
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
            | "wifi.set_dual_band"
            | "wifi.set_module"
            | "wifi.set_chip"
            | "wifi.configure"
            | "wireless.config"
            | "wifi.txpower.apply"
            | "wifi.txpower.set_percent"
            | "wifi.txpower.set_limit"
            | "wifi.txpower.restore_limit"
            | "wifi.psm.set"
            | "wifi.txpower.set_dbm"
            | "wifi.interface.create"
            | "wifi.interface.configure"
            | "wifi.interface.delete"
            | "lan.set"
            | "lan.set_mtu"
            | "dns.set"
            | "power.direct_supply.set"
            | "sleep.set"
            | "nfc.set"
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
            | "traffic.set_limit"
            | "traffic.set_clear_day"
            | "traffic.calibrate"
            | "sms.delete"
            | "sms.send_raw"
            | "sms.forward.set"
            | "sms.forward.test"
            | "client.kick"
            | "client.block"
            | "client.unblock"
            | "qos.clear"
            | "schedule.reboot.set"
            | "schedule.task.put"
            | "schedule.task.remove"
            | "speedtest.start"
            | "cloud.remote_features.set"
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

async fn control_result(
    request: ControlRequest,
    schedule: Option<&Arc<Mutex<Schedule>>>,
    speedtest: Option<&Arc<Mutex<SpeedTest>>>,
    cloud_app: Option<&App>,
) -> Message {
    let code = if request.action == "usb.set"
        || !control::ACTIONS.contains(&request.action.as_str())
    {
        Some("unsupported_action")
    } else if needs_confirmation(&request.action) && !request.confirmed {
        Some("confirmation_required")
    } else if !valid_remote_band_list(&request.action, &request.params) {
        Some("invalid_parameter")
    } else if request.action == "schedule.reboot.set" {
        let params = request
            .params
            .as_object()
            .filter(|params| params.len() == 2);
        let enabled = params
            .and_then(|params| params.get("enabled"))
            .and_then(Value::as_bool);
        let time = params
            .and_then(|params| params.get("time"))
            .and_then(Value::as_str);
        match (schedule, enabled, time) {
            (Some(schedule), Some(enabled), Some(time)) => {
                let conflict = reboot_schedule::oem_conflict().await;
                match schedule.lock().await.set(enabled, time, conflict) {
                    Ok(_) => None,
                    Err(_) => Some("invalid_parameter"),
                }
            }
            _ => Some("invalid_parameter"),
        }
    } else if request.action == "schedule.task.put" {
        match (
            cloud_app,
            serde_json::from_value::<TaskInput>(request.params.clone()),
        ) {
            (Some(app), Ok(input)) => match app.panel_task_upsert(input).await {
                Ok(_) => None,
                Err(error) if error == "task_storage_failed" => Some("device_call_failed"),
                Err(_) => Some("invalid_parameter"),
            },
            _ => Some("invalid_parameter"),
        }
    } else if request.action == "schedule.task.remove" {
        let id = request
            .params
            .as_object()
            .filter(|params| params.len() == 1)
            .and_then(|params| params.get("id"))
            .and_then(Value::as_str);
        match (cloud_app, id) {
            (Some(app), Some(id)) => match app.panel_task_remove(id).await {
                Ok(_) => None,
                Err(error) if error == "task_storage_failed" => Some("device_call_failed"),
                Err(_) => Some("invalid_parameter"),
            },
            _ => Some("invalid_parameter"),
        }
    } else if request.action == "sms.forward.set" {
        match (
            cloud_app,
            serde_json::from_value::<SmsForwardUpdate>(request.params.clone()),
        ) {
            (Some(app), Ok(input)) => match tokio::time::timeout(
                Duration::from_secs(40),
                app.panel_sms_forward_update(input),
            )
            .await
            {
                Ok(Ok(_)) => None,
                Ok(Err(error))
                    if error == "forward_storage_failed" || error == "sms_list_unavailable" =>
                {
                    Some("device_call_failed")
                }
                Ok(Err(_)) => Some("invalid_parameter"),
                Err(_) => Some("device_call_timeout"),
            },
            _ => Some("invalid_parameter"),
        }
    } else if request.action == "sms.forward.test" {
        if !request
            .params
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
        {
            Some("invalid_parameter")
        } else if let Some(app) = cloud_app {
            match tokio::time::timeout(Duration::from_secs(40), app.panel_sms_forward_test()).await
            {
                Ok(Ok(_)) => None,
                Ok(Err(_)) => Some("device_call_failed"),
                Err(_) => Some("device_call_timeout"),
            }
        } else {
            Some("unsupported_action")
        }
    } else if request.action == "speedtest.start" {
        match speedtest {
            Some(speedtest) => match SpeedTest::start(speedtest.clone(), &request.params).await {
                Ok(_) => None,
                Err(_) => Some("invalid_parameter"),
            },
            None => Some("unsupported_action"),
        }
    } else if request.action == "speedtest.stop" {
        if !request
            .params
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
        {
            Some("invalid_parameter")
        } else if let Some(speedtest) = speedtest {
            SpeedTest::stop(speedtest.clone()).await;
            None
        } else {
            Some("unsupported_action")
        }
    } else if request.action == "cloud.remote_features.set" {
        match (
            cloud_app,
            serde_json::from_value::<RemoteFeatures>(request.params.clone()),
        ) {
            (Some(app), Ok(input)) => match app.cloud_panel_save_features(input).await {
                Ok(_) => None,
                Err(error) if error == "保存配置失败" => Some("device_call_failed"),
                Err(_) => Some("invalid_parameter"),
            },
            _ => Some("invalid_parameter"),
        }
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
    feeds: PanelFeeds,
    control_mode: bool,
    config: &Config,
    url: &str,
    token: &str,
    ttl: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let PanelFeeds {
        state: state_rx,
        history: mut history_rx,
        schedule,
        speedtest,
        cloud_app,
    } = feeds;
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
    let mut history = history_rx.borrow_and_update().clone();
    let cloud_management = if let Some(app) = &cloud_app {
        Some(app.cloud_panel_config().await)
    } else {
        None
    };
    let mut scheduled_tasks = if let Some(app) = &cloud_app {
        Some(app.panel_task_status().await)
    } else {
        None
    };
    let mut sms_forward_status = if let Some(app) = &cloud_app {
        Some(app.panel_sms_forward_status().await)
    } else {
        None
    };
    let Some(first) = state_message_with_config(
        &last,
        if control_mode { 2 } else { 1 },
        None,
        None,
        None,
        Some(&history),
        PanelManagement {
            cloud: cloud_management.as_ref(),
            tasks: scheduled_tasks.as_ref(),
            sms_forward: sms_forward_status.as_ref(),
        },
    ) else {
        return;
    };
    if socket.send(first).await.is_err() {
        return;
    }
    // Extra configuration is fetched only while a remote panel is open. It
    // contains reviewed UI fields, never Wi-Fi keys or raw vendor responses.
    let (mut wifi_config, mut apn_config, mut cooling_config) = if cloud_app.is_some() {
        let (wifi_result, apn_result, cooling_result) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(6), wifi_panel_config()),
            tokio::time::timeout(Duration::from_secs(6), apn_panel_config()),
            tokio::time::timeout(Duration::from_secs(6), cooling::panel_config())
        );
        (
            wifi_result.ok().flatten(),
            apn_result.ok().flatten(),
            cooling_result.ok().flatten(),
        )
    } else {
        (None, None, None)
    };
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
                let next_history = history_rx.borrow_and_update().clone();
                let next_tasks = if let Some(app)=&cloud_app {Some(app.panel_task_status().await)} else {None};
                let next_sms_forward = if let Some(app)=&cloud_app {Some(app.panel_sms_forward_status().await)} else {None};
                if next != last || config_pending || next_history != history || next_tasks != scheduled_tasks || next_sms_forward != sms_forward_status {
                    let Some(message) = state_message_with_config(&next, if control_mode {2} else {1}, wifi_config.as_ref(), apn_config.as_ref(), cooling_config.as_ref(), Some(&next_history), PanelManagement {cloud:cloud_management.as_ref(),tasks:next_tasks.as_ref(),sms_forward:next_sms_forward.as_ref()}) else { return; };
                    if socket.send(message).await.is_err() { return; }
                    last = next;
                    history = next_history;
                    scheduled_tasks = next_tasks;
                    sms_forward_status = next_sms_forward;
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
                    let refresh_tasks = request.action.starts_with("schedule.task.");
                    let refresh_sms_forward = request.action.starts_with("sms.forward.");
                    if socket.send(control_result(request, schedule.as_ref(), speedtest.as_ref(), cloud_app.as_ref()).await).await.is_err() { return; }
                    if refresh_wifi || refresh_apn || refresh_cooling || refresh_tasks || refresh_sms_forward {
                        if refresh_wifi {wifi_config = tokio::time::timeout(Duration::from_secs(6), wifi_panel_config()).await.ok().flatten();}
                        if refresh_apn {apn_config = tokio::time::timeout(Duration::from_secs(6), apn_panel_config()).await.ok().flatten();}
                        if refresh_cooling {cooling_config = tokio::time::timeout(Duration::from_secs(6), cooling::panel_config()).await.ok().flatten();}
                        if refresh_tasks { scheduled_tasks = if let Some(app)=&cloud_app {Some(app.panel_task_status().await)} else {None}; }
                        if refresh_sms_forward {sms_forward_status = if let Some(app)=&cloud_app {Some(app.panel_sms_forward_status().await)} else {None};}
                        if let Some(message) = state_message_with_config(&last, 2, wifi_config.as_ref(), apn_config.as_ref(), cooling_config.as_ref(), Some(&history), PanelManagement {cloud:cloud_management.as_ref(),tasks:scheduled_tasks.as_ref(),sms_forward:sms_forward_status.as_ref()})
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
        fields.insert(
            "reboot_schedule".into(),
            json!({"supported":true,"enabled":false,"time":"02:03"}),
        );
        fields.insert(
            "speedtest".into(),
            json!({"supported":true,"state":"idle","provider":"cloudflare"}),
        );
        fields.insert("future_secret".into(), json!({"token":"must-not-leak"}));
        fields.insert(
            "interfaces".into(),
            json!({"wan4":{"ipv4":[{"address":"198.51.100.7","token":"must-not-leak"}]},
                "wan6":{"ipv6":[{"address":"2001:db8::7"},{"address":"not-an-ip"}]},
                "cellular":{"password":"must-not-leak"}}),
        );
        fields.insert(
            "uci_device_info".into(),
            json!({
                "mac_address":"aa:bb:cc:dd:ee:ff", "modem_msn":"fixture-msn",
                "password":"must-not-leak", "oversized":"x".repeat(200)
            }),
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
        assert_eq!(
            serde_json::from_str::<Value>(&raw).unwrap()["snapshot"]["reboot_schedule"]["time"],
            "02:03"
        );
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["snapshot"]["speedtest"]["provider"], "cloudflare");
        assert_eq!(parsed["snapshot"]["net"]["type"], "SA");
        assert!(parsed["snapshot"].get("future_secret").is_none());
        assert_eq!(
            parsed["snapshot"]["uci_device_info"]["mac_address"],
            "aa:bb:cc:dd:ee:ff"
        );
        assert_eq!(
            parsed["snapshot"]["uci_device_info"]["modem_msn"],
            "fixture-msn"
        );
        assert!(
            parsed["snapshot"]["uci_device_info"]
                .get("password")
                .is_none()
        );
        assert_eq!(
            parsed["snapshot"]["interfaces"]["wan4"]["ipv4"][0]["address"],
            "198.51.100.7"
        );
        assert_eq!(
            parsed["snapshot"]["interfaces"]["wan6"]["ipv6"][0]["address"],
            "2001:db8::7"
        );
        assert_eq!(
            parsed["snapshot"]["interfaces"]["wan6"]["ipv6"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(!raw.contains("must-not-leak"));
        assert_eq!(parsed["protocol_version"], 1);
        assert!(parsed["snapshot"].get("cloud_management").is_none());
    }

    #[test]
    fn cloud_management_is_only_added_to_authorized_panel_frames() {
        let snapshot = Snapshot {
            ts: 7,
            datad: DatadVersion::default(),
            fields: Map::new(),
        };
        let reviewed =
            json!({"state":"connected","enabled":true,"password_configured":true,"services":[]});
        let Message::Text(raw) = state_message_with_config(
            &snapshot,
            1,
            None,
            None,
            None,
            None,
            PanelManagement {
                cloud: Some(&reviewed),
                tasks: None,
                sms_forward: None,
            },
        )
        .unwrap() else {
            panic!("expected text frame")
        };
        let frame: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(frame["snapshot"]["cloud_management"], reviewed);
        assert!(
            frame["snapshot"]["cloud_management"]
                .get("password")
                .is_none()
        );
    }

    #[test]
    fn scheduled_tasks_only_leave_through_an_open_panel_session() {
        let snapshot = Snapshot {
            ts: 7,
            datad: DatadVersion::default(),
            fields: Map::new(),
        };
        let tasks = json!({"supported":true,"tasks":[{"id":"fixture","time":"23:59","repeat_daily":false,"action":"nfc.set","params":{"enabled":true},"last_attempt_date":"","has_triggered":false,"last_result":""}],"error":null});
        let Message::Text(plain) = state_message(&snapshot).unwrap() else {
            panic!("expected state");
        };
        let plain: Value = serde_json::from_str(&plain).unwrap();
        assert!(plain["snapshot"].get("scheduled_tasks").is_none());
        let Message::Text(raw) = state_message_with_config(
            &snapshot,
            2,
            None,
            None,
            None,
            None,
            PanelManagement {
                cloud: None,
                tasks: Some(&tasks),
                sms_forward: None,
            },
        )
        .unwrap() else {
            panic!("expected panel state");
        };
        let frame: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(frame["snapshot"]["scheduled_tasks"], tasks);
    }

    #[test]
    fn sms_forward_status_is_panel_only_and_contains_no_destination() {
        let snapshot = Snapshot {
            ts: 7,
            datad: DatadVersion::default(),
            fields: Map::new(),
        };
        let status = json!({"supported":true,"enabled":false,"method":"webhook","webhook_configured":true,"dingtalk_configured":false,"dingtalk_secret_configured":false,"last_result":"idle"});
        let Message::Text(plain) = state_message(&snapshot).unwrap() else {
            panic!("expected state")
        };
        let plain: Value = serde_json::from_str(&plain).unwrap();
        assert!(plain["snapshot"].get("sms_forward").is_none());
        let Message::Text(raw) = state_message_with_config(
            &snapshot,
            2,
            None,
            None,
            None,
            None,
            PanelManagement {
                sms_forward: Some(&status),
                ..Default::default()
            },
        )
        .unwrap() else {
            panic!("expected panel state")
        };
        let frame: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(frame["snapshot"]["sms_forward"], status);
        assert!(!raw.contains("webhook_url"));
    }

    #[test]
    fn daily_history_is_only_in_the_on_demand_panel_frame() {
        let snapshot = Snapshot {
            ts: 7,
            datad: DatadVersion::default(),
            fields: Map::new(),
        };
        let history = [Usage {
            date: "2026-09-29".into(),
            bytes: 1536,
        }];
        let Message::Text(plain) = state_message(&snapshot).unwrap() else {
            panic!("expected a text frame")
        };
        let plain: Value = serde_json::from_str(&plain).unwrap();
        assert!(plain["snapshot"].get("traffic_history").is_none());
        let Message::Text(with_history) = state_message_with_config(
            &snapshot,
            1,
            None,
            None,
            None,
            Some(&history),
            PanelManagement::default(),
        )
        .unwrap() else {
            panic!("expected a text frame")
        };
        let with_history: Value = serde_json::from_str(&with_history).unwrap();
        assert_eq!(
            with_history["snapshot"]["traffic_history"],
            json!([{"date":"2026-09-29","bytes":1536}])
        );
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
        assert!(needs_confirmation("speedtest.start"));
        assert!(needs_confirmation("cloud.remote_features.set"));
        assert!(needs_confirmation("schedule.task.put"));
        assert!(needs_confirmation("schedule.task.remove"));
        assert!(needs_confirmation("sms.forward.set"));
        assert!(needs_confirmation("sms.forward.test"));
        assert!(!needs_confirmation("speedtest.stop"));
        for action in [
            "wifi.set_dual_band",
            "wifi.interface.delete",
            "dns.set",
            "power.direct_supply.set",
            "sleep.set",
            "nfc.set",
            "traffic.set_limit",
            "traffic.set_clear_day",
            "traffic.calibrate",
            "client.unblock",
            "qos.clear",
        ] {
            assert!(
                needs_confirmation(action),
                "{action} must require confirmation"
            );
        }
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
        let Message::Text(reply) = control_result(request, None, None, None).await else {
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
        let Message::Text(reply) =
            control_result(parse_control(&unsupported).unwrap(), None, None, None).await
        else {
            panic!("expected an unsupported-action result")
        };
        assert_eq!(
            serde_json::from_str::<Value>(&reply).unwrap()["code"],
            "unsupported_action"
        );
        let skipped_adb = allowed.replace("device.reboot", "usb.set");
        let Message::Text(reply) =
            control_result(parse_control(&skipped_adb).unwrap(), None, None, None).await
        else {
            panic!("expected USB mode to be excluded from the remote panel")
        };
        assert_eq!(
            serde_json::from_str::<Value>(&reply).unwrap()["code"],
            "unsupported_action"
        );
    }

    #[tokio::test]
    async fn remote_reboot_schedule_requires_confirmation_and_saves_disabled_plan() {
        let dir = std::env::temp_dir().join(format!(
            "datad-panel-schedule-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let manager = Arc::new(Mutex::new(Schedule::load(&dir)));
        let raw = json!({"type":"control","protocol_version":2,"request_id":"a".repeat(32),
            "action":"schedule.reboot.set","params":{"enabled":false,"time":"02:03"},"confirmed":false}).to_string();
        let rejected =
            control_result(parse_control(&raw).unwrap(), Some(&manager), None, None).await;
        let Message::Text(rejected) = rejected else {
            panic!("expected a text result")
        };
        assert_eq!(
            serde_json::from_str::<Value>(&rejected).unwrap()["code"],
            "confirmation_required"
        );
        assert!(!dir.join("reboot-schedule.json").exists());
        let accepted = control_result(
            parse_control(&raw.replace("\"confirmed\":false", "\"confirmed\":true")).unwrap(),
            Some(&manager),
            None,
            None,
        )
        .await;
        let Message::Text(accepted) = accepted else {
            panic!("expected a text result")
        };
        assert_eq!(
            serde_json::from_str::<Value>(&accepted).unwrap()["ok"],
            true
        );
        assert_eq!(manager.lock().await.status()["time"], "02:03");
        std::fs::remove_dir_all(dir).unwrap();
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
        let (_history_sender, history_receiver) = watch::channel(vec![Usage {
            date: "2026-09-29".into(),
            bytes: 1536,
        }]);
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let task = tokio::spawn(async move {
            run(
                PanelFeeds {
                    state: receiver,
                    history: history_receiver,
                    schedule: None,
                    speedtest: None,
                    cloud_app: None,
                },
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
        assert!(
            matches!(first,Message::Text(ref body) if body.contains("\"traffic_history\"") && body.contains("\"bytes\":1536"))
        );
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
        let (_history_sender, history_receiver) = watch::channel(Vec::<Usage>::new());
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let task = tokio::spawn(async move {
            run(
                PanelFeeds {
                    state: receiver,
                    history: history_receiver,
                    schedule: None,
                    speedtest: None,
                    cloud_app: None,
                },
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
