//! The ZTE U30 Pro running native OpenWrt (board name `zte,u30pro`).
//!
//! None of the ZTE vendor objects (`zwrt_*`, `zte_nwinfo_api`, ...) exist on that system.  This backend reads and controls the
//! device only through ubus: the device's own services (`u30pro-modem`, `u30pro-native-control`, `u30pro-cpu`,
//! `u30pro-storage`, `u30pro-firmware`), the standard OpenWrt ones (`system`, `network`, `network.interface.*`, `network.device`,
//! `hostapd.*`, `luci-rpc`, `session`) and the `uci` object; the Wi-Fi is the standard `/etc/config/wireless`.  It sends no AT commands and reads no vendor files or sysfs: the
//! modem is asked through `u30pro-modem snapshot`, which shares one cached query with the device's screen.  Blocks and fields
//! keep the shape of docs/STATE_SCHEMA.md; what this device does not report is left out rather than faked.

use crate::{control::Outcome, model::Snapshot, state::ubus};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    sync::OnceLock,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

pub const BOARD: &str = "zte,u30pro";
static ACTIVE: OnceLock<bool> = OnceLock::new();

/// Decides once, at startup, whether datad runs on a U30 Pro with native OpenWrt.  `ZWRT_DATAD_BACKEND=u30pro` forces this
/// backend and any other value (e.g. `zwrt`) the ZTE one.
pub async fn init() -> bool {
    let on = match std::env::var("ZWRT_DATAD_BACKEND").ok().as_deref() {
        Some("u30pro") => true,
        Some(_) => false,
        None => ubus("system", "board", json!({}))
            .await
            .ok()
            .and_then(|board| {
                board
                    .get("board_name")
                    .and_then(Value::as_str)
                    .map(|name| name == BOARD)
            })
            .unwrap_or(false),
    };
    *ACTIVE.get_or_init(|| on)
}

pub fn active() -> bool {
    ACTIVE.get().copied().unwrap_or(false)
}

/// Updates of datad itself come with the device's package feed (apk), not from the ZWRT release channel.
pub const UPDATE_REFUSED: &str = "datad on the U30 Pro is updated through the device package feed (apk upgrade zwrt-datad)";

// ---------------------------------------------------------------------------------------------------------------- cache

/// One ubus answer that is asked again only after `max_age`; a failed call keeps the previous answer.
#[derive(Default)]
struct Timed {
    at: Option<Instant>,
    value: Value,
}

impl Timed {
    async fn get(&mut self, max_age: Duration, service: &str, method: &str, args: Value) -> Value {
        if self.at.is_none_or(|at| at.elapsed() >= max_age) {
            if let Ok(value) = ubus(service, method, args).await {
                self.value = value;
            }
            self.at = Some(Instant::now());
        }
        self.value.clone()
    }
    fn invalidate(&mut self) {
        self.at = None;
    }
}

/// Byte rates of one network device from two counter readings.
#[derive(Default)]
struct Rate {
    last: Option<(Instant, u64, u64)>,
    rx: i64,
    tx: i64,
    max_rx: i64,
    max_tx: i64,
    window_ms: i64,
}

impl Rate {
    fn update(&mut self, rx: Option<u64>, tx: Option<u64>) {
        let (Some(rx), Some(tx)) = (rx, tx) else {
            return;
        };
        let now = Instant::now();
        match self.last {
            Some((at, last_rx, last_tx)) if rx >= last_rx && tx >= last_tx => {
                let ms = now.duration_since(at).as_millis() as i64;
                if ms < 200 {
                    return;
                }
                self.rx = (rx - last_rx) as i64 * 1000 / ms;
                self.tx = (tx - last_tx) as i64 * 1000 / ms;
                self.max_rx = self.max_rx.max(self.rx);
                self.max_tx = self.max_tx.max(self.tx);
                self.window_ms = ms;
            }
            // first reading, or the counters restarted (the interface was recreated)
            _ => {
                self.rx = 0;
                self.tx = 0;
            }
        }
        self.last = Some((now, rx, tx));
    }
}

#[derive(Default)]
struct Cache {
    board: Timed,
    device: Timed,
    firmware: Timed,
    bands: Timed,
    qos: Timed,
    storage: Timed,
    stations: Timed,
    leases: Timed,
    wifi: Timed,
    network_uci: Timed,
    dhcp_uci: Timed,
    sms: Timed,
    wan: Rate,
    lan: Rate,
    /// SMS id (first storage index) -> all storage indexes of the (possibly multi-part) message
    sms_parts: BTreeMap<i64, Vec<i64>>,
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}

// ---------------------------------------------------------------------------------------------------------------- values

fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|v| v.is_finite())
}

fn int(value: &Value) -> Option<i64> {
    number(value).map(|v| v.round() as i64)
}

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        // uci list options: the first entry
        Value::Array(list) => list.first().map(text).unwrap_or_default(),
        _ => String::new(),
    }
}

fn ints(value: &Value) -> Vec<i64> {
    value
        .as_array()
        .map(|list| list.iter().filter_map(int).collect())
        .unwrap_or_default()
}

fn join(list: &[i64]) -> String {
    list.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
}

fn scaled(value: &Value, factor: f64) -> Value {
    number(value)
        .map(|v| json!((v * factor).round() as i64))
        .unwrap_or(Value::Null)
}

fn or_object(value: Result<Value, String>) -> Value {
    value.unwrap_or_else(|_| json!({}))
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// `MM-DD HH:MM` of a local time given as seconds (days from the civil calendar algorithm of H. Hinnant)
fn month_day_time(local: i64) -> String {
    let days = local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{month:02}-{day:02} {:02}:{:02}", secs / 3600, secs % 3600 / 60)
}

async fn device_counters(name: &str) -> (Option<u64>, Option<u64>) {
    let status = ubus("network.device", "status", json!({ "name": name }))
        .await
        .unwrap_or_default();
    (
        status["statistics"]["rx_bytes"].as_u64(),
        status["statistics"]["tx_bytes"].as_u64(),
    )
}

/// the value of a uci option from a `uci get {"config": ...}` answer
fn uci(config: &Value, section: &str, option: &str) -> String {
    text(&config["values"][section][option])
}

// ---------------------------------------------------------------------------------------------------------------- state

pub async fn collect(sample_interval_ms: u64) -> Snapshot {
    let secs = Duration::from_secs;
    let mut c = cache().lock().await;
    // serial, like the ZWRT collector: rpcd exec plugins start one process per call
    let board = c.board.get(secs(300), "system", "board", json!({})).await;
    let info = or_object(ubus("system", "info", json!({})).await);
    let modem = or_object(
        ubus(
            "u30pro-modem",
            "snapshot",
            json!({ "max_age": "10", "sinr": "1" }),
        )
        .await,
    );
    let device = c
        .device
        .get(secs(2), "u30pro-native-control", "get", json!({}))
        .await;
    let cpu = or_object(ubus("u30pro-cpu", "get", json!({})).await);
    let firmware = c
        .firmware
        .get(secs(300), "u30pro-firmware", "status", json!({}))
        .await;
    let bands = c
        .bands
        .get(secs(300), "u30pro-modem", "bands_get", json!({}))
        .await;
    let qos = c.qos.get(secs(60), "u30pro-modem", "qos", json!({})).await;
    let storage = c
        .storage
        .get(secs(60), "u30pro-storage", "get", json!({}))
        .await;
    let stations = c
        .stations
        .get(secs(5), "hostapd.wlan0", "get_clients", json!({}))
        .await;
    let leases = c
        .leases
        .get(secs(10), "luci-rpc", "getDHCPLeases", json!({}))
        .await;
    let wifi = wifi_from_uci(
        &c.wifi
            .get(secs(10), "uci", "get", json!({ "config": "wireless" }))
            .await,
    );
    let network_uci = c
        .network_uci
        .get(secs(30), "uci", "get", json!({ "config": "network" }))
        .await;
    let dhcp_uci = c
        .dhcp_uci
        .get(secs(30), "uci", "get", json!({ "config": "dhcp" }))
        .await;
    let sms = c
        .sms
        .get(secs(60), "u30pro-modem", "sms_list", json!({}))
        .await;
    let wan = or_object(ubus("network.interface.wan", "status", json!({})).await);
    let lan = or_object(ubus("network.interface.lan", "status", json!({})).await);
    let wan_device = match text(&wan["l3_device"]) {
        name if !name.is_empty() => name,
        _ => "sipa_eth0".into(),
    };
    let (wan_rx, wan_tx) = device_counters(&wan_device).await;
    let (lan_rx, lan_tx) = device_counters("br-lan").await;
    c.wan.update(wan_rx, wan_tx);
    c.lan.update(lan_rx, lan_tx);

    let mut fields = Map::new();
    let cpu_temp = number(&device["soc_temperature_c"]).map(|t| t.round() as i64);
    // ZTE's model code of the U30 Pro is MU5358; system board reports it as "ZTE U30 Pro (MU5358)"
    let model = text(&board["model"])
        .rsplit_once('(')
        .and_then(|(_, rest)| rest.strip_suffix(')'))
        .filter(|code| !code.is_empty() && code.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("MU5358")
        .to_string();
    fields.insert(
        "device".into(),
        json!({"profile":model.to_ascii_lowercase(),"profile_source":"board_model","api_template":"U30PRO",
            "api_template_label":"U30 Pro (native OpenWrt)","api_template_supported":1,"full_ubus":1,"vendor":"ZTE",
            "model_name":model,"hardware_version":"","market_name":"U30 Pro","alias_name":"U30 Pro",
            "board_name":text(&board["board_name"])}),
    );
    fields.insert(
        "system".into(),
        system(&info, &board, &cpu, &firmware, cpu_temp),
    );
    let zones = temperatures(&device);
    fields.insert(
        "thermal".into(),
        json!({"cpu_celsius":cpu_temp,"zones":zones.iter().map(|(name,t)| json!({"name":name,"celsius":(t*1000.0).round()/1000.0})).collect::<Vec<_>>(),"modems":[]}),
    );
    fields.insert(
        "runtime".into(),
        runtime(&info, &cpu, &storage, &zones, &c.lan),
    );
    if device["battery"]["available"] == true {
        fields.insert("battery".into(), battery(&device));
    }
    if device["charger"]["policy_available"] == true {
        fields.insert(
            "power".into(),
            json!({ "direct_supply": direct_supply(&device) }),
        );
    }
    let cellular = &modem["cellular"];
    fields.insert("sim".into(), sim(cellular));
    fields.insert(
        "net".into(),
        net(&modem, &bands, &wan),
    );
    fields.insert(
        "traffic".into(),
        json!({"rx_speed":c.wan.rx,"tx_speed":c.wan.tx,"max_rx_speed":c.wan.max_rx,"max_tx_speed":c.wan.max_tx,
            "rx_bytes":wan_rx,"tx_bytes":wan_tx,"session_time":int(&wan["uptime"]).unwrap_or(0),"limit":{},"clear_day":{}}),
    );
    fields.insert("qos".into(), qos_block(&qos));
    let interface = crate::state::interface;
    fields.insert(
        "interfaces".into(),
        json!({"lan":interface(&lan),"wan4":interface(&wan),"wan6":interface(&wan),"lan_config":{},
            "cellular":{"state":text(&cellular["state"]),"connected":cellular["connected"]==true,"apn":text(&cellular["apn"]),
                "ipv4_address":text(&cellular["address"]),"dns":cellular["dns"].clone(),"device":text(&cellular["device"])}}),
    );
    fields.insert("clients".into(), clients(&stations, &leases));
    if wifi["available"] == true {
        fields.insert("wlan".into(), wlan(&wifi));
    }
    fields.insert(
        "dhcp".into(),
        json!({"ip":uci(&network_uci,"lan","ipaddr").split('/').next().unwrap_or_default(),"netmask":uci(&network_uci,"lan","netmask"),
            "disabled":uci(&dhcp_uci,"lan","ignore")=="1","start":uci(&dhcp_uci,"lan","start"),"limit":uci(&dhcp_uci,"lan","limit"),
            "range_start":"","range_end":"","leasetime":uci(&dhcp_uci,"lan","leasetime")}),
    );
    if sms["ok"] == true {
        let offset = int(&info["localtime"]).map_or(0, |local| local - unix_now());
        let (block, parts) = sms_block(&sms, offset);
        fields.insert("sms".into(), block);
        c.sms_parts = parts;
    }
    fields.insert("modems".into(), json!([]));
    fields.insert("sample_interval_ms".into(), json!(sample_interval_ms));
    Snapshot {
        ts: unix_now(),
        datad: Default::default(),
        fields,
    }
}

fn system(info: &Value, board: &Value, cpu: &Value, firmware: &Value, cpu_temp: Option<i64>) -> Value {
    let total = int(&info["memory"]["total"]).unwrap_or(0);
    let avail = int(&info["memory"]["available"])
        .or_else(|| int(&info["memory"]["free"]))
        .unwrap_or(0);
    let used_pct = if total > 0 {
        ((total - avail) * 100 + total / 2) / total
    } else {
        0
    };
    let usage = int(&cpu["usage"]["total_tenths"]).map_or(-1, |tenths| (tenths + 5) / 10);
    let running = text(&firmware["slots"]["running"]);
    json!({"uptime":int(&info["uptime"]).unwrap_or(0),"cpu_temp":cpu_temp.unwrap_or(0),"cpu_usage":usage,"mem_used_pct":used_pct,
        "mem_total":total,"mem_avail":avail,"model":text(&board["model"]),"hostname":text(&board["hostname"]),
        "fw":text(&board["release"]["description"]),"sw_version":text(&firmware["slots"][running.as_str()]["version"]),"imei":""})
}

/// (name, °C) of every temperature the device reports, sorted by name
fn temperatures(device: &Value) -> Vec<(String, f64)> {
    let mut zones = Vec::new();
    if let Some(t) = number(&device["soc_temperature_c"]) {
        zones.push(("soc".to_string(), t));
    }
    if let Some(board) = device["board_temperatures_c"].as_object() {
        for (name, value) in board {
            if let Some(t) = number(value) {
                zones.push((name.clone(), t));
            }
        }
    }
    if let Some(t) = number(&device["battery"]["temperature_c"]) {
        zones.push(("battery".to_string(), t));
    }
    zones.sort_by(|a, b| a.0.cmp(&b.0));
    zones
}

fn runtime(info: &Value, cpu: &Value, storage: &Value, zones: &[(String, f64)], lan: &Rate) -> Value {
    let kb = |v: &Value| scaled(v, 1.0 / 1024.0);
    let mut freq = Map::new();
    for core in cpu["cpus"].as_array().into_iter().flatten() {
        if core["online"] == true
            && let Some(id) = int(&core["id"])
        {
            freq.insert(
                format!("cpu{id}"),
                json!({"cur":scaled(&core["cur_khz"],0.001),"max":Value::Null}),
            );
        }
    }
    let store = &storage["store"];
    let disk = if store["mounted"] == true {
        json!({"total":scaled(&store["size_kb"],1024.0),"used":scaled(&store["used_kb"],1024.0),"available":scaled(&store["free_kb"],1024.0)})
    } else {
        json!({})
    };
    let memory = &info["memory"];
    json!({"cpu_usage_tenths":int(&cpu["usage"]["total_tenths"]).unwrap_or(-1),"cpu_cores":cpu["usage"]["cores"].clone(),
        "cpu_freq_mhz":freq,"thermal_zones":zones.iter().map(|(name,t)| json!({"type":name,"temp_milli":(t*1000.0).round() as i64})).collect::<Vec<_>>(),
        "memory_kb":{"total":kb(&memory["total"]),"free":kb(&memory["free"]),"available":kb(&memory["available"]),"buffers":kb(&memory["buffered"]),
            "cached":kb(&memory["cached"]),"swap_total":kb(&info["swap"]["total"]),"swap_free":kb(&info["swap"]["free"])},
        "storage":disk,"throughput":{"rx_bps":lan.rx,"tx_bps":lan.tx,"window_ms":lan.window_ms}})
}

fn battery(device: &Value) -> Value {
    let battery = &device["battery"];
    let charger = &device["charger"];
    let charging = text(&charger["status"]) == "Charging" || text(&battery["status"]) == "Charging";
    let source = &charger["typec_source"];
    json!({"percent":int(&battery["capacity_percent"]),"temp":int(&battery["temperature_c"]),"online":1,
        "health":1,"charging":i64::from(charging),"charger_connect":int(&charger["online"]).unwrap_or(0),
        "chg_uv":scaled(&source["voltage_now_v"],1e6),"chg_ua":scaled(&source["current_now_ma"],1e3),
        "bat_uv":scaled(&battery["voltage_v"],1e6),"bat_ua":scaled(&battery["current_ma"],1e3),
        "capacity_level":text(&battery["capacity_level"]),"charger_status":text(&charger["status"])})
}

/// "direct supply" is the charger's inhibit-charge behaviour: the device runs from the adapter, the battery is not charged
fn direct_supply(device: &Value) -> Value {
    let enabled = match text(&device["charger"]["charge_behaviour"]).as_str() {
        "inhibit-charge" => Some(true),
        "auto" => Some(false),
        _ => None,
    };
    json!({"supported":device["charger"]["policy_available"]==true,"enabled":enabled,
        "mode":enabled.map(|on| if on { "enable" } else { "disable" })})
}

fn sim(cellular: &Value) -> Value {
    let raw = text(&cellular["sim"]);
    let upper = raw.to_ascii_uppercase();
    let state = if upper == "READY" {
        "ready"
    } else if upper.is_empty() {
        "unknown"
    } else if upper.contains("PIN") || upper.contains("PUK") {
        "locked"
    } else if upper.contains("ABSENT") || upper.contains("NOT INSERTED") || upper.contains("NOT_INSERTED") {
        "absent"
    } else {
        "unknown"
    };
    json!({"state":state,"raw_state":raw,"modem_state":text(&cellular["state"])})
}

fn bars(rsrp: Option<i64>) -> i64 {
    match rsrp {
        Some(v) if v >= -85 => 5,
        Some(v) if v >= -95 => 4,
        Some(v) if v >= -105 => 3,
        Some(v) if v >= -115 => 2,
        Some(_) => 1,
        None => 0,
    }
}

fn net(modem: &Value, bands: &Value, wan: &Value) -> Value {
    let serving = &modem["serving"];
    let cellular = &modem["cellular"];
    let act = int(&serving["eps"]["act"]);
    let sa = matches!(int(&serving["nr_reg"]["stat"]), Some(1 | 5));
    let endc = serving["endc"] == true || matches!(act, Some(10..=13));
    let kind = if sa {
        "SA"
    } else if endc {
        // EN-DC: the measurements are those of the LTE anchor
        "LTE-NSA"
    } else {
        match act {
            Some(7) => "LTE",
            Some(2 | 4..=6) => "WCDMA",
            Some(0 | 1 | 3) => "GSM",
            _ => "",
        }
    };
    let stat = if sa {
        int(&serving["nr_reg"]["stat"])
    } else {
        int(&serving["eps"]["stat"])
    };
    let roaming = match stat {
        Some(1) => "Home",
        Some(5) => "Roaming",
        _ => "",
    };
    let rsrp = int(&serving["cesq"]["rsrp_dbm"]);
    let rsrq = int(&serving["cesq"]["rsrq_db"]);
    let snr = number(&modem["sinr_db"]).map(|v| format!("{v:.1}")).unwrap_or_default();
    let band = int(&serving["phy"]["band"]).filter(|b| *b > 0);
    let band_text = match band {
        Some(b) if sa => format!("n{b}"),
        Some(b) => format!("LTE BAND {b}"),
        None => String::new(),
    };
    let pci = int(&serving["phy"]["pci"]).unwrap_or(0);
    let channel = int(&serving["phy"]["dl_earfcn"]).unwrap_or(0);
    let bandwidth = match int(&serving["phy"]["dl_bw_idx"]) {
        Some(0) => "1.4MHz",
        Some(1) => "3MHz",
        Some(2) => "5MHz",
        Some(3) => "10MHz",
        Some(4) => "15MHz",
        Some(5) => "20MHz",
        _ => "",
    };
    let csq = int(&serving["csq"]).or_else(|| int(&cellular["signal_csq"]));
    let rssi = csq.filter(|v| (0..=31).contains(v)).map_or(0, |v| -113 + 2 * v);
    let (lte_rsrp, lte_rsrq, lte_snr, nr_rsrp, nr_rsrq, nr_snr) = if sa {
        (0, 0, String::new(), rsrp.unwrap_or(0), rsrq.unwrap_or(0), snr)
    } else {
        (rsrp.unwrap_or(0), rsrq.unwrap_or(0), snr, 0, 0, String::new())
    };
    let lte_hw = ints(&bands["lte_hw"]);
    let nr_hw = ints(&bands["nr_hw"]);
    let lte = ints(&bands["lte"]);
    let nr = ints(&bands["nr"]);
    let lock = |current: &[i64], all: &[i64]| {
        if current.is_empty() || current == all {
            String::new()
        } else {
            join(current)
        }
    };
    let complete = !lte_hw.is_empty() && !nr_hw.is_empty();
    let cell_lock = |rat: &str| {
        if bands["cell_lock"][rat]["locked"] == true {
            "locked".to_string()
        } else {
            String::new()
        }
    };
    let has4 = wan["ipv4-address"].as_array().is_some_and(|a| !a.is_empty());
    let has6 = wan["ipv6-address"].as_array().is_some_and(|a| !a.is_empty());
    let wan_status = match (wan["up"] == true, has4, has6) {
        (true, true, true) => "ipv4_ipv6_connected",
        (true, true, false) => "ipv4_connected",
        (true, false, true) => "ipv6_connected",
        _ => "disconnected",
    };
    let operator = match text(&serving["network"]["name"]) {
        name if !name.is_empty() => name,
        _ => text(&cellular["operator"]),
    };
    // in parts: one json! literal of this size exceeds the macro recursion limit
    let mut net = Map::new();
    for part in [
        json!({"type":kind,"bars":bars(rsrp),"roaming":roaming,"roaming_allowed":Value::Null,"operator":operator,"band":band_text,
            "nr_band":if sa { band.map(|b| b.to_string()).unwrap_or_default() } else { String::new() },
            "mcc":int(&serving["network"]["mcc"]).unwrap_or(0),"mnc":int(&serving["network"]["mnc"]).unwrap_or(0),"endc":endc,
            "wan_status":wan_status,"HSR":false,"high_speed_rail":{"active":false,"raw":Value::Null},
            "signal_age_s":modem["serving_age_s"].clone()}),
        json!({"nr_rsrp":nr_rsrp,"nr_rsrq":nr_rsrq,"nr_snr":nr_snr,"nr_rssi":0,"lte_rsrp":lte_rsrp,"lte_rsrq":lte_rsrq,"lte_rssi":0,
            "lte_snr":lte_snr,"rssi":rssi}),
        json!({"lte_pci":if sa {0} else {pci},"lte_cell_id":if sa {0} else {int(&serving["eps"]["eci"]).unwrap_or(0)},
            "lte_channel":if sa {0} else {channel},"lte_action_channel":if sa {0} else {channel},"lte_bw":if sa {""} else {bandwidth},
            "lte_tac":if sa {0} else {int(&serving["eps"]["tac_dec"]).unwrap_or(0)},
            "nr_pci":if sa {pci} else {0},"nr_cell_id":0,"nr_channel":if sa {channel} else {0},"nr_tac":0,"nr_bw":if sa {bandwidth} else {""},
            "nrca":"","lteca":"","ltecasig":"","net_select":""}),
        json!({"lte_bands":join(&lte),"sa_bands":join(&nr),"nsa_bands":join(&nr),
            "lte_band_lock":lock(&lte,&lte_hw),"gw_band_lock":"","nr5g_sa_band_lock":lock(&nr,&nr_hw),"nr5g_nsa_band_lock":lock(&nr,&nr_hw),
            "nr5g_nrdc_band_lock":"","lock_lte_cell":cell_lock("lte"),"lock_nr_cell":cell_lock("nr"),
            "lte_supported_bands":join(&lte_hw),"nr_sa_supported_bands":join(&nr_hw),"nr_nsa_supported_bands":join(&nr_hw)}),
        json!({"band_capabilities":{"source":if complete {"modem"} else {"unavailable"},"complete":complete,
            "lte":if complete {lte_hw.clone()} else {vec![]},"nr_sa":if complete {nr_hw.clone()} else {vec![]},
            "nr_nsa":if complete {nr_hw.clone()} else {vec![]}}}),
    ] {
        if let Value::Object(entries) = part {
            net.extend(entries);
        }
    }
    Value::Object(net)
}

fn qos_block(qos: &Value) -> Value {
    let bearer = qos["eps"]
        .get(0)
        .or_else(|| qos["nr"].get(0))
        .cloned()
        .unwrap_or(Value::Null);
    let mbps = |v: &Value| number(v).map(|kbps| format!("{:.3}", kbps / 1000.0)).unwrap_or_default();
    json!({"qci":int(&bearer["qi"]).unwrap_or(0),"ambr_dl":mbps(&bearer["dl_ambr"]),"ambr_ul":mbps(&bearer["ul_ambr"])})
}

fn clients(stations: &Value, leases: &Value) -> Value {
    let mut names = BTreeMap::new();
    for lease in leases["dhcp_leases"].as_array().into_iter().flatten() {
        names.insert(
            text(&lease["macaddr"]).to_ascii_lowercase(),
            (text(&lease["hostname"]), text(&lease["ipaddr"])),
        );
    }
    let band = if int(&stations["freq"]).is_some_and(|f| f < 3000) {
        "2g"
    } else {
        "5g"
    };
    let mut list = Vec::new();
    for mac in stations["clients"].as_object().into_iter().flat_map(Map::keys) {
        let mac = mac.to_ascii_lowercase();
        let (name, ip) = names.get(&mac).cloned().unwrap_or_default();
        list.push(json!({"name":name,"ip":ip,"mac":mac,"band":band,"guest":false}));
        if list.len() == 32 {
            break;
        }
    }
    let count = list.len();
    json!({"total":count,"wifi":count,"lan":0,"list":list,"blocked":[],"allowed":[],"acl_mode":"disable"})
}

fn encryption(security: &str) -> &str {
    match security {
        "open" => "none",
        "wpa2" => "psk2",
        "mixed" => "sae-mixed",
        "wpa3" => "sae",
        other => other,
    }
}

/// The hotspot in the standard wireless configuration (`uci get wireless`, applied by netifd and wpad): the radio and its first
/// interface of mode ap, in the shape the blocks below read (available, enabled, config: band, ssid, security, ...).
fn wifi_from_uci(reply: &Value) -> Value {
    let Some(sections) = reply["values"].as_object() else {
        return json!({ "available": false });
    };
    let Some((name, iface)) = sections
        .iter()
        .find(|(_, s)| s[".type"] == "wifi-iface" && s["mode"] == "ap")
    else {
        return json!({ "available": false });
    };
    let radio = sections.get(&text(&iface["device"])).cloned().unwrap_or_else(|| json!({}));
    let encryption = text(&iface["encryption"]);
    let security = match encryption.as_str() {
        "" | "none" => "open",
        "sae" => "wpa3",
        "sae-mixed" => "mixed",
        _ => "wpa2",
    };
    json!({"available":true,"enabled":iface["disabled"] != "1" && radio["disabled"] != "1","section":name,
        "config":{"band":text(&radio["band"]),"channel":text(&radio["channel"]),"htmode":text(&radio["htmode"]),"ssid":text(&iface["ssid"]),
            "security":security,"encryption":encryption,"password_set":!text(&iface["key"]).is_empty(),"pmf":text(&iface["ieee80211w"]),
            "hidden":text(&iface["hidden"]) == "1","isolate":text(&iface["isolate"]) == "1","max_clients":text(&iface["maxassoc"])}})
}

async fn wifi_now() -> Value {
    wifi_from_uci(&or_object(ubus("uci", "get", json!({ "config": "wireless" })).await))
}

fn wlan(wifi: &Value) -> Value {
    let config = &wifi["config"];
    let band = if text(&config["band"]) == "2g" { "2g" } else { "5g" };
    let ssid = text(&config["ssid"]);
    let enc = encryption(&text(&config["security"])).to_string();
    let enabled = i64::from(wifi["enabled"] == true);
    json!({"ssid":ssid,"enc":enc,"enabled":enabled,"bands":[{"band":band,"ssid":ssid,"enc":enc,"enabled":enabled}]})
}

fn sms_block(sms: &Value, offset: i64) -> (Value, BTreeMap<i64, Vec<i64>>) {
    let mut parts = BTreeMap::new();
    let mut list = Vec::new();
    let mut unread = 0;
    for message in sms["messages"].as_array().into_iter().flatten() {
        let Some(id) = int(&message["index"]) else {
            continue;
        };
        let indexes = ints(&message["indexes"]);
        parts.insert(id, if indexes.is_empty() { vec![id] } else { indexes });
        // +CMGL stat 0 is "received unread"
        let is_unread = int(&message["stat"]) == Some(0);
        unread += i64::from(is_unread);
        let received = int(&message["ts"]);
        list.push(json!({"id":id,"num":text(&message["address"]),"date":received.map(|t| month_day_time(t + offset)).unwrap_or_default(),
            "received_at":received,"unread":i64::from(is_unread),"text":text(&message["text"])}));
    }
    (
        json!({"unread":unread,"list":list,"total":list.len(),"capacity":sms["capacity"].clone(),"stale":false}),
        parts,
    )
}

// ---------------------------------------------------------------------------------------------------------------- control

/// device actions of this backend (POST /control)
pub const ACTIONS: &[&str] = &[
    "device.reboot",
    "cellular.connect",
    "cellular.disconnect",
    "band.set_lte",
    "band.set_nr_sa",
    "band.set_nr_nsa",
    "cell.lock_lte",
    "cell.lock_nr",
    "cell.unlock_all",
    "wifi.configure",
    "client.kick",
    "sms.send_raw",
    "sms.delete",
    "power.direct_supply.set",
    "wifi.status",
    "wifi.state",
    "power.direct_supply.status",
    "qos.reload",
];

/// datad's own device-independent actions, handled by the common code
pub const GENERIC: &[&str] = &[
    "hosts.save",
    "hosts.restore",
    "hosts.status",
    "time.config.set",
    "time.sync",
    "time.status",
    "schedule.reboot.set",
    "schedule.task.put",
    "schedule.task.remove",
    "speedtest.start",
    "speedtest.stop",
    "nat.detect",
    "nat.status",
    "state.refresh",
    "state.set_interval",
    "neighbor.status",
];

pub fn capability_controls() -> Vec<&'static str> {
    ACTIONS.iter().chain(GENERIC).copied().collect()
}

/// `None`: a generic action for the common code.  `Some(NotHandled)`: an action of the ZTE firmware this device does not have.
pub async fn control(action: &str, params: &Value) -> Option<Outcome> {
    if GENERIC.contains(&action) {
        return None;
    }
    Some(match action {
        "device.reboot" => call("u30pro-native-control", "reboot", json!({})).await,
        "cellular.connect" => cellular("reconnect").await,
        "cellular.disconnect" => cellular("disconnect").await,
        "band.set_lte" => set_bands(params, "lte").await,
        "band.set_nr_sa" | "band.set_nr_nsa" => set_bands(params, "nr").await,
        "cell.lock_lte" => lock_cell(params, "lte", "earfcn").await,
        "cell.lock_nr" => lock_cell(params, "nr", "arfcn").await,
        "cell.unlock_all" => {
            let outcome = call("u30pro-modem", "lock_reset", json!({})).await;
            cache().lock().await.bands.invalidate();
            outcome
        }
        "wifi.configure" => wifi_configure(params).await,
        "client.kick" => client_kick(params).await,
        "sms.send_raw" => sms_send(params).await,
        "sms.delete" => sms_delete(params).await,
        "power.direct_supply.set" => direct_supply_set(params).await,
        "wifi.status" | "wifi.state" => {
            let wifi = wifi_now().await;
            Outcome::Ok(json!({"available":wifi["available"]==true,"enabled":wifi["enabled"]==true,"config":wifi["config"].clone(),
                "wlan":if wifi["available"]==true { wlan(&wifi) } else { Value::Null }}))
        }
        "power.direct_supply.status" => {
            let device = or_object(ubus("u30pro-native-control", "get", json!({})).await);
            Outcome::Ok(direct_supply(&device))
        }
        "qos.reload" => {
            cache().lock().await.qos.invalidate();
            Outcome::Ok(json!({ "reloaded": true }))
        }
        _ => Outcome::NotHandled,
    })
}

/// a ubus call whose answer reports failure as `ok: false` or an `error` text
async fn call(service: &str, method: &str, args: Value) -> Outcome {
    match ubus(service, method, args).await {
        Ok(reply) if reply["ok"] == false || reply.get("error").is_some() => {
            let error = text(&reply["error"]);
            Outcome::Failed(if error.is_empty() {
                format!("{service} {method} failed")
            } else {
                error
            })
        }
        Ok(reply) => Outcome::Ok(reply),
        Err(error) => Outcome::Failed(error),
    }
}

async fn cellular(action: &str) -> Outcome {
    call(
        "u30pro-native-control",
        "cellular_action",
        json!({ "action": action }),
    )
    .await
}

fn param_int(params: &Value, name: &str) -> Result<i64, String> {
    match &params[name] {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .ok_or_else(|| format!("{name} must be a decimal integer"))
}

/// The modem undoes a band or cell lock after 120 s unless it is confirmed (u30pro-modem lock_apply): confirm it as soon as the
/// modem is registered again, so a lock that loses the network reverts on its own.
fn confirm_when_registered() {
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(10)).await;
        for _ in 0..16 {
            if let Ok(serving) = ubus("u30pro-modem", "serving", json!({})).await
                && (matches!(int(&serving["eps"]["stat"]), Some(1 | 5))
                    || matches!(int(&serving["nr_reg"]["stat"]), Some(1 | 5)))
            {
                let _ = ubus("u30pro-modem", "lock_confirm", json!({})).await;
                return;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}

async fn apply_lock(config: Value) -> Outcome {
    let outcome = call(
        "u30pro-modem",
        "lock_apply",
        json!({ "config": config.to_string() }),
    )
    .await;
    cache().lock().await.bands.invalidate();
    if let Outcome::Ok(mut reply) = outcome {
        confirm_when_registered();
        reply["confirm"] = json!("automatic once the modem is registered again; otherwise undone after the revert time");
        return Outcome::Ok(reply);
    }
    outcome
}

async fn set_bands(params: &Value, rat: &str) -> Outcome {
    let Some(raw) = params["bands"].as_str() else {
        return Outcome::Invalid("bands must be a comma-separated string".into());
    };
    let mut wanted = Vec::new();
    for item in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match item.trim_start_matches(['n', 'N', 'b', 'B']).parse::<i64>() {
            Ok(band) if (1..=512).contains(&band) => wanted.push(band),
            _ => return Outcome::Invalid(format!("invalid band: {item}")),
        }
    }
    let hardware = {
        let mut c = cache().lock().await;
        c.bands.invalidate();
        let bands = c
            .bands
            .get(Duration::from_secs(300), "u30pro-modem", "bands_get", json!({}))
            .await;
        ints(&bands[format!("{rat}_hw").as_str()])
    };
    if hardware.is_empty() {
        return Outcome::Failed("the modem did not report its bands".into());
    }
    // an empty list means automatic: every band of the hardware
    if wanted.is_empty() {
        wanted = hardware.clone();
    }
    if let Some(band) = wanted.iter().find(|b| !hardware.contains(b)) {
        return Outcome::Invalid(format!("band {band} is not supported by this modem ({})", join(&hardware)));
    }
    wanted.sort_unstable();
    wanted.dedup();
    apply_lock(json!({ rat: wanted })).await
}

async fn lock_cell(params: &Value, rat: &str, channel: &str) -> Outcome {
    let pci = match param_int(params, "pci") {
        Ok(v) if (0..=1007).contains(&v) => v,
        Ok(_) => return Outcome::Invalid("pci out of range".into()),
        Err(e) => return Outcome::Invalid(e),
    };
    let earfcn = match param_int(params, channel) {
        Ok(v) if (0..=3_279_165).contains(&v) => v,
        Ok(_) => return Outcome::Invalid(format!("{channel} out of range")),
        Err(e) => return Outcome::Invalid(e),
    };
    apply_lock(json!({ "cell": { "rat": rat, "earfcn": earfcn, "pci": pci } })).await
}

fn flag(params: &Value, name: &str) -> Result<Option<bool>, String> {
    match &params[name] {
        Value::Null => Ok(None),
        Value::Bool(b) => Ok(Some(*b)),
        Value::Number(n) if n.as_i64() == Some(0) => Ok(Some(false)),
        Value::Number(n) if n.as_i64() == Some(1) => Ok(Some(true)),
        Value::String(s) if s == "0" || s == "1" => Ok(Some(s == "1")),
        _ => Err(format!("{name} must be boolean or 0/1")),
    }
}

async fn wifi_configure(params: &Value) -> Outcome {
    let wifi = wifi_now().await;
    let Some(section) = wifi["section"].as_str().map(str::to_string) else {
        return Outcome::Failed("no Wi-Fi hotspot is configured".into());
    };
    let mut values = Map::new();
    if let Some(ssid) = params["ssid"].as_str() {
        if ssid.is_empty() || ssid.len() > 32 || ssid.chars().any(char::is_control) {
            return Outcome::Invalid("ssid must be 1 to 32 bytes without control characters".into());
        }
        values.insert("ssid".into(), json!(ssid));
    }
    if let Some(enc) = params["encryption"].as_str() {
        let enc = match enc {
            "none" | "open" => "none",
            "psk2" | "psk" | "psk2+ccmp" => "psk2+ccmp",
            "psk-mixed" => "psk-mixed",
            "sae-mixed" | "psk2-sae" => "sae-mixed",
            "sae" => "sae",
            other => return Outcome::Invalid(format!("unsupported encryption: {other}")),
        };
        values.insert("encryption".into(), json!(enc));
    }
    if let Some(key) = params["key"].as_str() {
        if key.len() < 8 || key.len() > 63 || !key.chars().all(|c| (' '..='~').contains(&c)) {
            return Outcome::Invalid("key must be 8 to 63 printable ASCII characters".into());
        }
        values.insert("key".into(), json!(key));
    }
    if let Some(pmf) = params.get("pmf").and_then(|v| int(v)) {
        if !(0..=2).contains(&pmf) {
            return Outcome::Invalid("pmf must be 0, 1 or 2".into());
        }
        values.insert("ieee80211w".into(), json!(pmf.to_string()));
    }
    if let Some(max) = params.get("maxassoc").and_then(|v| int(v)) {
        values.insert("maxassoc".into(), json!(max.to_string()));
    }
    for name in ["hidden", "isolate"] {
        match flag(params, name) {
            Ok(Some(on)) => {
                values.insert(name.into(), json!(if on { "1" } else { "0" }));
            }
            Ok(None) => {}
            Err(e) => return Outcome::Invalid(e),
        }
    }
    match flag(params, "enabled") {
        Ok(Some(on)) => {
            values.insert("disabled".into(), json!(if on { "0" } else { "1" }));
        }
        Ok(None) => {}
        Err(e) => return Outcome::Invalid(e),
    }
    if values.is_empty() {
        return Outcome::Invalid("no fields supplied".into());
    }
    let encryption = values.get("encryption").and_then(Value::as_str).map(str::to_string)
        .unwrap_or_else(|| text(&wifi["config"]["encryption"]));
    if encryption != "none" && !values.contains_key("key") && wifi["config"]["password_set"] != true {
        return Outcome::Invalid("an encrypted hotspot needs a key".into());
    }
    if let Err(e) = ubus("uci", "set", json!({ "config": "wireless", "section": section, "values": Value::Object(values.clone()) })).await {
        return Outcome::Failed(format!("uci set: {e}"));
    }
    if let Err(e) = ubus("uci", "commit", json!({ "config": "wireless" })).await {
        return Outcome::Failed(format!("uci commit: {e}"));
    }
    if let Err(e) = ubus("network", "reload", json!({})).await {
        return Outcome::Failed(format!("network reload: {e}"));
    }
    values.remove("key");
    cache().lock().await.wifi.invalidate();
    Outcome::Ok(json!({ "applied": Value::Object(values) }))
}

fn valid_mac(mac: &str) -> bool {
    mac.len() == 17
        && mac
            .split(':')
            .all(|part| part.len() == 2 && part.chars().all(|c| c.is_ascii_hexdigit()))
}

async fn client_kick(params: &Value) -> Outcome {
    let Some(raw) = params["macs"].as_str() else {
        return Outcome::Invalid("macs must be a comma-separated string".into());
    };
    let macs: Vec<String> = raw
        .split(',')
        .map(|m| m.trim().to_ascii_lowercase())
        .filter(|m| !m.is_empty())
        .collect();
    if macs.is_empty() || macs.len() > 32 || !macs.iter().all(|m| valid_mac(m)) {
        return Outcome::Invalid("macs must hold 1 to 32 MAC addresses".into());
    }
    for mac in &macs {
        // hostapd of the hotspot (wpad): deauthenticate, no ban
        if let Err(error) = ubus("hostapd.wlan0", "del_client", json!({ "addr": mac, "reason": 5, "deauth": true, "ban_time": 0 })).await {
            return Outcome::Failed(format!("{mac}: {error}"));
        }
    }
    cache().lock().await.stations.invalidate();
    Outcome::Ok(json!({ "kicked": macs }))
}

/// message_hex: the text as UTF-16BE (UCS-2) hex, as the ZTE web pages send it
fn ucs2_hex(hex: &str) -> Option<String> {
    if hex.is_empty() || hex.len() % 4 != 0 || hex.len() > 4 * 1530 {
        return None;
    }
    let units: Option<Vec<u16>> = (0..hex.len())
        .step_by(4)
        .map(|i| u16::from_str_radix(hex.get(i..i + 4)?, 16).ok())
        .collect();
    String::from_utf16(&units?).ok()
}

async fn sms_send(params: &Value) -> Outcome {
    let Some(number) = params["number"].as_str().map(str::trim) else {
        return Outcome::Invalid("missing parameter: number".into());
    };
    let digits = number.strip_prefix('+').unwrap_or(number);
    if digits.len() < 3 || digits.len() > 32 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Outcome::Invalid("number must be 3 to 32 digits".into());
    }
    let text = match (params["message_hex"].as_str(), params["text"].as_str()) {
        (Some(hex), _) => match ucs2_hex(hex) {
            Some(text) => text,
            None => return Outcome::Invalid("message_hex must be UTF-16BE hex".into()),
        },
        (None, Some(text)) if !text.is_empty() && text.chars().count() <= 1530 => text.to_string(),
        _ => return Outcome::Invalid("missing parameter: message_hex".into()),
    };
    let outcome = call(
        "u30pro-modem",
        "sms_send",
        json!({ "number": number, "text": text }),
    )
    .await;
    cache().lock().await.sms.invalidate();
    outcome
}

async fn sms_delete(params: &Value) -> Outcome {
    let Some(raw) = params["ids"].as_str() else {
        return Outcome::Invalid("ids must be a ;-separated string".into());
    };
    let mut indexes = Vec::new();
    {
        let c = cache().lock().await;
        for item in raw.split([';', ',']).map(str::trim).filter(|s| !s.is_empty()) {
            let Ok(id) = item.parse::<i64>() else {
                return Outcome::Invalid(format!("invalid id: {item}"));
            };
            indexes.extend(c.sms_parts.get(&id).cloned().unwrap_or_else(|| vec![id]));
        }
    }
    if indexes.is_empty() || indexes.len() > 40 {
        return Outcome::Invalid("ids must name 1 to 40 messages".into());
    }
    let outcome = call(
        "u30pro-modem",
        "sms_delete",
        json!({ "config": json!({ "indexes": indexes }).to_string() }),
    )
    .await;
    cache().lock().await.sms.invalidate();
    outcome
}

async fn direct_supply_set(params: &Value) -> Outcome {
    let enabled = match flag(params, "enabled") {
        Ok(Some(on)) => on,
        Ok(None) => return Outcome::Invalid("missing parameter: enabled".into()),
        Err(e) => return Outcome::Invalid(e),
    };
    let device = or_object(ubus("u30pro-native-control", "get", json!({})).await);
    let charger = &device["charger"];
    if charger["policy_available"] != true {
        return Outcome::Failed("this kernel offers no charge policy".into());
    }
    let before = direct_supply(&device);
    let outcome = call(
        "u30pro-native-control",
        "set_charger_policy",
        json!({"input_limit_ma":int(&charger["user_iin_ma"]).unwrap_or(0).to_string(),
            "charge_current_ma":int(&charger["user_ichg_ma"]).unwrap_or(0).to_string(),
            "behaviour":if enabled { "inhibit-charge" } else { "auto" }}),
    )
    .await;
    if let Outcome::Ok(_) = outcome {
        cache().lock().await.device.invalidate();
        let after = direct_supply(&or_object(ubus("u30pro-native-control", "get", json!({})).await));
        let mut result = after.clone();
        result["changed"] = json!(before["enabled"] != after["enabled"]);
        result["verified"] = json!(after["enabled"] == json!(enabled));
        return Outcome::Ok(result);
    }
    outcome
}

// ---------------------------------------------------------------------------------------------------------------- login

/// LAN login (POST /auth/login) with the router's own account: rpcd's `session login`, as LuCI does
pub async fn verify_password(username: &str, password: &str) -> bool {
    let Ok(reply) = ubus(
        "session",
        "login",
        json!({"username":username,"password":password,"timeout":30}),
    )
    .await
    else {
        return false;
    };
    let Some(session) = reply["ubus_rpc_session"].as_str() else {
        return false;
    };
    let _ = ubus(
        "session",
        "destroy",
        json!({ "ubus_rpc_session": session }),
    )
    .await;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_dates() {
        assert_eq!(month_day_time(0), "01-01 00:00");
        assert_eq!(month_day_time(1_792_846_362), "10-24 12:52");
    }

    #[test]
    fn ucs2() {
        assert_eq!(ucs2_hex("004F004B").as_deref(), Some("OK"));
        assert_eq!(ucs2_hex("4F60597D").as_deref(), Some("你好"));
        assert_eq!(ucs2_hex("004"), None);
    }

    #[test]
    fn rates() {
        let mut rate = Rate::default();
        rate.update(Some(100), Some(100));
        assert_eq!(rate.rx, 0);
        rate.last = Some((Instant::now() - Duration::from_secs(2), 100, 100));
        rate.update(Some(2100), Some(300));
        assert_eq!((rate.rx, rate.tx), (1000, 100));
        rate.update(Some(0), Some(0));
        assert_eq!(rate.rx, 0);
    }

    #[test]
    fn network_types() {
        let modem = json!({"serving":{"eps":{"act":13,"stat":1,"eci":1,"tac_dec":2},"nr_reg":{"stat":4},"endc":true,
            "cesq":{"rsrp_dbm":-106,"rsrq_db":-17.5},"csq":20,"phy":{"band":8,"pci":57,"dl_earfcn":3725,"dl_bw_idx":2},
            "network":{"mcc":"454","mnc":"12","name":"CMHK"}},"sinr_db":-7.0,"cellular":{}});
        let bands = json!({"lte_hw":[1,3],"nr_hw":[78],"lte":[3],"nr":[78]});
        let n = net(&modem, &bands, &json!({"up":true,"ipv4-address":[{"address":"10.0.0.1"}]}));
        assert_eq!(n["type"], "LTE-NSA");
        assert_eq!(n["band"], "LTE BAND 8");
        assert_eq!(n["lte_rsrp"], -106);
        assert_eq!(n["lte_snr"], "-7.0");
        assert_eq!(n["lte_band_lock"], "3");
        assert_eq!(n["nr5g_sa_band_lock"], "");
        assert_eq!(n["roaming"], "Home");
        assert_eq!(n["mcc"], 454);
        assert_eq!(n["wan_status"], "ipv4_connected");
        assert_eq!(n["bars"], 2);
    }
}
