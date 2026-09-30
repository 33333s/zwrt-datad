//! Reviewed, credential-free views of original-firmware U50 configuration.
//! Raw OEM replies stay local. Only explicitly selected UI fields leave this
//! module; APN records contain account secrets and must never be forwarded.
use serde_json::{Map, Value, json};

fn text<'a>(value: &'a Value, key: &str, limit: usize) -> Option<&'a str> {
    value
        .get(key)?
        .as_str()
        .filter(|v| v.len() <= limit && !v.chars().any(char::is_control))
}

fn number(value: &Value, key: &str, max: u64) -> Option<u64> {
    match value.get(key)? {
        Value::String(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => {
            s.parse().ok()
        }
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
    .filter(|n| *n <= max)
}

fn flag(value: &Value, key: &str) -> Option<bool> {
    number(value, key, 1).map(|n| n == 1)
}

pub fn wifi_config(access_points: &Value, steering: &Value) -> Result<Value, String> {
    let rows = access_points
        .get("ResponseList")
        .and_then(Value::as_array)
        .filter(|rows| rows.len() <= 8)
        .ok_or("invalid OEM Wi-Fi list")?;
    let mut bands = Map::from_iter([
        ("2g".into(), json!({"supported":false})),
        ("5g".into(), json!({"supported":false})),
    ]);
    let mut countries = Vec::new();
    for row in rows {
        // Guest SSIDs are separate resources. The main SSID is index zero.
        if number(row, "AccessPointIndex", 3) != Some(0) || number(row, "ChipIndex", 1).is_none() {
            continue;
        }
        // Select by the firmware's band value, not by a guessed chip order.
        let band = match text(row, "Band", 8) {
            Some("b") => "2g",
            Some("a") => "5g",
            _ => continue,
        };
        if bands[band]["supported"] == true {
            return Err("ambiguous OEM Wi-Fi band".into());
        }
        let ssid = text(row, "SSID", 128).ok_or("invalid OEM Wi-Fi name")?;
        let auth = text(row, "AuthMode", 64).unwrap_or("");
        let encryption = match auth {
            "OPEN" => "none",
            "WPA2PSK" => "psk2",
            "WPAPSKWPA2PSK" => "psk-mixed",
            "WPA3PSK" => "sae",
            "WPA2PSKWPA3PSK" => "sae-mixed",
            other => other,
        };
        let country = text(row, "CountryCode", 2)
            .unwrap_or("")
            .to_ascii_uppercase();
        let country = if country.len() == 2 && country.bytes().all(|b| b.is_ascii_alphabetic()) {
            country
        } else {
            String::new()
        };
        if !country.is_empty() && !countries.contains(&country) {
            countries.push(country.clone());
        }
        let channel = number(row, "Channel", 233);
        let mut channels = vec![0];
        if let Some(channel) = channel.filter(|n| *n != 0) {
            channels.push(channel);
        }
        // OEM values are bandwidth policies, not a measurement of live width.
        let width = match number(row, "BandWidth", 4) {
            Some(0) => "20 MHz",
            Some(1) => "20/40 MHz",
            Some(4) => "20/40/80 MHz",
            _ => "",
        };
        bands.insert(band.into(), json!({
            "supported":true,"ssid":ssid,"encryption":encryption,
            "hidden":flag(row,"ApBroadcastDisabled"),"pmf":number(row,"Pmf_switch",2).map(|v|v.to_string()),
            "enabled":flag(row,"AccessPointSwitchStatus"),"maxassoc":number(row,"ApMaxStationNumber",1024),
            "country":country,"channel":channel.map(|v|v.to_string()),"htmode":width,"channels":channels
        }));
    }
    if !bands.values().any(|item| item["supported"] == true) {
        return Err("OEM Wi-Fi configuration unavailable".into());
    }
    Ok(json!({"writable":false,"countries":countries,"bands":bands,
        "dual_band":flag(steering,"wifi_lbd_enable")}))
}

fn auth_mode(value: &str) -> Option<u8> {
    match value.to_ascii_lowercase().as_str() {
        "none" => Some(0),
        "pap" => Some(1),
        "chap" => Some(2),
        "papchap" | "pap/chap" => Some(3),
        _ => None,
    }
}

fn pdp_type(value: &str) -> Option<u8> {
    match value {
        "IP" | "IPv4" => Some(1),
        "IPv6" => Some(2),
        "IPv4v6" => Some(3),
        _ => None,
    }
}

fn profile(raw: &str, id: String) -> Option<Value> {
    if raw.is_empty() || raw.len() > 8192 || raw.chars().any(char::is_control) {
        return None;
    }
    let parts: Vec<_> = raw.split("($)").collect();
    if parts.len() < 8
        || parts.len() > 20
        || parts[0].is_empty()
        || parts[0].len() > 64
        || parts[1].len() > 128
    {
        return None;
    }
    // Positions 5 and 6 hold a username and password. Never copy them, or the
    // original serialized profile, into a return value or snapshot.
    Some(json!({"id":id,"name":parts[0],"apn":parts[1],
        "auth_mode":auth_mode(parts[4]),"pdp_type":pdp_type(parts[7]),"enabled":false}))
}

pub fn apn_config(reply: &Value) -> Result<Value, String> {
    let mode = match text(reply, "apn_mode", 16) {
        Some("auto") => 0,
        Some("manual") => 1,
        _ => return Err("OEM APN mode unavailable".into()),
    };
    let mut manual = Vec::new();
    for index in 0..20 {
        let name = format!("APN_config{index}");
        let v6_name = format!("ipv6_APN_config{index}");
        let id = format!("manual-{index}");
        let parsed = text(reply, &name, 8192)
            .and_then(|raw| profile(raw, id.clone()))
            .or_else(|| text(reply, &v6_name, 8192).and_then(|raw| profile(raw, id)));
        if let Some(parsed) = parsed {
            manual.push(parsed);
        }
    }
    let automatic_raw = text(reply, "apn_auto_config", 32 * 1024)
        .filter(|raw| !raw.is_empty())
        .or_else(|| text(reply, "ipv6_apn_auto_config", 32 * 1024))
        .unwrap_or("");
    let mut automatic: Vec<Value> = automatic_raw
        .split("||")
        .take(20)
        .enumerate()
        .filter_map(|(index, raw)| profile(raw, format!("automatic-{index}")))
        .collect();
    let v2 = number(reply, "apn_interface_version", 100).is_some_and(|v| v >= 2);
    let active_name = if v2 {
        text(reply, "profile_name_ui", 64)
    } else {
        text(reply, "m_profile_name", 64)
            .filter(|s| !s.is_empty())
            .or_else(|| text(reply, "profile_name", 64))
    }
    .unwrap_or("");
    let active_apn = text(reply, if v2 { "wan_apn_ui" } else { "wan_apn" }, 128).unwrap_or("");
    let active = if mode == 1 {
        &mut manual
    } else {
        &mut automatic
    };
    // An OEM image may expose only the active profile. Report that known
    // value without inventing a full profile catalog or editable profile id.
    if active.is_empty() && !active_apn.is_empty() {
        active.push(json!({"id":"current","name":active_name,"apn":active_apn,
            "auth_mode":text(reply,if v2 {"ppp_auth_mode_ui"} else {"ppp_auth_mode"},32).and_then(auth_mode),
            "pdp_type":text(reply,if v2 {"pdp_type_ui"} else {"pdp_type"},16).and_then(pdp_type),"enabled":false}));
    }
    let matching: Vec<_> = active
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            (!active_name.is_empty() && item["name"] == active_name)
                || (active_name.is_empty() && !active_apn.is_empty() && item["apn"] == active_apn)
        })
        .map(|(index, _)| index)
        .collect();
    let enabled_id = if matching.len() == 1 {
        let index = matching[0];
        active[index]["enabled"] = json!(true);
        active[index]["id"].as_str().unwrap_or("").to_owned()
    } else {
        String::new()
    };
    if automatic.is_empty() && manual.is_empty() {
        return Err("OEM APN profiles unavailable".into());
    }
    Ok(json!({"writable":false,"mode":mode,"enabled_id":enabled_id,
        "automatic":automatic,"manual":manual}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wifi_maps_bands_and_drops_every_unreviewed_field() {
        let access = json!({"ResponseList":[
            {"ChipIndex":"1","AccessPointIndex":"0","Band":"b","SSID":"fixture-24",
                "AuthMode":"WPA2PSK","Password":"wifi-secret","Cookie":"oem-cookie","ApBroadcastDisabled":"1",
                "Pmf_switch":"1","AccessPointSwitchStatus":"1","ApMaxStationNumber":"16",
                "CountryCode":"cn","Channel":"11","BandWidth":"1"},
            {"ChipIndex":"0","AccessPointIndex":"0","Band":"a","SSID":"fixture-5","AuthMode":"WPA3PSK",
                "CountryCode":"CN","Channel":"149","BandWidth":"4"},
            {"ChipIndex":"0","AccessPointIndex":"1","Band":"a","SSID":"guest"}
        ]});
        let config = wifi_config(&access, &json!({"wifi_lbd_enable":"0"})).unwrap();
        assert_eq!(config["bands"]["2g"]["ssid"], "fixture-24");
        assert_eq!(config["bands"]["2g"]["hidden"], true);
        assert_eq!(config["bands"]["5g"]["encryption"], "sae");
        assert_eq!(config["bands"]["5g"]["htmode"], "20/40/80 MHz");
        assert_eq!(config["dual_band"], false);
        assert_eq!(config["writable"], false);
        for secret in ["wifi-secret", "oem-cookie", "Password", "Cookie", "guest"] {
            assert!(!config.to_string().contains(secret));
        }
    }

    #[test]
    fn wifi_refuses_unknown_or_ambiguous_shapes_without_guessing_support() {
        assert!(wifi_config(&json!({"ResponseList":""}), &json!({})).is_err());
        let row = json!({"ChipIndex":"0","AccessPointIndex":"0","Band":"unknown","SSID":"fixture"});
        assert!(wifi_config(&json!({"ResponseList":[row]}), &json!({})).is_err());
        let row = json!({"ChipIndex":"0","AccessPointIndex":"0","Band":"b","SSID":"fixture"});
        assert!(wifi_config(&json!({"ResponseList":[row.clone(),row]}), &json!({})).is_err());
    }

    #[test]
    fn apn_normalizes_profiles_and_excludes_embedded_account_secrets() {
        let record = "Carrier($)internet($)unused($)unused($)PAP($)account-secret($)password-secret($)IPv4v6($)0($)0($)auto($)($)";
        let config = apn_config(&json!({"apn_mode":"manual","apn_interface_version":"2",
            "profile_name_ui":"Carrier","APN_config0":record,"apn_auto_config":record,
            "ppp_passwd_ui":"current-secret","admin_Password":"hash-secret"}))
        .unwrap();
        assert_eq!(config["enabled_id"], "manual-0");
        assert_eq!(config["manual"][0]["pdp_type"], 3);
        assert_eq!(config["manual"][0]["auth_mode"], 1);
        assert_eq!(config["manual"][0]["enabled"], true);
        assert_eq!(config["automatic"][0]["enabled"], false);
        assert_eq!(config["writable"], false);
        for secret in [
            "account-secret",
            "password-secret",
            "current-secret",
            "hash-secret",
            "APN_config",
            "ppp_passwd",
        ] {
            assert!(!config.to_string().contains(secret));
        }
    }

    #[test]
    fn apn_keeps_unknown_values_unknown_and_supports_active_profile_only() {
        assert!(apn_config(&json!({})).is_err());
        assert!(apn_config(&json!({"apn_mode":"manual","APN_config0":"malformed"})).is_err());
        let config = apn_config(
            &json!({"apn_mode":"auto","wan_apn":"fixture.apn","m_profile_name":"Live",
            "ppp_auth_mode":"unknown","pdp_type":"unknown","APN_config0":"malformed"}),
        )
        .unwrap();
        assert_eq!(config["enabled_id"], "current");
        assert_eq!(config["automatic"][0]["auth_mode"], Value::Null);
        assert_eq!(config["automatic"][0]["pdp_type"], Value::Null);
        assert_eq!(config["manual"], json!([]));
    }
}
