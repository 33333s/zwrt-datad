//! Kernel-level readers for the U50 collector.
//!
//! The U50 firmware has no ubus/uci, but it is an ordinary Linux system: CPU,
//! memory, thermal zones, battery, interfaces and Wi-Fi stations are read from
//! procfs/sysfs and the stock `ip`/`iw` tools, using the same `/state` shapes
//! as the ZWRT collector. Every path can be redirected with
//! `ZWRT_DATAD_U50_ROOT` for fixture tests.
use crate::command;
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, fs, net::IpAddr, path::PathBuf, time::Duration};

const MAX_ZONES: usize = 64;
const MAX_CLIENTS: usize = 32;
/// Thermal zones that report a valid reading are always in this range.
const TEMP_MIN_MILLI: i64 = -40_000;
const TEMP_MAX_MILLI: i64 = 150_000;

fn at(path: &str) -> PathBuf {
    let root = std::env::var("ZWRT_DATAD_U50_ROOT").unwrap_or_default();
    PathBuf::from(format!("{}{path}", root.trim_end_matches('/')))
}

fn read(path: &str) -> Option<String> {
    fs::read_to_string(at(path))
        .ok()
        .map(|value| value.trim().to_owned())
}

fn read_i64(path: &str) -> Option<i64> {
    read(path)?.parse().ok()
}

fn tool(env: &str, default: &str) -> String {
    std::env::var(env).unwrap_or_else(|_| default.into())
}

/// `(name, milli-degrees)` for every plausible temperature sensor.
///
/// Unavailable sensors read `-273000` or fail with EINVAL, the PMIC exposes
/// current/voltage "level" pseudo-zones, and each SoC sensor is duplicated as
/// `-usr`/`-lowf`; none of those are temperatures worth reporting.
pub fn thermal_zones() -> Vec<(String, i64)> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(at("/sys/class/thermal")) else {
        return out;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with("thermal_zone"))
        .collect();
    names.sort_by_key(|name| {
        name.trim_start_matches("thermal_zone")
            .parse::<u32>()
            .unwrap_or(u32::MAX)
    });
    for name in names {
        let base = format!("/sys/class/thermal/{name}");
        let Some(kind) = read(&format!("{base}/type")) else {
            continue;
        };
        if kind.contains("-lvl") || kind.ends_with("-lowf") || kind == "battery_zte" {
            continue;
        }
        let Some(milli) = read_i64(&format!("{base}/temp"))
            .filter(|v| (TEMP_MIN_MILLI..=TEMP_MAX_MILLI).contains(v))
        else {
            continue;
        };
        out.push((kind, milli));
        if out.len() == MAX_ZONES {
            break;
        }
    }
    out
}

pub fn cpu_celsius(zones: &[(String, i64)]) -> Option<i64> {
    ["cpu0-0-usr", "mdm-core-0-usr", "mdm-q6-usr"]
        .iter()
        .find_map(|wanted| zones.iter().find(|(name, _)| name == wanted))
        .map(|(_, milli)| (milli + 500) / 1000)
}

pub fn thermal_block(zones: &[(String, i64)]) -> Value {
    let list: Vec<Value> = zones
        .iter()
        .map(|(name, milli)| json!({"name":name,"celsius":*milli as f64 / 1000.0}))
        .collect();
    let mut out = json!({"zones":list});
    if let Some(cpu) = cpu_celsius(zones) {
        out["cpu_celsius"] = json!(cpu);
    }
    out
}

pub fn runtime_zones(zones: &[(String, i64)]) -> Value {
    Value::Array(
        zones
            .iter()
            .map(|(name, milli)| json!({"type":name,"temp_milli":milli}))
            .collect(),
    )
}

/// `system` block: uptime, memory, hostname and the OS build.
pub fn system_block(cpu_usage_pct: Option<i64>, cpu_celsius: Option<i64>) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(uptime) =
        read("/proc/uptime").and_then(|v| v.split_whitespace().next()?.parse::<f64>().ok())
    {
        out.insert("uptime".into(), json!(uptime as i64));
    }
    let mut mem = BTreeMap::new();
    for line in read("/proc/meminfo").unwrap_or_default().lines() {
        if let Some((key, rest)) = line.split_once(':')
            && let Some(kb) = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<u64>().ok())
        {
            mem.insert(key.to_owned(), kb.saturating_mul(1024));
        }
    }
    let total = mem.get("MemTotal").copied().unwrap_or_default();
    let available = mem.get("MemAvailable").copied().unwrap_or_default();
    if total > 0 {
        out.insert("mem_total".into(), json!(total));
        out.insert("mem_avail".into(), json!(available));
        out.insert(
            "mem_used_pct".into(),
            json!(total.saturating_sub(available) * 100 / total),
        );
    }
    if let Some(value) = cpu_usage_pct {
        out.insert("cpu_usage".into(), json!(value));
    }
    if let Some(value) = cpu_celsius {
        out.insert("cpu_temp".into(), json!(value));
    }
    if let Some(hostname) = read("/proc/sys/kernel/hostname").filter(|v| !v.is_empty()) {
        out.insert("hostname".into(), json!(hostname));
    }
    let os_name = read("/etc/os-release")
        .map(|text| os_release(&text))
        .filter(|v| !v.is_empty());
    let kernel = read("/proc/sys/kernel/osrelease").unwrap_or_default();
    let fw = match (os_name, kernel.is_empty()) {
        (Some(name), false) => format!("{name} (Linux {kernel})"),
        (Some(name), true) => name,
        (None, false) => format!("Linux {kernel}"),
        (None, true) => String::new(),
    };
    if !fw.is_empty() {
        out.insert("fw".into(), json!(fw));
    }
    out
}

fn os_release(text: &str) -> String {
    let get = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .map(|v| v.trim().trim_matches('"').to_owned())
            .unwrap_or_default()
    };
    let pretty = get("PRETTY_NAME");
    if !pretty.is_empty() {
        return pretty;
    }
    format!("{} {}", get("NAME"), get("VERSION"))
        .trim()
        .to_owned()
}

/// Battery details beyond the OEM `cfg` percentage/temperature: measured
/// voltage/current, charger presence and health from the power-supply class.
pub fn battery_extra() -> Map<String, Value> {
    battery_extra_with(|name, file| read(&format!("/sys/class/power_supply/{name}/{file}")))
}

/// Match the mainline `battery.charging` contract (Linux power-supply status),
/// rather than turning every state other than charging into discharging.
fn charging_status(text: &str) -> Option<i64> {
    match text.trim() {
        "Unknown" => Some(0),
        "Charging" => Some(1),
        "Discharging" => Some(2),
        "Not charging" => Some(3),
        "Full" => Some(4),
        _ => None,
    }
}

fn battery_extra_with(ps: impl Fn(&str, &str) -> Option<String>) -> Map<String, Value> {
    let mut out = Map::new();
    let ps_i64 = |name: &str, file: &str| ps(name, file)?.parse::<i64>().ok();
    if let Some(online) = ps_i64("battery_zte", "online") {
        out.insert("online".into(), json!(online));
    }
    if let Some(health) = ps("battery_zte", "health").or_else(|| ps("battery", "health")) {
        out.insert("health".into(), json!(i64::from(health == "Good")));
        out.insert("health_text".into(), json!(health));
    }
    let primary_status = ps("battery_zte", "status");
    let fallback_status = ps("battery", "status");
    let recognized = primary_status
        .as_ref()
        .and_then(|status| charging_status(status).map(|value| (status, value)))
        .or_else(|| {
            fallback_status
                .as_ref()
                .and_then(|status| charging_status(status).map(|value| (status, value)))
        });
    if let Some((status, value)) = recognized {
        out.insert("charging".into(), json!(value));
        out.insert("status".into(), json!(status));
    } else if let Some(status) = primary_status.or(fallback_status) {
        // Keep diagnostic text without asserting a known charging state.
        out.insert("status".into(), json!(status));
    }
    let presence = [
        ("charger_zte", "present"),
        ("usb", "present"),
        ("usb", "online"),
    ]
    .iter()
    .find_map(|(name, file)| ps_i64(name, file).filter(|value| matches!(value, 0 | 1)));
    if let Some(present) = presence {
        out.insert("charger_connect".into(), json!(present));
    }
    if let Some(kind) = ps("charger_zte", "type") {
        out.insert("charger_type_name".into(), json!(kind));
    }
    if let Some(value) = ps_i64("usb", "voltage_now") {
        out.insert("chg_uv".into(), json!(value));
    }
    if let Some(value) = ps_i64("usb", "input_current_now") {
        out.insert("chg_ua".into(), json!(value));
    }
    if let Some(value) = ps_i64("battery", "voltage_now") {
        out.insert("bat_uv".into(), json!(value));
    }
    // Same sign convention as the ZWRT collector: positive while charging.
    // The fuel-gauge node already follows it; the charger-side `battery` node
    // reports the opposite sign.
    if let Some(value) =
        ps_i64("bms", "current_now").or_else(|| ps_i64("battery", "current_now").map(|v| -v))
    {
        out.insert("bat_ua".into(), json!(value));
    }
    if let Some(value) = ps_i64("battery", "cycle_count") {
        out.insert("cycle_count".into(), json!(value));
    }
    if let Some(value) = ps_i64("battery_zte", "nominal_capacity_mah_mbb") {
        out.insert("capacity_mah".into(), json!(value));
    }
    out
}

fn parse_addr(text: &str) -> Option<(IpAddr, u32)> {
    let (address, prefix) = text.split_once('/')?;
    Some((address.parse().ok()?, prefix.parse().ok()?))
}

/// Global-scope addresses of one interface from `ip -o addr show dev X`.
async fn addresses(dev: &str, family: &str) -> Vec<Value> {
    let program = tool("ZWRT_DATAD_U50_IP_BIN", "/sbin/ip");
    let Ok(raw) = command::run(
        &program,
        ["-o", family, "addr", "show", "dev", dev],
        Duration::from_secs(2),
    )
    .await
    else {
        return Vec::new();
    };
    parse_addresses(&String::from_utf8_lossy(&raw), family == "-4")
}

fn parse_addresses(text: &str, v4: bool) -> Vec<Value> {
    let want = if v4 { "inet" } else { "inet6" };
    let mut out = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(pos) = fields.iter().position(|f| *f == want) else {
            continue;
        };
        let global = fields
            .iter()
            .position(|f| *f == "scope")
            .and_then(|i| fields.get(i + 1))
            == Some(&"global");
        let Some((address, mask)) = fields.get(pos + 1).and_then(|v| parse_addr(v)) else {
            continue;
        };
        if global && address.is_ipv4() == v4 {
            out.push(json!({"address":address.to_string(),"mask":mask}));
        }
        if out.len() == 4 {
            break;
        }
    }
    out
}

fn iface_up(dev: &str) -> bool {
    read(&format!("/sys/class/net/{dev}/flags"))
        .and_then(|v| i64::from_str_radix(v.trim_start_matches("0x"), 16).ok())
        .is_some_and(|flags| flags & 1 == 1)
}

fn dns_list(cfg: &BTreeMap<String, String>, keys: &[&str]) -> Vec<Value> {
    keys.iter()
        .filter_map(|key| cfg.get(*key))
        .filter_map(|value| value.parse::<IpAddr>().ok())
        .map(|ip| json!(ip.to_string()))
        .collect()
}

/// `interfaces.{lan,wan4,wan6}` in the ubus `network.interface.*` shape the
/// ZWRT collector emits (`up`, `proto`, `device`, `ipv4[]`, `ipv6[]`, `dns[]`).
pub async fn interfaces(cfg: &BTreeMap<String, String>) -> Value {
    let lan_dev = "bridge0";
    let wan4_dev = cfg
        .get("wan_v4_dev_name")
        .map_or("rmnet_data0", String::as_str);
    let wan6_dev = cfg.get("wan_v6_dev_name").map_or(wan4_dev, String::as_str);
    let lan4 = addresses(lan_dev, "-4").await;
    let wan4 = addresses(wan4_dev, "-4").await;
    let wan6 = addresses(wan6_dev, "-6").await;
    let iface = |dev: &str, proto: &str, ipv4: Vec<Value>, ipv6: Vec<Value>, dns: Vec<Value>| {
        json!({
            "up": iface_up(dev) && (!ipv4.is_empty() || !ipv6.is_empty()),
            "proto": proto,
            "device": dev,
            "ipv4": ipv4,
            "ipv6": ipv6,
            "dns": dns
        })
    };
    json!({
        "lan": iface(lan_dev, "static", lan4, Vec::new(), dns_list(cfg, &["lan_ipaddr"])),
        "wan4": iface(wan4_dev, "cellular", wan4, Vec::new(),
            dns_list(cfg, &["prefer_dns_auto", "standby_dns_auto"])),
        "wan6": iface(wan6_dev, "cellular", Vec::new(), wan6,
            dns_list(cfg, &["ipv6_prefer_dns_auto", "ipv6_standby_dns_auto"])),
    })
}

fn valid_mac(value: &str) -> bool {
    value.len() == 17
        && value.split(':').count() == 6
        && value
            .split(':')
            .all(|part| part.len() == 2 && part.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// dnsmasq lease file lines are `expiry mac ip hostname client-id`.
fn parse_leases(text: &str) -> BTreeMap<String, (String, String)> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 || !valid_mac(fields[1]) || fields[2].parse::<IpAddr>().is_err() {
            continue;
        }
        let name = if fields[3] == "*" { "" } else { fields[3] };
        out.insert(
            fields[1].to_ascii_lowercase(),
            (fields[2].to_owned(), name.chars().take(64).collect()),
        );
    }
    out
}

/// `iw dev X station dump` header lines: `Station <mac> (on <dev>)`.
fn parse_stations(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.strip_prefix("Station "))
        .filter_map(|rest| rest.split_whitespace().next())
        .filter(|mac| valid_mac(mac))
        .map(str::to_ascii_lowercase)
        .collect()
}

/// `/proc/net/arp` rows for completed neighbours on one device.
fn parse_arp(text: &str, dev: &str) -> Vec<(String, String)> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            (f.len() >= 6 && f[2] == "0x2" && f[5] == dev && valid_mac(f[3]))
                .then(|| (f[3].to_ascii_lowercase(), f[0].to_owned()))
        })
        .collect()
}

/// Online clients: Wi-Fi stations from `iw` (the AP's own address is listed as
/// a station by this driver and is skipped), plus wired/USB neighbours that
/// answered ARP while the USB link has carrier. DHCP leases only supply names
/// and addresses; they never decide who is online.
pub async fn clients() -> Option<Value> {
    let iw = tool("ZWRT_DATAD_U50_IW_BIN", "/usr/sbin/iw");
    let raw = command::run(
        &iw,
        ["dev", "wlan0", "station", "dump"],
        Duration::from_secs(2),
    )
    .await
    .ok()?;
    let own: Vec<String> = ["wlan0", "bridge0"]
        .iter()
        .filter_map(|dev| read(&format!("/sys/class/net/{dev}/address")))
        .map(|mac| mac.to_ascii_lowercase())
        .collect();
    let wifi: Vec<String> = parse_stations(&String::from_utf8_lossy(&raw))
        .into_iter()
        .filter(|mac| !own.contains(mac))
        .collect();
    let leases = parse_leases(&read("/etc_rw/ztembb/configs/dnsmasq.leases").unwrap_or_default());
    let arp = parse_arp(&read("/proc/net/arp").unwrap_or_default(), "bridge0");
    let usb_link = read_i64("/sys/class/net/rndis0/carrier") == Some(1);
    let lan: Vec<(String, String)> = if usb_link {
        arp.iter()
            .filter(|(mac, _)| !wifi.contains(mac) && !own.contains(mac))
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let mut list = Vec::new();
    for (mac, arp_ip) in wifi
        .iter()
        .map(|mac| {
            let ip = arp.iter().find(|(m, _)| m == mac).map(|(_, ip)| ip.clone());
            (mac.clone(), ip.unwrap_or_default())
        })
        .chain(lan.iter().cloned())
    {
        let (ip, name) = leases
            .get(&mac)
            .cloned()
            .unwrap_or_else(|| (arp_ip.clone(), String::new()));
        let ip = if arp_ip.is_empty() { ip } else { arp_ip };
        list.push(json!({"name":name,"ip":ip,"mac":mac}));
        if list.len() == MAX_CLIENTS {
            break;
        }
    }
    Some(json!({
        "total": wifi.len() + lan.len(),
        "wifi": wifi.len(),
        "lan": lan.len(),
        "list": list
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn battery_fixture(name: &str, values: &[(&str, &str, &str)]) -> Map<String, Value> {
        let dir =
            std::env::temp_dir().join(format!("u50-sys-battery-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for (supply, file, value) in values {
            let supply = dir.join(supply);
            fs::create_dir_all(&supply).unwrap();
            fs::write(supply.join(file), format!("{value}\n")).unwrap();
        }
        // Use a local reader so these fixtures cannot race the process-wide
        // ZWRT_DATAD_U50_ROOT override used by the thermal test.
        let out = battery_extra_with(|supply, file| {
            fs::read_to_string(dir.join(supply).join(file))
                .ok()
                .map(|value| value.trim().to_owned())
        });
        let _ = fs::remove_dir_all(dir);
        out
    }

    #[test]
    fn battery_status_preserves_full_unplugged_and_unknown_states() {
        for (status, expected, present) in [
            ("Charging", 1, "1"),
            ("Discharging", 2, "0"),
            ("Not charging", 3, "1"),
            ("Full", 4, "1"),
            ("Unknown", 0, "0"),
        ] {
            let mut battery = battery_fixture(
                "states",
                &[
                    ("battery_zte", "status", status),
                    ("charger_zte", "present", present),
                    ("charger_zte", "type", "Mains"),
                ],
            );
            assert_eq!(battery["charging"], expected, "{status}");
            assert_eq!(battery["status"], status);
            assert_eq!(battery["charger_connect"], present.parse::<i64>().unwrap());
            assert_eq!(battery["charger_type_name"], "Mains");
            assert!(!battery.contains_key("charger_type"));

            // Full/unplugged states must remain usable by power forwarding,
            // whose mainline contract accepts known status values 1..=4.
            battery.insert("percent".into(), json!(100));
            assert_eq!(
                crate::sms_forward::power_state(Some(&Value::Object(battery))),
                (expected > 0).then_some((100, expected))
            );
        }
    }

    #[test]
    fn battery_status_uses_valid_fallback_without_fabricating_a_state() {
        let fallback = battery_fixture(
            "fallback",
            &[
                ("battery_zte", "status", "unsupported"),
                ("battery", "status", "Full"),
            ],
        );
        assert_eq!(fallback["charging"], 4);
        assert_eq!(fallback["status"], "Full");
        let absent = battery_fixture("fallback", &[("battery", "status", "Discharging")]);
        assert_eq!(absent["charging"], 2);
        let unknown = battery_fixture(
            "fallback",
            &[
                ("battery_zte", "status", "Unknown"),
                ("battery", "status", "Charging"),
            ],
        );
        assert_eq!(unknown["charging"], 0);
        let invalid = battery_fixture(
            "fallback",
            &[
                ("battery_zte", "status", "unsupported"),
                ("battery", "status", "invalid"),
            ],
        );
        assert!(!invalid.contains_key("charging"));
        assert_eq!(invalid["status"], "unsupported");
        assert!(battery_fixture("fallback", &[]).is_empty());
    }

    #[test]
    fn charger_presence_validates_flags_and_preserves_explicit_disconnect() {
        let fallback = battery_fixture(
            "presence",
            &[("charger_zte", "present", "9"), ("usb", "present", "1")],
        );
        assert_eq!(fallback["charger_connect"], 1);
        let disconnected = battery_fixture(
            "presence",
            &[("charger_zte", "present", "0"), ("usb", "online", "1")],
        );
        assert_eq!(disconnected["charger_connect"], 0);
        let online = battery_fixture("presence", &[("usb", "online", "1")]);
        assert_eq!(online["charger_connect"], 1);
        let invalid = battery_fixture(
            "presence",
            &[
                ("charger_zte", "present", "-1"),
                ("usb", "present", "2"),
                ("usb", "online", "bad"),
            ],
        );
        assert!(!invalid.contains_key("charger_connect"));
    }

    #[test]
    fn parses_ip_output_and_keeps_only_global_addresses_of_the_family() {
        let v4 = "12: rmnet_data0    inet 10.38.1.22/30 scope global rmnet_data0\\       valid_lft forever\n";
        assert_eq!(
            parse_addresses(v4, true),
            vec![json!({"address":"10.38.1.22","mask":30})]
        );
        let v6 = "12: rmnet_data0    inet6 2408:8418:6380:4f5a::1/64 scope global \\  valid_lft forever\n\
                  12: rmnet_data0    inet6 fe80::1/64 scope link \\  valid_lft forever\n";
        assert_eq!(
            parse_addresses(v6, false),
            vec![json!({"address":"2408:8418:6380:4f5a::1","mask":64})]
        );
        assert!(parse_addresses(v4, false).is_empty());
    }

    #[test]
    fn stations_leases_and_arp_parse_strictly() {
        let dump = "Station b8:d4:bc:b5:48:cf (on wlan0)\n\trx packets:\t0\nStation AA:BB:CC:00:11:22 (on wlan0)\nStation bad (on wlan0)\n";
        assert_eq!(
            parse_stations(dump),
            vec!["b8:d4:bc:b5:48:cf", "aa:bb:cc:00:11:22"]
        );
        let leases = parse_leases(
            "1790785134 aa:bb:cc:00:11:22 192.168.0.195 LAPTOP 01:aa\n1 zz 1.1.1.1 x\n2 aa:bb:cc:00:11:33 192.168.0.9 * *\n",
        );
        assert_eq!(leases["aa:bb:cc:00:11:22"].1, "LAPTOP");
        assert_eq!(leases["aa:bb:cc:00:11:33"].1, "");
        assert_eq!(leases.len(), 2);
        let arp = "IP address       HW type     Flags       HW address            Mask     Device\n\
                   192.168.0.5      0x1         0x2         aa:bb:cc:00:11:44     *        bridge0\n\
                   192.168.0.6      0x1         0x0         aa:bb:cc:00:11:55     *        bridge0\n\
                   10.0.0.1         0x1         0x2         aa:bb:cc:00:11:66     *        rmnet_data0\n";
        assert_eq!(
            parse_arp(arp, "bridge0"),
            vec![("aa:bb:cc:00:11:44".into(), "192.168.0.5".into())]
        );
    }

    #[test]
    fn thermal_selection_drops_unavailable_and_pseudo_zones() {
        let dir = std::env::temp_dir().join(format!("u50-sys-thermal-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for (index, (kind, temp)) in [
            ("cpu0-0-usr", "42800"),
            ("cpu0-0-lowf", "43200"),
            ("modem-mmw0-usr", "-273000"),
            ("pm8150b-vbat-lvl0", "4334"),
            ("battery_zte", "3400"),
            ("battery", "34700"),
        ]
        .into_iter()
        .enumerate()
        {
            let zone = dir.join(format!("sys/class/thermal/thermal_zone{index}"));
            fs::create_dir_all(&zone).unwrap();
            fs::write(zone.join("type"), format!("{kind}\n")).unwrap();
            fs::write(zone.join("temp"), format!("{temp}\n")).unwrap();
        }
        // A zone whose temp cannot be read (EINVAL on the device) is skipped.
        let broken = dir.join("sys/class/thermal/thermal_zone9");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join("type"), "soc\n").unwrap();
        // SAFETY: tests in this module are the only users of the variable.
        unsafe { std::env::set_var("ZWRT_DATAD_U50_ROOT", &dir) };
        let zones = thermal_zones();
        unsafe { std::env::remove_var("ZWRT_DATAD_U50_ROOT") };
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(
            zones,
            vec![
                ("cpu0-0-usr".to_owned(), 42800),
                ("battery".to_owned(), 34700)
            ]
        );
        assert_eq!(cpu_celsius(&zones), Some(43));
        assert_eq!(thermal_block(&zones)["cpu_celsius"], 43);
    }

    #[test]
    fn os_release_prefers_pretty_name() {
        assert_eq!(
            os_release("ID=\"mdm\"\nNAME=\"mdm\"\nVERSION=\"202607090026\"\n"),
            "mdm 202607090026"
        );
        assert_eq!(os_release("PRETTY_NAME=\"X 1\"\nNAME=x\n"), "X 1");
    }
}
