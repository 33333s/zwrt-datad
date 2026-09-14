use crate::{command, model::Snapshot};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs,
    path::Path,
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
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
fn read_i64(path: impl AsRef<Path>) -> i64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or_default()
}
fn thermal_zones() -> (i64, Value) {
    let mut zones = Vec::new();
    let mut cpuss = Vec::new();
    let root =
        std::env::var("ZWRT_DATAD_THERMAL_ROOT").unwrap_or_else(|_| "/sys/class/thermal".into());
    let Ok(entries) = fs::read_dir(root) else {
        return (0, json!([]));
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
        if name.is_empty() || raw <= 0 || raw > 200_000 {
            continue;
        }
        let c = if raw >= 1000 {
            raw as f64 / 1000.0
        } else {
            raw as f64
        };
        if name.starts_with("cpuss") {
            cpuss.push(c)
        }
        zones.push(json!({"name":name,"celsius":c}));
    }
    zones.sort_by_key(|a| string(a, "name"));
    let cpu = if cpuss.is_empty() {
        zones
            .iter()
            .filter_map(|v| v.get("celsius").and_then(Value::as_f64))
            .fold(0.0, f64::max)
    } else {
        cpuss.iter().sum::<f64>() / cpuss.len() as f64
    };
    (cpu.round() as i64, Value::Array(zones))
}
fn clients_from_leases() -> Value {
    Value::Array(
        fs::read_to_string("/tmp/dhcp.leases")
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let p: Vec<_> = line.split_whitespace().collect();
                (p.len() >= 4)
                    .then(|| json!({"name":if p[3]=="*"{""}else{p[3]},"ip":p[2],"mac":p[1]}))
            })
            .collect(),
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

fn runtime() -> (i64, Value) {
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
    (
        total_usage,
        json!({"cpu_usage_tenths":total_usage,"cpu_cores":usage,"cpu_freq_mhz":freqs,"thermal_zones":[],"memory_kb":meminfo(),"storage":storage(),"connections":connections,"link_rates":[],"throughput":{"rx_bps":0,"tx_bps":0,"window_ms":0}}),
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
    let user_count = ubus("zwrt_router.api", "router_get_user_list_num", json!({})).await;
    let router_status = ubus("zwrt_router.api", "router_get_status_no_auth", json!({})).await;
    let thermal = ubus("zwrt_bsp.thermal", "get_cpu_temp", json!({})).await;
    let usb = ubus("zwrt_bsp.usb", "list", json!({})).await;
    let battery = ubus("zwrt_bsp.battery", "list", json!({})).await;
    let charger = ubus("zwrt_bsp.charger", "list", json!({})).await;
    let nfc = ubus("zwrt_nfc", "zwrt_nfc_wifi_get", json!({})).await;
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
    let raw_net = object(net);
    let traffic = object(traffic);
    let accounting = object(accounting);
    let limit = object(limit);
    let clear_day = object(clear_day);
    let sim = object(sim);
    let imei = object(imei);
    let user_count = object(user_count);
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
    let (cpu_sys, zones) = thermal_zones();
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
    let cl = clients_from_leases();
    fields.insert("clients".into(),json!({"total":integer(&user_count,"access_total_num"),"wifi":integer(&user_count,"wireless_num"),"lan":integer(&user_count,"lan_num"),"list":cl}));
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
        let mut list = Vec::new();
        for reply in [&sms_nv, &sms_sim] {
            if let Some(items) = reply.get("list").and_then(Value::as_array) {
                list.extend(items.iter().take(32 - list.len()).cloned())
            }
        }
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
    fields.insert("sim".into(),json!({"iccid":string(&sim,"sim_iccid"),"imsi":string(&sim,"sim_imsi"),"msisdn":string(&sim,"msisdn"),"state":string(&sim,"sim_states"),"modem_state":string(&sim,"modem_main_state"),"pin_status":string(&sim,"pin_status"),"current_slot":integer(&sim,"current_sim_slot"),"dual_sim":integer(&sim,"support_dual_sim"),"sim1_provision":integer(&sim,"sim1_provision_state"),"sim2_provision":integer(&sim,"sim2_provision_state")}));
    fields.insert("modems".into(), json!([]));
    fields.insert("dhcp".into(),json!({"ip":uci_get(&uci_sets,"network.lan.ipaddr"),"start":uci_get(&uci_sets,"dhcp.lan.start"),"limit":uci_get(&uci_sets,"dhcp.lan.limit"),"leasetime":uci_get(&uci_sets,"dhcp.lan.leasetime")}));
    let template_label = if template == "legacy_compat" {
        "Legacy compatibility fallback"
    } else {
        template
    };
    fields.insert("device".into(),json!({"profile":profile,"profile_source":profile_source,"api_template":template,"api_template_label":template_label,"api_template_supported":i64::from(template!="legacy_compat"),"full_ubus":1,"vendor":string(&common,"manufacturer"),"model_name":model_name,"hardware_version":hardware_version,"market_name":string(&common,"device_market_name"),"alias_name":string(&common,"device_alias_name"),"board_name":string(&board,"board_name")}));
    let (cpu_usage_tenths, runtime) = runtime();
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
}
