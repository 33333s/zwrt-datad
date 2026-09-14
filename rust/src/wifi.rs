use crate::{command, state};
use futures_util::future::join_all;
use serde_json::{Map, Value, json};
use std::{collections::HashSet, time::Duration};

#[derive(Clone, Debug, Default)]
struct Live {
    name: String,
    ssid: String,
    kind: String,
    phy: String,
    index: i64,
    frequency: i64,
    dbm: Option<f64>,
    ready: bool,
    psm: Option<bool>,
}

#[derive(Clone, Debug)]
struct Interface {
    section: &'static str,
    kind: &'static str,
    band: usize,
    ssid: String,
    configured: String,
    encryption: String,
    psm_mode: String,
    enabled: bool,
    hidden: bool,
    isolate: bool,
    has_key: bool,
    live: Option<usize>,
}

fn iw_bin() -> String {
    std::env::var("ZWRT_DATAD_IW_BIN").unwrap_or_else(|_| "/usr/sbin/iw".into())
}
fn hostapd_cli_bin() -> String {
    std::env::var("ZWRT_DATAD_HOSTAPD_CLI_BIN").unwrap_or_else(|_| "/usr/sbin/hostapd_cli".into())
}
fn runtime_dir() -> String {
    std::env::var("ZWRT_DATAD_WIFI_RUNTIME_DIR").unwrap_or_else(|_| "/data/zwrt-datad/wifi".into())
}

pub async fn advanced_status() -> Result<Value, String> {
    let model = state::uci_read("zwrt_common_info.common_config.model_name").await;
    if model != "MU5252" {
        return Ok(json!({"supported":false}));
    }
    let wireless = state::ubus("uci", "get", json!({"config":"wireless"})).await?;
    let values = wireless
        .get("values")
        .and_then(Value::as_object)
        .ok_or("cannot read wireless configuration")?;
    let extras = state::ubus("uci", "get", json!({"config":"datad_wifi"}))
        .await
        .ok()
        .and_then(|value| value.get("values").and_then(Value::as_object).cloned())
        .unwrap_or_default();
    let radios = [radio(values, "wifi0", "2g")?, radio(values, "wifi1", "5g")?];
    let mut interfaces = Vec::new();
    for (section, kind, band) in [
        ("main_2g", "main", 0),
        ("guest_2g", "guest", 0),
        ("main_5g", "main", 1),
        ("guest_5g", "guest", 1),
    ] {
        interfaces.push(interface(values, section, kind, band, false)?);
    }
    for (section, configured) in [("datad_ssid_1", "wlan4"), ("datad_ssid_2", "wlan5")] {
        if let Some(value) = extras.get(section).and_then(Value::as_object) {
            let band = usize::from(text(value, "band") == "5g");
            let mut item = interface(&extras, section, "extra", band, true)?;
            item.configured = configured.into();
            interfaces.push(item);
        }
    }
    let raw = command::run(&iw_bin(), ["dev"], Duration::from_secs(5))
        .await
        .unwrap_or_default();
    let mut live = parse_iw_dev(&String::from_utf8_lossy(&raw));
    match_live(&mut interfaces, &live);
    let checks = live.iter().map(|item| live_details(item.clone()));
    live = join_all(checks).await;
    let mut radio_output = Vec::new();
    for (index, configured) in radios.iter().enumerate() {
        let selected = interfaces
            .iter()
            .filter(|item| item.band == index)
            .filter_map(|item| item.live)
            .find(|slot| live.get(*slot).is_some_and(|item| item.ready));
        let reported = selected.and_then(|slot| live[slot].dbm);
        let limit = match selected {
            Some(slot) => regulatory_limit(&live[slot]).await,
            None => None,
        };
        radio_output.push(json!({
            "band":configured["band"],
            "enabled":configured["enabled"],
            "percent":configured["percent"],
            "configured_dbm":configured["configured_dbm"],
            "requested_dbm":configured["requested_dbm"],
            "reported_dbm":reported,
            "regulatory_limit_dbm":limit,
        }));
    }
    let interface_output: Vec<_> = interfaces
        .iter()
        .map(|item| {
            let current = item.live.and_then(|slot| live.get(slot));
            json!({
                "section":item.section,"kind":item.kind,"band":if item.band == 0 {"2g"} else {"5g"},
                "ssid":item.ssid,"enabled":item.enabled,"radio_enabled":radios[item.band]["enabled"],
                "active":current.is_some_and(|value| value.ready),
                "ifname":current.map(|value| value.name.as_str()).unwrap_or(""),
                "hidden":item.hidden,"isolate":item.isolate,"has_key":item.has_key,
                "encryption":item.encryption,
                "txpower_dbm":current.filter(|value| value.ready).and_then(|value| value.dbm),
                "error":"","psm_mode":item.psm_mode,
                "psm_actual":current.and_then(|value| value.psm),
            })
        })
        .collect();
    Ok(json!({
        "supported":true,"max_configured_ssids":6,"extra_ssid_slots":2,
        "extra_ssid_supported":true,"hardware_limit_dbm":Value::Null,
        "radios":radio_output,"interfaces":interface_output
    }))
}

pub async fn wireless_config_status() -> Result<Value, String> {
    let mut channel_2g = Vec::new();
    let mut channel_5g = Vec::new();
    if let Some(results) = iwinfo("freqlist").await {
        parse_channels(&results, &mut channel_2g, &mut channel_5g);
    }
    let from_iwinfo = !channel_2g.is_empty() || !channel_5g.is_empty();
    if channel_2g.is_empty() {
        channel_2g = parse_channel_list(&state::uci_read("wireless.wifi0.channellist").await);
    }
    if channel_5g.is_empty() {
        channel_5g = parse_channel_list(&state::uci_read("wireless.wifi1.channellist").await);
    }
    let country_2g = state::uci_read("wireless.wifi0.country").await;
    let country_5g = state::uci_read("wireless.wifi1.country").await;
    let mut countries = iwinfo("countrylist")
        .await
        .map(|results| parse_countries(&results))
        .unwrap_or_default();
    for country in [&country_2g, &country_5g] {
        let normalized = country.to_ascii_uppercase();
        if valid_country(&normalized) && !countries.contains(&normalized) {
            countries.push(normalized);
        }
    }
    Ok(json!({
        "country_scope":"device",
        "country":if country_2g == country_5g { country_2g } else { String::new() },
        "channel_source":if from_iwinfo { "iwinfo" } else { "uci" },
        "countries":countries,
        "radios":{
            "2g":wireless_radio("wifi0","main_2g",channel_2g).await,
            "5g":wireless_radio("wifi1","main_5g",channel_5g).await,
        }
    }))
}

async fn iwinfo(method: &str) -> Option<Vec<Value>> {
    let mut devices = vec![
        state::uci_read("wireless.main_5g.ifname").await,
        state::uci_read("wireless.main_2g.ifname").await,
    ];
    devices.extend(["wlan0".into(), "wlan1".into(), "wlanx".into()]);
    for device in devices {
        if !safe_ifname(&device) {
            continue;
        }
        if let Ok(value) = state::ubus("iwinfo", method, json!({"device":device})).await
            && let Some(results) = value.get("results").and_then(Value::as_array)
        {
            return Some(results.clone());
        }
    }
    None
}

async fn wireless_radio(section: &str, ap_section: &str, mut channels: Vec<i64>) -> Value {
    channels.sort_unstable();
    channels.dedup();
    channels.retain(|channel| *channel > 0 && *channel <= 255);
    channels.insert(0, 0);
    let prefix = format!("wireless.{section}");
    let country = state::uci_read(&format!("{prefix}.country")).await;
    let channel = state::uci_read(&format!("{prefix}.channel")).await;
    let htmode = state::uci_read(&format!("{prefix}.htmode")).await;
    let disabled = state::uci_read(&format!("{prefix}.disabled")).await;
    json!({
        "section":section,"ap_section":ap_section,"country":country,
        "channel":if channel.is_empty() { "0" } else { &channel },
        "htmode":htmode,"enabled":disabled != "1","supported_channels":channels
    })
}

fn parse_channels(results: &[Value], channel_2g: &mut Vec<i64>, channel_5g: &mut Vec<i64>) {
    for item in results {
        let restricted = item
            .get("restricted")
            .is_some_and(|value| value == true || value.as_i64() == Some(1));
        if restricted {
            continue;
        }
        let channel = item
            .get("channel")
            .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
            .unwrap_or_default();
        match item.get("band").and_then(Value::as_i64) {
            Some(2) if channel > 0 => channel_2g.push(channel),
            Some(5) if channel > 0 => channel_5g.push(channel),
            _ => {}
        }
    }
}

fn parse_channel_list(value: &str) -> Vec<i64> {
    let mut channels: Vec<_> = value
        .split(',')
        .filter_map(|value| value.trim().parse::<i64>().ok())
        .filter(|value| (1..=255).contains(value))
        .collect();
    channels.sort_unstable();
    channels.dedup();
    channels
}

fn parse_countries(results: &[Value]) -> Vec<String> {
    let mut countries = Vec::new();
    for item in results {
        let country = item
            .get("iso3166")
            .or_else(|| item.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_uppercase();
        if valid_country(&country) && !countries.contains(&country) {
            countries.push(country);
        }
    }
    countries
}

fn valid_country(value: &str) -> bool {
    value == "00" || (value.len() == 2 && value.bytes().all(|byte| byte.is_ascii_alphabetic()))
}
fn safe_ifname(value: &str) -> bool {
    !value.is_empty()
        && value.len() < 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
}

fn radio(values: &Map<String, Value>, section: &str, band: &str) -> Result<Value, String> {
    let value = values
        .get(section)
        .and_then(Value::as_object)
        .ok_or("cannot read wireless configuration")?;
    let requested = number(value, "datad_txpower_dbm", -1);
    Ok(json!({
        "band":band,"enabled":number(value,"disabled",0)==0,
        "percent":number(value,"txpowerpercent",100),
        "configured_dbm":number(value,"txpower",-1),
        "requested_dbm":if requested < 0 { Value::Null } else { json!(requested) }
    }))
}

fn interface(
    values: &Map<String, Value>,
    section: &'static str,
    kind: &'static str,
    band: usize,
    extra: bool,
) -> Result<Interface, String> {
    let value = values
        .get(section)
        .and_then(Value::as_object)
        .ok_or("cannot read wireless configuration")?;
    Ok(Interface {
        section,
        kind,
        band,
        ssid: text(value, "ssid"),
        configured: if extra {
            String::new()
        } else {
            text(value, "ifname")
        },
        encryption: text(value, "encryption"),
        psm_mode: match text(value, "datad_psm").as_str() {
            "on" => "on".into(),
            "off" => "off".into(),
            _ => "default".into(),
        },
        enabled: number(value, "disabled", 0) == 0,
        hidden: number(value, "hidden", 0) != 0,
        isolate: number(value, "isolate", 0) != 0,
        has_key: !text(value, "key").is_empty(),
        live: None,
    })
}

fn text(value: &Map<String, Value>, key: &str) -> String {
    match value.get(key) {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Number(value)) => value.to_string(),
        Some(Value::Bool(value)) => i64::from(*value).to_string(),
        _ => String::new(),
    }
}
fn number(value: &Map<String, Value>, key: &str, default: i64) -> i64 {
    value
        .get(key)
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
                .or_else(|| value.as_bool().map(i64::from))
        })
        .unwrap_or(default)
}

fn parse_iw_dev(input: &str) -> Vec<Live> {
    let mut output = Vec::new();
    let mut phy = String::new();
    for raw in input.lines() {
        let line = raw.trim();
        if let Some(value) = line.strip_prefix("phy#") {
            phy = format!("phy{}", value.trim());
        } else if let Some(value) = line.strip_prefix("Interface ") {
            output.push(Live {
                name: value.trim().into(),
                phy: phy.clone(),
                ..Default::default()
            });
        } else if let Some(current) = output.last_mut() {
            if let Some(value) = line.strip_prefix("ssid ") {
                current.ssid = value.into();
            } else if let Some(value) = line.strip_prefix("ifindex ") {
                current.index = value.parse().unwrap_or_default();
            } else if let Some(value) = line.strip_prefix("type ") {
                current.kind = value.into();
            } else if let Some(value) = line.strip_prefix("channel ") {
                current.frequency = value
                    .split_once('(')
                    .and_then(|(_, rest)| rest.split_whitespace().next())
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_default();
            } else if let Some(value) = line.strip_prefix("txpower ") {
                current.dbm = value
                    .split_whitespace()
                    .next()
                    .and_then(|value| value.parse().ok());
            }
        }
    }
    output
}

fn match_live(configured: &mut [Interface], live: &[Live]) {
    for item in configured.iter_mut() {
        let matches: Vec<_> = live
            .iter()
            .enumerate()
            .filter(|(_, value)| {
                value.kind == "AP"
                    && value.ssid == item.ssid
                    && usize::from(value.frequency >= 4900) == item.band
                    && value.frequency != 0
            })
            .map(|(index, _)| index)
            .collect();
        item.live = matches
            .iter()
            .copied()
            .find(|index| live[*index].name == item.configured)
            .or_else(|| (matches.len() == 1).then_some(matches[0]));
    }
    let mut seen = HashSet::new();
    let duplicates: HashSet<_> = configured
        .iter()
        .filter_map(|item| item.live)
        .filter(|slot| !seen.insert(*slot))
        .collect();
    for item in configured {
        if item.live.is_some_and(|slot| duplicates.contains(&slot)) {
            item.live = None;
        }
    }
}

async fn live_details(mut live: Live) -> Live {
    let control = if matches!(live.name.as_str(), "wlan4" | "wlan5") {
        runtime_dir()
    } else {
        "/data/vendor/wifi/hostapd".into()
    };
    let status = command::run(
        &hostapd_cli_bin(),
        ["-p", &control, "-i", &live.name, "status"],
        Duration::from_secs(3),
    )
    .await
    .unwrap_or_default();
    live.ready = String::from_utf8_lossy(&status)
        .lines()
        .any(|line| line.trim() == "state=ENABLED");
    let psm = command::run(
        &iw_bin(),
        ["dev", &live.name, "get", "power_save"],
        Duration::from_secs(3),
    )
    .await
    .unwrap_or_default();
    let psm = String::from_utf8_lossy(&psm);
    live.psm = if psm.contains("Power save: on") {
        Some(true)
    } else if psm.contains("Power save: off") {
        Some(false)
    } else {
        None
    };
    live
}

async fn regulatory_limit(live: &Live) -> Option<i64> {
    let raw = command::run(
        &iw_bin(),
        ["phy", &live.phy, "info"],
        Duration::from_secs(3),
    )
    .await
    .ok()?;
    parse_regulatory_limit(&String::from_utf8_lossy(&raw), live.frequency)
}

fn parse_regulatory_limit(input: &str, frequency: i64) -> Option<i64> {
    input.lines().find_map(|raw| {
        let line = raw.trim().strip_prefix('*')?.trim();
        if line.contains("disabled") {
            return None;
        }
        let (frequency_raw, rest) = line.split_once(" MHz [")?;
        if frequency_raw.parse::<i64>().ok()? != frequency {
            return None;
        }
        let power = rest.split_once("] (")?.1.split_once(" dBm)")?.0;
        power.parse::<f64>().ok().map(|value| value as i64)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iw_and_regulatory_limit() {
        let live = parse_iw_dev(
            "phy#1\n Interface wlan2\n  ifindex 12\n  ssid Main \\\"quoted\\\"\n  type AP\n  channel 36 (5180 MHz), width: 80 MHz\n  txpower 17.50 dBm\n",
        );
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].name, "wlan2");
        assert_eq!(live[0].ssid, "Main \\\"quoted\\\"");
        assert_eq!(live[0].frequency, 5180);
        assert_eq!(live[0].dbm, Some(17.5));
        assert_eq!(
            parse_regulatory_limit("* 5180 MHz [36] (23.0 dBm)\n", 5180),
            Some(23)
        );
        assert_eq!(
            parse_regulatory_limit("* 5180 MHz [36] (23.0 dBm) (disabled)\n", 5180),
            None
        );
    }

    #[test]
    fn never_assigns_one_live_ap_to_two_sections() {
        let mut configured = vec![
            Interface {
                section: "main_2g",
                kind: "main",
                band: 0,
                ssid: "same".into(),
                configured: "missing0".into(),
                encryption: String::new(),
                psm_mode: "default".into(),
                enabled: true,
                hidden: false,
                isolate: false,
                has_key: false,
                live: None,
            },
            Interface {
                section: "guest_2g",
                kind: "guest",
                configured: "missing1".into(),
                ..configured_fixture()
            },
        ];
        let live = vec![Live {
            name: "wlan0".into(),
            ssid: "same".into(),
            kind: "AP".into(),
            frequency: 2412,
            ..Default::default()
        }];
        match_live(&mut configured, &live);
        assert_eq!(configured[0].live, None);
        assert_eq!(configured[1].live, None);
    }

    #[test]
    fn parses_wireless_capabilities_without_restricted_channels() {
        let results = vec![
            json!({"band":2,"channel":1,"restricted":false}),
            json!({"band":5,"channel":100,"restricted":true}),
            json!({"band":5,"channel":149}),
        ];
        let (mut two, mut five) = (Vec::new(), Vec::new());
        parse_channels(&results, &mut two, &mut five);
        assert_eq!(two, [1]);
        assert_eq!(five, [149]);
        assert_eq!(parse_channel_list("36, 40,40,bad"), [36, 40]);
        assert_eq!(
            parse_countries(&[json!({"iso3166":"hk"}), json!({"code":"00"})]),
            ["HK", "00"]
        );
        assert!(safe_ifname("wlan0"));
        assert!(!safe_ifname("wlan0;reboot"));
    }

    fn configured_fixture() -> Interface {
        Interface {
            section: "guest_2g",
            kind: "guest",
            band: 0,
            ssid: "same".into(),
            configured: String::new(),
            encryption: String::new(),
            psm_mode: "default".into(),
            enabled: true,
            hidden: false,
            isolate: false,
            has_key: false,
            live: None,
        }
    }
}
