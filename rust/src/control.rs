use crate::state;
use serde_json::{Map, Value, json};

pub enum Outcome {
    NotHandled,
    Invalid(String),
    Failed(String),
    Ok(Value),
}

pub const ACTIONS: &[&str] = &[
    "device.reboot",
    "device.poweroff",
    "cellular.connect",
    "cellular.disconnect",
    "cellular.set",
    "network.set_mode",
    "band.set_lte",
    "band.set_nr_sa",
    "band.set_nr_nsa",
    "cell.lock_lte",
    "cell.lock_nr",
    "cell.unlock_all",
    "sim.set_slot",
    "wifi.set_dual_band",
    "wifi.set_module",
    "wifi.set_chip",
    "wifi.configure",
    "lan.set",
    "lan.set_mtu",
    "dns.set",
    "power.direct_supply.set",
    "usb.set",
    "sleep.set",
    "nfc.set",
    "apn.set_mode",
    "apn.add",
    "apn.modify",
    "apn.delete",
    "apn.enable",
    "traffic.set_limit",
    "traffic.set_clear_day",
    "traffic.calibrate",
    "sms.delete",
    "sms.mark_read",
    "client.kick",
    "client.rename",
    "client.block",
    "client.unblock",
];

fn object(params: &Value) -> &Map<String, Value> {
    params.as_object().expect("server validates params")
}
fn string(params: &Value, name: &str, required: bool) -> Result<Option<String>, String> {
    match object(params).get(name) {
        Some(Value::String(value)) if value.len() <= 8192 => Ok(Some(value.clone())),
        Some(_) => Err(format!("{name} must be a string")),
        None if required => Err(format!("missing parameter: {name}")),
        None => Ok(None),
    }
}
fn integer(params: &Value, name: &str, required: bool) -> Result<Option<i64>, String> {
    match object(params).get(name) {
        Some(Value::Number(value)) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| format!("{name} must be an integer")),
        Some(_) => Err(format!("{name} must be an integer")),
        None if required => Err(format!("missing parameter: {name}")),
        None => Ok(None),
    }
}
fn boolean(params: &Value, name: &str) -> Result<bool, String> {
    object(params)
        .get(name)
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("{name} must be boolean"))
}
fn mapped(params: &Value, specs: &[(&str, &str, bool, bool)]) -> Result<Value, String> {
    let mut args = Map::new();
    for (input, output, required, is_int) in specs {
        if *is_int {
            if let Some(value) = integer(params, input, *required)? {
                args.insert((*output).into(), json!(value));
            }
        } else if let Some(value) = string(params, input, *required)? {
            args.insert((*output).into(), json!(value));
        }
    }
    Ok(Value::Object(args))
}
async fn call(service: &str, method: &str, args: Value) -> Outcome {
    match state::ubus(service, method, args).await {
        Ok(value) => Outcome::Ok(value),
        Err(error) => Outcome::Failed(error),
    }
}
async fn mapped_call(
    params: &Value,
    service: &str,
    method: &str,
    specs: &[(&str, &str, bool, bool)],
    require_any: bool,
) -> Outcome {
    match mapped(params, specs) {
        Ok(Value::Object(args)) if require_any && args.is_empty() => {
            Outcome::Invalid("no fields supplied".into())
        }
        Ok(args) => call(service, method, args).await,
        Err(error) => Outcome::Invalid(error),
    }
}

pub async fn execute(action: &str, params: &Value) -> Outcome {
    match action {
        "device.reboot" => {
            call(
                "zwrt_mc.device.manager",
                "device_reboot",
                json!({"moduleName":"web"}),
            )
            .await
        }
        "device.poweroff" => {
            call(
                "zwrt_mc.device.manager",
                "device_poweroff",
                json!({"moduleName":"web"}),
            )
            .await
        }
        "cellular.connect" => {
            call(
                "zwrt_data",
                "set_wwaniface",
                json!({"enable":1,"source_module":"WEBUI","cid":1}),
            )
            .await
        }
        "cellular.disconnect" => {
            call(
                "zwrt_data",
                "set_wwaniface",
                json!({"enable":0,"source_module":"WEBUI","cid":1}),
            )
            .await
        }
        "cellular.set" => cellular_set(params).await,
        "network.set_mode" => {
            mapped_call(
                params,
                "zte_nwinfo_api",
                "nwinfo_set_netselect",
                &[("mode", "net_select", true, false)],
                false,
            )
            .await
        }
        "band.set_lte" => band(params, true, false).await,
        "band.set_nr_sa" => band(params, false, false).await,
        "band.set_nr_nsa" => band(params, false, true).await,
        "cell.lock_lte" => {
            mapped_call(
                params,
                "zte_nwinfo_api",
                "nwinfo_lock_lte_cell",
                &[
                    ("pci", "lock_lte_pci", true, false),
                    ("earfcn", "lock_lte_earfcn", true, false),
                ],
                false,
            )
            .await
        }
        "cell.lock_nr" => {
            mapped_call(
                params,
                "zte_nwinfo_api",
                "nwinfo_lock_nr_cell",
                &[
                    ("pci", "lock_nr_pci", true, false),
                    ("arfcn", "lock_nr_earfcn", true, false),
                    ("band", "lock_nr_cell_band", true, false),
                ],
                false,
            )
            .await
        }
        "cell.unlock_all" => unlock_all().await,
        "sim.set_slot" => sim_slot(params).await,
        "wifi.set_dual_band" => wifi_dual_band(params).await,
        "wifi.set_module" => {
            mapped_call(
                params,
                "zwrt_wlan",
                "set",
                &[("enabled", "SwitchOption", true, true)],
                false,
            )
            .await
        }
        "wifi.set_chip" => {
            mapped_call(
                params,
                "zwrt_wlan",
                "set",
                &[
                    ("chip", "ChipEnum", true, false),
                    ("guest_enabled", "GuestEnable", false, true),
                ],
                false,
            )
            .await
        }
        "wifi.configure" => wifi_configure(params).await,
        "lan.set" => {
            mapped_call(
                params,
                "zwrt_router.api",
                "router_set_lan_para",
                &[
                    ("ip", "ipaddr", false, false),
                    ("netmask", "netmask", false, false),
                    ("dhcp_disabled", "ignore", false, true),
                    ("dhcp_start", "zte_start", false, false),
                    ("dhcp_end", "zte_end", false, false),
                    ("lease_seconds", "leasetime", false, false),
                ],
                true,
            )
            .await
        }
        "lan.set_mtu" => {
            mapped_call(
                params,
                "zwrt_router.api",
                "router_set_wan_mtu",
                &[("mtu", "wan_mtu", true, false)],
                false,
            )
            .await
        }
        "dns.set" => {
            mapped_call(
                params,
                "zwrt_router.api",
                "router_set_lan_dns",
                &[
                    ("primary", "dns1", false, false),
                    ("secondary", "dns2", false, false),
                    ("manual_ipv4", "lan_dns_manual_enable", false, true),
                    ("manual_ipv6", "lan_dns_manual_enable_v6", false, true),
                ],
                true,
            )
            .await
        }
        "power.direct_supply.set" => direct_supply(params).await,
        "usb.set" => {
            mapped_call(
                params,
                "zwrt_bsp.usb",
                "set",
                &[
                    ("mode", "mode", false, false),
                    ("port_switch", "usb_port_switch", false, false),
                    ("network_protocol", "usb_network_protocal", false, false),
                ],
                true,
            )
            .await
        }
        "sleep.set" => {
            mapped_call(
                params,
                "zwrt_zte_sleep_faw.wakelock",
                "set_ufi_sleep",
                &[("seconds", "ufiSleepTime", true, false)],
                false,
            )
            .await
        }
        "nfc.set" => nfc(params).await,
        "apn.set_mode" => {
            mapped_call(
                params,
                "zwrt_apn_object",
                "set_apn_mode",
                &[("mode", "apn_mode", true, true)],
                false,
            )
            .await
        }
        "apn.add" => apn(params, "add_manu_apn", false).await,
        "apn.modify" => apn(params, "modify_manu_apn", true).await,
        "apn.delete" => {
            mapped_call(
                params,
                "zwrt_apn_object",
                "delete_manu_apn",
                &[("profile_id", "profileId", true, false)],
                false,
            )
            .await
        }
        "apn.enable" => {
            mapped_call(
                params,
                "zwrt_apn_object",
                "enable_manu_apn_id",
                &[("profile_id", "profileId", true, false)],
                false,
            )
            .await
        }
        "traffic.set_limit" => {
            traffic(
                params,
                "set_wwandst_monthlimit",
                &[
                    ("enabled", "enable", true, true),
                    ("value", "value", false, false),
                    ("type", "type", false, true),
                    ("ratio", "ratio", false, true),
                ],
                json!({}),
            )
            .await
        }
        "traffic.set_clear_day" => {
            traffic(
                params,
                "set_wwandst_clearday",
                &[("day", "clearday", true, true)],
                json!({"enable":1}),
            )
            .await
        }
        "traffic.calibrate" => {
            traffic(
                params,
                "set_wwandst_calibmonth",
                &[("value", "value", true, false)],
                json!({"type":2}),
            )
            .await
        }
        "sms.delete" => {
            mapped_call(
                params,
                "zwrt_wms",
                "zwrt_wms_delete_sms",
                &[("ids", "id", true, false)],
                false,
            )
            .await
        }
        "sms.mark_read" => {
            mapped_call(
                params,
                "zwrt_wms",
                "zwrt_wms_modify_tag",
                &[("ids", "id", true, false), ("tag", "tag", false, true)],
                false,
            )
            .await
        }
        "client.kick" => {
            mapped_call(
                params,
                "zwrt_wlan",
                "kick_macs",
                &[("macs", "macs", true, false)],
                false,
            )
            .await
        }
        "client.rename" => client_rename(params).await,
        "client.block" => client_access(params, true).await,
        "client.unblock" => client_access(params, false).await,
        _ => Outcome::NotHandled,
    }
}

async fn cellular_set(params: &Value) -> Outcome {
    let overrides = match mapped(
        params,
        &[
            ("enabled", "enable", false, true),
            ("roaming", "roam_enable", false, true),
            ("connect_mode", "connect_mode", false, false),
        ],
    ) {
        Ok(Value::Object(v)) if !v.is_empty() => v,
        Ok(_) => return Outcome::Invalid("no cellular fields supplied".into()),
        Err(e) => return Outcome::Invalid(e),
    };
    let mut current = match state::ubus(
        "zwrt_data",
        "get_wwaniface",
        json!({"source_module":"web","cid":1,"connect_status":""}),
    )
    .await
    {
        Ok(Value::Object(v)) => v,
        Ok(_) => return Outcome::Failed("invalid get_wwaniface response".into()),
        Err(e) => return Outcome::Failed(e),
    };
    current.extend(overrides);
    current.insert("source_module".into(), json!("WEBUI"));
    current.insert("cid".into(), json!(1));
    call("zwrt_data", "set_wwaniface", Value::Object(current)).await
}
async fn band(params: &Value, lte: bool, nsa: bool) -> Outcome {
    let bands = match string(params, "bands", false) {
        Ok(v) => v.unwrap_or_default(),
        Err(e) => return Outcome::Invalid(e),
    };
    if !bands.bytes().all(|b| b.is_ascii_digit() || b == b',') {
        return Outcome::Invalid("bands must contain only numbers and commas".into());
    }
    if lte {
        call(
            "zte_nwinfo_api",
            "nwinfo_set_lte_ext_band",
            json!({"lte_band":bands}),
        )
        .await
    } else {
        call(
            "zte_nwinfo_api",
            "nwinfo_set_nrbandlock",
            json!({"nr5g_type":if nsa{"1"}else{"0"},"nr5g_band":bands}),
        )
        .await
    }
}
async fn unlock_all() -> Outcome {
    if let Err(e) = state::ubus(
        "zte_nwinfo_api",
        "nwinfo_lock_lte_cell",
        json!({"lock_lte_pci":"0","lock_lte_earfcn":"0"}),
    )
    .await
    {
        return Outcome::Failed(e);
    }
    call(
        "zte_nwinfo_api",
        "nwinfo_lock_nr_cell",
        json!({"lock_nr_pci":"0","lock_nr_earfcn":"0","lock_nr_cell_band":"0"}),
    )
    .await
}
async fn sim_slot(params: &Value) -> Outcome {
    let slot = match integer(params, "slot", true) {
        Ok(Some(v @ 1..=2)) => v,
        Ok(_) => return Outcome::Invalid("slot must be 1 or 2".into()),
        Err(e) => return Outcome::Invalid(e),
    };
    let _ = state::ubus(
        "zwrt_zte_mdm.api",
        "zwrt_mdm_change_provision_session",
        json!({"active_slot":if slot==2{1}else{2},"active_flag":0}),
    )
    .await;
    call(
        "zwrt_zte_mdm.api",
        "zwrt_mdm_change_provision_session",
        json!({"active_slot":slot,"active_flag":1}),
    )
    .await
}
async fn wifi_dual_band(params: &Value) -> Outcome {
    let enabled = match boolean(params, "enabled") {
        Ok(v) => v,
        Err(e) => return Outcome::Invalid(e),
    };
    let mut current =
        match state::ubus("zwrt_router.api", "router_get_wifi_isolate", json!({})).await {
            Ok(Value::Object(v)) => v,
            Ok(_) => return Outcome::Failed("invalid router_get_wifi_isolate response".into()),
            Err(e) => return Outcome::Failed(e),
        };
    current.insert(
        "wifimain24_wifimain5_enable".into(),
        json!(i32::from(enabled)),
    );
    call(
        "zwrt_router.api",
        "router_set_wifi_isolate",
        Value::Object(current),
    )
    .await
}
async fn direct_supply(params: &Value) -> Outcome {
    let wanted = match boolean(params, "enabled") {
        Ok(v) => v,
        Err(e) => return Outcome::Invalid(e),
    };
    let before = match state::ubus("zwrt_bsp.charger", "list", json!({})).await {
        Ok(v) => v,
        Err(e) => return Outcome::Failed(e),
    };
    let mode = before
        .get("direct_power_supply_mode")
        .and_then(Value::as_str);
    let Some(mode @ ("enable" | "disable")) = mode else {
        return Outcome::Failed("direct supply is not supported or state is unknown".into());
    };
    let changed = (mode == "enable") != wanted;
    if changed {
        if let Err(e) = state::ubus(
            "zwrt_bsp.charger",
            "set",
            json!({"direct_power_supply_mode":if wanted{"enable"}else{"disable"}}),
        )
        .await
        {
            return Outcome::Failed(e);
        }
        let expected = if wanted { "enable" } else { "disable" };
        let mut verified = false;
        for attempt in 0..5 {
            if attempt != 0 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            if state::ubus("zwrt_bsp.charger", "list", json!({}))
                .await
                .ok()
                .and_then(|value| {
                    value
                        .get("direct_power_supply_mode")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .as_deref()
                == Some(expected)
            {
                verified = true;
                break;
            }
        }
        if !verified {
            return Outcome::Failed(
                "direct supply readback did not confirm the requested mode".into(),
            );
        }
    }
    Outcome::Ok(
        json!({"supported":true,"enabled":wanted,"mode":if wanted{"enable"}else{"disable"},"changed":changed,"verified":true}),
    )
}
async fn nfc(params: &Value) -> Outcome {
    let args = match mapped(
        params,
        &[
            ("enabled", "switch", true, true),
            ("flag", "flag", false, true),
        ],
    ) {
        Ok(v) => v,
        Err(e) => return Outcome::Invalid(e),
    };
    match state::ubus("zwrt_nfc", "zwrt_nfc_wifi_set", args).await {
        Ok(value) => {
            let _ = state::ubus("zwrt_nfc", "zwrt_nfc_wifi_change", json!({})).await;
            Outcome::Ok(value)
        }
        Err(e) => Outcome::Failed(e),
    }
}
async fn apn(params: &Value, method: &str, profile_required: bool) -> Outcome {
    mapped_call(
        params,
        "zwrt_apn_object",
        method,
        &[
            ("profile_id", "profileId", profile_required, false),
            ("name", "profilename", true, false),
            ("apn", "wanapn", true, false),
            ("username", "username", false, false),
            ("password", "password", false, false),
            ("auth_mode", "pppAuthMode", false, true),
            ("pdp_type", "pdpType", false, true),
            ("roaming_pdp_type", "roamingPdpType", false, true),
        ],
        false,
    )
    .await
}
async fn traffic(
    params: &Value,
    method: &str,
    specs: &[(&str, &str, bool, bool)],
    extra: Value,
) -> Outcome {
    let mut args = match mapped(params, specs) {
        Ok(Value::Object(v)) => v,
        Ok(_) => unreachable!(),
        Err(e) => return Outcome::Invalid(e),
    };
    if let Value::Object(v) = extra {
        args.extend(v)
    }
    args.insert("source_module".into(), json!("web"));
    args.insert("cid".into(), json!(1));
    call("zwrt_data", method, Value::Object(args)).await
}
fn valid_mac(v: &str) -> bool {
    v.len() == 17
        && v.split(':').count() == 6
        && v.split(':')
            .all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit()))
}
async fn client_rename(params: &Value) -> Outcome {
    let mac = match string(params, "mac", true) {
        Ok(Some(v)) if valid_mac(&v) => v,
        Ok(_) => return Outcome::Invalid("invalid mac address".into()),
        Err(e) => return Outcome::Invalid(e),
    };
    let hostname = match string(params, "hostname", true) {
        Ok(Some(v)) => v,
        Ok(_) => unreachable!(),
        Err(e) => return Outcome::Invalid(e),
    };
    call(
        "zwrt_router.api",
        "router_modify_lan_hostname",
        json!({"mac":mac,"hostname":hostname}),
    )
    .await
}

async fn revert_wireless(error: String) -> Outcome {
    let _ = state::uci_write("revert", "wireless", None).await;
    Outcome::Failed(error)
}

async fn wifi_configure(params: &Value) -> Outcome {
    let section = match string(params, "section", true) {
        Ok(Some(v)) if matches!(v.as_str(), "main_2g" | "main_5g" | "guest_2g" | "guest_5g") => v,
        Ok(_) => return Outcome::Invalid("unsupported wifi section".into()),
        Err(e) => return Outcome::Invalid(e),
    };
    let mut updates = Vec::new();
    for field in [
        "ssid",
        "encryption",
        "key",
        "pmf",
        "maxassoc",
        "hidden",
        "isolate",
    ] {
        match string(params, field, false) {
            Ok(Some(value)) => updates.push((field, value)),
            Ok(None) => {}
            Err(e) => return Outcome::Invalid(e),
        }
    }
    if let Some(enabled) = object(params).get("enabled") {
        let enabled = match enabled {
            Value::Bool(v) => *v,
            Value::Number(v) if v.as_i64() == Some(0) => false,
            Value::Number(v) if v.as_i64() == Some(1) => true,
            _ => return Outcome::Invalid("enabled must be boolean or 0/1".into()),
        };
        updates.push(("disabled", if enabled { "0" } else { "1" }.into()));
    }
    if updates.is_empty() {
        return Outcome::Invalid("no wifi fields supplied".into());
    }
    for (field, value) in &updates {
        let valid = match *field {
            "ssid" => !value.is_empty() && value.len() <= 32 && !value.contains(['\r', '\n']),
            "encryption" => matches!(
                value.as_str(),
                "none"
                    | "psk2+ccmp"
                    | "sae-mixed"
                    | "sae"
                    | "psk-mixed+tkip+ccmp"
                    | "psk2"
                    | "psk-mixed"
            ),
            "key" => {
                value.is_empty()
                    || ((8..=63).contains(&value.len()) && !value.contains(['\r', '\n']))
            }
            "hidden" | "isolate" => matches!(value.as_str(), "0" | "1"),
            _ => value.len() <= 128 && !value.contains(['\r', '\n']),
        };
        if !valid {
            return Outcome::Invalid(format!("invalid Wi-Fi {field}"));
        }
    }
    let mut changed = false;
    for (field, value) in updates {
        if field == "key" && value.is_empty() {
            continue;
        }
        let path = format!("wireless.{section}.{field}");
        if state::uci_read(&path).await == value {
            continue;
        }
        if let Err(e) = state::uci_write("set", &path, Some(&value)).await {
            return revert_wireless(e).await;
        }
        changed = true;
    }
    if !changed {
        return Outcome::Ok(json!({"section":section,"changed":false}));
    }
    if let Err(e) = state::uci_write("commit", "wireless", None).await {
        return revert_wireless(e).await;
    }
    if let Err(e) = state::ubus("zwrt_wlan", "reload", json!({})).await {
        return Outcome::Failed(format!(
            "wifi configuration committed but reload failed: {e}"
        ));
    }
    Outcome::Ok(json!({"section":section,"changed":true}))
}

async fn client_access(params: &Value, block: bool) -> Outcome {
    let mac = match string(params, "mac", true) {
        Ok(Some(v)) if valid_mac(&v) => v.to_ascii_lowercase(),
        Ok(_) => return Outcome::Invalid("invalid mac address".into()),
        Err(e) => return Outcome::Invalid(e),
    };
    for section in ["main_2g", "main_5g", "guest_2g", "guest_5g"] {
        let filter = format!("wireless.{section}.macfilter");
        let list = format!("wireless.{section}.denymaclist");
        if let Err(e) = state::uci_write("set", &filter, Some("deny")).await {
            return revert_wireless(e).await;
        }
        let _ = state::uci_write("del_list", &list, Some(&mac)).await;
        if block {
            if let Err(e) = state::uci_write("add_list", &list, Some(&mac)).await {
                return revert_wireless(e).await;
            }
        }
    }
    if let Err(e) = state::uci_write("commit", "wireless", None).await {
        return revert_wireless(e).await;
    }
    if let Err(e) = state::ubus("zwrt_wlan", "reload", json!({})).await {
        return Outcome::Failed(format!("client policy committed but reload failed: {e}"));
    }
    if block {
        let _ = state::ubus("zwrt_wlan", "kick_macs", json!({"macs":mac})).await;
    }
    Outcome::Ok(json!({"mac":mac,"blocked":block}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_non_hex_mac() {
        assert!(!valid_mac("00:11:22:33:44:zz"));
        assert!(valid_mac("00:11:22:33:44:aa"));
    }
    #[test]
    fn band_validation() {
        for bad in ["1 3", "n78", "1;reboot"] {
            assert!(!bad.bytes().all(|b| b.is_ascii_digit() || b == b','));
        }
    }
}
