//! Wi-Fi switches and the guest network, in the stock zwrt_wlan model:
//! `zte_mbb.wifi_onoff` is the module switch, each wifi-device `disabled` a
//! radio switch and each wifi-iface `disabled` one AP. Status reports both the
//! configuration and what is actually on air; anything the firmware does not
//! report is null, never a made-up 0.
use crate::{state, wifi};
use serde_json::{Map, Value, json};
use std::time::Duration;

/// Guest access time choices of the stock UI, in minutes (0 = no limit).
pub const GUEST_ACTIVE_TIMES: [i64; 5] = [0, 120, 240, 480, 720];

fn net_class_dir() -> String {
    std::env::var("ZWRT_DATAD_NET_CLASS_DIR").unwrap_or_else(|_| "/sys/class/net".into())
}

/// "1"/"0" option values; anything else is unknown.
fn flag(raw: &str) -> Option<bool> {
    match raw {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

/// UCI treats a missing `disabled` as enabled; a missing section is unknown.
async fn enabled(section: &str) -> Option<bool> {
    if state::uci_read(&format!("wireless.{section}"))
        .await
        .is_empty()
    {
        return None;
    }
    let disabled = state::uci_read(&format!("wireless.{section}.disabled")).await;
    Some(disabled != "1")
}

/// The kernel's view of an AP interface: `absent` when it does not exist.
fn operstate(ifname: &str) -> String {
    let path = std::path::Path::new(&net_class_dir()).join(ifname);
    if !path.exists() {
        return "absent".into();
    }
    std::fs::read_to_string(path.join("operstate"))
        .map(|value| value.trim().to_owned())
        .unwrap_or_else(|_| "unknown".into())
}

fn safe_ifname(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 15
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

/// Whether zwrt_wlan is still applying a change. While busy the module flag
/// can read 0 although the user switched Wi-Fi on.
async fn busy() -> Option<bool> {
    let status = state::ubus("zwrt_wlan", "status", json!({})).await.ok()?;
    let app = status.get("app_status")?.as_str()?;
    let driver = status.get("driver_status").and_then(Value::as_str);
    Some(app != "idle" || driver.is_some_and(|driver| driver != "idle"))
}

async fn ap(section: &str, kind: &str, band: &str) -> Option<Value> {
    let enabled = enabled(section).await?;
    let ifname = state::uci_read(&format!("wireless.{section}.ifname")).await;
    let (ifname, live) = if safe_ifname(&ifname) {
        let live = operstate(&ifname);
        (Value::String(ifname), Some(live))
    } else {
        (Value::Null, None)
    };
    Some(json!({
        "section":section,
        "kind":kind,
        "band":band,
        "ifname":ifname,
        "enabled":enabled,
        "state":live,
        "active":live.as_deref().map(|state| state == "up"),
    }))
}

/// Module, radio and AP switches with the live AP state.
pub async fn state() -> Value {
    let read = |option: &'static str| async move {
        flag(&state::uci_read(&format!("wireless.zte_mbb.{option}")).await)
    };
    let mut radios = Vec::new();
    let mut aps = Vec::new();
    for band in wifi::present_bands().await {
        let device = wifi::radio_for(band).await;
        if !device.is_empty() {
            // `enabled` is the band switch of wifi.radio.set: the radio and the
            // band's main SSID both on. `radio_enabled` is the radio alone.
            let radio = enabled(&device).await;
            let main = enabled(&format!("main_{band}")).await;
            let band_enabled = match (radio, main) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            };
            let htmode = state::uci_read(&format!("wireless.{device}.htmode")).await;
            let hwmode = state::uci_read(&format!("wireless.{device}.hwmode")).await;
            radios.push(json!({
                "band":band,"device":device,"enabled":band_enabled,"radio_enabled":radio,
                "width":width_of(&htmode),"widths":widths(band, &hwmode),"htmode":htmode,
            }));
        }
        for kind in ["main", "guest"] {
            if let Some(item) = ap(&format!("{kind}_{band}"), kind, band).await {
                aps.push(item);
            }
        }
    }
    json!({
        "busy":busy().await,
        "module":{"enabled":read("wifi_onoff").await,"user_enabled":read("wifi_onoff_by_user").await},
        "radios":radios,
        "aps":aps,
    })
}

/// The htmode family the stock web UI writes for a hardware mode
/// (`getWifiBandwidthPrefix`): Wi-Fi 7 EHT, Wi-Fi 6 HE, 11ac VHT, else HT.
fn htmode_prefix(hwmode: &str) -> &'static str {
    match hwmode {
        "11bea" | "11beg" => "EHT",
        "11axa" | "11axg" => "HE",
        "11ac" => "VHT",
        _ => "HT",
    }
}

/// Channel widths (MHz) a band can use in its hardware mode.
fn widths(band: &str, hwmode: &str) -> Vec<u32> {
    let prefix = htmode_prefix(hwmode);
    match band {
        "2g" => vec![20, 40],
        _ if prefix == "HT" => vec![20, 40],
        "6g" if prefix == "EHT" => vec![20, 40, 80, 160, 320],
        "5g" | "6g" => vec![20, 40, 80, 160],
        _ => Vec::new(),
    }
}

/// The width in an htmode such as `EHT160`, or null.
fn width_of(htmode: &str) -> Option<u32> {
    htmode
        .trim_start_matches(|c: char| c.is_ascii_alphabetic())
        .parse()
        .ok()
}

/// Sets a band's channel width the way the stock advanced page does:
/// `zwrt_wlan set {<radio>:{htmode:<prefix><width>}}`, which restarts that
/// radio's APs.
pub async fn set_bandwidth(band: &str, width: u32) -> Result<Value, String> {
    if !wifi::present_bands().await.contains(&band) {
        return Err(format!("invalid:unsupported band {band}"));
    }
    let device = wifi::radio_for(band).await;
    if device.is_empty() {
        return Err(format!("invalid:no radio for band {band}"));
    }
    let hwmode = state::uci_read(&format!("wireless.{device}.hwmode")).await;
    let allowed = widths(band, &hwmode);
    if !allowed.contains(&width) {
        return Err(format!(
            "invalid:width must be one of {} MHz for {band}",
            allowed
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join("/")
        ));
    }
    let path = format!("wireless.{device}.htmode");
    let old = state::uci_read(&path).await;
    let htmode = format!("{}{width}", htmode_prefix(&hwmode));
    let changed = old != htmode;
    if changed {
        wlan_set(json!({ device.clone(): {"htmode": htmode} })).await?;
        if !confirm(|| async { state::uci_read(&path).await == htmode }).await {
            return Err("Wi-Fi readback did not confirm the requested width".into());
        }
    }
    Ok(
        json!({"band":band,"device":device,"width":width,"htmode":htmode,"previous":old,"changed":changed}),
    )
}

/// Waits until `check` holds; the firmware applies some writes asynchronously.
async fn confirm<F, Fut>(mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for attempt in 0..10 {
        if attempt != 0 {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        if check().await {
            return true;
        }
    }
    false
}

async fn wlan_set(sections: Value) -> Result<(), String> {
    let response = state::ubus("zwrt_wlan", "set", sections).await?;
    wifi::check_write_response(&response)
}

/// The Wi-Fi module switch. `wifi_onoff_by_user` holds the request; the
/// plain flag also drops to 0 while the firmware restarts Wi-Fi. Switching
/// the module on re-enables every radio (stock behaviour).
pub async fn set_module(enabled: bool) -> Result<Value, String> {
    let wanted = if enabled { "1" } else { "0" };
    let current = |option: &'static str| async move {
        state::uci_read(&format!("wireless.zte_mbb.{option}")).await
    };
    let changed =
        current("wifi_onoff_by_user").await != wanted || current("wifi_onoff").await != wanted;
    if changed {
        wlan_set(json!({"zte_mbb":{"wifi_onoff":wanted}})).await?;
        if !confirm(|| async { current("wifi_onoff_by_user").await == wanted }).await {
            return Err("Wi-Fi module readback did not confirm the requested state".into());
        }
    }
    Ok(json!({"enabled":enabled,"changed":changed,"verified":true}))
}

/// Whether a radio runs on the single-chip qcacld32 firmwares (U60 Pro,
/// MC7523, TopFlow). Their QCMAP starts a single AP only on wlan0 and every
/// interface is created through wlan0, so one band cannot simply be switched
/// off there: the band that stays on has to take over wlan0.
pub async fn single_chip(device: &str) -> bool {
    !device.is_empty() && state::uci_read(&format!("wireless.{device}.type")).await == "qcacld32"
}

/// The stock interface pair of a qcacld32 radio: wifiN uses wlan(2N) for its
/// main SSID and wlan(2N+1) for the guest and backhaul SSIDs.
fn default_ifnames(device: &str) -> Option<(String, String)> {
    let index: u32 = device.strip_prefix("wifi")?.parse().ok()?;
    (index < 3).then(|| {
        (
            format!("wlan{}", index * 2),
            format!("wlan{}", index * 2 + 1),
        )
    })
}

async fn section_exists(section: &str) -> bool {
    !state::uci_read(&format!("wireless.{section}"))
        .await
        .is_empty()
}

/// One band on or off.
///
/// qcacld32 firmwares: the layout TopFlow's own firmware uses. Off: the
/// band's radio is disabled and its SSIDs park on ifname "wlanx" while the
/// other band's main SSID takes wlan0 (guest/backhaul wlan1). On: the radio is
/// enabled and every band gets its stock ifnames back. Writing only an SSID
/// or radio `disabled` left the remaining AP on wlan2, QCMAP never found
/// hostapd on wlan0 and Wi-Fi stayed down (U60 Pro, 0.10.74-0.10.76).
///
/// Other firmwares: through the band's SSIDs like the stock web UI (off:
/// main and guest SSIDs; on: the main SSID, and the radio if an older release
/// disabled it).
///
/// With EasyMesh on, a band is not switched off on the qcacld32 firmwares.
///
/// The firmware restarts all of Wi-Fi to apply either (about a minute on the
/// qcacld32 firmwares); callers poll wifi.state.
pub async fn set_radio(band: &str, enabled: bool) -> Result<Value, String> {
    let bands = wifi::present_bands().await;
    if !bands.contains(&band) {
        return Err(format!("invalid:unsupported band {band}"));
    }
    let device = wifi::radio_for(band).await;
    let main = format!("main_{band}");
    let Some(main_enabled) = self::enabled(&main).await else {
        return Err(format!("invalid:no SSID for band {band}"));
    };
    if single_chip(&device).await {
        return set_single_chip_band(band, &device, enabled, main_enabled, &bands).await;
    }
    let guest = format!("guest_{band}");
    let mut wanted: Vec<(String, &'static str)> = Vec::new();
    if enabled {
        if !main_enabled {
            wanted.push((main, "0"));
        }
        if !device.is_empty() && self::enabled(&device).await == Some(false) {
            wanted.push((device.clone(), "0"));
        }
    } else {
        if main_enabled {
            wanted.push((main, "1"));
        }
        if self::enabled(&guest).await == Some(true) {
            wanted.push((guest, "1"));
        }
    }
    let changed = !wanted.is_empty();
    if changed {
        let mut sections = Map::new();
        for (section, value) in &wanted {
            sections.insert(section.clone(), json!({"disabled":value}));
        }
        wlan_set(Value::Object(sections)).await?;
        let confirmed = confirm(|| async {
            for (section, value) in &wanted {
                let disabled = state::uci_read(&format!("wireless.{section}.disabled")).await;
                if (disabled == "1") != (*value == "1") {
                    return false;
                }
            }
            true
        })
        .await;
        if !confirmed {
            return Err("Wi-Fi readback did not confirm the requested band state".into());
        }
    }
    let sections: Vec<&str> = wanted.iter().map(|(section, _)| section.as_str()).collect();
    Ok(
        json!({"band":band,"device":device,"enabled":enabled,"changed":changed,"verified":true,"sections":sections}),
    )
}

async fn set_single_chip_band(
    band: &str,
    device: &str,
    enabled: bool,
    main_enabled: bool,
    bands: &[&'static str],
) -> Result<Value, String> {
    let mut radios = Vec::new();
    for other in bands {
        let radio = wifi::radio_for(other).await;
        let Some(names) = default_ifnames(&radio) else {
            return Err(format!("invalid:unknown radio layout for band {other}"));
        };
        radios.push((*other, radio, names));
    }
    let mut wanted: Vec<(String, String)> = Vec::new();
    if enabled {
        // Every SSID that is on runs again once the stock names are back.
        if let Some(bad) = unsupported_aps().await {
            let mut count = 0;
            for (other, radio, _) in &radios {
                if *other != band && self::enabled(radio).await != Some(true) {
                    continue;
                }
                for kind in ["main", "guest"] {
                    let on = (kind == "main" && *other == band)
                        || self::enabled(&format!("{kind}_{other}")).await == Some(true);
                    count += usize::from(on);
                }
            }
            if count == bad {
                return Err(count_error(bad));
            }
        }
        wanted.push((format!("{device}.disabled"), "0".into()));
        if !main_enabled {
            wanted.push((format!("main_{band}.disabled"), "0".into()));
        }
        for (other, _, (main_if, extra_if)) in &radios {
            wanted.push((format!("main_{other}.ifname"), main_if.clone()));
            for kind in ["guest", "backhaul"] {
                wanted.push((format!("{kind}_{other}.ifname"), extra_if.clone()));
            }
        }
    } else {
        // With EasyMesh on, the firmware starts Wi-Fi through ezmesh with both
        // bands' backhaul APs; the single-band layout never came up (MC7523).
        if state::uci_read("wireless.zte_mbb.mesh_onoff").await == "1" {
            return Err("invalid:Mesh is on; switch Mesh off before switching a band off".into());
        }
        let keep: Vec<_> = radios
            .iter()
            .filter(|(other, _, _)| *other != band)
            .collect();
        let mut on = None;
        for (other, radio, _) in &keep {
            if self::enabled(radio).await == Some(true)
                && self::enabled(&format!("main_{other}")).await == Some(true)
            {
                on = Some(*other);
                break;
            }
        }
        let Some(on) = on else {
            return Err(
                "invalid:no other band would stay on; switch Wi-Fi off with wifi.set_module".into(),
            );
        };
        wanted.push((format!("{device}.disabled"), "1".into()));
        for kind in ["main", "guest", "backhaul"] {
            wanted.push((format!("{kind}_{band}.ifname"), "wlanx".into()));
        }
        wanted.push((format!("main_{on}.ifname"), "wlan0".into()));
        for kind in ["guest", "backhaul"] {
            wanted.push((format!("{kind}_{on}.ifname"), "wlan1".into()));
        }
    }
    let options = write_layout(wanted).await?;
    Ok(
        json!({"band":band,"device":device,"enabled":enabled,"changed":!options.is_empty(),"verified":true,"options":options}),
    )
}

/// Writes `section.option=value` pairs with uci (`zwrt_wlan set` refuses
/// ifname), only for sections the firmware has and values that differ, then
/// commits and reloads Wi-Fi. Returns the options written.
async fn write_layout(wanted: Vec<(String, String)>) -> Result<Vec<String>, String> {
    let mut writes = Vec::new();
    for (option, value) in wanted {
        let section = option.split('.').next().unwrap_or_default().to_owned();
        if !section_exists(&section).await {
            continue;
        }
        let path = format!("wireless.{option}");
        if state::uci_read(&path).await != value {
            writes.push((path, value));
        }
    }
    if !writes.is_empty() {
        for (path, value) in &writes {
            if let Err(error) = state::uci_write("set", path, Some(value)).await {
                let _ = state::uci_write("revert", "wireless", None).await;
                return Err(error);
            }
        }
        if let Err(error) = state::uci_write("commit", "wireless", None).await {
            let _ = state::uci_write("revert", "wireless", None).await;
            return Err(error);
        }
        for (path, value) in &writes {
            if state::uci_read(path).await != *value {
                return Err(format!("Wi-Fi readback did not confirm {path}"));
            }
        }
        let response = state::ubus("zwrt_wlan", "reload", json!({})).await?;
        wifi::check_write_response(&response)?;
    }
    Ok(writes
        .iter()
        .map(|(path, value)| format!("{}={value}", path.trim_start_matches("wireless.")))
        .collect())
}

/// An AP count the firmware cannot start. The U60 Pro (MU5250) QCMAP runs one,
/// two or four APs (mode 1, 2, 7) but rejects three (`zte_wlan_ex_start
/// mode=4`: "unsupported wifi mode") and Wi-Fi stays down. Its stock web UI
/// only switches both guest SSIDs together, so it never asks for three.
async fn unsupported_aps() -> Option<usize> {
    (state::uci_read("zwrt_common_info.common_config.model_name").await == "MU5250").then_some(3)
}

/// Main and guest APs that would run, with `overrides` (section, on) applied.
async fn running_aps(overrides: &[(String, bool)]) -> usize {
    let mut count = 0;
    for band in wifi::present_bands().await {
        let radio = wifi::radio_for(band).await;
        if radio.is_empty() || enabled(&radio).await == Some(false) {
            continue;
        }
        for kind in ["main", "guest"] {
            let section = format!("{kind}_{band}");
            let on = match overrides.iter().find(|(name, _)| *name == section) {
                Some((_, on)) => Some(*on),
                None => enabled(&section).await,
            };
            if on == Some(true)
                && state::uci_read(&format!("wireless.{section}.ifname")).await != "wlanx"
            {
                count += 1;
            }
        }
    }
    count
}

/// Refuses a change that would leave the firmware with an AP count it cannot
/// start.
pub async fn check_ap_count(overrides: &[(String, bool)]) -> Result<(), String> {
    if let Some(bad) = unsupported_aps().await {
        let after = running_aps(overrides).await;
        if after == bad && after != running_aps(&[]).await {
            return Err(count_error(bad));
        }
    }
    Ok(())
}

fn count_error(bad: usize) -> String {
    format!(
        "invalid:this firmware cannot run {bad} Wi-Fi networks at once (Wi-Fi would stay down); switch the guest Wi-Fi of both bands on or off together"
    )
}

/// Whether `section` carries its band on wlan0 of a single-chip firmware.
async fn holds_wlan0(section: &str) -> bool {
    state::uci_read(&format!("wireless.{section}.ifname")).await == "wlan0"
        && single_chip(&state::uci_read(&format!("wireless.{section}.device")).await).await
}

/// The SSID switch of `wifi.configure` on the single-chip firmwares, where
/// QCMAP needs an AP on wlan0. A main SSID on wlan0 can go off only while the
/// band's guest SSID stays on: the guest takes wlan0 and the main SSID is
/// parked as wlanx (verified on the U60 Pro). Switching it on restores the
/// stock interface names. None leaves the switch to the plain SSID write.
pub async fn single_chip_ssid(section: &str, on: bool) -> Option<Result<Value, String>> {
    let (kind, band) = section.split_once('_')?;
    let device = state::uci_read(&format!("wireless.{section}.device")).await;
    if !single_chip(&device).await {
        return None;
    }
    let ifname = state::uci_read(&format!("wireless.{section}.ifname")).await;
    let guest = format!("guest_{band}");
    let result = match (kind, on) {
        ("main", false) if ifname == "wlan0" => {
            if enabled(&guest).await != Some(true) {
                return Some(Err(
                    "invalid:this firmware cannot switch the band on wlan0 off through its SSID; use wifi.radio.set".into(),
                ));
            }
            if let Some(bad) = unsupported_aps().await
                && running_aps(&[]).await.checked_sub(1) == Some(bad)
            {
                return Some(Err(count_error(bad)));
            }
            write_layout(vec![
                (format!("{section}.disabled"), "1".into()),
                (format!("{section}.ifname"), "wlanx".into()),
                (format!("{guest}.ifname"), "wlan0".into()),
            ])
            .await
        }
        ("main", true) if ifname == "wlanx" => {
            if enabled(&device).await == Some(false) {
                return Some(Err(format!(
                    "invalid:band {band} is switched off; use wifi.radio.set"
                )));
            }
            let Some((main_if, extra_if)) = default_ifnames(&device) else {
                return Some(Err(format!("invalid:unknown radio layout for band {band}")));
            };
            // The count skips the parked SSID itself: it comes back as one more.
            if let Some(bad) = unsupported_aps().await
                && running_aps(&[]).await + 1 == bad
            {
                return Some(Err(count_error(bad)));
            }
            write_layout(vec![
                (format!("{section}.disabled"), "0".into()),
                (format!("{section}.ifname"), main_if),
                (format!("{guest}.ifname"), extra_if.clone()),
                (format!("backhaul_{band}.ifname"), extra_if),
            ])
            .await
        }
        ("guest", false) if ifname == "wlan0" => {
            return Some(Err(format!(
                "invalid:the guest SSID carries band {band} while its main SSID is off; switch the main SSID on first, or the band off with wifi.radio.set"
            )));
        }
        _ => return None,
    };
    Some(result.map(|options| {
        json!({"section":section,"enabled":on,"changed":!options.is_empty(),"verified":true,"options":options})
    }))
}

/// Guest support flags of the stock `wireless.readonly` section; chip1 is the
/// 2.4 GHz radio and chip2 the 5 GHz one. None when the firmware has no flag.
async fn guest_supported(band: &str) -> Option<bool> {
    let chip = match band {
        "2g" => "chip1",
        "5g" => "chip2",
        _ => return None,
    };
    flag(
        &state::uci_read(&format!(
            "wireless.readonly.wifi_attr_{chip}_support_guest_ssid"
        ))
        .await,
    )
}

fn number(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// Guest SSIDs with their switch, on-air state, remaining time and clients,
/// plus the shared access-time and management-access settings.
pub async fn guest_status() -> Value {
    let left = state::ubus("zwrt_wlan", "wlan_get_guest_access_left_time", json!({}))
        .await
        .ok();
    let left_time = |section: &str| {
        left.as_ref()?
            .get("wlan_guest_left_time_info")?
            .as_array()?
            .iter()
            .find(|item| item.get("section_name").and_then(Value::as_str) == Some(section))
            .and_then(|item| number(item.get("guest_left_time")))
    };
    let users = state::ubus("zwrt_router.api", "router_get_user_list_num", json!({}))
        .await
        .ok();
    let isolate = state::ubus("zwrt_router.api", "router_get_wifi_isolate", json!({}))
        .await
        .ok();
    let mut guests = Vec::new();
    let mut active_times = Vec::new();
    for band in wifi::present_bands().await {
        let section = format!("guest_{band}");
        let Some(mut item) = ap(&section, "guest", band).await else {
            continue;
        };
        let enabled = item["enabled"].as_bool() == Some(true);
        let minutes = number(Some(&json!(
            state::uci_read(&format!("wireless.{section}.guest_active_time")).await
        )));
        active_times.push(minutes);
        let counter = if band == "2g" {
            "guest_num_24g".to_owned()
        } else {
            format!("guest_num_{band}")
        };
        item["supported"] = json!(guest_supported(band).await);
        item["ssid"] = json!(state::uci_read(&format!("wireless.{section}.ssid")).await);
        item["encryption"] =
            json!(state::uci_read(&format!("wireless.{section}.encryption")).await);
        item["active_time_minutes"] = json!(minutes);
        // The firmware keeps counting a timer after the guest SSID is switched
        // off, so the remaining time only means something while it is on.
        item["left_seconds"] = json!(if enabled { left_time(&section) } else { None });
        item["clients"] = json!(users.as_ref().and_then(|users| number(users.get(&counter))));
        guests.push(item);
    }
    let shared = match active_times.first() {
        Some(first) if active_times.iter().all(|time| time == first) => *first,
        _ => None,
    };
    let restrict = isolate
        .as_ref()
        .and_then(|isolate| number(isolate.get("guest_access_restrict_enable")));
    json!({
        "guests":guests,
        "active_time_minutes":shared,
        "active_time_supported":flag(&state::uci_read("wireless.readonly.wifi_attr_support_guest_ssid_active_time").await),
        "active_time_options":GUEST_ACTIVE_TIMES,
        "management_access":match restrict { Some(0) => Some(true), Some(1) => Some(false), _ => None },
    })
}

/// Requested guest changes; None leaves a setting as it is.
#[derive(Default)]
pub struct GuestUpdate {
    /// Every guest SSID the model supports.
    pub all: Option<bool>,
    /// Per band: true switches that band's guest SSID on.
    pub bands: Vec<(String, bool)>,
    pub active_time: Option<i64>,
    pub management_access: Option<bool>,
}

/// Applies only the fields that differ from the current configuration, in one
/// zwrt_wlan write (one Wi-Fi restart) plus one isolation write.
pub async fn set_guest(update: GuestUpdate) -> Result<Value, String> {
    let bands = wifi::present_bands().await;
    let mut guest_sections = Vec::new();
    for band in &bands {
        let section = format!("guest_{band}");
        if enabled(&section).await.is_some() {
            guest_sections.push(((*band).to_owned(), section));
        }
    }
    let mut sections: Map<String, Value> = Map::new();
    let mut expected: Vec<(String, String)> = Vec::new();
    let mut change = |section: &str, option: &str, value: String, current: String| {
        if current == value {
            return;
        }
        expected.push((format!("wireless.{section}.{option}"), value.clone()));
        sections
            .entry(section.to_owned())
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .expect("object")
            .insert(option.to_owned(), Value::String(value));
    };
    let mut requested = update.bands.clone();
    if let Some(on) = update.all {
        for (band, _) in &guest_sections {
            // Switching on skips bands the firmware says cannot host a guest SSID.
            if (!on || guest_supported(band).await != Some(false))
                && !requested.iter().any(|(known, _)| known == band)
            {
                requested.push((band.clone(), on));
            }
        }
    }
    let mut overrides = Vec::new();
    for (band, on) in &requested {
        let Some((_, section)) = guest_sections.iter().find(|(known, _)| known == band) else {
            return Err(format!("invalid:no guest SSID for band {band}"));
        };
        if !*on && holds_wlan0(section).await {
            return Err(format!(
                "invalid:the guest SSID carries band {band} while its main SSID is off; switch the main SSID on first, or the band off with wifi.radio.set"
            ));
        }
        overrides.push((section.clone(), *on));
        if *on && guest_supported(band).await == Some(false) {
            return Err(format!(
                "invalid:guest SSID is not supported on band {band}"
            ));
        }
        let current = state::uci_read(&format!("wireless.{section}.disabled")).await;
        let current = if current == "1" { "1" } else { "0" }.to_owned();
        change(
            section,
            "disabled",
            if *on { "0" } else { "1" }.into(),
            current,
        );
    }
    check_ap_count(&overrides).await?;
    if let Some(minutes) = update.active_time {
        if state::uci_read("wireless.readonly.wifi_attr_support_guest_ssid_active_time").await
            == "0"
        {
            return Err("invalid:guest access time is not supported".into());
        }
        for (_, section) in &guest_sections {
            let current = state::uci_read(&format!("wireless.{section}.guest_active_time")).await;
            change(section, "guest_active_time", minutes.to_string(), current);
        }
    }
    let mut changed = false;
    if !sections.is_empty() {
        wlan_set(Value::Object(sections)).await?;
        let confirmed = confirm(|| {
            let expected = expected.clone();
            async move {
                for (path, value) in &expected {
                    if state::uci_read(path).await != *value {
                        return false;
                    }
                }
                true
            }
        })
        .await;
        if !confirmed {
            return Err("guest configuration readback did not confirm the request".into());
        }
        changed = true;
    }
    if let Some(allow) = update.management_access {
        let wanted = i64::from(!allow);
        let current = || async {
            state::ubus("zwrt_router.api", "router_get_wifi_isolate", json!({}))
                .await
                .ok()
                .and_then(|value| number(value.get("guest_access_restrict_enable")))
        };
        match current().await {
            None => return Err("guest management access is not available".into()),
            Some(value) if value == wanted => {}
            Some(_) => {
                // Only this field is sent: the firmware keeps the others, and
                // some models have isolation fields their read call omits.
                let response = state::ubus(
                    "zwrt_router.api",
                    "router_set_wifi_isolate",
                    json!({"guest_access_restrict_enable":wanted}),
                )
                .await?;
                wifi::check_write_response(&response)?;
                if !confirm(|| async { current().await == Some(wanted) }).await {
                    return Err("guest management access readback did not confirm".into());
                }
                changed = true;
            }
        }
    }
    let mut result = guest_status().await;
    result["changed"] = json!(changed);
    result["verified"] = json!(true);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_follow_band_and_hardware_mode() {
        assert_eq!(htmode_prefix("11beg"), "EHT");
        assert_eq!(htmode_prefix("11axa"), "HE");
        assert_eq!(htmode_prefix("11ac"), "VHT");
        assert_eq!(htmode_prefix("11ng"), "HT");
        assert_eq!(widths("2g", "11beg"), [20, 40]);
        assert_eq!(widths("5g", "11bea"), [20, 40, 80, 160]);
        assert_eq!(widths("5g", "11na"), [20, 40]);
        assert_eq!(widths("6g", "11bea"), [20, 40, 80, 160, 320]);
        assert_eq!(widths("6g", "11axa"), [20, 40, 80, 160]);
        assert_eq!(width_of("EHT160"), Some(160));
        assert_eq!(width_of("HT40"), Some(40));
        assert_eq!(width_of(""), None);
    }

    #[test]
    fn flags_and_numbers_never_default_to_zero() {
        assert_eq!(flag("1"), Some(true));
        assert_eq!(flag("0"), Some(false));
        assert_eq!(flag(""), None);
        assert_eq!(number(Some(&json!("7179"))), Some(7179));
        assert_eq!(number(Some(&json!(3))), Some(3));
        assert_eq!(number(Some(&json!(""))), None);
        assert_eq!(number(None), None);
    }

    #[test]
    fn interface_names_are_checked_before_touching_sysfs() {
        assert!(safe_ifname("wlan0"));
        assert!(safe_ifname("ath1"));
        assert!(!safe_ifname("../wlan0"));
        assert!(!safe_ifname(""));
        assert!(!safe_ifname("averyveryverylongname"));
    }
}
