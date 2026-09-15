use crate::state;
use serde_json::{Map, Value, json};
use std::{net::IpAddr, time::Duration};

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
    "wifi.txpower.apply",
    "wifi.txpower.set_percent",
    "wifi.txpower.set_limit",
    "wifi.txpower.restore_limit",
    "wifi.psm.set",
    "wifi.txpower.set_dbm",
    "wifi.interface.create",
    "wifi.interface.configure",
    "wifi.interface.delete",
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
    "sms.send_raw",
    "client.kick",
    "client.rename",
    "client.block",
    "client.unblock",
    "multiwan.interface.set",
    "multiwan.member.set",
    "multiwan.policy.set",
    "multiwan.rule.set",
    "aggregation.set",
    "qos.clear",
    "cooling.fan.set_enabled",
    "cooling.fan.set_mode",
    "cooling.fan.set_curve",
    "cooling.liquid.set_enabled",
    "cooling.liquid.set_mode",
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
        "wifi.txpower.apply" => wifi_power(params, "apply").await,
        "wifi.txpower.set_percent" => wifi_power(params, "percent").await,
        "wifi.txpower.set_limit" => wifi_power(params, "limit").await,
        "wifi.txpower.restore_limit" => wifi_power(params, "restore").await,
        "wifi.psm.set" => wifi_psm(params).await,
        "wifi.txpower.set_dbm" => wifi_dbm(params).await,
        "wireless.config" => wireless_config(params).await,
        "wifi.interface.create" => extra_wifi(params, "create").await,
        "wifi.interface.configure" => extra_wifi(params, "configure").await,
        "wifi.interface.delete" => extra_wifi(params, "delete").await,
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
        "sms.send_raw" => match crate::sms::send(params).await {
            Ok(value) => Outcome::Ok(value),
            Err((true, error)) => Outcome::Invalid(error),
            Err((false, error)) => Outcome::Failed(error),
        },
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
        "multiwan.interface.set" => multiwan_interface(params).await,
        "multiwan.member.set" => multiwan_member(params).await,
        "multiwan.policy.set" => multiwan_policy(params).await,
        "multiwan.rule.set" => multiwan_rule(params).await,
        "aggregation.set" => aggregation(params).await,
        "qos.clear" => qos_clear().await,
        "cooling.fan.set_enabled"
        | "cooling.fan.set_mode"
        | "cooling.fan.set_curve"
        | "cooling.liquid.set_enabled"
        | "cooling.liquid.set_mode" => match crate::cooling::execute(action, params).await {
            Ok(value) => Outcome::Ok(value),
            Err((true, error)) => Outcome::Invalid(error),
            Err((false, error)) => Outcome::Failed(error),
        },
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
        if block && let Err(e) = state::uci_write("add_list", &list, Some(&mac)).await {
            return revert_wireless(e).await;
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

fn flexible_i64(params: &Value, name: &str) -> Result<Option<i64>, String> {
    match object(params).get(name) {
        Some(Value::Number(v)) => v
            .as_i64()
            .map(Some)
            .ok_or_else(|| format!("invalid {name}")),
        Some(Value::String(v)) => v
            .parse::<i64>()
            .map(Some)
            .map_err(|_| format!("invalid {name}")),
        Some(_) => Err(format!("invalid {name}")),
        None => Ok(None),
    }
}
async fn restore_wifi_power(section: &str, old: [i64; 3]) -> bool {
    for (option, value) in ["txpowerpercent", "txpower", "max_power"]
        .into_iter()
        .zip(old)
    {
        if state::uci_write(
            "set",
            &format!("wireless.{section}.{option}"),
            Some(&value.to_string()),
        )
        .await
        .is_err()
        {
            return false;
        }
    }
    state::uci_write("commit", "wireless", None).await.is_ok()
        && state::ubus("zwrt_wlan", "reload", json!({})).await.is_ok()
}
async fn wifi_power(params: &Value, operation: &str) -> Outcome {
    let model = state::uci_read("zwrt_common_info.common_config.model_name").await;
    let hardware = state::uci_read("zwrt_common_info.common_config.hardware_version").await;
    if model != "MU5252" && !hardware.starts_with("MU5252_") {
        return Outcome::Invalid("wifi power control is only supported on MU5252".into());
    }
    let band = match string(params, "band", true) {
        Ok(Some(v)) if matches!(v.as_str(), "2g" | "5g") => v,
        Ok(_) => return Outcome::Invalid("band must be 2g or 5g".into()),
        Err(e) => return Outcome::Invalid(e),
    };
    let (section, factory) = if band == "2g" {
        ("wifi0", 19)
    } else {
        ("wifi1", 18)
    };
    let read = |option: &str| format!("wireless.{section}.{option}");
    let (Ok(old_percent), Ok(old_tx), Ok(old_limit)) = (
        state::uci_read(&read("txpowerpercent"))
            .await
            .parse::<i64>(),
        state::uci_read(&read("txpower")).await.parse::<i64>(),
        state::uci_read(&read("max_power")).await.parse::<i64>(),
    ) else {
        return Outcome::Failed("failed to read existing wifi power configuration".into());
    };
    if !state::uci_read(&format!("wireless.{section}.datad_txpower_dbm"))
        .await
        .is_empty()
    {
        return Outcome::Invalid("dBm policy is active; select OEM power mode first".into());
    }
    let mut percent = old_percent;
    let mut limit = old_limit;
    let (percent_present, limit_present) = match operation {
        "apply" => {
            let p = match flexible_i64(params, "percent") {
                Ok(v) => v,
                Err(e) => return Outcome::Invalid(e),
            };
            let l = match flexible_i64(params, "limit_dbm") {
                Ok(v) => v,
                Err(e) => return Outcome::Invalid(e),
            };
            if p.is_none() && l.is_none() {
                return Outcome::Invalid("percent or limit_dbm is required".into());
            }
            if let Some(v) = p {
                percent = v;
            }
            if let Some(v) = l {
                limit = v;
            }
            (p.is_some(), l.is_some())
        }
        "percent" => match flexible_i64(params, "percent") {
            Ok(Some(v)) => {
                percent = v;
                (true, false)
            }
            Ok(None) => return Outcome::Invalid("missing power value".into()),
            Err(e) => return Outcome::Invalid(e),
        },
        "limit" => match flexible_i64(params, "limit_dbm") {
            Ok(Some(v)) => {
                limit = v;
                (false, true)
            }
            Ok(None) => return Outcome::Invalid("missing power value".into()),
            Err(e) => return Outcome::Invalid(e),
        },
        "restore" => {
            limit = factory;
            (false, true)
        }
        _ => unreachable!(),
    };
    if percent_present && (!(10..=100).contains(&percent) || percent % 10 != 0) {
        return Outcome::Invalid("percent must be 10 to 100 in steps of 10".into());
    }
    if limit_present && !(1..=30).contains(&limit) {
        return Outcome::Invalid("limit_dbm must be between 1 and 30".into());
    }
    let percent_changed = percent_present && percent != old_percent;
    let limit_changed = limit_present && (limit != old_tx || limit != old_limit);
    if !percent_changed && !limit_changed {
        return Outcome::Ok(
            json!({"band":band,"changed":false,"percent":old_percent,"txpower_dbm":old_tx,"limit_dbm":old_limit,"factory_limit_dbm":factory}),
        );
    }
    if percent_changed
        && state::uci_write(
            "set",
            &format!("wireless.{section}.txpowerpercent"),
            Some(&percent.to_string()),
        )
        .await
        .is_err()
    {
        return revert_wireless("failed to stage wifi power configuration".into()).await;
    }
    if limit_changed {
        for option in ["txpower", "max_power"] {
            if state::uci_write(
                "set",
                &format!("wireless.{section}.{option}"),
                Some(&limit.to_string()),
            )
            .await
            .is_err()
            {
                return revert_wireless("failed to stage wifi power configuration".into()).await;
            }
        }
    }
    if let Err(e) = state::uci_write("commit", "wireless", None).await {
        return revert_wireless(e).await;
    }
    if state::ubus("zwrt_wlan", "reload", json!({})).await.is_err() {
        let restored = restore_wifi_power(section, [old_percent, old_tx, old_limit]).await;
        return Outcome::Failed(format!(
            "wifi reload failed; previous configuration {}",
            if restored {
                "restored"
            } else {
                "could not be restored"
            }
        ));
    }
    Outcome::Ok(
        json!({"band":band,"changed":true,"percent":if percent_changed{percent}else{old_percent},"txpower_dbm":if limit_changed{limit}else{old_tx},"limit_dbm":if limit_changed{limit}else{old_limit},"factory_limit_dbm":factory}),
    )
}

fn iw_bin() -> String {
    std::env::var("ZWRT_DATAD_IW_BIN").unwrap_or_else(|_| "/usr/sbin/iw".into())
}
async fn advanced_wifi_supported() -> bool {
    state::uci_read("zwrt_common_info.common_config.model_name").await == "MU5252"
}
async fn policy_path(section: &str, option: &str) -> Result<(String, String, String), Outcome> {
    let main = matches!(section, "main_2g" | "guest_2g" | "main_5g" | "guest_5g");
    let extra = matches!(section, "datad_ssid_1" | "datad_ssid_2");
    if !main && !extra {
        return Err(Outcome::Invalid("invalid advanced Wi-Fi settings".into()));
    }
    let package = if extra { "datad_wifi" } else { "wireless" };
    if extra
        && state::uci_read(&format!("datad_wifi.{section}"))
            .await
            .is_empty()
    {
        return Err(Outcome::Invalid("invalid advanced Wi-Fi settings".into()));
    }
    let ifname = if extra {
        if section.ends_with('1') {
            "wlan4".into()
        } else {
            "wlan5".into()
        }
    } else {
        state::uci_read(&format!("wireless.{section}.ifname")).await
    };
    Ok((
        format!("{package}.{section}.{option}"),
        package.into(),
        ifname,
    ))
}
async fn restore_policy(path: &str, package: &str, old: &str) {
    let _ = if old.is_empty() {
        state::uci_write("delete", path, None).await
    } else {
        state::uci_write("set", path, Some(old)).await
    };
    let _ = state::uci_write("commit", package, None).await;
}
async fn wifi_psm(params: &Value) -> Outcome {
    if !advanced_wifi_supported().await {
        return Outcome::Failed("advanced Wi-Fi unsupported by this model".into());
    }
    let section = match string(params, "section", true) {
        Ok(Some(v)) => v,
        Ok(_) => unreachable!(),
        Err(e) => return Outcome::Invalid(e),
    };
    let mode = match string(params, "mode", true) {
        Ok(Some(v)) if matches!(v.as_str(), "default" | "on" | "off") => v,
        Ok(_) => return Outcome::Invalid("invalid advanced Wi-Fi settings".into()),
        Err(e) => return Outcome::Invalid(e),
    };
    let (path, package, ifname) = match policy_path(&section, "datad_psm").await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let old = state::uci_read(&path).await;
    let write = if mode == "default" {
        state::uci_write("delete", &path, None).await
    } else {
        state::uci_write("set", &path, Some(&mode)).await
    };
    if let Err(e) = write {
        return Outcome::Failed(format!("could not persist advanced Wi-Fi policy: {e}"));
    }
    if let Err(e) = state::uci_write("commit", &package, None).await {
        restore_policy(&path, &package, &old).await;
        return Outcome::Failed(e);
    }
    let mut pending = mode != "default";
    if mode != "default" && !ifname.is_empty() {
        let wanted = if mode == "on" { "on" } else { "off" };
        if crate::command::run(
            &iw_bin(),
            ["dev", &ifname, "set", "power_save", wanted],
            Duration::from_secs(5),
        )
        .await
        .is_err()
        {
            restore_policy(&path, &package, &old).await;
            return Outcome::Failed("advanced Wi-Fi setting could not be applied".into());
        }
        let verified = crate::command::run(
            &iw_bin(),
            ["dev", &ifname, "get", "power_save"],
            Duration::from_secs(5),
        )
        .await
        .ok()
        .is_some_and(|v| String::from_utf8_lossy(&v).contains(&format!("Power save: {wanted}")));
        if !verified {
            restore_policy(&path, &package, &old).await;
            return Outcome::Failed("advanced Wi-Fi setting could not be verified".into());
        }
        pending = false;
    }
    Outcome::Ok(json!({"section":section,"mode":mode,"saved":true,"pending":pending}))
}
async fn wifi_dbm(params: &Value) -> Outcome {
    if !advanced_wifi_supported().await {
        return Outcome::Failed("advanced Wi-Fi unsupported by this model".into());
    }
    let band = match string(params, "band", true) {
        Ok(Some(v)) if matches!(v.as_str(), "2g" | "5g") => v,
        Ok(_) => return Outcome::Invalid("invalid advanced Wi-Fi settings".into()),
        Err(e) => return Outcome::Invalid(e),
    };
    let restore = object(params).get("mode").and_then(Value::as_str) == Some("oem");
    let dbm = if restore {
        -1
    } else {
        match flexible_i64(params, "dbm") {
            Ok(Some(v @ 1..=30)) => v,
            Ok(_) => return Outcome::Invalid("dbm must be between 1 and 30".into()),
            Err(e) => return Outcome::Invalid(e),
        }
    };
    let section = if band == "2g" { "wifi0" } else { "wifi1" };
    let path = format!("wireless.{section}.datad_txpower_dbm");
    let old = state::uci_read(&path).await;
    let write = if restore {
        state::uci_write("delete", &path, None).await
    } else {
        state::uci_write("set", &path, Some(&dbm.to_string())).await
    };
    if let Err(e) = write {
        return Outcome::Failed(e);
    }
    if let Err(e) = state::uci_write("commit", "wireless", None).await {
        restore_policy(&path, "wireless", &old).await;
        return Outcome::Failed(e);
    }
    let status = crate::wifi::advanced_status().await.ok();
    let mut applied = 0;
    if let Some(interfaces) = status
        .as_ref()
        .and_then(|v| v.get("interfaces"))
        .and_then(Value::as_array)
    {
        for item in interfaces.iter().filter(|v| {
            v.get("band").and_then(Value::as_str) == Some(&band)
                && v.get("active").and_then(Value::as_bool) == Some(true)
        }) {
            let Some(ifname) = item.get("ifname").and_then(Value::as_str) else {
                continue;
            };
            let target = if restore {
                state::uci_read(&format!("wireless.{section}.txpower"))
                    .await
                    .parse()
                    .unwrap_or(30)
            } else {
                dbm
            };
            if crate::command::run(
                &iw_bin(),
                [
                    "dev",
                    ifname,
                    "set",
                    "txpower",
                    "fixed",
                    &(target * 100).to_string(),
                ],
                Duration::from_secs(5),
            )
            .await
            .is_err()
            {
                restore_policy(&path, "wireless", &old).await;
                return Outcome::Failed("advanced Wi-Fi setting could not be applied".into());
            }
            applied += 1;
        }
    }
    Outcome::Ok(
        json!({"band":band,"mode":if restore{"oem"}else{"fixed"},"dbm":if restore{Value::Null}else{json!(dbm)},"saved":true,"pending":!restore&&applied==0,"applied_interfaces":applied}),
    )
}

fn status_channels(status: &Value, band: &str) -> Vec<i64> {
    status
        .get("radios")
        .and_then(|v| v.get(band))
        .and_then(|v| v.get("supported_channels"))
        .and_then(Value::as_array)
        .map(|v| v.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}
async fn restore_wireless_config(radio: &str, old0: &str, old1: &str, old_channel: &str) -> bool {
    for (path, value) in [
        ("wireless.wifi0.country", old0),
        ("wireless.wifi1.country", old1),
    ] {
        let result = if value.is_empty() {
            state::uci_write("delete", path, None).await
        } else {
            state::uci_write("set", path, Some(value)).await
        };
        if result.is_err() {
            return false;
        }
    }
    let path = format!("wireless.{radio}.channel");
    let result = if old_channel.is_empty() {
        state::uci_write("delete", &path, None).await
    } else {
        state::uci_write("set", &path, Some(old_channel)).await
    };
    result.is_ok()
        && state::uci_write("commit", "wireless", None).await.is_ok()
        && state::ubus("zwrt_wlan", "reload", json!({})).await.is_ok()
}
async fn wireless_config(params: &Value) -> Outcome {
    let band = match string(params, "band", true) {
        Ok(Some(v)) if matches!(v.as_str(), "2g" | "5g") => v,
        Ok(_) => return Outcome::Invalid("band must be 2g or 5g".into()),
        Err(e) => return Outcome::Invalid(e),
    };
    let radio = if band == "2g" { "wifi0" } else { "wifi1" };
    let initial = match crate::wifi::wireless_config_status().await {
        Ok(v) => v,
        Err(e) => return Outcome::Failed(e),
    };
    let country = match string(params, "country", false) {
        Ok(v) => v.map(|v| v.to_ascii_uppercase()),
        Err(e) => return Outcome::Invalid(e),
    };
    if let Some(ref value) = country {
        if !(value == "00" || (value.len() == 2 && value.bytes().all(|b| b.is_ascii_alphabetic())))
        {
            return Outcome::Invalid("country must be an ISO 3166-1 alpha-2 code".into());
        }
        let supported = initial
            .get("countries")
            .and_then(Value::as_array)
            .is_some_and(|v| v.iter().any(|x| x.as_str() == Some(value)));
        if !supported {
            return Outcome::Invalid("country is not supported by the device".into());
        }
    }
    let channel = match object(params).get("channel") {
        Some(Value::String(v)) if v.eq_ignore_ascii_case("auto") => Some(0),
        Some(Value::String(v)) => match v.parse::<i64>() {
            Ok(v @ 0..=255) => Some(v),
            _ => {
                return Outcome::Invalid(
                    "channel must be auto, 0, or a valid channel number".into(),
                );
            }
        },
        Some(Value::Number(v)) => match v.as_i64() {
            Some(v @ 0..=255) => Some(v),
            _ => {
                return Outcome::Invalid(
                    "channel must be auto, 0, or a valid channel number".into(),
                );
            }
        },
        Some(_) => {
            return Outcome::Invalid("channel must be auto, 0, or a valid channel number".into());
        }
        None => None,
    };
    let old0 = state::uci_read("wireless.wifi0.country").await;
    let old1 = state::uci_read("wireless.wifi1.country").await;
    let channel_path = format!("wireless.{radio}.channel");
    let old_channel = state::uci_read(&channel_path).await;
    let country_changed = country.as_ref().is_some_and(|v| v != &old0 || v != &old1);
    let channel_changed = channel.is_some_and(|v| v.to_string() != old_channel);
    if !country_changed && !channel_changed {
        return Outcome::Ok(
            json!({"changed":false,"band":band,"country":country.unwrap_or(old0),"channel":channel.map(|v|v.to_string()).unwrap_or(old_channel)}),
        );
    }
    if !country_changed
        && let Some(v) = channel
        && !status_channels(&initial, &band).contains(&v)
    {
        return Outcome::Invalid(format!(
            "channel {v} is not permitted for {band} under country {old0}"
        ));
    }
    if country_changed {
        let value = country.as_deref().unwrap();
        for path in ["wireless.wifi0.country", "wireless.wifi1.country"] {
            if state::uci_write("set", path, Some(value)).await.is_err() {
                let _ = restore_wireless_config(radio, &old0, &old1, &old_channel).await;
                return Outcome::Failed(
                    "failed to apply country; previous configuration restored".into(),
                );
            }
        }
        if channel.is_some()
            && state::uci_write("set", &channel_path, Some("0"))
                .await
                .is_err()
        {
            let _ = restore_wireless_config(radio, &old0, &old1, &old_channel).await;
            return Outcome::Failed("failed to stage automatic channel".into());
        }
        if state::uci_write("commit", "wireless", None).await.is_err()
            || state::ubus("zwrt_wlan", "reload", json!({})).await.is_err()
        {
            let restored = restore_wireless_config(radio, &old0, &old1, &old_channel).await;
            return Outcome::Failed(format!(
                "failed to apply country; previous configuration {}",
                if restored {
                    "restored"
                } else {
                    "could not be restored"
                }
            ));
        }
    }
    let mut final_status = initial;
    if let Some(target_channel) = channel.filter(|_| country_changed) {
        for attempt in 0..80 {
            final_status = match crate::wifi::wireless_config_status().await {
                Ok(v) => v,
                Err(_) => json!({}),
            };
            if status_channels(&final_status, &band).contains(&target_channel) {
                break;
            }
            if attempt < 79 {
                tokio::time::sleep(Duration::from_millis(250)).await
            }
        }
    }
    if let Some(v) = channel {
        if !status_channels(&final_status, &band).contains(&v) {
            let restored = restore_wireless_config(radio, &old0, &old1, &old_channel).await;
            return Outcome::Invalid(format!(
                "channel {v} is not permitted for {band} under country {}{}",
                country.as_deref().unwrap_or(&old0),
                if country_changed && !restored {
                    "; rollback failed"
                } else {
                    ""
                }
            ));
        }
        if (channel_changed || country_changed)
            && (state::uci_write("set", &channel_path, Some(&v.to_string()))
                .await
                .is_err()
                || state::uci_write("commit", "wireless", None).await.is_err()
                || state::ubus("zwrt_wlan", "reload", json!({})).await.is_err())
        {
            let restored = restore_wireless_config(radio, &old0, &old1, &old_channel).await;
            return Outcome::Failed(format!(
                "failed to apply channel; previous configuration {}",
                if restored {
                    "restored"
                } else {
                    "could not be restored"
                }
            ));
        }
    }
    Outcome::Ok(
        json!({"changed":true,"band":band,"country":country.unwrap_or(old0),"channel":channel.map(|v|v.to_string()).unwrap_or(old_channel),"channel_source":final_status.get("channel_source").cloned().unwrap_or(json!("uci"))}),
    )
}

async fn extra_value(section: &str, option: &str) -> String {
    state::uci_read(&format!("datad_wifi.{section}.{option}")).await
}
async fn extra_exists(section: &str) -> bool {
    !state::uci_read(&format!("datad_wifi.{section}"))
        .await
        .is_empty()
}
async fn write_extra(section: &str, values: &[(&str, String)]) -> Result<(), String> {
    let config =
        std::env::var("ZWRT_DATAD_WIFI_CONFIG").unwrap_or_else(|_| "/etc/config/datad_wifi".into());
    if let Some(parent) = std::path::Path::new(&config).parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| e.to_string())?
    }
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config)
        .await
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(|e| e.to_string())?;
    }
    state::uci_write("set", &format!("datad_wifi.{section}"), Some("wifi-iface")).await?;
    for (option, value) in values {
        if let Err(e) = state::uci_write(
            "set",
            &format!("datad_wifi.{section}.{option}"),
            Some(value),
        )
        .await
        {
            let _ = state::uci_write("revert", "datad_wifi", None).await;
            return Err(e);
        }
    }
    if let Err(e) = state::uci_write("commit", "datad_wifi", None).await {
        let _ = state::uci_write("revert", "datad_wifi", None).await;
        return Err(e);
    }
    Ok(())
}
async fn extra_wifi(params: &Value, operation: &str) -> Outcome {
    if !advanced_wifi_supported().await {
        return Outcome::Failed("advanced Wi-Fi unsupported by this model".into());
    }
    if operation == "configure"
        && let Some(section) = object(params).get("section").and_then(Value::as_str)
        && matches!(section, "main_2g" | "main_5g" | "guest_2g" | "guest_5g")
    {
        return wifi_configure(params).await;
    }
    let section = if operation == "create" {
        if !extra_exists("datad_ssid_1").await {
            "datad_ssid_1".to_owned()
        } else if !extra_exists("datad_ssid_2").await {
            "datad_ssid_2".to_owned()
        } else {
            return Outcome::Invalid("two extra SSID slots are already configured".into());
        }
    } else {
        match string(params, "section", true) {
            Ok(Some(v))
                if matches!(v.as_str(), "datad_ssid_1" | "datad_ssid_2")
                    && extra_exists(&v).await =>
            {
                v
            }
            Ok(_) => {
                return Outcome::Invalid(
                    "invalid extra SSID settings or duplicate SSID on this band".into(),
                );
            }
            Err(e) => return Outcome::Invalid(e),
        }
    };
    let old = if operation == "create" {
        None
    } else {
        Some(crate::extra_wifi::Config {
            section: section.clone(),
            band: extra_value(&section, "band").await,
            ssid: extra_value(&section, "ssid").await,
            encryption: extra_value(&section, "encryption").await,
            key: extra_value(&section, "key").await,
            enabled: extra_value(&section, "disabled").await != "1",
            hidden: extra_value(&section, "hidden").await == "1",
            isolate: extra_value(&section, "isolate").await == "1",
        })
    };
    if operation == "delete" {
        if let Err(e) = crate::extra_wifi::stop(&section).await {
            return Outcome::Failed(format!("could not stop extra SSID: {e}"));
        }
        if let Err(e) = state::uci_write("delete", &format!("datad_wifi.{section}"), None).await {
            if let Some(old) = &old {
                let _ = crate::extra_wifi::apply(old).await;
            }
            return Outcome::Failed(e);
        }
        if let Err(e) = state::uci_write("commit", "datad_wifi", None).await {
            let _ = state::uci_write("revert", "datad_wifi", None).await;
            if let Some(old) = &old {
                let _ = crate::extra_wifi::apply(old).await;
            }
            return Outcome::Failed(e);
        }
        return Outcome::Ok(json!({"section":section,"changed":true,"deleted":true}));
    }
    let band = match string(params, "band", false) {
        Ok(Some(v)) if matches!(v.as_str(), "2g" | "5g") => v,
        Ok(None) if operation != "create" => extra_value(&section, "band").await,
        Ok(_) => {
            return Outcome::Invalid(
                "invalid extra SSID settings or duplicate SSID on this band".into(),
            );
        }
        Err(e) => return Outcome::Invalid(e),
    };
    let ssid = match string(params, "ssid", false) {
        Ok(Some(v)) => v,
        Ok(None) if operation != "create" => extra_value(&section, "ssid").await,
        Ok(_) => {
            return Outcome::Invalid(
                "invalid extra SSID settings or duplicate SSID on this band".into(),
            );
        }
        Err(e) => return Outcome::Invalid(e),
    };
    if ssid.is_empty() || ssid.len() > 32 || ssid.contains(['\r', '\n']) {
        return Outcome::Invalid(
            "invalid extra SSID settings or duplicate SSID on this band".into(),
        );
    }
    for main in if band == "2g" {
        ["main_2g", "guest_2g"]
    } else {
        ["main_5g", "guest_5g"]
    } {
        if state::uci_read(&format!("wireless.{main}.ssid")).await == ssid {
            return Outcome::Invalid(
                "invalid extra SSID settings or duplicate SSID on this band".into(),
            );
        }
    }
    for other in ["datad_ssid_1", "datad_ssid_2"] {
        if other != section
            && extra_exists(other).await
            && extra_value(other, "band").await == band
            && extra_value(other, "ssid").await == ssid
        {
            return Outcome::Invalid(
                "invalid extra SSID settings or duplicate SSID on this band".into(),
            );
        }
    }
    let encryption = match string(params, "encryption", false) {
        Ok(Some(v)) => v,
        Ok(None) if operation == "create" => "sae-mixed".into(),
        Ok(None) => extra_value(&section, "encryption").await,
        Err(e) => return Outcome::Invalid(e),
    };
    if !matches!(
        encryption.as_str(),
        "none" | "psk2+ccmp" | "sae-mixed" | "sae"
    ) {
        return Outcome::Invalid(
            "invalid extra SSID settings or duplicate SSID on this band".into(),
        );
    }
    let key = match string(params, "key", false) {
        Ok(Some(v)) if !v.is_empty() => v,
        Ok(_) => extra_value(&section, "key").await,
        Err(e) => return Outcome::Invalid(e),
    };
    if encryption != "none" && (!(8..=63).contains(&key.len()) || key.contains(['\r', '\n'])) {
        return Outcome::Invalid(
            "invalid extra SSID settings or duplicate SSID on this band".into(),
        );
    }
    let flag = |name: &str, default: i64| -> Result<i64, String> {
        match object(params).get(name) {
            Some(Value::Bool(v)) => Ok(i64::from(*v)),
            Some(Value::Number(v)) if matches!(v.as_i64(), Some(0 | 1)) => Ok(v.as_i64().unwrap()),
            Some(_) => Err(format!("{name} must be boolean or 0/1")),
            None => Ok(default),
        }
    };
    let old_enabled = extra_value(&section, "disabled")
        .await
        .parse::<i64>()
        .map(|v| 1 - v)
        .unwrap_or(1);
    let enabled = match flag("enabled", old_enabled) {
        Ok(v) => v,
        Err(e) => return Outcome::Invalid(e),
    };
    let hidden = match flag(
        "hidden",
        extra_value(&section, "hidden").await.parse().unwrap_or(0),
    ) {
        Ok(v) => v,
        Err(e) => return Outcome::Invalid(e),
    };
    let isolate = match flag(
        "isolate",
        extra_value(&section, "isolate").await.parse().unwrap_or(0),
    ) {
        Ok(v) => v,
        Err(e) => return Outcome::Invalid(e),
    };
    let psm = extra_value(&section, "datad_psm").await;
    let values = [
        ("band", band),
        ("ssid", ssid),
        ("encryption", encryption),
        ("key", key),
        ("disabled", (1 - enabled).to_string()),
        ("hidden", hidden.to_string()),
        ("isolate", isolate.to_string()),
        ("datad_psm", psm),
    ];
    if let Err(e) = write_extra(&section, &values).await {
        return Outcome::Failed(format!("could not save extra SSID: {e}"));
    }
    let current = crate::extra_wifi::Config {
        section: section.clone(),
        band: values[0].1.clone(),
        ssid: values[1].1.clone(),
        encryption: values[2].1.clone(),
        key: values[3].1.clone(),
        enabled: enabled == 1,
        hidden: hidden == 1,
        isolate: isolate == 1,
    };
    let runtime_result = match crate::extra_wifi::stop(&section).await {
        Ok(()) => {
            crate::extra_wifi::reset_attempts(&section).await;
            crate::extra_wifi::apply(&current).await
        }
        Err(e) => Err(e),
    };
    if let Err(e) = runtime_result {
        let _ = crate::extra_wifi::stop(&section).await;
        if let Some(old) = &old {
            let old_values = [
                ("band", old.band.clone()),
                ("ssid", old.ssid.clone()),
                ("encryption", old.encryption.clone()),
                ("key", old.key.clone()),
                ("disabled", i64::from(!old.enabled).to_string()),
                ("hidden", i64::from(old.hidden).to_string()),
                ("isolate", i64::from(old.isolate).to_string()),
                ("datad_psm", values[7].1.clone()),
            ];
            let _ = write_extra(&section, &old_values).await;
            let _ = crate::extra_wifi::apply(old).await;
        } else {
            let _ = state::uci_write("delete", &format!("datad_wifi.{section}"), None).await;
            let _ = state::uci_write("commit", "datad_wifi", None).await;
        }
        return Outcome::Failed(format!(
            "extra SSID could not start; previous configuration restored: {e}"
        ));
    }
    Outcome::Ok(json!({"section":section,"saved":true,"changed":true,"pending":enabled==1}))
}

fn valid_section(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}
async fn mwan_section(params: &Value, kind: &str) -> Result<String, Outcome> {
    let section = match string(params, "section", true) {
        Ok(Some(v)) if valid_section(&v) => v,
        Ok(_) => return Err(Outcome::Invalid(format!("unknown mwan3 {kind}"))),
        Err(e) => return Err(Outcome::Invalid(e)),
    };
    if state::uci_read(&format!("mwan3.{section}")).await != kind {
        return Err(Outcome::Invalid(format!("unknown mwan3 {kind}")));
    }
    Ok(section)
}
async fn mwan_set(section: &str, option: &str, value: &str) -> Result<(), Outcome> {
    state::uci_write("set", &format!("mwan3.{section}.{option}"), Some(value))
        .await
        .map_err(Outcome::Failed)
}
async fn mwan_int(
    params: &Value,
    section: &str,
    option: &str,
    min: i64,
    max: i64,
) -> Result<bool, Outcome> {
    let Some(value) = object(params).get(option) else {
        return Ok(false);
    };
    let Some(value) = value.as_i64() else {
        return Err(Outcome::Invalid(format!(
            "{option} must be between {min} and {max}"
        )));
    };
    if !(min..=max).contains(&value) {
        return Err(Outcome::Invalid(format!(
            "{option} must be between {min} and {max}"
        )));
    }
    mwan_set(section, option, &value.to_string()).await?;
    Ok(true)
}
async fn mwan_revert(outcome: Outcome) -> Outcome {
    let _ = state::uci_write("revert", "mwan3", None).await;
    outcome
}
async fn mwan_finish(section: &str, changed: bool) -> Outcome {
    if !changed {
        return mwan_revert(Outcome::Invalid("no mwan3 fields supplied".into())).await;
    }
    if let Err(e) = state::uci_write("commit", "mwan3", None).await {
        return mwan_revert(Outcome::Failed(e)).await;
    }
    let mut applied = false;
    if state::uci_read("zwrt_router.network.opms_wan_mode").await == "MULTIWAN" {
        let init =
            std::env::var("ZWRT_DATAD_MWAN3_INIT").unwrap_or_else(|_| "/etc/init.d/mwan3".into());
        if let Err(e) = crate::command::run(&init, ["restart"], Duration::from_secs(10)).await {
            return Outcome::Failed(format!(
                "mwan3 configuration committed but restart failed: {e}"
            ));
        }
        applied = true;
    }
    Outcome::Ok(json!({"section":section,"applied":applied}))
}
async fn multiwan_interface(params: &Value) -> Outcome {
    let section = match mwan_section(params, "interface").await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let mut changed = false;
    for (name, min, max) in [
        ("enabled", 0, 1),
        ("reliability", 0, 16),
        ("count", 1, 16),
        ("size", 1, 4096),
        ("max_ttl", 1, 255),
        ("check_quality", 0, 1),
        ("timeout", 1, 60),
        ("interval", 1, 3600),
        ("failure_interval", 1, 3600),
        ("recovery_interval", 1, 3600),
        ("down", 1, 100),
        ("up", 1, 100),
    ] {
        match mwan_int(params, &section, name, min, max).await {
            Ok(v) => changed |= v,
            Err(e) => return mwan_revert(e).await,
        }
    }
    if let Some(method) = object(params).get("track_method") {
        if method.as_str() != Some("ping") {
            return mwan_revert(Outcome::Invalid("only ping tracking is supported".into())).await;
        }
        if let Err(e) = mwan_set(&section, "track_method", "ping").await {
            return mwan_revert(e).await;
        }
        changed = true;
    }
    if let Some(raw) = object(params).get("track_ip") {
        let Some(raw) = raw.as_str() else {
            return mwan_revert(Outcome::Invalid("track_ip must be a string".into())).await;
        };
        let items: Vec<_> = raw
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|v| !v.is_empty())
            .collect();
        if items.len() > 16 || items.iter().any(|v| v.parse::<IpAddr>().is_err()) {
            return mwan_revert(Outcome::Invalid("invalid tracking address".into())).await;
        }
        let path = format!("mwan3.{section}.track_ip");
        let _ = state::uci_write("delete", &path, None).await;
        for item in items {
            if let Err(e) = state::uci_write("add_list", &path, Some(item)).await {
                return mwan_revert(Outcome::Failed(e)).await;
            }
        }
        changed = true;
    }
    mwan_finish(&section, changed).await
}
async fn multiwan_member(params: &Value) -> Outcome {
    let section = match mwan_section(params, "member").await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let mut changed = false;
    for (n, a, b) in [("metric", 1, 65535), ("weight", 1, 1000)] {
        match mwan_int(params, &section, n, a, b).await {
            Ok(v) => changed |= v,
            Err(e) => return mwan_revert(e).await,
        }
    }
    mwan_finish(&section, changed).await
}
async fn multiwan_policy(params: &Value) -> Outcome {
    let section = match mwan_section(params, "policy").await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let mut changed = false;
    if let Some(v) = object(params).get("last_resort") {
        let Some(v) = v.as_str() else {
            return Outcome::Invalid("invalid last_resort".into());
        };
        if !matches!(v, "default" | "unreachable" | "blackhole") {
            return Outcome::Invalid("invalid last_resort".into());
        }
        if let Err(e) = mwan_set(&section, "last_resort", v).await {
            return mwan_revert(e).await;
        }
        changed = true;
    }
    if let Some(v) = object(params).get("use_member") {
        let Some(v) = v.as_str() else {
            return Outcome::Invalid("use_member must be a string".into());
        };
        let items: Vec<_> = v
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|x| !x.is_empty())
            .collect();
        if items.len() > 16 {
            return Outcome::Invalid("too many use_member entries".into());
        }
        for item in &items {
            if !valid_section(item) || state::uci_read(&format!("mwan3.{item}")).await != "member" {
                return mwan_revert(Outcome::Invalid(format!("unknown mwan3 member: {item}")))
                    .await;
            }
        }
        let path = format!("mwan3.{section}.use_member");
        let _ = state::uci_write("delete", &path, None).await;
        for item in items {
            if let Err(e) = state::uci_write("add_list", &path, Some(item)).await {
                return mwan_revert(Outcome::Failed(e)).await;
            }
        }
        changed = true;
    }
    mwan_finish(&section, changed).await
}
async fn multiwan_rule(params: &Value) -> Outcome {
    let section = match mwan_section(params, "rule").await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let mut changed = false;
    if let Some(v) = object(params).get("use_policy") {
        let Some(v) = v.as_str() else {
            return Outcome::Invalid("use_policy must be a string".into());
        };
        if !valid_section(v) || state::uci_read(&format!("mwan3.{v}")).await != "policy" {
            return Outcome::Invalid("unknown mwan3 policy".into());
        }
        if let Err(e) = mwan_set(&section, "use_policy", v).await {
            return mwan_revert(e).await;
        }
        changed = true;
    }
    for (n, a, b) in [("sticky", 0, 1), ("logging", 0, 1)] {
        match mwan_int(params, &section, n, a, b).await {
            Ok(v) => changed |= v,
            Err(e) => return mwan_revert(e).await,
        }
    }
    mwan_finish(&section, changed).await
}

async fn run_mwan3(verb: &str, required: bool) -> Result<(), String> {
    let init =
        std::env::var("ZWRT_DATAD_MWAN3_INIT").unwrap_or_else(|_| "/etc/init.d/mwan3".into());
    match crate::command::run(&init, [verb], Duration::from_secs(10)).await {
        Ok(_) => Ok(()),
        Err(_) if !required => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}
async fn aggregation(params: &Value) -> Outcome {
    let enabled = match boolean(params, "enabled") {
        Ok(v) => v,
        Err(e) => return Outcome::Invalid(e),
    };
    if enabled {
        if let Err(e) = state::ubus(
            "zwrt_router.api", "router_set_wan_mode",
            json!({"opms_wan_mode":"SMULTIWAN","wan_ippass_device_type":"","wan_ippass_device_mac":""}),
        ).await { return Outcome::Failed(e); }
        let _ = run_mwan3("stop", false).await;
    } else {
        if let Err(e) = state::ubus(
            "zwrt_router.api",
            "router_stop_agg_mode",
            json!({"agg_mode_switch":0}),
        )
        .await
        {
            return Outcome::Failed(e);
        }
        if let Err(e) = state::ubus(
            "zwrt_router.api", "router_set_wan_mode",
            json!({"opms_wan_mode":"MULTIWAN","wan_ippass_device_type":"","wan_ippass_device_mac":""}),
        ).await { return Outcome::Failed(format!("aggregation stopped but MULTIWAN mode switch failed: {e}")); }
        if let Err(e) = run_mwan3("restart", true).await {
            return Outcome::Failed(format!(
                "aggregation disabled but mwan3 restart failed: {e}"
            ));
        }
    }
    Outcome::Ok(json!({"enabled":enabled}))
}

async fn qos_clear() -> Outcome {
    let paths = [
        std::env::var("ZWRT_DATAD_QOS_LOG").unwrap_or_else(|_| "/data/logfs/key.log".into()),
        std::env::var("ZWRT_DATAD_QOS_LOG_ROTATED")
            .unwrap_or_else(|_| "/data/logfs/key.log.0".into()),
    ];
    let mut cleared = 0;
    for path in paths {
        match tokio::fs::OpenOptions::new().write(true).open(&path).await {
            Ok(file) => match file.set_len(0).await {
                Ok(()) => cleared += 1,
                Err(e) => return Outcome::Failed(format!("failed to clear {path}: {e}")),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Outcome::Failed(format!("failed to clear {path}: {e}")),
        }
    }
    Outcome::Ok(json!({"cleared":true,"files":cleared}))
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
