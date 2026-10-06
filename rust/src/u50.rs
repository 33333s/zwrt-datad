//! Original-firmware U50 state collector and platform runtime.
//!
//! OEM cfg/GoAhead and kernel sources are mapped into the public state
//! contract without depending on ZWRT UBus/UCI.
use crate::{
    cloud::{Cloud, QuickConnect},
    command,
    model::{DatadVersion, Snapshot},
    server::App,
    u50_oem::Bridge,
    u50_sys,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

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
    "daily_down_bytes",
    "daily_up_bytes",
    "wifi_onoff_state",
    "wifi_access_sta_num",
    "modem_main_state",
    "pin_status",
    "simcard_active_slot",
    "lte_rsrp",
    "lte_rsrq",
    "lte_snr",
    "wan_active_band",
    "wan_active_channel",
    "wan_lte_ca",
    "lte_pci",
    "wifi_chip_temp",
    "pm_sensor_mdm",
    "data_volume_limit_switch",
    "data_volume_limit_size",
    "data_volume_alert_percent",
    "wifi_chip1_ssid1_access_sta_num",
    "wifi_chip2_ssid1_access_sta_num",
    "wifi_5g_enable",
    "wifi_lbd_enable",
    "wan_connect_status",
    "nr5g_action_band",
    "nr5g_cell_lock",
    "lte_pci_lock",
    "lte_earfcn_lock",
    "nr5g_action_channel",
    "nr5g_pci",
    "Z5g_rsrp",
    "Z5g_SINR",
    "Z5g_rsrq",
    "Z5g_rssi",
    "bandwidth",
    "rmcc",
    "rmnc",
    "rplmn_num",
    "simcard_roam",
    "roam_setting_option",
    "inter_roam_switch",
    "cell_id",
    "nr5g_cell_id",
    "nr5g_tac",
    "lte_rssi",
    "net_select",
    "nr5g_sa_band_lock",
    "nr5g_nsa_band_lock",
    "nr5g_sa_band_factory",
    "nr5g_nsa_band_factory",
    "lte_band_ext_lock",
    "lte_band_lock",
    "lte_band_1_64_factory",
    "hightemp_datalimit_status",
    "imei",
    "sim_imsi",
    "iccid",
    "sim_iccid",
    "msisdn",
    "sim_states",
    "wifi_chip1_ssid1_ssid",
    "wifi_chip1_ssid1_auth_mode",
    "slot1_daily_rx_bytes",
    "slot1_daily_tx_bytes",
    "traffic_total_home_rx",
    "traffic_total_home_tx",
    "traffic_total_roam_rx",
    "traffic_total_roam_tx",
    "realtime_time",
    "peak_rx_bytes",
    "peak_tx_bytes",
    "data_volume_limit_unit",
    "traffic_clear_date",
    "wan_auto_clear_flow_data_switch",
    "flux_limited_disconnect",
    "dhcpEnabled",
    "dhcpStart",
    "dhcpEnd",
    "dhcpLease_hour",
    "prefer_dns_auto",
    "standby_dns_auto",
    "ipv6_prefer_dns_auto",
    "ipv6_standby_dns_auto",
    "wan_v4_dev_name",
    "wan_v6_dev_name",
    "device_alias_name",
    "product_manufacturer",
    "hardware_version",
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

/// Signal-to-noise ratios are fractional dB in the OEM store. Keep the
/// mainline string contract, preserve zero, and reject unknown/sentinel values.
fn source_snr(cfg: &BTreeMap<String, String>, goform: Option<&Value>, key: &str) -> Option<String> {
    let valid = |v: f64| v.is_finite() && (-30.0..=60.0).contains(&v);
    goform
        .and_then(|raw| raw.get(key))
        .and_then(|v| match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        })
        .filter(|v| valid(*v))
        .or_else(|| {
            cfg.get(key)?
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|v| valid(*v))
        })
        .map(|v| v.to_string())
}

fn cfg_bin() -> String {
    std::env::var("ZWRT_DATAD_U50_CFG_BIN").unwrap_or_else(|_| "/usr/bin/cfg".into())
}

async fn enrollment_identity() -> Result<(&'static str, String)> {
    let program = cfg_bin();
    for key in ["modem_msn", "serial_number", "imei"] {
        let Ok(raw) = command::run(&program, ["get", key], Duration::from_secs(2)).await else {
            continue;
        };
        if raw.len() > 128 {
            continue;
        }
        let Ok(value) = String::from_utf8(raw) else {
            continue;
        };
        let value = value.trim();
        let valid = if key == "imei" {
            (14..=16).contains(&value.len()) && value.bytes().all(|c| c.is_ascii_digit())
        } else {
            (6..=64).contains(&value.len())
                && value
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
                && !matches!(
                    value.to_ascii_lowercase().as_str(),
                    "unknown" | "invalid" | "null"
                )
        };
        if valid {
            return Ok((key, value.to_owned()));
        }
    }
    anyhow::bail!("U50 stable device identity unavailable")
}

/// Provision a private cloud config without exposing the hardware identifier
/// or one-time MQTT password through the public U50 state or CLI output.
pub async fn enroll(model: Model, dir: &Path, input: QuickConnect) -> Result<()> {
    ensure!(
        !dir.join("cloud.json").exists(),
        "U50 cloud config already exists; enrollment would replace it"
    );
    let cfg = cfg_values().await?;
    let mut snapshot = from_sources(model, &cfg, None)?;
    let (key, value) = enrollment_identity().await?;
    let mut private_info = Map::new();
    private_info.insert(key.into(), json!(value));
    snapshot
        .fields
        .insert("uci_device_info".into(), Value::Object(private_info));
    let mut cloud = Cloud::load(dir);
    cloud
        .quick_connect(input, &snapshot)
        .map_err(anyhow::Error::msg)?;
    println!(
        "{}",
        json!({"configured":true,"model":model.template(),"password_configured":true})
    );
    Ok(())
}

fn accept_cfg_value(raw: &str) -> Option<&str> {
    let value = raw.trim();
    (!value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control))
        .then_some(value)
}

/// One `cfg show` replaces ~100 `cfg get` forks per sample. Only the keys this
/// collector maps are kept (the dump also contains passwords and tokens, which
/// are dropped before anything else can see them).
fn cfg_from_show(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(key, _)| CFG_KEYS.contains(key))
        .filter_map(|(key, value)| Some((key.to_owned(), accept_cfg_value(value)?.to_owned())))
        .collect()
}

async fn cfg_values() -> Result<BTreeMap<String, String>> {
    let program = cfg_bin();
    let mut values = match command::run(&program, ["show"], Duration::from_secs(4)).await {
        Ok(raw) => cfg_from_show(&String::from_utf8_lossy(&raw)),
        Err(_) => BTreeMap::new(),
    };
    if !values.contains_key("model_name") {
        // Older firmware or a mock without `show`: fall back to per-key reads.
        values.clear();
        for key in CFG_KEYS {
            let Ok(raw) = command::run(&program, ["get", key], Duration::from_secs(2)).await else {
                continue;
            };
            ensure!(raw.len() <= 512, "U50 cfg output too large for {key}");
            let value = String::from_utf8(raw).context("U50 cfg returned invalid UTF-8")?;
            if let Some(value) = accept_cfg_value(&value) {
                values.insert((*key).into(), value.into());
            }
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
    let supported = model == Model::U50S && observed.contains("U50S");
    let mut fields = Map::new();
    fields.insert(
        "device".into(),
        json!({
            "profile": model.template().to_ascii_lowercase(),
            "profile_source": if supported { "oem_u50s" } else { "explicit_candidate" },
            "api_template": model.template(),
            "api_template_label": model.template(),
            "api_template_supported": i64::from(supported),
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
    ] {
        if let Some(value) = source_number(cfg, goform, source, min, max) {
            net.insert(target.into(), json!(value));
        }
    }
    if let Some(value) = source_snr(cfg, goform, "lte_snr") {
        net.insert("lte_snr".into(), json!(value));
    }
    if let Some(value) = cfg.get("ppp_status") {
        net.insert("wan_status".into(), json!(value));
    }
    if let Some(value) = cfg.get("wan_active_band") {
        net.insert("band".into(), json!(normalize_lte_band(value)));
    }
    if let Some(value) = cfg_number(cfg, "wan_active_channel", 0, 1_000_000) {
        net.insert("lte_channel".into(), json!(value));
    }
    if let Some(value) = cfg.get("lte_pci").and_then(|v| hex_number(v, 503)) {
        net.insert("lte_pci".into(), json!(value));
    }
    if let Some(value) = cfg.get("wan_lte_ca") {
        net.insert("lteca".into(), json!(value));
    }
    if let Some(value) = cfg.get("nr5g_action_band") {
        net.insert("nr_band".into(), json!(value));
    }
    if let Some(value) = cfg_number(cfg, "nr5g_action_channel", 0, 3_279_165) {
        net.insert("nr_channel".into(), json!(value));
    }
    if let Some(value) = cfg.get("nr5g_pci").and_then(|v| hex_number(v, 1007)) {
        net.insert("nr_pci".into(), json!(value));
    }
    // Cell-lock readback so clients can show "locked": NR is
    // "pci,arfcn,band,scs" (unlock sentinel `1,1,1,1`), LTE is the
    // pci/earfcn pair (0 = unlocked).
    if let Some(lock) = cfg
        .get("nr5g_cell_lock")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty() && *v != "1,1,1,1")
    {
        let parts: Vec<i64> = lock
            .split(',')
            .filter_map(|p| p.trim().parse().ok())
            .collect();
        if parts.len() == 4 {
            net.insert(
                "nr_cell_lock".into(),
                json!({"pci": parts[0], "arfcn": parts[1], "band": parts[2], "scs": parts[3]}),
            );
        }
    }
    if let (Some(pci), Some(earfcn)) = (
        cfg_number(cfg, "lte_pci_lock", 1, 1007),
        cfg_number(cfg, "lte_earfcn_lock", 1, 262_143),
    ) {
        net.insert(
            "lte_cell_lock".into(),
            json!({"pci": pci, "earfcn": earfcn}),
        );
    }
    if let Some(value) = cfg_number(cfg, "Z5g_rsrp", -160, -20) {
        net.insert("nr_rsrp".into(), json!(value));
    }
    if let Some(value) = source_snr(cfg, goform, "Z5g_SINR") {
        net.insert("nr_snr".into(), json!(value));
    }
    // The firmware has no high-speed-rail source, so `HSR` is omitted (unknown)
    // rather than reported as a constant "off" that the NMS panel would display.
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
        ("daily_down_bytes", "today_rx_bytes"),
        ("daily_up_bytes", "today_tx_bytes"),
    ] {
        if let Some(value) = source_number(cfg, goform, source, 0, i64::MAX) {
            traffic.insert(target.into(), json!(value));
        }
    }
    if let Some(value) = cfg_number(cfg, "data_volume_limit_switch", 0, 1) {
        traffic.insert("limit_enabled".into(), json!(value));
    }
    if !traffic.is_empty() {
        fields.insert("traffic".into(), Value::Object(traffic));
    }

    let mut wlan = Map::new();
    if let Some(value) = cfg
        .get("wifi_onoff_state")
        .filter(|v| *v == "0" || *v == "1")
    {
        wlan.insert("enabled".into(), json!(i64::from(value == "1")));
    }
    for (source, target) in [
        ("wifi_5g_enable", "five_ghz_enabled"),
        ("wifi_lbd_enable", "band_steering_enabled"),
    ] {
        if let Some(value) = cfg_number(cfg, source, 0, 1) {
            wlan.insert(target.into(), json!(value));
        }
    }
    if !wlan.is_empty() {
        fields.insert("wlan".into(), Value::Object(wlan));
    }
    if let Some(value) = cfg_number(cfg, "wifi_access_sta_num", 0, 1024) {
        let mut clients = json!({"total":value,"wifi":value});
        for (source, target) in [
            ("wifi_chip1_ssid1_access_sta_num", "chip1"),
            ("wifi_chip2_ssid1_access_sta_num", "chip2"),
        ] {
            if let Some(count) = cfg_number(cfg, source, 0, 1024) {
                clients[target] = json!(count);
            }
        }
        fields.insert("clients".into(), clients);
    }
    let mut zones = Vec::new();
    for (source, name) in [("wifi_chip_temp", "wifi_chip"), ("pm_sensor_mdm", "modem")] {
        if let Some(value) = cfg_number(cfg, source, -40, 120) {
            zones.push(json!({"name":name,"celsius":value}));
        }
    }
    if !zones.is_empty() {
        fields.insert("thermal".into(), json!({"zones":zones}));
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
    extend_from_cfg(&mut fields, cfg);
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

/// The OEM WebUI renders these cfg values with `parseInt(value, 16)`: cell ids,
/// PCIs and (by the same adapter) the NR TAC are hexadecimal strings.
fn hex_number(value: &str, max: u64) -> Option<i64> {
    let digits = value.trim().trim_start_matches("0x");
    if digits.is_empty() || digits.len() > 16 {
        return None;
    }
    u64::from_str_radix(digits, 16)
        .ok()
        .filter(|v| *v <= max)
        .and_then(|v| i64::try_from(v).ok())
}

/// `wan_active_band` arrives from cfg either already normalized ("B3") or as
/// the firmware's verbose form ("LTE BAND 8"); reduce both to the "B8" form
/// the state schema documents so clients can parse one shape.
fn normalize_lte_band(value: &str) -> String {
    let text = value.trim();
    text.rsplit(' ')
        .find(|token| {
            let digits = token.trim_start_matches(['b', 'B']);
            !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
        })
        .map(|token| format!("B{}", token.trim_start_matches(['b', 'B'])))
        .unwrap_or_else(|| text.to_owned())
}

/// Bands from a `0x…` bitmask where bit 0 is band `offset + 1`.
fn band_mask(value: &str, offset: u32) -> Vec<u32> {
    let digits = value.trim().trim_start_matches("0x");
    let Ok(mask) = u64::from_str_radix(digits, 16) else {
        return Vec::new();
    };
    (0..64)
        .filter(|bit| mask >> bit & 1 == 1)
        .map(|bit| offset + bit + 1)
        .collect()
}

/// A comma-separated band list; invalid entries invalidate the whole list.
fn band_csv(value: &str) -> Vec<u32> {
    let mut bands = Vec::new();
    for part in value.split(',') {
        match part.trim().parse::<u32>() {
            Ok(band) if (1..=512).contains(&band) => bands.push(band),
            _ => return Vec::new(),
        }
    }
    bands.sort_unstable();
    bands.dedup();
    bands
}

fn join_bands(bands: &[u32]) -> String {
    bands
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn insert_text(
    map: &mut Map<String, Value>,
    target: &str,
    cfg: &BTreeMap<String, String>,
    key: &str,
) {
    if let Some(value) = cfg.get(key) {
        map.insert(target.into(), json!(value));
    }
}

fn insert_number(
    map: &mut Map<String, Value>,
    target: &str,
    cfg: &BTreeMap<String, String>,
    key: &str,
    max: i64,
) -> bool {
    if let Some(value) = cfg_number(cfg, key, 0, max) {
        map.insert(target.into(), json!(value));
        true
    } else {
        false
    }
}

fn block<'a>(fields: &'a mut Map<String, Value>, name: &str) -> &'a mut Map<String, Value> {
    let entry = fields
        .entry(name.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(Map::new());
    }
    entry.as_object_mut().expect("object block")
}

/// Everything beyond the first-pass mapping that the OEM `cfg` store provides:
/// registration/cell identity, band locks and capabilities, SIM, Wi-Fi and
/// traffic accounting, DHCP and device labels.
fn extend_from_cfg(fields: &mut Map<String, Value>, cfg: &BTreeMap<String, String>) {
    {
        let net = block(fields, "net");
        insert_number(net, "mcc", cfg, "rmcc", 999);
        insert_number(net, "mnc", cfg, "rmnc", 999);
        if let Some(plmn) = cfg
            .get("rplmn_num")
            .filter(|v| (5..=6).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_digit()))
        {
            net.insert("plmn".into(), json!(plmn));
        }
        insert_text(net, "roaming", cfg, "simcard_roam");
        if let Some(value) = cfg
            .get("roam_setting_option")
            .or_else(|| cfg.get("inter_roam_switch"))
            .filter(|v| matches!(v.as_str(), "on" | "off"))
        {
            net.insert("roaming_allowed".into(), json!(i64::from(value == "on")));
        }
        insert_text(net, "net_select", cfg, "net_select");
        // The firmware's `bandwidth` key follows the active RAT: LTE bandwidth
        // while camped on LTE, NR bandwidth on NR. Route it to the matching
        // field so LTE mode never labels its bandwidth as `nr_bw`.
        // network_type reports "SA"/"ENDC" style labels as well as "NR5G".
        let nr_active = net
            .get("type")
            .and_then(Value::as_str)
            .map(|t| {
                let t = t.to_ascii_uppercase();
                t.contains("5G") || t.contains("NR") || t == "SA"
            })
            .unwrap_or(false);
        if let Some(value) = cfg.get("bandwidth").filter(|v| !v.trim().is_empty()) {
            if nr_active {
                net.insert("nr_bw".into(), json!(value));
            } else {
                net.insert("lte_bw".into(), json!(value));
            }
        }
        if let Some(value) = cfg_number(cfg, "Z5g_rsrq", -50, 0) {
            net.insert("nr_rsrq".into(), json!(value));
        }
        if let Some(value) = cfg
            .get("Z5g_rssi")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| (-140.0..=-20.0).contains(v))
        {
            net.insert("nr_rssi".into(), json!(value.round() as i64));
        }
        if let Some(value) = cfg_number(cfg, "lte_rssi", -140, -20) {
            net.insert("lte_rssi".into(), json!(value));
        }
        if let Some(value) = cfg
            .get("nr5g_cell_id")
            .and_then(|v| hex_number(v, (1 << 36) - 1))
        {
            net.insert("nr_cell_id".into(), json!(value));
        }
        if let Some(value) = cfg
            .get("cell_id")
            .and_then(|v| hex_number(v, (1 << 28) - 1))
        {
            net.insert("lte_cell_id".into(), json!(value));
        }
        if let Some(value) = cfg.get("nr5g_tac").and_then(|v| hex_number(v, 0xFF_FFFF)) {
            net.insert("nr_tac".into(), json!(value));
        }
        for (source, target, alias) in [
            ("nr5g_sa_band_lock", "sa_bands", "nr5g_sa_band_lock"),
            ("nr5g_nsa_band_lock", "nsa_bands", "nr5g_nsa_band_lock"),
        ] {
            let bands = cfg.get(source).map(|v| band_csv(v)).unwrap_or_default();
            if !bands.is_empty() {
                let joined = join_bands(&bands);
                net.insert(target.into(), json!(joined));
                // Mainline-compatible alias so generic clients can verify
                // band locks from /state with the same field names.
                net.insert(alias.into(), json!(joined));
            }
        }
        // The hex mask is authoritative: the firmware's `lte_band_ext_lock`
        // list only refreshes on its own schedule, so a freshly applied lock
        // would otherwise still report the previous bands.
        let lte_locked = match cfg.get("lte_band_lock").map(|v| band_mask(v, 0)) {
            Some(bands) => bands,
            None => cfg
                .get("lte_band_ext_lock")
                .map(|v| band_csv(v))
                .unwrap_or_default(),
        };
        if !lte_locked.is_empty() {
            let joined = join_bands(&lte_locked);
            net.insert("lte_bands".into(), json!(joined.clone()));
            // Mainline exposes lte_ext_band_lock as `lte_band`.
            net.insert("lte_band".into(), json!(joined));
        }
        let lte = cfg
            .get("lte_band_1_64_factory")
            .map(|v| band_mask(v, 0))
            .unwrap_or_default();
        let nr_sa = cfg
            .get("nr5g_sa_band_factory")
            .map(|v| band_csv(v))
            .unwrap_or_default();
        let nr_nsa = cfg
            .get("nr5g_nsa_band_factory")
            .map(|v| band_csv(v))
            .unwrap_or_default();
        let complete = !lte.is_empty() && !nr_sa.is_empty() && !nr_nsa.is_empty();
        if complete {
            net.insert("lte_supported_bands".into(), json!(join_bands(&lte)));
            net.insert("nr_sa_supported_bands".into(), json!(join_bands(&nr_sa)));
            net.insert("nr_nsa_supported_bands".into(), json!(join_bands(&nr_nsa)));
        }
        net.insert(
            "band_capabilities".into(),
            json!({
                "source": if complete { "device_factory_band_lock" } else { "unavailable" },
                "complete": complete,
                "lte": if complete { json!(lte) } else { json!([]) },
                "nr_sa": if complete { json!(nr_sa) } else { json!([]) },
                "nr_nsa": if complete { json!(nr_nsa) } else { json!([]) },
            }),
        );
    }
    {
        let sim = block(fields, "sim");
        insert_text(sim, "imsi", cfg, "sim_imsi");
        insert_text(sim, "msisdn", cfg, "msisdn");
        insert_text(sim, "state", cfg, "sim_states");
        if let Some(iccid) = cfg.get("iccid").or_else(|| cfg.get("sim_iccid")) {
            sim.insert("iccid".into(), json!(iccid));
        }
        if sim.is_empty() {
            fields.remove("sim");
        }
    }
    {
        let system = block(fields, "system");
        insert_text(system, "imei", cfg, "imei");
        insert_text(system, "model", cfg, "device_alias_name");
    }
    {
        let device = block(fields, "device");
        insert_text(device, "vendor", cfg, "product_manufacturer");
        insert_text(device, "alias_name", cfg, "device_alias_name");
        insert_text(device, "market_name", cfg, "device_alias_name");
        insert_text(device, "hardware_version", cfg, "hardware_version");
    }
    {
        let wlan = block(fields, "wlan");
        insert_text(wlan, "ssid", cfg, "wifi_chip1_ssid1_ssid");
        insert_text(wlan, "enc", cfg, "wifi_chip1_ssid1_auth_mode");
        if wlan.is_empty() {
            fields.remove("wlan");
        }
    }
    {
        let traffic = block(fields, "traffic");
        insert_number(traffic, "session_time", cfg, "realtime_time", i64::MAX);
        // OEM key is daily_down_bytes on U50 Pro; ZWRT uses slot1_daily_rx_bytes.
        if !insert_number(
            traffic,
            "day_rx_bytes",
            cfg,
            "slot1_daily_rx_bytes",
            i64::MAX,
        ) {
            insert_number(traffic, "day_rx_bytes", cfg, "daily_down_bytes", i64::MAX);
        }
        if !insert_number(
            traffic,
            "day_tx_bytes",
            cfg,
            "slot1_daily_tx_bytes",
            i64::MAX,
        ) {
            insert_number(traffic, "day_tx_bytes", cfg, "daily_up_bytes", i64::MAX);
        }
        insert_number(traffic, "max_rx_speed", cfg, "peak_rx_bytes", i64::MAX);
        insert_number(traffic, "max_tx_speed", cfg, "peak_tx_bytes", i64::MAX);
        for (target, home, roam) in [
            (
                "total_rx_bytes",
                "traffic_total_home_rx",
                "traffic_total_roam_rx",
            ),
            (
                "total_tx_bytes",
                "traffic_total_home_tx",
                "traffic_total_roam_tx",
            ),
        ] {
            if let Some(home) = cfg_number(cfg, home, 0, i64::MAX) {
                let roam = cfg_number(cfg, roam, 0, i64::MAX).unwrap_or_default();
                traffic.insert(target.into(), json!(home.saturating_add(roam)));
            }
        }
        // Same shapes as the ZWRT collector: `limit` and `clear_day`.
        let mut limit = Map::new();
        insert_number(&mut limit, "enable", cfg, "data_volume_limit_switch", 1);
        if let Some(unit) = cfg.get("data_volume_limit_unit") {
            limit.insert("type".into(), json!(if unit == "time" { 2 } else { 1 }));
        }
        insert_text(&mut limit, "value", cfg, "data_volume_limit_size");
        insert_number(&mut limit, "ratio", cfg, "data_volume_alert_percent", 100);
        if !limit.is_empty() {
            traffic.insert("limit".into(), Value::Object(limit));
        }
        let mut clear_day = Map::new();
        insert_number(&mut clear_day, "clearday", cfg, "traffic_clear_date", 31);
        if let Some(auto) = cfg
            .get("wan_auto_clear_flow_data_switch")
            .filter(|v| matches!(v.as_str(), "on" | "off"))
        {
            clear_day.insert("enable".into(), json!(i64::from(auto == "on")));
        }
        if !clear_day.is_empty() {
            traffic.insert("clear_day".into(), Value::Object(clear_day));
        }
        if traffic.is_empty() {
            fields.remove("traffic");
        }
    }
    {
        let dhcp = block(fields, "dhcp");
        for (source, target) in [
            ("lan_netmask", "netmask"),
            ("dhcpStart", "range_start"),
            ("dhcpEnd", "range_end"),
        ] {
            if let Some(value) = cfg.get(source).filter(|v| v.parse::<Ipv4Addr>().is_ok()) {
                dhcp.insert(target.into(), json!(value));
            }
        }
        if let Some(hours) = cfg_number(cfg, "dhcpLease_hour", 1, 720) {
            dhcp.insert("leasetime".into(), json!(format!("{hours}h")));
        }
        if let Some(value) = cfg
            .get("dhcpEnabled")
            .filter(|v| matches!(v.as_str(), "0" | "1"))
        {
            dhcp.insert("disabled".into(), json!(value == "0"));
        }
        if dhcp.is_empty() {
            fields.remove("dhcp");
        }
    }
    // `hightemp_datalimit_status` exists on this firmware but is empty when no
    // protection is active, so an absent value is level 0 (same levels as the
    // ZWRT collector: 1 = speed limited, 2 = network restricted).
    let raw = cfg
        .get("hightemp_datalimit_status")
        .map_or("", String::as_str);
    let level = match raw {
        "1" => 2,
        "2" => 3,
        _ => 0,
    };
    block(fields, "thermal").insert(
        "protection".into(),
        json!({
            "active": level > 0,
            "level": level,
            "speed_limited": level == 2,
            "network_restricted": level == 3,
            "raw": if raw.is_empty() { Value::Null } else { json!(raw) }
        }),
    );
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

/// Kernel-level data that does not come from the OEM `cfg` store: CPU/memory,
/// thermal zones, battery telemetry, interfaces and online clients. Blocks
/// already produced from `cfg` are merged, never replaced by empty values.
async fn enrich(
    fields: &mut Map<String, Value>,
    cfg: &BTreeMap<String, String>,
    interval: Duration,
) {
    let zones = u50_sys::thermal_zones();
    let cpu_celsius = u50_sys::cpu_celsius(&zones);
    let (cpu_tenths, runtime) =
        crate::state::runtime_with(u50_sys::runtime_zones(&zones), "/etc_rw");
    {
        let system = block(fields, "system");
        for (key, value) in u50_sys::system_block(
            (cpu_tenths >= 0).then_some((cpu_tenths + 5) / 10),
            cpu_celsius,
        ) {
            system.insert(key, value);
        }
    }
    fields.insert("runtime".into(), runtime);
    let thermal = block(fields, "thermal");
    let sysfs = u50_sys::thermal_block(&zones);
    if let Some(list) = sysfs
        .get("zones")
        .filter(|v| v.as_array().is_some_and(|a| !a.is_empty()))
    {
        thermal.insert("zones".into(), list.clone());
    }
    if let Some(cpu) = sysfs.get("cpu_celsius") {
        thermal.insert("cpu_celsius".into(), cpu.clone());
    }
    thermal.entry("modems").or_insert_with(|| json!([]));
    let extra = u50_sys::battery_extra();
    if !extra.is_empty() {
        let battery = block(fields, "battery");
        for (key, value) in extra {
            battery.entry(key).or_insert(value);
        }
    }
    let mcc = cfg_number(cfg, "rmcc", 0, 999).unwrap_or_default();
    let mnc = cfg_number(cfg, "rmnc", 0, 999).unwrap_or_default();
    let qos = crate::qos::read_u50_for_plmn(mcc, mnc);
    let available = qos.qci > 0 || !qos.ambr_dl.is_empty() || !qos.ambr_ul.is_empty();
    fields.insert(
        "qos".into(),
        json!({
            "qci":qos.qci, "ambr_dl":qos.ambr_dl, "ambr_ul":qos.ambr_ul,
            "available":available, "source":if available {"oem_key_log"} else {"unavailable"},
            "reason":if available {""} else {"no_oem_qos_sample"}
        }),
    );
    fields.insert("interfaces".into(), u50_sys::interfaces(cfg).await);
    fields.insert("u50_diag".into(), crate::u50_diag::block());
    fields.insert("u50_signaling".into(), crate::u50_signal::block());
    if let Some(clients) = u50_sys::clients().await {
        // The OEM per-chip counters stay when they are present.
        if let Some(Value::Object(old)) = fields.get("clients") {
            let mut merged = old.clone();
            if let Value::Object(new) = clients {
                merged.extend(new);
            }
            fields.insert("clients".into(), Value::Object(merged));
        } else {
            fields.insert("clients".into(), clients);
        }
    }
    fields.insert(
        "sample_interval_ms".into(),
        json!(u64::try_from(interval.as_millis()).unwrap_or(u64::MAX)),
    );
}

/// Everything needed to sample the device; cloneable, cheap.
#[derive(Clone)]
pub struct Collector {
    client: reqwest::Client,
    url: reqwest::Url,
    pub(crate) model: Model,
    interval: Duration,
}

impl Collector {
    pub async fn snapshot(&self) -> Result<Snapshot> {
        collect(&self.client, &self.url, self.model, self.interval).await
    }
}

async fn collect(
    client: &reqwest::Client,
    url: &reqwest::Url,
    model: Model,
    interval: Duration,
) -> Result<Snapshot> {
    let cfg = cfg_values().await?;
    let goform = fetch_goform(client, url, &cfg).await.ok();
    let mut snapshot = from_sources(model, &cfg, goform.as_ref())?;
    enrich(&mut snapshot.fields, &cfg, interval).await;
    Ok(snapshot)
}

/// U50 runtime listener and feature options.
#[derive(Clone, Default)]
pub struct RunOptions {
    pub enable_webshell: bool,
    /// Enable the read-only signaling capture worker (diag DCI client).
    pub signaling: bool,
    pub lan_addr: Option<SocketAddr>,
    pub auth_token: Option<String>,
    /// Directory for datad's own state (cloud.json, OTA, schedules, SMS
    /// forwarding); must be on the same volume as the running executable.
    pub data_dir: PathBuf,
}

/// Run the mainline runtime on the U50S: the same `App` as on ARM64 (cloud, OTA,
/// WebShell, schedules, SMS forwarding, panel) with the platform seams served by
/// `u50_ctl`.
pub async fn run(
    model: Model,
    goform_url: &str,
    bind: SocketAddr,
    once: bool,
    interval: Duration,
    options: RunOptions,
) -> Result<()> {
    let RunOptions {
        enable_webshell,
        signaling,
        lan_addr,
        auth_token,
        data_dir,
    } = options;
    ensure!(
        bind.ip().is_loopback(),
        "U50 datad only listens on the loopback interface"
    );
    let url = validate_goform_url(goform_url)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    if signaling {
        crate::u50_signal::enable();
        crate::u50_signal::configure(&data_dir);
    }
    let initial = if signaling && once {
        // Give the worker a moment to register and produce a first status.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let snapshot = collect(&client, &url, model, interval).await?;
        crate::u50_signal::shutdown();
        snapshot
    } else {
        collect(&client, &url, model, interval).await?
    };
    if once {
        println!("{}", serde_json::to_string(&initial)?);
        return Ok(());
    }
    let host = initial
        .fields
        .get("u50_cfg")
        .and_then(|v| v.get("lan_ipaddr"))
        .and_then(Value::as_str)
        .context("U50 LAN address unavailable")?
        .to_owned();
    let bridge = Bridge::new(client.clone(), url.clone(), host).map_err(anyhow::Error::msg)?;
    crate::u50_ctl::install(
        Collector {
            client,
            url,
            model,
            interval,
        },
        bridge,
    );
    let local_requires_auth = lan_addr.is_none() && auth_token.is_some();
    let app = App::new(data_dir, interval, auth_token, false, enable_webshell).await?;
    app.spawn_reboot_schedule();
    app.spawn_task_schedule();
    app.spawn_sms_forward();
    let result = if let Some(lan_addr) = lan_addr {
        tokio::try_join!(
            app.clone().serve(bind, false, false),
            app.serve(lan_addr, true, true)
        )
        .map(|_| ())
    } else {
        app.serve(bind, local_requires_auth, false)
            .await
            .map(|_| ())
    };
    if signaling {
        crate::u50_signal::shutdown();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fractional_snr_preserves_zero_and_rejects_sentinels() {
        let mut cfg = BTreeMap::from([
            ("model_name".into(), "U50S".into()),
            ("lte_snr".into(), "4.4".into()),
        ]);
        assert_eq!(
            from_sources(Model::U50S, &cfg, None).unwrap().fields["net"]["lte_snr"],
            "4.4"
        );
        for value in ["0", "-2.5", "13.6"] {
            cfg.insert("lte_snr".into(), value.into());
            cfg.insert("Z5g_SINR".into(), value.into());
            let state = from_sources(Model::U50S, &cfg, None).unwrap();
            assert_eq!(state.fields["net"]["lte_snr"], value);
            assert_eq!(state.fields["net"]["nr_snr"], value);
        }
        for value in ["NaN", "inf", "255", "-32768", "--", ""] {
            cfg.insert("lte_snr".into(), value.into());
            cfg.insert("Z5g_SINR".into(), value.into());
            let state = from_sources(Model::U50S, &cfg, None).unwrap();
            assert!(state.fields["net"].get("lte_snr").is_none());
            assert!(state.fields["net"].get("nr_snr").is_none());
        }
        cfg.insert("lte_snr".into(), "6.2".into());
        assert_eq!(
            source_snr(&cfg, Some(&json!({"lte_snr":"255"})), "lte_snr"),
            Some("6.2".into())
        );
        assert_eq!(
            source_snr(&cfg, Some(&json!({"lte_snr":1.25})), "lte_snr"),
            Some("1.25".into())
        );
    }

    #[test]
    fn radio_identity_bounds_match_lte_and_nr() {
        let cfg = BTreeMap::from([
            ("model_name".into(), "U50S".into()),
            ("lte_pci".into(), "504".into()),
            ("nr5g_pci".into(), "3EF".into()),
            ("nr5g_action_channel".into(), "2070833".into()),
        ]);
        let state = from_sources(Model::U50S, &cfg, None).unwrap();
        assert!(state.fields["net"].get("lte_pci").is_none());
        assert_eq!(state.fields["net"]["nr_pci"], 1007);
        assert_eq!(state.fields["net"]["nr_channel"], 2070833);
    }
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
            ("wan_active_band".into(), "B3".into()),
            ("wan_active_channel".into(), "1650".into()),
            ("wifi_chip_temp".into(), "43".into()),
            ("pm_sensor_mdm".into(), "41".into()),
            ("data_volume_limit_switch".into(), "0".into()),
            ("wifi_5g_enable".into(), "0".into()),
            ("nr5g_action_band".into(), "n78".into()),
            ("nr5g_cell_lock".into(), "393,627264,78,30".into()),
            ("lte_pci_lock".into(), "0".into()),
            ("lte_earfcn_lock".into(), "0".into()),
            ("nr5g_action_channel".into(), "633984".into()),
            ("Z5g_rsrp".into(), "-84".into()),
            ("Z5g_SINR".into(), "2.5".into()),
        ]);
        let value = from_sources(Model::U50Pro, &cfg, None).unwrap();
        assert_eq!(value.fields["device"]["api_template_supported"], 0);
        assert_eq!(value.fields["system"]["sw_version"], "B02");
        assert_eq!(value.fields["dhcp"]["ip"], "192.168.0.1");
        assert_eq!(value.fields["net"]["type"], "LTE");
        assert_eq!(value.fields["net"]["bars"], 5);
        assert_eq!(value.fields["net"]["lte_rsrp"], -83);
        assert!(value.fields["net"].get("HSR").is_none());
        assert_eq!(value.fields["battery"]["percent"], 66);
        assert_eq!(value.fields["battery"]["temp"], 34);
        assert_eq!(value.fields["traffic"]["rx_speed"], 1814);
        assert_eq!(value.fields["traffic"]["month_tx_bytes"], 211028951);
        assert_eq!(value.fields["wlan"]["enabled"], 1);
        assert_eq!(value.fields["clients"]["total"], 2);
        assert_eq!(value.fields["sim"]["current_slot"], 1);
        assert_eq!(value.fields["net"]["band"], "B3");
        assert_eq!(normalize_lte_band("LTE BAND 8"), "B8");
        assert_eq!(normalize_lte_band("LTE BAND 41"), "B41");
        assert_eq!(normalize_lte_band("B3"), "B3");
        assert_eq!(value.fields["net"]["lte_channel"], 1650);
        assert_eq!(value.fields["net"]["nr_band"], "n78");
        assert_eq!(value.fields["net"]["nr_channel"], 633984);
        assert_eq!(value.fields["net"]["nr_rsrp"], -84);
        assert_eq!(value.fields["net"]["nr_snr"], "2.5");
        assert!(value.fields["net"].get("nr_pci").is_none());
        assert_eq!(value.fields["traffic"]["limit_enabled"], 0);
        assert_eq!(value.fields["wlan"]["five_ghz_enabled"], 0);
        assert_eq!(value.fields["thermal"]["zones"][0]["celsius"], 43);
        assert_eq!(value.fields["u50_sources"]["goform"], "unavailable");
    }
    #[test]
    fn maps_extended_cfg_like_a_real_u50s_on_sa() {
        let cfg: BTreeMap<String, String> = [
            ("model_name", "ZTE U50S"),
            ("network_type", "SA"),
            ("nr5g_cell_lock", "393,627264,78,30"),
            ("lte_pci_lock", "0"),
            ("lte_earfcn_lock", "0"),
            ("rmcc", "460"),
            ("rmnc", "1"),
            ("rplmn_num", "46001"),
            ("roam_setting_option", "off"),
            ("net_select", "4G_AND_5G"),
            ("bandwidth", "100MHz"),
            ("Z5g_rsrq", "-11"),
            ("Z5g_rssi", "-71.2"),
            ("nr5g_pci", "384"),
            ("nr5g_cell_id", "32343f007"),
            ("nr5g_tac", "320903"),
            ("nr5g_sa_band_lock", "5,7,78,257,258"),
            ("nr5g_sa_band_factory", "1,3,5,8,28,41,78"),
            ("nr5g_nsa_band_factory", "1,3,5,8,28,41,78"),
            // A freshly applied lock (bands 1,3,5) while the firmware's ext
            // list still shows the previous set: the hex mask must win.
            ("lte_band_lock", "0x15"),
            ("lte_band_ext_lock", "1,3,5,8,34,39,40,41"),
            ("lte_band_1_64_factory", "0x1c200000095"),
            ("sim_imsi", "460010000000000"),
            ("iccid", "89860100000000000000"),
            ("slot1_daily_rx_bytes", "100"),
            ("traffic_total_home_rx", "1000"),
            ("traffic_total_roam_rx", "5"),
            ("dhcpStart", "192.168.0.2"),
            ("dhcpEnd", "192.168.0.253"),
            ("dhcpLease_hour", "24"),
            ("dhcpEnabled", "1"),
            ("lan_netmask", "255.255.255.0"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let state = from_sources(Model::U50S, &cfg, None).unwrap().fields;
        let net = &state["net"];
        assert_eq!(
            (net["mcc"].as_i64(), net["mnc"].as_i64()),
            (Some(460), Some(1))
        );
        assert_eq!(net["plmn"], "46001");
        assert_eq!(net["roaming_allowed"], 0);
        // OEM WebUI parses PCI, cell id and TAC with parseInt(x, 16).
        assert_eq!(net["nr_pci"], 0x384);
        assert_eq!(net["nr_cell_id"], 0x32343f007_i64);
        assert_eq!(net["nr_tac"], 0x320903);
        assert_eq!(state["device"]["api_template_supported"], 1);
        assert_eq!(net["nr_rssi"], -71);
        assert_eq!(net["nr_bw"], "100MHz");
        assert_eq!(net["nr_cell_lock"]["pci"], 393);
        assert_eq!(net["nr_cell_lock"]["arfcn"], 627264);
        assert!(net.get("lte_cell_lock").is_none());
        assert_eq!(net["sa_bands"], "5,7,78,257,258");
        assert_eq!(net["nr5g_sa_band_lock"], "5,7,78,257,258");
        assert_eq!(state["dhcp"]["leasetime"], "24h");
        assert_eq!(net["lte_bands"], "1,3,5");
        assert_eq!(net["lte_band"], "1,3,5");
        assert_eq!(net["lte_supported_bands"], "1,3,5,8,34,39,40,41");
        assert_eq!(net["nr_sa_supported_bands"], "1,3,5,8,28,41,78");
        assert_eq!(net["band_capabilities"]["complete"], true);
        assert_eq!(state["sim"]["imsi"], "460010000000000");
        assert_eq!(state["traffic"]["day_rx_bytes"], 100);
        assert_eq!(state["traffic"]["total_rx_bytes"], 1005);
        assert_eq!(state["dhcp"]["range_end"], "192.168.0.253");
        assert_eq!(state["dhcp"]["disabled"], false);
        assert_eq!(state["thermal"]["protection"]["level"], 0);
        // Secrets and unmapped keys never leak into the snapshot.
        assert!(!Value::Object(state).to_string().contains("password"));
    }

    #[test]
    fn dhcp_lease_uses_oem_hours_and_rejects_invalid_or_legacy_keys() {
        let mut cfg = BTreeMap::from([
            ("model_name".into(), "U50S".into()),
            ("integrate_version".into(), "B02".into()),
            ("lan_ipaddr".into(), "192.168.0.1".into()),
            ("dhcpLease".into(), "86400".into()),
        ]);
        for raw in ["", "0", "721", "24s", "not-a-number"] {
            cfg.insert("dhcpLease_hour".into(), raw.into());
            let state = from_sources(Model::U50S, &cfg, None).unwrap();
            assert!(state.fields["dhcp"].get("leasetime").is_none());
        }
        for raw in ["1", "24", "720"] {
            cfg.insert("dhcpLease_hour".into(), raw.into());
            let state = from_sources(Model::U50S, &cfg, None).unwrap();
            assert_eq!(state.fields["dhcp"]["leasetime"], format!("{raw}h"));
        }
    }

    #[test]
    fn cfg_show_keeps_only_mapped_keys() {
        let dump = "model_name=ZTE U50S\nadmin_password=secret\nzte_web_cookie=abc\nrmcc=460\nnotakeyline\nlte_pci=\n";
        let parsed = cfg_from_show(dump);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed["rmcc"], "460");
        assert!(!parsed.values().any(|v| v == "secret"));
    }

    #[test]
    fn band_decoding_is_strict() {
        assert_eq!(
            band_mask("0x1c200000095", 0),
            vec![1, 3, 5, 8, 34, 39, 40, 41]
        );
        assert_eq!(band_csv("78,1,3,3"), vec![1, 3, 78]);
        assert!(band_csv("1,x").is_empty());
        assert!(band_csv("0").is_empty());
        assert_eq!(hex_number("0x1F", 100), Some(31));
        assert_eq!(hex_number("zz", 100), None);
        assert_eq!(hex_number("ffff", 100), None);
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
