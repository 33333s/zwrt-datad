use crate::{command, model::Snapshot};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    ffi::CString,
    fs,
    path::Path,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn ubus_bin() -> String {
    std::env::var("ZWRT_DATAD_UBUS_BIN").unwrap_or_else(|_| "/bin/ubus".into())
}
fn uci_bin() -> String {
    std::env::var("ZWRT_DATAD_UCI_BIN").unwrap_or_else(|_| "/sbin/uci".into())
}
fn object(v: Result<Value, String>) -> Value {
    v.unwrap_or_else(|_| json!({}))
}
fn string(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Number(x)) => x.to_string(),
        Some(Value::Bool(x)) => x.to_string(),
        _ => String::new(),
    }
}
fn integer(v: &Value, key: &str) -> i64 {
    match v.get(key) {
        Some(Value::Number(x)) => x.as_i64().unwrap_or_default(),
        Some(Value::String(x)) => x.parse().unwrap_or_default(),
        Some(Value::Bool(x)) => i64::from(*x),
        _ => 0,
    }
}
fn interface(v: &Value) -> Value {
    json!({"up":v.get("up").and_then(Value::as_bool).unwrap_or(false),"proto":string(v,"proto"),"device":string(v,"l3_device"),"ipv4":v.get("ipv4-address").cloned().unwrap_or_else(||json!([])),"ipv6":v.get("ipv6-address").cloned().unwrap_or_else(||json!([])),"dns":v.get("dns-server").cloned().unwrap_or_else(||json!([]))})
}

async fn uci_show(package: &str) -> BTreeMap<String, String> {
    if validate_name(package).is_err() {
        return BTreeMap::new();
    }
    let Ok(raw) = command::run(&uci_bin(), ["-q", "show", package], Duration::from_secs(5)).await
    else {
        return BTreeMap::new();
    };
    String::from_utf8_lossy(&raw)
        .lines()
        .filter_map(|line| {
            let (k, raw) = line.split_once('=')?;
            let v = raw
                .strip_prefix('\'')
                .and_then(|x| x.strip_suffix('\''))
                .unwrap_or(raw);
            Some((k.into(), v.into()))
        })
        .collect()
}
pub async fn uci_read(path: &str) -> String {
    if path.is_empty()
        || path.len() > 256
        || !path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-@[]".contains(&b))
    {
        return String::new();
    }
    command::run(&uci_bin(), ["-q", "get", path], Duration::from_secs(5))
        .await
        .ok()
        .map(|raw| String::from_utf8_lossy(&raw).trim().to_owned())
        .unwrap_or_default()
}
pub async fn uci_write(operation: &str, path: &str, value: Option<&str>) -> Result<(), String> {
    if !matches!(
        operation,
        "set" | "add_list" | "del_list" | "delete" | "commit" | "revert"
    ) || path.is_empty()
        || path.len() > 256
        || !path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-@[]".contains(&b))
        || value.is_some_and(|v| v.len() > 8192 || v.contains(['\0', '\r', '\n']))
    {
        return Err("invalid UCI mutation".into());
    }
    let mut args = vec![operation.to_owned()];
    if matches!(operation, "set" | "add_list" | "del_list") {
        let Some(value) = value else {
            return Err("missing UCI value".into());
        };
        args.push(format!("{path}={value}"));
    } else {
        args.push(path.to_owned());
    }
    command::run(&uci_bin(), args, Duration::from_secs(5))
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}
fn uci_get<'a>(sets: &'a [BTreeMap<String, String>], path: &str) -> &'a str {
    sets.iter()
        .find_map(|s| s.get(path))
        .map(String::as_str)
        .unwrap_or_default()
}
fn normalize_profile(v: &str) -> String {
    let mut out = String::new();
    let mut sep = true;
    for c in v.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            sep = false
        } else if !sep && matches!(c, '-' | '_' | ' ' | '/' | '.') {
            out.push('_');
            sep = true
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        "unknown".into()
    } else {
        out
    }
}
fn valid_imsi(value: &str) -> bool {
    (5..=20).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit())
}
fn valid_msisdn(value: &str) -> bool {
    let digits = value.strip_prefix('+').unwrap_or(value);
    (3..=32).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
}
fn realtime_traffic(value: &Value) -> Value {
    json!({
        "rx_speed":integer(value,"real_rx_speed"),
        "tx_speed":integer(value,"real_tx_speed"),
        "max_rx_speed":integer(value,"real_max_rx_speed"),
        "max_tx_speed":integer(value,"real_max_tx_speed"),
        "rx_bytes":integer(value,"real_rx_bytes"),
        "tx_bytes":integer(value,"real_tx_bytes"),
        "session_time":integer(value,"real_time")
    })
}
fn prefixed_string(value: &Value, prefix: &str, suffix: &str) -> String {
    string(value, &format!("{prefix}{suffix}"))
}
fn prefixed_integer(value: &Value, prefix: &str, suffix: &str) -> i64 {
    integer(value, &format!("{prefix}{suffix}"))
}
fn topflow_external_net(
    live: &Value,
    sets: &[BTreeMap<String, String>],
    index: usize,
    slot: i64,
) -> Value {
    let prefix = format!("msim_{}_{}_", index + 1, slot);
    let uci_prefix = format!("zte_nwinfo.sys_info.{prefix}");
    let use_uci = live.as_object().is_none_or(|value| value.is_empty());
    let text = |suffix: &str| {
        let live_value = prefixed_string(live, &prefix, suffix);
        if live_value.is_empty() && use_uci {
            uci_get(sets, &format!("{uci_prefix}{suffix}")).to_owned()
        } else {
            live_value
        }
    };
    let number = |suffix: &str| {
        let live_value = prefixed_string(live, &prefix, suffix);
        if live_value.is_empty() && use_uci {
            uci_get(sets, &format!("{uci_prefix}{suffix}"))
                .parse::<i64>()
                .unwrap_or_default()
        } else {
            prefixed_integer(live, &prefix, suffix)
        }
    };
    let bandwidth = text("lte_bandwidth");
    let bandwidth = bandwidth
        .trim_end_matches("MHz")
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|v| [1.4, 3.0, 5.0, 10.0, 15.0, 20.0].contains(v))
        .map(|v| {
            if v == 1.4 {
                "1.4".into()
            } else {
                format!("{v:.0}")
            }
        });
    let mut out = json!({
        "type":text("network_type"),
        "bars":number("signalbar"),
        "roaming":text("simcard_roam"),
        "operator":text("network_provider"),
        "plmn":text("rplmn_num"),
        "band":text("wan_active_band"),
        "lte_rsrp":number("lte_rsrp"),
        "lte_rsrq":number("lte_rsrq"),
        "lte_rssi":number("lte_rssi"),
        "lte_snr":text("lte_snr"),
        "lte_pci":number("lte_pci"),
        "cell_id":number("cell_id"),
        "channel":number("wan_active_channel"),
        "mode":text("net_select"),
        "operate_mode":text("operate_mode")
    });
    if let Some(bandwidth) = bandwidth {
        out["bandwidth"] = json!(bandwidth);
    }
    out
}
fn parse_bool(value: &Value, key: &str) -> bool {
    value
        .get(key)
        .and_then(|v| v.as_bool().or_else(|| v.as_i64().map(|n| n != 0)))
        .unwrap_or(false)
}
fn tcp_aggregation_summary() -> (usize, String, u16, bool) {
    let path = std::env::var("ZWRT_DATAD_PROC_NET_TCP").unwrap_or_else(|_| "/proc/net/tcp".into());
    let owned: Vec<u64> = std::env::var("ZWRT_DATAD_ICG_SOCKET_INODES")
        .unwrap_or_default()
        .split(',')
        .filter_map(|v| v.trim().parse().ok())
        .collect();
    if owned.is_empty() {
        return (0, String::new(), 0, false);
    }
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut listeners = Vec::new();
    let mut established = Vec::new();
    for line in text.lines().skip(1) {
        let columns: Vec<_> = line.split_whitespace().collect();
        if columns.len() < 10 {
            continue;
        }
        let inode = columns.last().and_then(|v| v.parse::<u64>().ok());
        if !inode.is_some_and(|v| owned.contains(&v)) {
            continue;
        }
        let local = columns[1].split_once(':');
        let remote = columns[2].split_once(':');
        if columns[3] == "0A" {
            if let Some((_, port)) = local {
                listeners.push(u16::from_str_radix(port, 16).unwrap_or_default());
            }
        } else if columns[3] == "01"
            && let (Some((_, local_port)), Some((address, remote_port))) = (local, remote)
        {
            established.push((
                u16::from_str_radix(local_port, 16).unwrap_or_default(),
                address.to_owned(),
                u16::from_str_radix(remote_port, 16).unwrap_or_default(),
            ));
        }
    }
    let outgoing: Vec<_> = established
        .into_iter()
        .filter(|(port, _, _)| !listeners.contains(port))
        .collect();
    let (ip, port) = outgoing
        .first()
        .map(|(_, address, port)| {
            let raw = u32::from_str_radix(address, 16)
                .unwrap_or_default()
                .to_le_bytes();
            (
                format!("{}.{}.{}.{}", raw[0], raw[1], raw[2], raw[3]),
                *port,
            )
        })
        .unwrap_or_default();
    (outgoing.len(), ip, port, true)
}
fn split_uci_list(value: &str) -> Vec<String> {
    value
        .split("' '")
        .map(|v| v.trim_matches('\'').to_owned())
        .filter(|v| !v.is_empty())
        .collect()
}
fn topflow_multiwan(sets: &[BTreeMap<String, String>], mode: &str, running: bool) -> Value {
    let source = sets.iter().find(|set| set.contains_key("mwan3.globals"));
    let Some(source) = source else {
        return json!({"mode":mode,"active":mode=="MULTIWAN","service_running":running,"sections":[]});
    };
    let mut ids: Vec<_> = source
        .iter()
        .filter_map(|(key, value)| {
            let id = key.strip_prefix("mwan3.")?;
            (!id.contains('.')
                && matches!(
                    value.as_str(),
                    "interface" | "member" | "policy" | "rule" | "globals"
                ))
            .then(|| id.to_owned())
        })
        .collect();
    ids.sort();
    let mut sections = Vec::new();
    for id in ids {
        let kind = source
            .get(&format!("mwan3.{id}"))
            .cloned()
            .unwrap_or_default();
        let mut item =
            serde_json::Map::from_iter([("id".into(), json!(id)), ("type".into(), json!(kind))]);
        let names: &[&str] = match kind.as_str() {
            "interface" => &[
                "enabled",
                "family",
                "track_method",
                "reliability",
                "timeout",
                "interval",
                "down",
                "up",
            ],
            "member" => &["interface", "metric", "weight"],
            "policy" => &["last_resort"],
            "rule" => &[
                "family",
                "proto",
                "src_ip",
                "src_port",
                "dest_ip",
                "dest_port",
                "use_policy",
                "sticky",
                "logging",
            ],
            "globals" => &["mmx_mask"],
            _ => &[],
        };
        for name in names {
            if let Some(value) = source.get(&format!("mwan3.{id}.{name}")) {
                item.insert((*name).into(), json!(value));
            }
        }
        for (name, key) in [("track_ip", "track_ip"), ("use_member", "use_member")] {
            if let Some(value) = source.get(&format!("mwan3.{id}.{key}")) {
                item.insert(name.into(), json!(split_uci_list(value)));
            }
        }
        sections.push(Value::Object(item));
    }
    json!({"mode":mode,"active":mode=="MULTIWAN","service_running":running,"sections":sections})
}
fn cooling_state(fan_uci: i64, liquid_uci: i64) -> Value {
    let zone = std::env::var("ZWRT_DATAD_COOLING_ZONE_PATH").unwrap_or_default();
    let pwm_path = std::env::var("ZWRT_DATAD_FAN_PWM_PATH")
        .unwrap_or_else(|_| "/sys/class/hwmon/hwmon0/pwm1".into());
    let fan_thermal_path = std::env::var("ZWRT_DATAD_FAN_THERMAL_ENABLE_PATH").unwrap_or_default();
    let liquid_thermal_path =
        std::env::var("ZWRT_DATAD_LIQUID_THERMAL_ENABLE_PATH").unwrap_or_default();
    let config_path = std::env::var("ZWRT_DATAD_COOLING_CONFIG")
        .unwrap_or_else(|_| "/data/zwrt-datad/cooling.conf".into());
    let config: BTreeMap<String, i64> = fs::read_to_string(config_path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (k, v) = line.split_once('=')?;
            Some((k.into(), v.trim().parse().ok()?))
        })
        .collect();
    let fan_always = config
        .get("fan_always_on")
        .copied()
        .or_else(|| config.get("fan_enabled").copied())
        .unwrap_or(fan_uci)
        != 0;
    let liquid_always = config
        .get("liquid_always_on")
        .copied()
        .or_else(|| config.get("liquid_enabled").copied())
        .unwrap_or(liquid_uci)
        != 0;
    let fan_mode = config.get("fan_mode").copied().unwrap_or_default();
    let zone_enabled = fs::read_to_string(format!("{zone}/mode"))
        .unwrap_or_default()
        .trim()
        == "enabled";
    let pwm = read_i64(pwm_path);
    let temp = read_i64(format!("{zone}/temp"));
    let fan_thermal = fs::read_to_string(fan_thermal_path)
        .unwrap_or_default()
        .contains("thermal_enable:1");
    let liquid_thermal = fs::read_to_string(liquid_thermal_path)
        .unwrap_or_default()
        .contains("thermal_enable:1");
    let mut factory = Vec::new();
    for (index, fallback_pwm) in [76, 128, 179].into_iter().enumerate() {
        let temperature = read_i64(format!("{zone}/trip_point_{index}_temp"));
        let hysteresis = read_i64(format!("{zone}/trip_point_{index}_hyst"));
        factory.push(json!({"level":index+1,"temperature_celsius":temperature/1000,"hysteresis_celsius":hysteresis/1000,"pwm":fallback_pwm,"speed_percent":((fallback_pwm*100+127)/255)}));
    }
    let custom_count = config
        .get("custom_curve_count")
        .copied()
        .unwrap_or_default()
        .clamp(0, 8);
    let mut custom = Vec::new();
    for index in 1..=custom_count {
        if let (Some(temp), Some(pwm)) = (
            config.get(&format!("custom_temperature_{index}")),
            config.get(&format!("custom_pwm_{index}")),
        ) {
            custom.push(
                json!({"temperature_celsius":temp,"pwm":pwm,"speed_percent":((pwm*100+127)/255)}),
            );
        }
    }
    let curve = if custom.is_empty() {
        factory.clone()
    } else {
        custom.clone()
    };
    let liquid_level = config.get("liquid_level").copied().unwrap_or(1).clamp(1, 2);
    json!({
        "fan":{"enabled":fan_always,"always_on":fan_always,"mode":if fan_always{"always_on"}else if fan_mode==2{"custom"}else if fan_mode==1||zone_enabled{"automatic"}else{"manual"},"pwm":pwm,"max_pwm":255,"speed_percent":((pwm*100+127)/255),"manual_speed_percent":config.get("fan_speed_percent").copied().unwrap_or_default(),"temperature_celsius":if temp>0{json!(temp/1000)}else{Value::Null},"hard_full_speed_celsius":80,"thermal_enabled":fan_thermal,"kernel_zone_enabled":zone_enabled,"levels_percent":[0,30,50,70]},
        "liquid":{"enabled":liquid_always,"always_on":liquid_always,"thermal_enabled":liquid_thermal,"mode":if liquid_always{if liquid_level==2{"high"}else{"low"}}else{"automatic"},"level":if liquid_always{liquid_level}else{0},"speed_percent":if liquid_always{if liquid_level==2{100}else{30}}else{0},"amplitude":if liquid_always{if liquid_level==2{200}else{60}}else{0},"levels_percent":[30,100]},
        "factory_curve":factory,"custom_curve":custom,"curve":curve
    })
}
fn topflow_net_fallback(raw: &mut Value, sets: &[BTreeMap<String, String>]) {
    if raw.get("network_type").is_some_and(|v| !v.is_null()) {
        return;
    }
    let mut out = Map::new();
    for (key, path) in [
        ("network_type", "zte_nwinfo.sys_info.network_type"),
        ("signalbar", "zte_nwinfo.signal_strength.signalbar"),
        ("simcard_roam", "zte_nwinfo.sys_info.simcard_roam"),
        (
            "network_provider_fullname",
            "zte_nwinfo.plmn_info.network_provider_fullname",
        ),
        ("wan_active_band", "zte_nwinfo.wan_active_band.GWLSA_band"),
        ("nr5g_action_band", "zte_nwinfo.wan_active_band.odu_nrband"),
        ("nr5g_rsrp", "zte_nwinfo.signal_strength.nr5g_rsrp"),
        ("nr5g_rsrq", "zte_nwinfo.signal_strength.nr5g_rsrq"),
        ("nr5g_snr", "zte_nwinfo.signal_strength.nr5g_snr"),
        ("rmcc", "zte_nwinfo.plmn_info.rmcc"),
        ("rmnc", "zte_nwinfo.plmn_info.rmnc"),
        ("cell_id", "zte_nwinfo.cell_info.cell_id"),
        ("lte_pci", "zte_nwinfo.cell_info.lte_pci"),
        (
            "wan_active_channel",
            "zte_nwinfo.cell_info.wan_active_channel",
        ),
        ("nr5g_pci", "zte_nwinfo.cell_info.nr5g_pci"),
        ("nr5g_cell_id", "zte_nwinfo.cell_info.nr5g_cellid"),
        (
            "nr5g_action_channel",
            "zte_nwinfo.cell_info.nr5g_action_channel",
        ),
        ("nr5g_bandwidth", "zte_nwinfo.cell_info.nr5g_bandwidth"),
        ("lte_bandwidth", "zte_nwinfo.cell_info.lte_bandwidth"),
        ("net_select", "zte_nwinfo.sys_info.net_select"),
        (
            "nr5g_sa_band_lock",
            "zte_nwinfo.band_lock.nr5g_sa_band_lock",
        ),
        (
            "nr5g_nsa_band_lock",
            "zte_nwinfo.band_lock.nr5g_nsa_band_lock",
        ),
        ("lte_band", "zte_nwinfo.band_lock.lte_ext_band_lock"),
    ] {
        let value = uci_get(sets, path);
        if !value.is_empty() {
            out.insert(key.into(), json!(value));
        }
    }
    *raw = Value::Object(out);
}
fn read_i64(path: impl AsRef<Path>) -> i64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or_default()
}
fn thermal_zones() -> (i64, Value, Value) {
    let mut zones = Vec::new();
    let mut runtime_zones = Vec::new();
    let mut cpuss = Vec::new();
    let root =
        std::env::var("ZWRT_DATAD_THERMAL_ROOT").unwrap_or_else(|_| "/sys/class/thermal".into());
    let Ok(entries) = fs::read_dir(root) else {
        return (0, json!([]), json!([]));
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with("thermal_zone")
        {
            continue;
        }
        let name = fs::read_to_string(p.join("type"))
            .unwrap_or_default()
            .trim()
            .to_owned();
        let raw = read_i64(p.join("temp"));
        if name.is_empty() || raw <= -40_000 || raw == 0 || raw > 200_000 {
            continue;
        }
        let c = raw as f64 / 1000.0;
        if name.starts_with("cpuss") {
            cpuss.push(c)
        }
        runtime_zones.push(json!({"type":name,"temp_milli":raw}));
        zones.push(json!({"name":name,"celsius":c}));
    }
    zones.sort_by_key(|a| string(a, "name"));
    runtime_zones.sort_by_key(|a| string(a, "type"));
    let cpu = if cpuss.is_empty() {
        zones
            .iter()
            .filter_map(|v| v.get("celsius").and_then(Value::as_f64))
            .fold(0.0, f64::max)
    } else {
        cpuss.iter().sum::<f64>() / cpuss.len() as f64
    };
    (
        cpu.round() as i64,
        Value::Array(zones),
        Value::Array(runtime_zones),
    )
}
fn lease_metadata() -> BTreeMap<String, (String, String)> {
    let path =
        std::env::var("ZWRT_DATAD_DHCP_LEASES_PATH").unwrap_or_else(|_| "/tmp/dhcp.leases".into());
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let p: Vec<_> = line.split_whitespace().collect();
            (p.len() >= 4).then(|| {
                (
                    p[1].to_ascii_lowercase(),
                    (
                        p[2].to_owned(),
                        if p[3] == "*" { "" } else { p[3] }.to_owned(),
                    ),
                )
            })
        })
        .collect()
}

fn client_array(value: &Value, key: &str) -> Option<Vec<Value>> {
    let leases = lease_metadata();
    value.get(key)?.as_array().map(|items| {
        items
            .iter()
            .filter_map(|entry| {
                let mac = ["mac_address", "mac", "mac_addr"]
                    .into_iter()
                    .map(|key| string(entry, key))
                    .find(|value| !value.is_empty())?
                    .to_ascii_lowercase();
                let lease = leases.get(&mac);
                let ip = ["ip_address", "ip", "ip_addr"]
                    .into_iter()
                    .map(|key| string(entry, key))
                    .find(|value| !value.is_empty())
                    .or_else(|| lease.map(|value| value.0.clone()))
                    .unwrap_or_default();
                let name = ["hostname", "name"]
                    .into_iter()
                    .map(|key| string(entry, key))
                    .find(|value| !value.is_empty() && value != "--" && value != "*")
                    .or_else(|| lease.map(|value| value.1.clone()))
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| mac.clone());
                Some(json!({"name":name,"ip":ip,"mac":mac}))
            })
            .collect()
    })
}

fn connected_clients(lan: &Value, wifi: &Value) -> (Value, i64, i64) {
    let wifi = client_array(wifi, "wireless_access_list_info").unwrap_or_default();
    let lan = client_array(lan, "lan_access_list_info").unwrap_or_default();
    let wifi_count = wifi.len() as i64;
    let lan_count = lan.len() as i64;
    (
        Value::Array(wifi.into_iter().chain(lan).collect()),
        wifi_count,
        lan_count,
    )
}
fn memory_fields(info: &Value) -> (i64, i64, i64) {
    let m = info.get("memory").unwrap_or(&Value::Null);
    let t = integer(m, "total");
    let a = integer(m, "available");
    (t, a, if t > 0 { (t - a) * 100 / t } else { -1 })
}

type CpuCounters = BTreeMap<String, (u64, u64)>;
static CPU_PREVIOUS: OnceLock<Mutex<CpuCounters>> = OnceLock::new();

fn count_lines(path: &str, header: bool) -> u64 {
    let count = fs::read_to_string(path)
        .map(|v| v.lines().count() as u64)
        .unwrap_or_default();
    count.saturating_sub(u64::from(header && count > 0))
}

fn tcp_active() -> u64 {
    fs::read_to_string("/proc/net/tcp")
        .unwrap_or_default()
        .lines()
        .skip(1)
        .filter(|line| line.split_whitespace().nth(3) == Some("01"))
        .count() as u64
}

fn meminfo() -> Value {
    let mut values = BTreeMap::new();
    let contents = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    for line in contents.lines() {
        if let Some((key, rest)) = line.split_once(':') {
            values.insert(
                key.to_owned(),
                rest.split_whitespace()
                    .next()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or_default(),
            );
        }
    }
    json!({"total":values.get("MemTotal").copied().unwrap_or_default(),"free":values.get("MemFree").copied().unwrap_or_default(),"available":values.get("MemAvailable").copied().unwrap_or_default(),"buffers":values.get("Buffers").copied().unwrap_or_default(),"cached":values.get("Cached").copied().unwrap_or_default(),"swap_total":values.get("SwapTotal").copied().unwrap_or_default(),"swap_free":values.get("SwapFree").copied().unwrap_or_default()})
}

// libc exposes statvfs counters with different integer widths across targets.
#[allow(clippy::unnecessary_cast)]
fn storage() -> Value {
    let Ok(path) = CString::new("/data") else {
        return json!({"total":0,"used":0,"available":0});
    };
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return json!({"total":0,"used":0,"available":0});
    }
    let block = if stat.f_frsize > 0 {
        stat.f_frsize
    } else {
        stat.f_bsize
    } as u64;
    let total = block.saturating_mul(stat.f_blocks as u64);
    let free = block.saturating_mul(stat.f_bfree as u64);
    json!({"total":total,"used":total.saturating_sub(free),"available":block.saturating_mul(stat.f_bavail as u64)})
}

#[derive(Clone, Copy)]
struct SpeedSample {
    rx: u64,
    tx: u64,
    at_ms: u64,
}

static LINK_PREVIOUS: OnceLock<Mutex<BTreeMap<String, SpeedSample>>> = OnceLock::new();
static SPEED_RING: OnceLock<Mutex<VecDeque<SpeedSample>>> = OnceLock::new();

fn iface_bytes(name: &str) -> Option<(u64, u64)> {
    let root =
        std::env::var("ZWRT_DATAD_NET_CLASS_ROOT").unwrap_or_else(|_| "/sys/class/net".into());
    let path = Path::new(&root).join(name).join("statistics");
    let rx = fs::read_to_string(path.join("rx_bytes"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let tx = fs::read_to_string(path.join("tx_bytes"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some((rx, tx))
}

fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn link_rates(now: u64) -> Value {
    let previous = LINK_PREVIOUS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut previous = previous.lock().unwrap();
    let mut rates = Vec::new();
    for name in ["rmnet_data0", "V3E1net0", "V3E2net0", "eth0"] {
        let Some((rx, tx)) = iface_bytes(name) else {
            previous.remove(name);
            continue;
        };
        let old = previous.get(name).copied();
        let window = old.map_or(0, |sample| now.saturating_sub(sample.at_ms));
        let valid = old.is_some_and(|sample| window > 0 && rx >= sample.rx && tx >= sample.tx);
        rates.push(json!({
            "interface":name,
            "source":"netdev",
            "window_ms":if valid { window } else { 0 },
            "rx_bytes":rx,
            "tx_bytes":tx,
            "rx_bps":if valid { json!(rx.saturating_sub(old.unwrap().rx).saturating_mul(1000)/window) } else { Value::Null },
            "tx_bps":if valid { json!(tx.saturating_sub(old.unwrap().tx).saturating_mul(1000)/window) } else { Value::Null }
        }));
        previous.insert(name.into(), SpeedSample { rx, tx, at_ms: now });
    }
    Value::Array(rates)
}

fn user_bytes() -> (u64, u64) {
    if let Some((rx, tx)) = iface_bytes("br-lan") {
        return (tx, rx);
    }
    let wifi: Vec<_> = ["wlan0", "wlan2"]
        .into_iter()
        .filter_map(iface_bytes)
        .collect();
    if !wifi.is_empty() {
        return wifi
            .into_iter()
            .fold((0_u64, 0_u64), |(rx, tx), (raw_rx, raw_tx)| {
                (rx.saturating_add(raw_tx), tx.saturating_add(raw_rx))
            });
    }
    ["rmnet_data0", "rmnet_ipa0"]
        .into_iter()
        .filter_map(iface_bytes)
        .fold((0_u64, 0_u64), |(rx, tx), (raw_rx, raw_tx)| {
            (rx.saturating_add(raw_tx), tx.saturating_add(raw_rx))
        })
}

fn throughput(now: u64) -> Value {
    let (rx, tx) = user_bytes();
    let ring = SPEED_RING.get_or_init(|| Mutex::new(VecDeque::with_capacity(16)));
    let mut ring = ring.lock().unwrap();
    if ring.len() == 16 {
        ring.pop_front();
    }
    ring.push_back(SpeedSample { rx, tx, at_ms: now });
    let Some(old) = ring.front().copied().filter(|_| ring.len() >= 2) else {
        return json!({"rx_bps":0,"tx_bps":0,"window_ms":0});
    };
    let window = now.saturating_sub(old.at_ms);
    if window == 0 {
        return json!({"rx_bps":0,"tx_bps":0,"window_ms":0});
    }
    json!({
        "rx_bps":rx.saturating_sub(old.rx).saturating_mul(1000)/window,
        "tx_bps":tx.saturating_sub(old.tx).saturating_mul(1000)/window,
        "window_ms":window
    })
}

fn runtime(runtime_zones: Value) -> (i64, Value) {
    let mut current = BTreeMap::new();
    for line in fs::read_to_string("/proc/stat").unwrap_or_default().lines() {
        let mut parts = line.split_whitespace();
        let Some(label) = parts.next() else { continue };
        if label != "cpu"
            && !label
                .strip_prefix("cpu")
                .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let nums: Vec<u64> = parts
            .take(8)
            .map(|v| v.parse().unwrap_or_default())
            .collect();
        if nums.len() < 4 {
            continue;
        }
        let total: u64 = nums.iter().sum();
        let idle = nums[3] + nums.get(4).copied().unwrap_or_default();
        current.insert(label.to_owned(), (total, idle));
    }
    let previous = CPU_PREVIOUS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut old = previous.lock().unwrap();
    let mut usage = Map::new();
    let mut total_usage = -1;
    for (label, (total, idle)) in &current {
        let value = old
            .get(label)
            .and_then(|(ot, oi)| {
                let dt = total.saturating_sub(*ot);
                let di = idle.saturating_sub(*oi);
                (dt > 0).then(|| ((dt.saturating_sub(di)) * 1000 / dt) as i64)
            })
            .unwrap_or(-1);
        if label == "cpu" {
            total_usage = value
        } else {
            usage.insert(label.clone(), json!(value));
        }
    }
    *old = current;
    drop(old);
    let mut freqs = Map::new();
    for core in usage.keys() {
        let root = format!("/sys/devices/system/cpu/{core}/cpufreq");
        let mut cur = read_i64(format!("{root}/scaling_cur_freq"));
        let mut max = read_i64(format!("{root}/scaling_max_freq"));
        if cur == 0 {
            cur = read_i64(format!("{root}/cpuinfo_cur_freq"))
        }
        if max == 0 {
            max = read_i64(format!("{root}/cpuinfo_max_freq"))
        }
        freqs.insert(core.clone(), json!({"cur":cur/1000,"max":max/1000}));
    }
    let active = tcp_active();
    let tcp4 = count_lines("/proc/net/tcp", true);
    let connections = json!({"tcp_active":active,"tcp_other":tcp4.saturating_sub(active),"tcp4":tcp4,"tcp6":count_lines("/proc/net/tcp6",true),"udp4":count_lines("/proc/net/udp",true),"udp6":count_lines("/proc/net/udp6",true),"unix":count_lines("/proc/net/unix",true)});
    let now = now_ms();
    (
        total_usage,
        json!({"cpu_usage_tenths":total_usage,"cpu_cores":usage,"cpu_freq_mhz":freqs,"thermal_zones":runtime_zones,"memory_kb":meminfo(),"storage":storage(),"connections":connections,"link_rates":link_rates(now),"throughput":throughput(now)}),
    )
}

pub async fn collect(sample_interval_ms: u64) -> Snapshot {
    // Vendor ubus implementations on these devices lose replies under a large
    // burst of concurrent clients, so state collection is deliberately serial.
    let common = ubus("zwrt_zte_mdm.api", "get_zwrt_common_info", json!({})).await;
    let board = ubus("system", "board", json!({})).await;
    let info = ubus("system", "info", json!({})).await;
    let net = ubus("zte_nwinfo_api", "nwinfo_get_netinfo", json!({})).await;
    let traffic = ubus(
        "zwrt_data",
        "get_wwandst",
        json!({"source_module":"deviceui","cid":1,"type":1}),
    )
    .await;
    let accounting = ubus(
        "zwrt_data",
        "get_wwandst",
        json!({"source_module":"web","cid":1,"type":4}),
    )
    .await;
    let limit = ubus(
        "zwrt_data",
        "get_wwandst_monthlimit",
        json!({"source_module":"web","cid":1}),
    )
    .await;
    let clear_day = ubus(
        "zwrt_data",
        "get_wwandst_clearday",
        json!({"source_module":"web","cid":1}),
    )
    .await;
    let sim = ubus("zwrt_zte_mdm.api", "get_sim_info", json!({})).await;
    let imei = ubus("zwrt_zte_mdm.api", "get_imei", json!({})).await;
    let lan_clients = ubus(
        "zwrt_router.api",
        "router_lan_access_list",
        json!({"start_id":1,"end_id":64}),
    )
    .await;
    let wifi_clients = ubus(
        "zwrt_router.api",
        "router_wireless_access_list",
        json!({"start_id":1,"end_id":64}),
    )
    .await;
    let router_status = ubus("zwrt_router.api", "router_get_status_no_auth", json!({})).await;
    let thermal = ubus("zwrt_bsp.thermal", "get_cpu_temp", json!({})).await;
    let usb = ubus("zwrt_bsp.usb", "list", json!({})).await;
    let battery = ubus("zwrt_bsp.battery", "list", json!({})).await;
    let charger = ubus("zwrt_bsp.charger", "list", json!({})).await;
    let nfc = ubus("zwrt_nfc", "zwrt_nfc_wifi_get", json!({})).await;
    let _ = crate::sms::prepare().await;
    let sms_capacity = ubus("zwrt_wms", "zwrt_wms_get_wms_capacity", json!({})).await;
    let sms_nv = ubus(
        "zwrt_wms",
        "zte_libwms_get_sms_data",
        json!({"page":0,"data_per_page":8,"mem_store":1,"tags":10,"order_by":"order by id desc"}),
    )
    .await;
    let sms_sim = ubus(
        "zwrt_wms",
        "zte_libwms_get_sms_data",
        json!({"page":0,"data_per_page":8,"mem_store":0,"tags":10,"order_by":"order by id desc"}),
    )
    .await;
    let lan_if = ubus("network.interface.lan", "status", json!({})).await;
    let wan4_if = ubus("network.interface.zte_wan", "status", json!({})).await;
    let wan6_if = ubus("network.interface.zte_wan6", "status", json!({})).await;
    let lan_config = ubus("zwrt_router.api", "router_get_lan_info", json!({})).await;
    let cellular = ubus(
        "zwrt_data",
        "get_wwaniface",
        json!({"source_module":"web","cid":1,"connect_status":""}),
    )
    .await;
    let common = object(common);
    let board = object(board);
    let info = object(info);
    let mut raw_net = object(net);
    let traffic = object(traffic);
    let accounting = object(accounting);
    let limit = object(limit);
    let clear_day = object(clear_day);
    let sim = object(sim);
    let imei = object(imei);
    let lan_clients = object(lan_clients);
    let wifi_clients = object(wifi_clients);
    let router_status = object(router_status);
    let thermal = object(thermal);
    let usb = object(usb);
    let battery_ok = battery.is_ok();
    let battery = object(battery);
    let charger = object(charger);
    let nfc_ok = nfc.is_ok();
    let nfc = object(nfc);
    let sms_ok = sms_capacity.is_ok();
    let sms_capacity = object(sms_capacity);
    let sms_nv = object(sms_nv);
    let sms_sim = object(sms_sim);
    let lan_if = object(lan_if);
    let wan4_if = object(wan4_if);
    let wan6_if = object(wan6_if);
    let lan_config = object(lan_config);
    let cellular = object(cellular);
    let packages = [
        "zwrt_zte_mdm",
        "zwrt_common_info",
        "network",
        "dhcp",
        "zwrt_data_commit",
        "system",
        "zwrt_web",
        "zwrt_tr069",
        "zwrt_router",
        "zte_nwinfo",
        "wireless",
        "mwan3",
    ];
    let mut uci_sets = Vec::new();
    for p in packages {
        uci_sets.push(uci_show(p).await)
    }
    let model_name = string(&common, "model_name");
    let hardware_version = string(&common, "hardware_version");
    let profile_source = if !model_name.is_empty() {
        "model_name"
    } else {
        "hardware_version"
    };
    let profile = normalize_profile(if !model_name.is_empty() {
        &model_name
    } else {
        &hardware_version
    });
    let template = match profile.as_str() {
        "mu5250" => "MU5250",
        "mu5252" => "MU5252",
        "mc7523" => "MC7523",
        "mc8532b" => "MC8532B",
        _ => "legacy_compat",
    };
    if matches!(template, "MU5250" | "MU5252" | "MC7523" | "MC8532B") {
        topflow_net_fallback(&mut raw_net, &uci_sets);
    }
    let mut net = Map::new();
    for (to, from) in [
        ("type", "network_type"),
        ("roaming", "simcard_roam"),
        ("operator", "network_provider_fullname"),
        ("band", "wan_active_band"),
        ("nr_band", "nr5g_action_band"),
        ("nr_snr", "nr5g_snr"),
        ("lte_snr", "lte_snr"),
        ("nr_bw", "nr5g_bandwidth"),
        ("nrca", "nrca"),
        ("lteca", "lteca"),
        ("ltecasig", "ltecasig"),
        ("net_select", "net_select"),
        ("sa_bands", "nr5g_sa_band_lock"),
        ("nsa_bands", "nr5g_nsa_band_lock"),
        ("lte_bands", "lte_band"),
        ("lte_supported_bands", "lte_band"),
        ("nr_sa_supported_bands", "nr5g_sa_band_lock"),
        ("nr_nsa_supported_bands", "nr5g_nsa_band_lock"),
    ] {
        net.insert(to.into(), json!(string(&raw_net, from)));
    }
    for (to, from) in [
        ("bars", "signalbar"),
        ("nr_rsrp", "nr5g_rsrp"),
        ("nr_rsrq", "nr5g_rsrq"),
        ("nr_rssi", "nr5g_rssi"),
        ("lte_rsrp", "lte_rsrp"),
        ("lte_rsrq", "lte_rsrq"),
        ("lte_rssi", "lte_rssi"),
        ("rssi", "rssi"),
        ("mcc", "rmcc"),
        ("mnc", "rmnc"),
        ("lte_pci", "lte_pci"),
        ("lte_cell_id", "cell_id"),
        ("lte_channel", "wan_active_channel"),
        ("nr_pci", "nr5g_pci"),
        ("nr_cell_id", "nr5g_cell_id"),
        ("nr_channel", "nr5g_action_channel"),
    ] {
        net.insert(to.into(), json!(integer(&raw_net, from)));
    }
    net.insert(
        "wan_status".into(),
        json!(string(&router_status, "current_wan_status")),
    );
    net.insert("HSR".into(), json!(false));
    let mut tout = Map::new();
    for (to, from) in [
        ("rx_speed", "real_rx_speed"),
        ("tx_speed", "real_tx_speed"),
        ("max_rx_speed", "real_max_rx_speed"),
        ("max_tx_speed", "real_max_tx_speed"),
        ("rx_bytes", "real_rx_bytes"),
        ("tx_bytes", "real_tx_bytes"),
        ("session_time", "real_time"),
    ] {
        tout.insert(to.into(), json!(integer(&traffic, from)));
    }
    for key in [
        "day_rx_bytes",
        "day_tx_bytes",
        "month_rx_bytes",
        "month_tx_bytes",
        "total_rx_bytes",
        "total_tx_bytes",
    ] {
        tout.insert(key.into(), json!(integer(&accounting, key)));
    }
    tout.insert("limit".into(), limit);
    tout.insert("clear_day".into(), clear_day);
    let wifi = ["main_2g", "main_5g"]
        .into_iter()
        .find(|s| {
            !uci_get(&uci_sets, &format!("wireless.{s}.ssid")).is_empty()
                && uci_get(&uci_sets, &format!("wireless.{s}.disabled")) != "1"
        })
        .or_else(|| {
            ["main_2g", "main_5g"]
                .into_iter()
                .find(|s| !uci_get(&uci_sets, &format!("wireless.{s}.ssid")).is_empty())
        });
    let (cpu_sys, zones, runtime_zones) = thermal_zones();
    let cpu_temp = ["cpuss_temp", "cpu_temp", "temperature", "temp"]
        .into_iter()
        .map(|k| integer(&thermal, k))
        .find(|v| *v > 0)
        .map(|v| if v >= 1000 { (v + 500) / 1000 } else { v })
        .unwrap_or(cpu_sys);
    let (mt, ma, mp) = memory_fields(&info);
    let release = board.get("release").unwrap_or(&Value::Null);
    let sw = {
        let a = string(&common, "wa_inner_version");
        if a.is_empty() {
            string(&common, "integrate_version")
        } else {
            a
        }
    };
    let mut fields = Map::new();
    fields.insert("net".into(), Value::Object(net));
    fields.insert("neighbor".into(),json!({"status":"disabled","enabled":false,"collector_running":false,"cells":[],"reason":"disabled_by_default","frames":0,"malformed":0,"partial":false,"discarded":0,"ambiguous_measurements":0,"capture_bytes":0,"generation":0,"sampled_at":Value::Null,"age_ms":Value::Null,"source":""}));
    let (client_list, wifi_count, lan_count) = connected_clients(&lan_clients, &wifi_clients);
    fields.insert(
        "clients".into(),
        json!({"total":wifi_count+lan_count,"wifi":wifi_count,"lan":lan_count,"list":client_list}),
    );
    let hide_battery = matches!(template, "MC7523" | "MC8532B");
    if !hide_battery
        && battery_ok
        && battery
            .as_object()
            .is_some_and(|v| v.keys().any(|k| k.starts_with("battery_")))
    {
        fields.insert("battery".into(),json!({"percent":integer(&battery,"battery_capacity"),"temp":integer(&battery,"battery_temperature"),"online":integer(&battery,"battery_online"),"health":integer(&battery,"battery_health"),"time_to_full":integer(&battery,"battery_time_to_full"),"charging":integer(&charger,"charge_status"),"charger_connect":integer(&charger,"charger_connect"),"charger_type":integer(&charger,"charger_type"),"chg_uv":read_i64("/sys/class/power_supply/usb/voltage_now"),"chg_ua":read_i64("/sys/class/power_supply/usb/current_now"),"bat_uv":read_i64("/sys/class/power_supply/battery/voltage_now"),"bat_ua":read_i64("/sys/class/power_supply/battery/current_now")}));
    }
    if let Some(mode) = charger
        .get("direct_power_supply_mode")
        .and_then(Value::as_str)
    {
        fields.insert("power".into(),json!({"direct_supply":{"supported":true,"enabled":match mode{"enable"=>json!(true),"disable"=>json!(false),_=>Value::Null},"mode":if matches!(mode,"enable"|"disable"){json!(mode)}else{Value::Null}}}));
    }
    if sms_ok {
        let list = crate::sms::normalize_lists(&[sms_nv.clone(), sms_sim.clone()]).await;
        fields.insert("sms".into(),json!({"unread":integer(&sms_capacity,"sms_dev_unread_num")+integer(&sms_capacity,"sms_sim_unread_num"),"list":list}));
    }
    fields.insert("traffic".into(), Value::Object(tout));
    let qos = crate::qos::read_for_plmn(integer(&raw_net, "rmcc"), integer(&raw_net, "rmnc"));
    fields.insert(
        "qos".into(),
        json!({"qci":qos.qci,"ambr_dl":qos.ambr_dl,"ambr_ul":qos.ambr_ul,"usb_mode":string(&usb,"mode")}),
    );
    if let Some(s) = wifi {
        fields.insert("wlan".into(),json!({"ssid":uci_get(&uci_sets,&format!("wireless.{s}.ssid")),"enc":uci_get(&uci_sets,&format!("wireless.{s}.encryption")),"enabled":i64::from(uci_get(&uci_sets,&format!("wireless.{s}.disabled"))!="1")}));
    }
    if nfc_ok
        && nfc.as_object().is_some_and(|v| {
            v.contains_key("switch") || v.contains_key("ap") || v.contains_key("wifi_ap")
        })
    {
        fields.insert("nfc".into(), json!({"switch":integer(&nfc,"switch")}));
    }
    fields.insert(
        "thermal".into(),
        json!({"cpu_celsius":cpu_temp,"zones":zones,"modems":[]}),
    );
    fields.insert("interfaces".into(),json!({"lan":interface(&lan_if),"wan4":interface(&wan4_if),"wan6":interface(&wan6_if),"lan_config":lan_config,"cellular":cellular}));
    const UF: &[(&str, &str)] = &[
        ("iccid", "zwrt_zte_mdm.sim_info.sim_iccid"),
        ("imsi", "zwrt_zte_mdm.sim_info.sim_imsi"),
        ("msisdn", "zwrt_zte_mdm.sim_info.msisdn"),
        ("mcc", "zwrt_zte_mdm.sim_info.mdm_mcc"),
        ("mnc", "zwrt_zte_mdm.sim_info.mdm_mnc"),
        ("imei", "zwrt_zte_mdm.device_info.imei"),
        ("mac_address", "zwrt_zte_mdm.device_info.wlan_mac_address"),
        ("modem_msn", "zwrt_zte_mdm.device_info.modem_msn"),
        (
            "wa_inner_version",
            "zwrt_common_info.common_config.wa_inner_version",
        ),
        (
            "integrate_version",
            "zwrt_common_info.common_config.integrate_version",
        ),
        (
            "common_model_name",
            "zwrt_common_info.common_config.model_name",
        ),
        (
            "device_alias_name",
            "zwrt_common_info.common_config.device_alias_name",
        ),
        (
            "device_market_name",
            "zwrt_common_info.common_config.device_market_name",
        ),
        ("lan_ipaddr", "network.lan.ipaddr"),
        ("lan_netmask", "network.lan.netmask"),
        ("wan_dns", "network.zte_wan.dns"),
        ("dhcpEnabled", "dhcp.lan.ignore"),
        ("dhcpStart", "dhcp.lan.zte_start"),
        ("dhcpEnd", "dhcp.lan.zte_end"),
        ("dhcpLease_hour", "dhcp.lan.leasetime"),
        ("hostname", "system.@system[0].hostname"),
        ("timezone", "system.@system[0].timezone"),
        ("web_language", "zwrt_web.setting.web_language"),
        ("login_timeout", "zwrt_web.config.login_timeout"),
        ("device_model", "zwrt_tr069.DeviceInfo.ModelName"),
        ("device_manufacturer", "zwrt_tr069.DeviceInfo.Manufacturer"),
        ("hardware_version", "zwrt_tr069.DeviceInfo.HardwareVersion"),
        ("software_version", "zwrt_tr069.DeviceInfo.SoftwareVersion"),
        ("serial_number", "zwrt_tr069.DeviceInfo.SerialNumber"),
        ("mtu", "zwrt_router.network.mtu"),
        ("mss", "zwrt_router.network.mss"),
        ("sim_states", "zwrt_zte_mdm.sim_info.sim_states"),
        ("modem_main_state", "zwrt_zte_mdm.sim_info.modem_main_state"),
        ("pin_status", "zwrt_zte_mdm.sim_info.pin_status"),
        (
            "hardware_version_ci",
            "zwrt_common_info.common_config.hardware_version",
        ),
        ("login_fail_num", "zwrt_web.config.login_fail_num"),
        (
            "login_fail_lock_timeout",
            "zwrt_web.config.login_fail_lock_timeout",
        ),
        ("day_tx_bytes", "zwrt_data_commit.wwancid1dst.day_tx_bytes"),
        ("day_rx_bytes", "zwrt_data_commit.wwancid1dst.day_rx_bytes"),
        ("day_time", "zwrt_data_commit.wwancid1dst.day_time"),
        (
            "month_tx_bytes",
            "zwrt_data_commit.wwancid1dst.month_tx_bytes",
        ),
        (
            "month_rx_bytes",
            "zwrt_data_commit.wwancid1dst.month_rx_bytes",
        ),
        ("month_time", "zwrt_data_commit.wwancid1dst.month_time"),
        (
            "total_tx_bytes",
            "zwrt_data_commit.wwancid1dst.total_tx_bytes",
        ),
        (
            "total_rx_bytes",
            "zwrt_data_commit.wwancid1dst.total_rx_bytes",
        ),
        ("total_time", "zwrt_data_commit.wwancid1dst.total_time"),
        ("radio_network_type", "zte_nwinfo.sys_info.network_type"),
        ("radio_signalbar", "zte_nwinfo.signal_strength.signalbar"),
        (
            "radio_operator",
            "zte_nwinfo.plmn_info.network_provider_fullname",
        ),
        ("radio_lte_band", "zte_nwinfo.wan_active_band.GWLSA_band"),
        ("radio_nr_band", "zte_nwinfo.wan_active_band.odu_nrband"),
        ("radio_lte_rsrp", "zte_nwinfo.signal_strength.lte_rsrp"),
        ("radio_lte_rsrq", "zte_nwinfo.signal_strength.lte_rsrq"),
        ("radio_lte_snr", "zte_nwinfo.signal_strength.lte_snr"),
        ("radio_nr_rsrp", "zte_nwinfo.signal_strength.nr5g_rsrp"),
        ("radio_nr_rsrq", "zte_nwinfo.signal_strength.nr5g_rsrq"),
        ("radio_nr_snr", "zte_nwinfo.signal_strength.nr5g_snr"),
        ("radio_lte_cell_id", "zte_nwinfo.cell_info.cell_id"),
        ("radio_lte_pci", "zte_nwinfo.cell_info.lte_pci"),
        (
            "radio_lte_channel",
            "zte_nwinfo.cell_info.wan_active_channel",
        ),
        ("radio_nr_pci", "zte_nwinfo.cell_info.nr5g_pci"),
        (
            "radio_nr_channel",
            "zte_nwinfo.cell_info.nr5g_action_channel",
        ),
        ("radio_nr_bandwidth", "zte_nwinfo.cell_info.nr5g_bandwidth"),
        ("radio_lteca", "zte_nwinfo.sys_info.lteca"),
        ("radio_net_select", "zte_nwinfo.sys_info.net_select"),
        (
            "radio_nr_sa_bands",
            "zte_nwinfo.band_lock.nr5g_sa_band_lock",
        ),
        (
            "radio_nr_nsa_bands",
            "zte_nwinfo.band_lock.nr5g_nsa_band_lock",
        ),
        ("radio_lte_bands", "zte_nwinfo.band_lock.lte_ext_band_lock"),
    ];
    let mut ui = Map::new();
    for (k, p) in UF {
        let v = uci_get(&uci_sets, p);
        if !v.is_empty() {
            ui.insert((*k).into(), json!(v));
        }
    }
    fields.insert("uci_device_info".into(), Value::Object(ui));
    let mut imsi = string(&sim, "sim_imsi");
    if !valid_imsi(&imsi) {
        imsi = uci_get(&uci_sets, "zwrt_zte_mdm.sim_info.sim_imsi").into();
        if !valid_imsi(&imsi) {
            imsi.clear();
        }
    }
    let mut msisdn = string(&sim, "msisdn");
    if !valid_msisdn(&msisdn) {
        msisdn = uci_get(&uci_sets, "zwrt_zte_mdm.sim_info.msisdn").into();
        if !valid_msisdn(&msisdn) {
            msisdn.clear();
        }
    }
    fields.insert("sim".into(),json!({"iccid":string(&sim,"sim_iccid"),"imsi":imsi.clone(),"msisdn":msisdn.clone(),"state":string(&sim,"sim_states"),"modem_state":string(&sim,"modem_main_state"),"pin_status":string(&sim,"pin_status"),"current_slot":integer(&sim,"current_sim_slot"),"dual_sim":integer(&sim,"support_dual_sim"),"sim1_provision":integer(&sim,"sim1_provision_state"),"sim2_provision":integer(&sim,"sim2_provision_state")}));
    if template == "MU5252" {
        let active_subid = integer(&sim, "current_sim_slot").clamp(1, 6);
        let x75_traffic = object(
            ubus(
                "zwrt_data",
                "get_wwandst",
                json!({"source_module":"deviceui","cid":1,"type":1,"subid":active_subid}),
            )
            .await,
        );
        let v3t = object(ubus("zwrt_zte_mdm.api", "get_v3t_sim_info", json!({})).await);
        let msim = object(ubus("zte_nwinfo_api", "nwinfo_get_msim_netinfo", json!({})).await);
        let mut modems = Vec::new();
        let network_type = string(&raw_net, "network_type");
        let bandwidth = if matches!(network_type.as_str(), "SA" | "NSA") {
            string(&raw_net, "nr5g_bandwidth")
        } else {
            string(&raw_net, "lte_bandwidth")
        };
        let x75_qos =
            crate::qos::read_for_plmn(integer(&raw_net, "rmcc"), integer(&raw_net, "rmnc"));
        modems.push(json!({
            "id":"x75","role":"integrated_5g","transport":"rmnet","subid":active_subid,
            "ifname":"rmnet_data0","wan_interface":"zte_mwan2",
            "net":{"type":network_type,"bars":integer(&raw_net,"signalbar"),"roaming":string(&raw_net,"simcard_roam"),"operator":string(&raw_net,"network_provider_fullname"),"band":string(&raw_net,"wan_active_band"),"bandwidth":bandwidth,"nr_rsrp":integer(&raw_net,"nr5g_rsrp"),"nr_rsrq":integer(&raw_net,"nr5g_rsrq"),"nr_snr":string(&raw_net,"nr5g_snr"),"nr_pci":integer(&raw_net,"nr5g_pci"),"nr_cell_id":integer(&raw_net,"nr5g_cell_id"),"nr_channel":integer(&raw_net,"nr5g_action_channel"),"lte_rsrp":integer(&raw_net,"lte_rsrp"),"lte_rsrq":integer(&raw_net,"lte_rsrq"),"lte_pci":integer(&raw_net,"lte_pci"),"cell_id":integer(&raw_net,"cell_id")},
            "sim":{"state":string(&sim,"sim_states"),"slot":integer(&sim,"current_sim_slot"),"iccid":string(&sim,"sim_iccid"),"imsi":imsi,"msisdn":msisdn,"imei":string(&imei,"imei")},
            "wwan":{"status":string(&cellular,"connect_status"),"ipv4_ifname":string(&cellular,"ipv4_dev_name"),"ipv6_ifname":string(&cellular,"ipv6_dev_name")},
            "interfaces":{"ipv4":interface(&wan4_if),"ipv6":interface(&wan6_if)},
            "traffic":realtime_traffic(&x75_traffic),
            "qos":{"qci":x75_qos.qci,"ambr_dl":x75_qos.ambr_dl,"ambr_ul":x75_qos.ambr_ul,"sampled_at":0}
        }));
        let mut thermal_modems = vec![
            json!({"id":"x75","available":cpu_temp>0,"celsius":if cpu_temp>0 {json!(cpu_temp)} else {Value::Null}}),
        ];
        let adb = std::env::var("ZWRT_DATAD_ADB_BIN").ok();
        for index in 0..2usize {
            let id = if index == 0 { "v3e1" } else { "v3e2" };
            let serial = if index == 0 {
                "V3E1T12345"
            } else {
                "V3E2T12345"
            };
            let ifname = if index == 0 { "V3E1net0" } else { "V3E2net0" };
            let wan = if index == 0 { "zte_mwan3" } else { "zte_mwan4" };
            let usb_path = if index == 0 { "1-1" } else { "1-2" };
            let usb_id = if index == 0 { "19d2:0581" } else { "19d2:1716" };
            let sim_prefix = format!("v3t_{}", index + 1);
            let slot = integer(&v3t, &format!("{sim_prefix}_st_slot")).clamp(0, 1);
            let subid = if index == 0 { 3 + slot } else { 5 + slot };
            let ext_net = topflow_external_net(&msim, &uci_sets, index, slot);
            let wwan = object(
                ubus(
                    "zwrt_data",
                    "get_wwaniface",
                    json!({"source_module":"deviceui","cid":1,"subid":subid}),
                )
                .await,
            );
            let ext_traffic = object(
                ubus(
                    "zwrt_data",
                    "get_wwandst",
                    json!({"source_module":"deviceui","cid":1,"type":1,"subid":subid}),
                )
                .await,
            );
            let ipv4 = object(ubus(&format!("network.interface.{wan}"), "status", json!({})).await);
            let ipv6 =
                object(ubus(&format!("network.interface.{wan}_6"), "status", json!({})).await);
            let mut ext_qos = crate::qos::Values::default();
            let mut sampled_at = 0i64;
            let mut external_temp = None;
            if let Some(adb) = &adb {
                if let Ok(raw) = command::run(
                    adb,
                    [
                        "-s",
                        serial,
                        "shell",
                        "grep QCI= /logfs/key.log | tail -n 64",
                    ],
                    Duration::from_secs(5),
                )
                .await
                    && let Some(parsed) = crate::qos::parse_external(&String::from_utf8_lossy(&raw))
                {
                    ext_qos = parsed;
                    sampled_at = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                }
                if let Ok(raw) = command::run(
                    adb,
                    [
                        "-s",
                        serial,
                        "shell",
                        "cat",
                        "/sys/devices/virtual/power/zte_power/adc2_temp",
                    ],
                    Duration::from_secs(5),
                )
                .await
                {
                    external_temp = String::from_utf8_lossy(&raw)
                        .trim()
                        .parse::<i64>()
                        .ok()
                        .filter(|v| (-40..=125).contains(v));
                }
            }
            thermal_modems.push(json!({"id":id,"available":external_temp.is_some(),"celsius":external_temp,"sampled_at":if external_temp.is_some(){sampled_at}else{0}}));
            modems.push(json!({
                "id":id,"role":"external_4g","transport":"cdc-ecm","subid":subid,"ifname":ifname,"wan_interface":wan,
                "usb":{"path":usb_path,"id":usb_id,"present":false,"carrier":0},
                "debug":{"transport":"adb","serial":serial,"available":adb.is_some()},
                "net":ext_net,
                "sim":{"state":prefixed_string(&v3t,&format!("{sim_prefix}_"),"modem_main_state"),"slot":slot,"iccid":prefixed_string(&v3t,&format!("{sim_prefix}_"),"sim_iccid"),"imsi":prefixed_string(&v3t,&format!("{sim_prefix}_"),"sim_imsi"),"msisdn":prefixed_string(&v3t,&format!("{sim_prefix}_"),"msisdn"),"imei":prefixed_string(&v3t,&format!("{sim_prefix}_"),"imei")},
                "wwan":{"status":string(&wwan,"connect_status"),"ipv4_ifname":string(&wwan,"ipv4_dev_name"),"ipv6_ifname":string(&wwan,"ipv6_dev_name")},
                "interfaces":{"ipv4":interface(&ipv4),"ipv6":interface(&ipv6)},
                "traffic":realtime_traffic(&ext_traffic),
                "qos":{"qci":ext_qos.qci,"ambr_dl":ext_qos.ambr_dl,"ambr_ul":ext_qos.ambr_ul,"sampled_at":sampled_at}
            }));
        }
        if let Some(thermal) = fields.get_mut("thermal") {
            thermal["modems"] = Value::Array(thermal_modems);
        }
        fields.insert("modems".into(), Value::Array(modems));
        let mode = uci_read("zwrt_router.network.opms_wan_mode").await;
        let mwan_running = std::env::var("ZWRT_DATAD_MWAN3_RUNNING")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .is_some_and(|v| v != 0);
        let mwan_status = object(ubus("mwan3", "status", json!({})).await);
        let mut paths = Vec::new();
        let interfaces = mwan_status
            .get("interfaces")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (id, label, name) in [
            ("x75", "X75", "zte_mwan2"),
            ("v3e1", "V3E1", "zte_mwan3"),
            ("v3e2", "V3E2", "zte_mwan4"),
            ("ethernet", "Ethernet", "waneth"),
        ] {
            let Some(path) = interfaces.get(name) else {
                continue;
            };
            let enabled = parse_bool(path, "enabled");
            let running = parse_bool(path, "running");
            let mwan_up = parse_bool(path, "up");
            let iface =
                object(ubus(&format!("network.interface.{name}"), "status", json!({})).await);
            let interface_up = parse_bool(&iface, "up");
            let up = mwan_up || interface_up;
            if !enabled && !running && !up {
                continue;
            }
            let status = string(path, "status");
            let online = status.eq_ignore_ascii_case("online")
                || (running && mwan_up)
                || (!mwan_running && interface_up);
            let mut targets = Vec::new();
            if let Some(raw_targets) = path.get("track_ip").and_then(Value::as_array) {
                for target in raw_targets {
                    let target_status = string(target, "status");
                    let target_online =
                        matches!(target_status.to_ascii_lowercase().as_str(), "online" | "up");
                    targets.push(json!({"ip":string(target,"ip"),"status":target_status,"online":target_online,"latency_ms":target.get("latency").cloned().unwrap_or(Value::Null),"packet_loss_percent":target.get("packetloss").cloned().unwrap_or(Value::Null)}));
                }
            }
            let preferred = targets
                .iter()
                .find(|target| target["online"] == true)
                .or_else(|| targets.iter().find(|target| target["status"] != "skipped"));
            let mut result = json!({"id":id,"label":label,"interface":name,"enabled":enabled,"running":running,"up":up,"online":online,"interface_up":interface_up,"interface_available":parse_bool(&iface,"available"),"interface_pending":parse_bool(&iface,"pending"),"status":status,"tracking":string(path,"tracking"),"uptime_seconds":integer(path,"uptime"),"targets":targets});
            if let Some(preferred) = preferred {
                if !preferred["latency_ms"].is_null() {
                    result["latency_ms"] = preferred["latency_ms"].clone();
                }
                if !preferred["packet_loss_percent"].is_null() {
                    result["packet_loss_percent"] = preferred["packet_loss_percent"].clone();
                }
            }
            paths.push(result);
        }
        let path_count = paths.len();
        let online_path_count = paths.iter().filter(|path| path["online"] == true).count();
        let (tcp_tunnel_count, runtime_ip, runtime_port, icg_running) = tcp_aggregation_summary();
        let provisioned = !uci_read("zwrt_router.icgmwan.IcgDevId").await.is_empty();
        let remaining = uci_read("zwrt_router.icgmwan.residual_flow").await;
        let today_used = uci_read("zwrt_router.icgmwan.count_flow_today").await;
        let config_path = std::env::var("ZWRT_DATAD_ICG_CONFIG")
            .unwrap_or_else(|_| "/etc/config/icg.conf".into());
        let icg_config: BTreeMap<String, String> = fs::read_to_string(config_path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let (k, v) = line.split_once('=')?;
                Some((k.trim().into(), v.trim().into()))
            })
            .collect();
        let server_ip = if runtime_ip.is_empty() {
            icg_config
                .get("AggregationServerIP")
                .cloned()
                .unwrap_or_default()
        } else {
            runtime_ip
        };
        let tcp_port = if runtime_port == 0 {
            icg_config
                .get("AggregationServerTcpPort")
                .and_then(|v| v.parse().ok())
                .unwrap_or_default()
        } else {
            runtime_port
        };
        let udp_port = icg_config
            .get("AggregationServerUdpStartPort")
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or_default();
        let enabled = mode == "SMULTIWAN";
        fields.insert("aggregation".into(), json!({"enabled":enabled,"mode":mode,"state":if !enabled{"disabled"}else if !provisioned{"unprovisioned"}else if tcp_tunnel_count>0{"online"}else{"waiting"},"provisioned":provisioned,"online":tcp_tunnel_count>0,"controller":{"icg_process_running":icg_running,"mwan3_running":mwan_running},"tcp_tunnel_count":tcp_tunnel_count,"server":{"ip":server_ip,"tcp_port":tcp_port,"udp_start_port":udp_port,"source":if runtime_port>0{"runtime"}else{"config"}},"paths":paths,"path_count":path_count,"online_path_count":online_path_count,"traffic":{"remaining_bytes":remaining.parse::<u64>().ok(),"remaining_raw":remaining,"today_used_bytes":today_used.parse::<u64>().ok(),"today_used_raw":today_used}}));
        fields.insert(
            "multiwan".into(),
            topflow_multiwan(&uci_sets, &mode, mwan_running),
        );
        let fan_enabled = uci_read("zwrt_deviceui.Device.fan_switch_status")
            .await
            .parse()
            .unwrap_or_default();
        let liquid_enabled = uci_read("zwrt_deviceui.Device.liquid_cooling_switch_status")
            .await
            .parse()
            .unwrap_or_default();
        fields.insert("cooling".into(), cooling_state(fan_enabled, liquid_enabled));
    } else {
        fields.insert("modems".into(), json!([]));
    }
    fields.insert("dhcp".into(),json!({"ip":uci_get(&uci_sets,"network.lan.ipaddr"),"start":uci_get(&uci_sets,"dhcp.lan.start"),"limit":uci_get(&uci_sets,"dhcp.lan.limit"),"leasetime":uci_get(&uci_sets,"dhcp.lan.leasetime")}));
    let template_label = if template == "legacy_compat" {
        "Legacy compatibility fallback"
    } else {
        template
    };
    fields.insert("device".into(),json!({"profile":profile,"profile_source":profile_source,"api_template":template,"api_template_label":template_label,"api_template_supported":i64::from(template!="legacy_compat"),"full_ubus":1,"vendor":string(&common,"manufacturer"),"model_name":model_name,"hardware_version":hardware_version,"market_name":string(&common,"device_market_name"),"alias_name":string(&common,"device_alias_name"),"board_name":string(&board,"board_name")}));
    let (cpu_usage_tenths, runtime) = runtime(runtime_zones);
    let cpu_usage = if cpu_usage_tenths >= 0 {
        (cpu_usage_tenths + 5) / 10
    } else {
        -1
    };
    fields.insert("system".into(),json!({"uptime":integer(&info,"uptime"),"cpu_temp":cpu_temp,"cpu_usage":cpu_usage,"mem_used_pct":mp,"mem_total":mt,"mem_avail":ma,"model":string(&board,"model"),"hostname":string(&board,"hostname"),"fw":string(release,"description"),"sw_version":sw,"imei":string(&imei,"imei")}));
    fields.insert("sample_interval_ms".into(), json!(sample_interval_ms));
    fields.insert("runtime".into(), runtime);
    Snapshot {
        ts: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
        datad: Default::default(),
        fields,
    }
}

pub async fn ubus(service: &str, method: &str, args: Value) -> Result<Value, String> {
    validate_name(service)?;
    validate_name(method)?;
    if !args.is_object() {
        return Err("args must be an object".into());
    }
    let body = serde_json::to_string(&args).map_err(|e| e.to_string())?;
    let raw = command::run(
        &ubus_bin(),
        ["call", service, method, &body],
        Duration::from_secs(8),
    )
    .await
    .map_err(|e| e.to_string())?;
    if raw.iter().all(u8::is_ascii_whitespace) {
        return Ok(json!({}));
    }
    serde_json::from_slice(&raw).map_err(|e| format!("invalid ubus JSON: {e}"))
}
pub async fn ubus_list(verbose: bool) -> Result<Value, String> {
    let args: Vec<&str> = if verbose {
        vec!["-v", "list"]
    } else {
        vec!["list"]
    };
    let raw = command::run(&ubus_bin(), args, Duration::from_secs(8))
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({"ok":true,"verbose":verbose,"output":String::from_utf8_lossy(&raw)}))
}
fn validate_name(v: &str) -> Result<(), String> {
    if v.is_empty()
        || v.len() > 128
        || !v
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err("invalid ubus name".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profile() {
        assert_eq!(normalize_profile("MC7523 HW1.0"), "mc7523_hw1_0");
        assert_eq!(normalize_profile("MU5250"), "mu5250")
    }
    #[test]
    fn iface() {
        let v = interface(&json!({}));
        assert_eq!(v["up"], false);
        assert_eq!(v["ipv4"], json!([]))
    }
    #[test]
    fn connected_clients_ignore_historical_leases() {
        let (items, wifi, lan) = connected_clients(
            &json!({"lan_access_list_info":[]}),
            &json!({"wireless_access_list_info":[{"mac_address":"AA:BB:CC:DD:EE:FF","ip_address":"192.168.0.2","hostname":"online"}]}),
        );
        assert_eq!(wifi, 1);
        assert_eq!(lan, 0);
        assert_eq!(items.as_array().unwrap().len(), 1);
        assert_eq!(items[0]["mac"], "aa:bb:cc:dd:ee:ff");
    }
}
