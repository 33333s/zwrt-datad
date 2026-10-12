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
            radios.push(json!({"band":band,"device":device,"enabled":enabled(&device).await}));
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

/// One band's radio. The firmware restarts all of Wi-Fi to apply it.
pub async fn set_radio(band: &str, enabled: bool) -> Result<Value, String> {
    if !wifi::present_bands().await.contains(&band) {
        return Err(format!("invalid:unsupported band {band}"));
    }
    let device = wifi::radio_for(band).await;
    if device.is_empty() || self::enabled(&device).await.is_none() {
        return Err(format!("invalid:no radio for band {band}"));
    }
    let wanted = if enabled { "0" } else { "1" };
    let path = format!("wireless.{device}.disabled");
    // A radio is in the requested state when its `disabled` flag differs from `enabled`.
    let changed = (state::uci_read(&path).await == "1") == enabled;
    if changed {
        let mut sections = Map::new();
        sections.insert(device.clone(), json!({"disabled":wanted}));
        wlan_set(Value::Object(sections)).await?;
        if !confirm(|| async { (state::uci_read(&path).await == "1") != enabled }).await {
            return Err("radio readback did not confirm the requested state".into());
        }
    }
    Ok(json!({"band":band,"device":device,"enabled":enabled,"changed":changed,"verified":true}))
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
    for (band, on) in &requested {
        let Some((_, section)) = guest_sections.iter().find(|(known, _)| known == band) else {
            return Err(format!("invalid:no guest SSID for band {band}"));
        };
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
