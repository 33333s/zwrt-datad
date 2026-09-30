//! ZTE U50S platform backend for the mainline runtime.
//!
//! The mainline `App` (cloud, OTA, WebShell, schedules, SMS forwarding, panel)
//! talks to the platform through a few seams: `state::collect`,
//! `control::execute` and the SMS source in `sms`. On the U50S those seams are
//! served here from the OEM `cfg` store and the local GoAhead API, using the
//! daemon's own local OEM session (see `u50_oem::Bridge::local_token`).
use crate::{
    control::Outcome,
    model::{DatadVersion, Snapshot},
    ota::Profile,
    u50::Collector,
    u50_oem::Bridge,
};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

pub struct Ctl {
    collector: Collector,
    oem: Bridge,
    last: Mutex<Option<Snapshot>>,
}

static CTL: OnceLock<Ctl> = OnceLock::new();

pub fn install(collector: Collector, oem: Bridge) {
    let _ = CTL.set(Ctl {
        collector,
        oem,
        last: Mutex::new(None),
    });
}

pub fn get() -> Option<&'static Ctl> {
    CTL.get()
}

/// Update behaviour of the running platform.
/// Controls the U50S runtime accepts: App-level ones (schedules, SMS
/// forwarding, cloud features, state sampling) plus the OEM-backed actions
/// mapped in `Ctl::execute`. Speed tests are intentionally not offered.
pub const CONTROLS: &[&str] = &[
    "wifi.status",
    "wifi.dual_band_status",
    "apn.list",
    "client.access",
    "state.refresh",
    "state.set_interval",
    "schedule.reboot.set",
    "schedule.task.put",
    "schedule.task.remove",
    "sms.forward.set",
    "sms.forward.test",
    "cloud.remote_features.set",
    "device.reboot",
    "device.poweroff",
    "cellular.connect",
    "cellular.disconnect",
    "cellular.set",
    "network.set_mode",
    "sim.set_slot",
    "sms.delete",
    "sms.mark_read",
    "sms.send_raw",
    "band.set_lte",
    "band.set_nr_sa",
    "band.set_nr_nsa",
    "cell.lock_lte",
    "cell.lock_nr",
    "cell.unlock_all",
    "traffic.set_limit",
    "traffic.set_clear_day",
    "client.block",
    "client.unblock",
    "client.kick",
    "client.rename",
    "lan.set",
    "lan.set_mtu",
    "wifi.set_module",
];

pub const READ_ACTIONS: &[&str] = &[
    "wifi.status",
    "wifi.dual_band_status",
    "apn.list",
    "client.access",
];

/// Verify the original WebUI admin password without creating or replacing
/// the daemon's OEM cookie session. The root-only cfg value is the same
/// uppercase SHA-256 used by the firmware's login challenge.
pub async fn verify_password(username: &str, password: &str) -> bool {
    if username != "admin" {
        return false;
    }
    let program = std::env::var("ZWRT_DATAD_U50_CFG_BIN").unwrap_or_else(|_| "/usr/bin/cfg".into());
    let Ok(raw) =
        crate::command::run(&program, ["get", "admin_Password"], Duration::from_secs(2)).await
    else {
        return false;
    };
    let raw = zeroize::Zeroizing::new(raw);
    let stored = String::from_utf8_lossy(&raw);
    let stored = stored.trim();
    if stored.len() != 64
        || !stored
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'A'..=b'F'))
    {
        return false;
    }
    use sha2::{Digest, Sha256};
    let first = zeroize::Zeroizing::new(format!("{:X}", Sha256::digest(password.as_bytes())));
    crate::auth::secure_eq(stored, first.as_str())
}

pub fn ota_profile() -> Profile {
    if CTL.get().is_some() {
        Profile::U50
    } else {
        Profile::ZWRT
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn text<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str)
}

fn integer(params: &Value, key: &str) -> Result<Option<i64>, String> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_i64()
            .map(Some)
            .ok_or_else(|| format!("{key} must be an integer")),
        Some(Value::String(s)) => s
            .parse()
            .map(Some)
            .map_err(|_| format!("{key} must be an integer")),
        Some(Value::Bool(b)) => Ok(Some(i64::from(*b))),
        _ => Err(format!("{key} must be an integer")),
    }
}

/// `1;2;3;` — the OEM list format for message ids. Accepts an array of
/// numbers/strings or a `;`/`,` separated string.
fn id_list(params: &Value, key: &str) -> Result<String, String> {
    let raw: Vec<String> = match params.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::Number(n) => Ok(n.to_string()),
                Value::String(s) => Ok(s.clone()),
                _ => Err(format!("{key} entries must be message ids")),
            })
            .collect::<Result<_, _>>()?,
        Some(Value::String(s)) => s
            .split([';', ','])
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect(),
        Some(Value::Number(n)) => vec![n.to_string()],
        _ => return Err(format!("{key} is required")),
    };
    if raw.is_empty() || raw.len() > 64 {
        return Err(format!("{key} must contain 1 to 64 message ids"));
    }
    if !raw
        .iter()
        .all(|id| !id.is_empty() && id.len() <= 12 && id.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(format!("{key} must contain numeric message ids"));
    }
    Ok(raw.iter().map(|id| format!("{id};")).collect())
}

/// A comma-separated band list of positive numbers up to `max` (`1,3,78`).
fn band_list(params: &Value, key: &str, max: u32) -> Result<Vec<u32>, String> {
    let values: Vec<u32> = match params.get(key) {
        Some(Value::String(raw)) if raw.trim().is_empty() => Vec::new(),
        Some(Value::String(raw)) => raw
            .split(',')
            .map(|part| {
                part.trim()
                    .parse::<u32>()
                    .map_err(|_| format!("{key} must contain band numbers"))
            })
            .collect::<Result<_, _>>()?,
        Some(Value::Array(raw)) => raw
            .iter()
            .map(|v| {
                v.as_u64()
                    .and_then(|v| u32::try_from(v).ok())
                    .ok_or_else(|| format!("{key} must contain integer band numbers"))
            })
            .collect::<Result<_, _>>()?,
        _ => {
            return Err(format!(
                "{key} is required as a comma-separated string or integer array"
            ));
        }
    };
    if values.len() > 64 || values.iter().any(|v| !(1..=max).contains(v)) {
        return Err(format!(
            "{key} must list up to 64 bands from 1 through {max}"
        ));
    }
    let mut bands = values;
    bands.sort_unstable();
    bands.dedup();
    Ok(bands)
}

/// Match the original U50 WebUI's minimal hex mask representation.
fn lte_mask(bands: &[u32]) -> String {
    let mask = bands
        .iter()
        .fold(0u64, |mask, band| mask | 1u64 << (band - 1));
    format!("0x{mask:x}")
}

fn network_mode(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_uppercase().as_str() {
        "4G_AND_5G" | "LTE_AND_5G" | "WL_AND_5G" | "AUTO" | "4G/5G" | "4G_5G" => Some("4G_AND_5G"),
        "ONLY_LTE" | "ONLY_4G" | "4G_ONLY" | "LTE" | "4G" => Some("Only_LTE"),
        "ONLY_5G" | "5G_ONLY" | "NR" | "5G" => Some("Only_5G"),
        _ => None,
    }
}

fn mask_value(text: &str) -> Option<u64> {
    u64::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok()
}

fn digits(params: &Value, key: &str, max: i64) -> Result<i64, String> {
    match integer(params, key)? {
        Some(value) if (0..=max).contains(&value) => Ok(value),
        _ => Err(format!("{key} must be between 0 and {max}")),
    }
}

/// Default NR subcarrier spacing (kHz) for a band, as the OEM cell-lock form
/// needs it: the FR1 TDD mid-band bands use 30 kHz, the rest 15 kHz.
fn nr_scs(band: i64) -> i64 {
    if matches!(band, 34 | 38 | 39 | 40 | 41 | 46 | 48 | 77 | 78 | 79) {
        30
    } else {
        15
    }
}

fn valid_mac(value: &str) -> bool {
    value.len() == 17
        && value.split(':').count() == 6
        && value
            .split(':')
            .all(|part| part.len() == 2 && part.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// `a;b;c` — the OEM access-list format; empty entries are dropped.
fn split_list(value: &str) -> Vec<String> {
    value
        .split(';')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

fn join_list(items: &[String]) -> String {
    items.iter().map(|item| format!("{item};")).collect()
}

/// A hostname the OEM list can carry: printable, no list separators.
fn clean_hostname(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control() && *c != ';' && *c != ',')
        .take(32)
        .collect()
}

fn outcome_from(result: Result<Value, String>) -> Outcome {
    match result {
        Ok(value) => Outcome::Ok(value),
        Err(error) => Outcome::Failed(error),
    }
}

impl Ctl {
    /// Same contract as `state::collect`: always a snapshot. A failed sample
    /// keeps the last good one (refreshed timestamp) instead of blanking data.
    pub async fn collect(&self) -> Snapshot {
        let mut fresh = match self.collector.snapshot().await {
            Ok(snapshot) => snapshot,
            Err(_) => {
                if let Some(previous) = self.last.lock().await.clone() {
                    let mut stale = previous;
                    stale.ts = now();
                    return stale;
                }
                Snapshot {
                    ts: now(),
                    datad: DatadVersion::default(),
                    fields: Map::new(),
                }
            }
        };
        if let Some(Value::Object(clients)) = self.clients_block().await {
            let entry = fresh
                .fields
                .entry("clients".to_owned())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Some(existing) = entry.as_object_mut() {
                existing.extend(clients);
            }
        }
        if let Some(sms) = crate::sms::snapshot().await {
            fresh.fields.insert("sms".into(), sms);
        }
        *self.last.lock().await = Some(fresh.clone());
        fresh
    }

    async fn read(&self, keys: &str) -> Result<Value, String> {
        self.oem.local_read(keys, &BTreeMap::new()).await
    }

    async fn write(&self, goform: &str, pairs: &[(&str, String)]) -> Result<Value, String> {
        let params: Map<String, Value> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), Value::String(value.clone())))
            .collect();
        self.oem.local_write(goform, &params).await
    }

    async fn field(&self, key: &str) -> Option<String> {
        if let Some(value) = self
            .read(key)
            .await
            .ok()
            .and_then(|v| v.get(key)?.as_str().map(str::to_owned))
            .filter(|v| !v.is_empty())
        {
            return Some(value);
        }
        let program =
            std::env::var("ZWRT_DATAD_U50_CFG_BIN").unwrap_or_else(|_| "/usr/bin/cfg".into());
        let raw = crate::command::run(&program, ["get", key], Duration::from_secs(2))
            .await
            .ok()?;
        let value = String::from_utf8(raw).ok()?.trim().to_owned();
        (!value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control))
            .then_some(value)
    }

    async fn wait_field(&self, key: &str, accept: impl Fn(&str) -> bool + Send + Sync) -> bool {
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                if self.field(key).await.is_some_and(|value| accept(&value)) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .unwrap_or(false)
    }

    /// Secretless configuration for the on-demand NMS panel. The complete
    /// AccessPoint reply includes a Wi-Fi key; only the reviewed view leaves
    /// this call, and no OEM write is made.
    pub async fn wifi_panel_config(&self) -> Result<Value, String> {
        let points = self
            .oem
            .local_read_single("queryAccessPointInfo", &BTreeMap::new())
            .await?;
        let steering = self.read("wifi_lbd_enable").await.unwrap_or(Value::Null);
        crate::u50_panel::wifi_config(&points, &steering)
    }

    /// OEM APN records contain username/password fields. Fetch a bounded
    /// catalog in batches allowed by the bridge, then discard those secrets
    /// while mapping onto the NMS's reviewed profile fields.
    pub async fn apn_panel_config(&self) -> Result<Value, String> {
        let mut raw = Map::new();
        for prefix in ["APN_config", "ipv6_APN_config"] {
            let keys = (0..20)
                .map(|n| format!("{prefix}{n}"))
                .collect::<Vec<_>>()
                .join(",");
            let reply = self.read(&keys).await?;
            raw.extend(reply.as_object().ok_or("invalid OEM APN reply")?.clone());
        }
        let reply = self.read("apn_interface_version,apn_mode,apn_auto_config,ipv6_apn_auto_config,m_profile_name,profile_name,profile_name_ui,wan_apn,wan_apn_ui,ppp_auth_mode,ppp_auth_mode_ui,pdp_type,pdp_type_ui").await?;
        raw.extend(reply.as_object().ok_or("invalid OEM APN reply")?.clone());
        crate::u50_panel::apn_config(&Value::Object(raw))
    }

    /// Original-firmware counterparts of the mainline local read actions.
    /// Wi-Fi keys and APN credentials remain absent even on the local API.
    pub async fn read_model(&self, action: &str, params: &Value) -> Outcome {
        if !params.as_object().is_some_and(Map::is_empty) {
            return Outcome::Invalid("this read action accepts no parameters".into());
        }
        let result = match action {
            "wifi.status" => self.wifi_panel_config().await.map(|config| {
                let mut sections = Map::new();
                for (band, section) in [("2g", "main_2g"), ("5g", "main_5g")] {
                    let row = &config["bands"][band];
                    if row["supported"] == true {
                        sections.insert(section.into(), json!({
                            "ssid":row["ssid"],"encryption":row["encryption"],
                            "disabled":row["enabled"].as_bool().map(|v|if v {"0"} else {"1"}),
                            "hidden":row["hidden"].as_bool().map(|v|if v {"1"} else {"0"}),
                            "pmf":row["pmf"],"maxassoc":row["maxassoc"],"writable":false
                        }));
                    }
                }
                Value::Object(sections)
            }),
            "wifi.dual_band_status" => self.read("wifi_lbd_enable").await.and_then(|raw| {
                let enabled = match raw.get("wifi_lbd_enable").and_then(Value::as_str) {
                    Some("0") => false,Some("1") => true,_ => return Err("OEM band steering state unavailable".into()),
                };
                Ok(json!({"supported":true,"enabled":enabled,"writable":false,
                    "WiFiDualBandSupported":"1","WiFiDualBandEnabled":if enabled {"1"} else {"0"},
                    "BandSteeringSwitch":if enabled {"1"} else {"0"}}))
            }),
            "apn.list" => self.apn_panel_config().await.map(|config| {
                let profiles = |key: &str| {
                    let items = config[key].as_array().into_iter().flatten().map(|row|json!({
                        "profileId":row["id"],"profilename":row["name"],"wanapn":row["apn"],
                        "pdpType":row["pdp_type"],"pppAuthMode":row["auth_mode"],"isEnable":row["enabled"]
                    })).collect::<Vec<_>>();
                    json!({"apnListArray":items})
                };
                json!({"mode":{"apn_mode":config["mode"]},"automatic":profiles("automatic"),
                    "manual":profiles("manual"),"enabled":{"profileId":config["enabled_id"]},"writable":false})
            }),
            "client.access" => self.clients_block().await.ok_or_else(||"OEM client list unavailable".into()),
            _ => return Outcome::NotHandled,
        };
        outcome_from(result)
    }

    // ---- SMS source for `crate::sms` -------------------------------------

    /// Counts used for the unread badge and change detection.
    pub async fn sms_capacity(&self) -> Result<Value, String> {
        self.read("sms_unread_num,sms_dev_unread_num,sms_sim_unread_num,sms_nv_num_total,sms_sim_num_total")
            .await
    }

    /// One page of the message store (`mem_store`: 1 = device NV, 0 = SIM).
    pub async fn sms_page(
        &self,
        page: usize,
        per_page: usize,
        store: i64,
    ) -> Result<Value, String> {
        let params = BTreeMap::from([
            ("page".to_owned(), page.to_string()),
            ("data_per_page".to_owned(), per_page.to_string()),
            ("mem_store".to_owned(), store.to_string()),
            ("tags".to_owned(), "10".to_owned()),
            ("order_by".to_owned(), "order by id desc".to_owned()),
        ]);
        let reply = self
            .oem
            .local_read_single("sms_data_total", &params)
            .await?;
        // An empty store answers with an empty string instead of a list.
        if reply.get("messages").is_some_and(Value::is_array) {
            return Ok(reply);
        }
        match reply.get("sms_data_total") {
            Some(Value::String(s)) if s.is_empty() => Ok(json!({"messages":[]})),
            Some(Value::Array(_)) => Ok(json!({"messages":reply["sms_data_total"]})),
            Some(inner) if inner.get("messages").is_some_and(Value::is_array) => Ok(inner.clone()),
            _ => Err("invalid OEM SMS list".into()),
        }
    }

    /// SEND_SMS, then wait for the modem's verdict. Never retried: a retry
    /// could deliver a real message twice.
    pub async fn sms_send(
        &self,
        sender: &str,
        number: &str,
        message_hex: &str,
        sms_time: &str,
    ) -> Result<Value, (bool, String)> {
        if matches!(sender, "sim1" | "sim2") {
            let slot = if sender == "sim1" { "1" } else { "2" };
            if self.field("simcard_active_slot").await.as_deref() != Some(slot) {
                self.write(
                    "SWITCH_SIMCARD_SLOT",
                    &[("simcard_active_slot", slot.into())],
                )
                .await
                .map_err(|e| (false, e))?;
                let mut active = false;
                for _ in 0..30 {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    if self.field("simcard_active_slot").await.as_deref() == Some(slot) {
                        active = true;
                        break;
                    }
                }
                if !active {
                    return Err((false, format!("SIM slot {slot} did not become active")));
                }
            }
        } else if !matches!(sender, "host" | "x75") {
            return Err((true, "invalid SMS sender".into()));
        }
        self.write(
            "SEND_SMS",
            &[
                ("Number", number.to_owned()),
                ("sms_time", sms_time.to_owned()),
                ("MessageBody", message_hex.to_ascii_uppercase()),
                ("ID", "-1".into()),
                ("encode_type", "UNICODE".into()),
            ],
        )
        .await
        .map_err(|e| (false, e))?;
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let params = BTreeMap::from([("sms_cmd".to_owned(), "4".to_owned())]);
            let Ok(status) = self
                .oem
                .local_read_single("sms_cmd_status_info", &params)
                .await
            else {
                continue;
            };
            match status.get("sms_cmd_status_result").and_then(Value::as_str) {
                Some("3") => return Ok(json!({"sender":sender,"status":3})),
                Some("2") => return Err((false, "OEM SMS command failed".into())),
                _ => {}
            }
        }
        Err((false, "OEM SMS command timed out".into()))
    }

    // ---- control ---------------------------------------------------------

    pub async fn execute(&self, action: &str, params: &Value) -> Outcome {
        match action {
            "device.reboot" => outcome_from(self.write("REBOOT_DEVICE", &[]).await),
            "device.poweroff" => outcome_from(self.write("SHUTDOWN_DEVICE", &[]).await),
            "cellular.connect" => outcome_from(self.write("CONNECT_NETWORK", &[]).await),
            "cellular.disconnect" => outcome_from(self.write("DISCONNECT_NETWORK", &[]).await),
            "cellular.set" => self.cellular_set(params).await,
            "network.set_mode" => self.network_set_mode(params).await,
            "sim.set_slot" => self.sim_set_slot(params).await,
            "sms.delete" => {
                let ids = match id_list(params, "ids") {
                    Ok(ids) => ids,
                    Err(error) => return Outcome::Invalid(error),
                };
                let outcome = outcome_from(self.write("DELETE_SMS", &[("msg_id", ids)]).await);
                if matches!(outcome, Outcome::Ok(_)) {
                    crate::sms::invalidate();
                }
                outcome
            }
            "sms.mark_read" => {
                let ids = match id_list(params, "ids") {
                    Ok(ids) => ids,
                    Err(error) => return Outcome::Invalid(error),
                };
                let tag = match integer(params, "tag") {
                    Ok(Some(tag @ (0 | 1))) => tag,
                    Ok(None) => 0,
                    _ => return Outcome::Invalid("tag must be 0 or 1".into()),
                };
                let outcome = outcome_from(
                    self.write("SET_MSG_READ", &[("msg_id", ids), ("tag", tag.to_string())])
                        .await,
                );
                if matches!(outcome, Outcome::Ok(_)) {
                    crate::sms::invalidate();
                }
                outcome
            }
            "sms.send_raw" => match crate::sms::send(params).await {
                Ok(value) => Outcome::Ok(value),
                Err((true, error)) => Outcome::Invalid(error),
                Err((false, error)) => Outcome::Failed(error),
            },
            "band.set_lte" => self.band_set_lte(params).await,
            "band.set_nr_sa" => self.band_set_nr(params, false).await,
            "band.set_nr_nsa" => self.band_set_nr(params, true).await,
            "cell.lock_lte" => self.cell_lock_lte(params).await,
            "cell.lock_nr" => self.cell_lock_nr(params).await,
            "cell.unlock_all" => self.cell_unlock_all().await,
            "traffic.set_limit" => self.traffic_set_limit(params).await,
            "traffic.set_clear_day" => self.traffic_set_clear_day(params).await,
            "client.block" => self.client_access(params, true).await,
            "client.unblock" => self.client_access(params, false).await,
            "client.kick" => self.client_kick(params).await,
            "client.rename" => self.client_rename(params).await,
            "lan.set" => self.lan_set(params).await,
            "lan.set_mtu" => self.lan_set_mtu(params).await,
            "wifi.set_module" => self.wifi_set_module(params).await,
            _ => Outcome::NotHandled,
        }
    }

    async fn band_set_lte(&self, params: &Value) -> Outcome {
        let bands = match band_list(params, "bands", 64) {
            Ok(bands) => bands,
            Err(error) => return Outcome::Invalid(error),
        };
        let bands = if bands.is_empty() {
            match self
                .field("lte_band_1_64_factory")
                .await
                .as_deref()
                .and_then(mask_value)
                .filter(|v| *v != 0)
            {
                Some(mask) => (1..=64)
                    .filter(|band| mask & (1u64 << (band - 1)) != 0)
                    .collect(),
                None => return Outcome::Failed("factory LTE bands unavailable".into()),
            }
        } else {
            bands
        };
        let mask = lte_mask(&bands);
        let result = if self.collector.model == crate::u50::Model::U50S {
            self.write("SET_NETWORK_BAND_LOCK", &[("lte_band_lock", mask.clone())])
                .await
        } else {
            self.write(
                "BAND_SELECT",
                &[
                    ("is_gw_band", "0".into()),
                    ("gw_band_mask", "0".into()),
                    ("is_lte_band", "1".into()),
                    ("lte_band_mask", mask.clone()),
                ],
            )
            .await
        };
        if let Err(error) = result {
            return Outcome::Failed(error);
        }
        let verified = self
            .wait_field("lte_band_lock", |value| {
                mask_value(value) == mask_value(&mask)
            })
            .await;
        if !verified {
            return Outcome::Failed(
                "OEM LTE band write was accepted but readback did not match".into(),
            );
        }
        Outcome::Ok(json!({"result":"success","bands":bands,"mask":mask,"verified":true}))
    }

    async fn band_set_nr(&self, params: &Value, nsa: bool) -> Outcome {
        let bands = match band_list(params, "bands", 512) {
            Ok(bands) => bands,
            Err(error) => return Outcome::Invalid(error),
        };
        let bands = if bands.is_empty() {
            let key = if nsa {
                "nr5g_nsa_band_factory"
            } else {
                "nr5g_sa_band_factory"
            };
            match self
                .field(key)
                .await
                .and_then(|value| band_list(&json!({"bands":value}), "bands", 512).ok())
                .filter(|v| !v.is_empty())
            {
                Some(bands) => bands,
                None => return Outcome::Failed("factory NR bands unavailable".into()),
            }
        } else {
            bands
        };
        let csv = bands
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        if let Err(error) = self
            .write(
                "WAN_PERFORM_NR5G_SANSA_BAND_LOCK",
                &[
                    ("nr5g_band_mask", csv.clone()),
                    ("type", if nsa { "1" } else { "0" }.into()),
                ],
            )
            .await
        {
            return Outcome::Failed(error);
        }
        let key = if nsa {
            "nr5g_nsa_band_lock"
        } else {
            "nr5g_sa_band_lock"
        };
        let verified = self
            .wait_field(key, |value| {
                band_list(&json!({"bands":value}), "bands", 512)
                    .ok()
                    .as_deref()
                    == Some(bands.as_slice())
            })
            .await;
        if !verified {
            return Outcome::Failed(
                "OEM NR band write was accepted but readback did not match".into(),
            );
        }
        Outcome::Ok(json!({"result":"success","bands":bands,"verified":true}))
    }

    async fn cell_lock_lte(&self, params: &Value) -> Outcome {
        let (pci, earfcn) = match (
            digits(params, "pci", 1007),
            digits(params, "earfcn", 262_143),
        ) {
            (Ok(pci), Ok(earfcn)) => (pci, earfcn),
            (Err(error), _) | (_, Err(error)) => return Outcome::Invalid(error),
        };
        outcome_from(
            self.write(
                "LTE_LOCK_CELL_SET",
                &[
                    ("lte_pci_lock", pci.to_string()),
                    ("lte_earfcn_lock", earfcn.to_string()),
                ],
            )
            .await,
        )
    }

    async fn cell_lock_nr(&self, params: &Value) -> Outcome {
        let (pci, arfcn, band) = match (
            digits(params, "pci", 1007),
            digits(params, "arfcn", 3_279_165),
            digits(params, "band", 512),
        ) {
            (Ok(pci), Ok(arfcn), Ok(band)) if band > 0 => (pci, arfcn, band),
            (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                return Outcome::Invalid(error);
            }
            _ => return Outcome::Invalid("band must be between 1 and 512".into()),
        };
        let scs = match integer(params, "scs") {
            Ok(Some(scs)) if matches!(scs, 15 | 30 | 60 | 120) => scs,
            Ok(None) => nr_scs(band),
            _ => return Outcome::Invalid("scs must be 15, 30, 60 or 120".into()),
        };
        outcome_from(
            self.write(
                "NR5G_LOCK_CELL_SET",
                &[("nr5g_cell_lock", format!("{pci},{arfcn},{band},{scs}"))],
            )
            .await,
        )
    }

    /// LTE: zero PCI/EARFCN; NR: the WebUI's `1,1,1,1` unlock value.
    async fn cell_unlock_all(&self) -> Outcome {
        let lte = self
            .write(
                "LTE_LOCK_CELL_SET",
                &[
                    ("lte_pci_lock", "0".into()),
                    ("lte_earfcn_lock", "0".into()),
                ],
            )
            .await;
        let nr = self
            .write(
                "NR5G_LOCK_CELL_SET",
                &[("nr5g_cell_lock", "1,1,1,1".into())],
            )
            .await;
        match (lte, nr) {
            (Ok(_), Ok(_)) => Outcome::Ok(json!({"lte":true,"nr":true})),
            (Err(error), _) | (_, Err(error)) => Outcome::Failed(error),
        }
    }

    /// Access control: `AclMode` 2 = blacklist. Mac and name lists are
    /// index-aligned, `;`-separated.
    async fn acl(&self) -> Result<(String, Vec<String>, Vec<String>, String, String), String> {
        let reply = self
            .oem
            .local_read_single("queryDeviceAccessControlList", &BTreeMap::new())
            .await?;
        let get = |key: &str| {
            reply
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned()
        };
        Ok((
            get("AclMode"),
            split_list(&get("BlackMacList")),
            split_list(&get("BlackNameList")),
            get("WhiteMacList"),
            get("WhiteNameList"),
        ))
    }

    async fn acl_write(
        &self,
        macs: &[String],
        names: &[String],
        white_macs: &str,
        white_names: &str,
    ) -> Result<Value, String> {
        self.write(
            "setDeviceAccessControlList",
            &[
                ("AclMode", "2".into()),
                ("WhiteMacList", white_macs.to_owned()),
                ("BlackMacList", join_list(macs)),
                ("WhiteNameList", white_names.to_owned()),
                ("BlackNameList", join_list(names)),
            ],
        )
        .await
    }

    async fn client_access(&self, params: &Value, block: bool) -> Outcome {
        let mac = match text(params, "mac") {
            Some(mac) if valid_mac(mac) => mac.to_ascii_lowercase(),
            _ => return Outcome::Invalid("invalid mac address".into()),
        };
        let (mode, mut macs, mut names, white_macs, white_names) = match self.acl().await {
            Ok(acl) => acl,
            Err(error) => return Outcome::Failed(error),
        };
        // Keep the name list aligned with the mac list.
        names.resize(macs.len(), String::new());
        if mode == "1" && block {
            return Outcome::Failed("the OEM access list is in whitelist mode".into());
        }
        let position = macs.iter().position(|m| m.eq_ignore_ascii_case(&mac));
        match (block, position) {
            (true, None) => {
                let name = self.station_name(&mac).await.unwrap_or_default();
                macs.push(mac.clone());
                names.push(name);
            }
            (false, Some(index)) => {
                macs.remove(index);
                names.remove(index);
            }
            _ => {}
        }
        if let Err(error) = self
            .acl_write(&macs, &names, &white_macs, &white_names)
            .await
        {
            return Outcome::Failed(error);
        }
        let verified = match self.acl().await {
            Ok((_, after, _, _, _)) => after.iter().any(|m| m.eq_ignore_ascii_case(&mac)) == block,
            Err(_) => false,
        };
        Outcome::Ok(json!({"mac":mac,"blocked":block,"verified":verified}))
    }

    /// Disconnect a Wi-Fi client: blocking removes it from the AP at once and
    /// it is allowed back immediately unless it was blocked before.
    async fn client_kick(&self, params: &Value) -> Outcome {
        let raw = match text(params, "macs") {
            Some(raw) => raw,
            None => return Outcome::Invalid("macs is required".into()),
        };
        let macs: Vec<String> = raw
            .split([',', ';'])
            .filter(|m| !m.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        if macs.is_empty() || macs.len() > 8 || !macs.iter().all(|m| valid_mac(m)) {
            return Outcome::Invalid("macs must list 1 to 8 mac addresses".into());
        }
        let Ok((_, blocked, _, _, _)) = self.acl().await else {
            return Outcome::Failed("OEM access list unavailable".into());
        };
        let mut kicked = Vec::new();
        for mac in macs {
            if blocked.iter().any(|m| m.eq_ignore_ascii_case(&mac)) {
                continue;
            }
            if !matches!(
                self.client_access(&json!({"mac":mac}), true).await,
                Outcome::Ok(_)
            ) {
                return Outcome::Failed("could not disconnect the client".into());
            }
            tokio::time::sleep(Duration::from_millis(1500)).await;
            if !matches!(
                self.client_access(&json!({"mac":mac}), false).await,
                Outcome::Ok(_)
            ) {
                return Outcome::Failed(
                    "client was disconnected but could not be re-allowed".into(),
                );
            }
            kicked.push(mac);
        }
        Outcome::Ok(json!({"kicked":kicked}))
    }

    async fn client_rename(&self, params: &Value) -> Outcome {
        let mac = match text(params, "mac") {
            Some(mac) if valid_mac(mac) => mac.to_ascii_lowercase(),
            _ => return Outcome::Invalid("invalid mac address".into()),
        };
        let hostname = match text(params, "hostname") {
            Some(name) if !name.is_empty() && name.len() <= 32 && clean_hostname(name) == name => {
                name
            }
            _ => return Outcome::Invalid("hostname must be 1 to 32 printable characters".into()),
        };
        outcome_from(
            self.write(
                "EDIT_HOSTNAME",
                &[("mac", mac.clone()), ("hostname", hostname.to_owned())],
            )
            .await
            .map(|_| json!({"mac":mac,"hostname":hostname})),
        )
    }

    async fn station_name(&self, mac: &str) -> Option<String> {
        let stations = self.stations().await.ok()?;
        stations
            .into_iter()
            .find(|(m, _, _, _)| m.eq_ignore_ascii_case(mac))
            .map(|(_, name, _, _)| clean_hostname(&name))
    }

    /// Connected devices: `(mac, hostname, ip, wired)`. Wi-Fi first.
    async fn stations(&self) -> Result<Vec<(String, String, String, bool)>, String> {
        let mut out = Vec::new();
        for (cmd, wired) in [("station_list", false), ("lan_station_list", true)] {
            let reply = match self.oem.local_read_single(cmd, &BTreeMap::new()).await {
                Ok(reply) => reply,
                Err(error) if wired => {
                    let _ = error;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let rows = reply
                .get(cmd)
                .or_else(|| reply.get("station_list"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for row in rows.iter().take(32) {
                let field = |k: &str| row.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
                let mac = field("mac_addr").to_ascii_lowercase();
                if valid_mac(&mac) {
                    out.push((mac, field("hostname"), field("ip_addr"), wired));
                }
            }
        }
        Ok(out)
    }

    /// `clients` block from the firmware's own device lists and blacklist.
    async fn clients_block(&self) -> Option<Value> {
        let stations = self.stations().await.ok()?;
        let (_, blocked, _, _, _) = self.acl().await.ok()?;
        let wifi = stations.iter().filter(|s| !s.3).count();
        let lan = stations.len() - wifi;
        let list: Vec<Value> = stations
            .iter()
            .map(|(mac, name, ip, _)| json!({"name":clean_hostname(name),"ip":ip,"mac":mac}))
            .collect();
        Some(json!({"total":stations.len(),"wifi":wifi,"lan":lan,"list":list,"blocked":blocked}))
    }

    /// The OEM form always posts the whole data-limit block, so the untouched
    /// fields are read back from the firmware and re-sent unchanged.
    async fn data_limit_write(
        &self,
        edit: impl FnOnce(&mut BTreeMap<String, String>),
    ) -> Result<Value, String> {
        let current = self
            .read(
                "data_volume_limit_switch,data_volume_limit_unit,data_volume_limit_size,data_volume_alert_percent,wan_auto_clear_flow_data_switch,traffic_clear_date,flux_limited_disconnect",
            )
            .await?;
        let mut form: BTreeMap<String, String> = current
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
            .collect();
        form.entry("wan_auto_clear_flow_data_switch".into())
            .or_insert_with(|| "on".into());
        form.entry("traffic_clear_date".into())
            .or_insert_with(|| "1".into());
        form.entry("flux_limited_disconnect".into())
            .or_insert_with(|| "off".into());
        form.entry("data_volume_limit_switch".into())
            .or_insert_with(|| "0".into());
        edit(&mut form);
        let mut pairs: Vec<(&str, String)> = vec![
            (
                "wan_auto_clear_flow_data_switch",
                form["wan_auto_clear_flow_data_switch"].clone(),
            ),
            ("traffic_clear_date", form["traffic_clear_date"].clone()),
            (
                "flux_limited_disconnect",
                form["flux_limited_disconnect"].clone(),
            ),
            (
                "data_volume_limit_switch",
                form["data_volume_limit_switch"].clone(),
            ),
            ("notify_deviceui_enable", "0".into()),
        ];
        if form["data_volume_limit_switch"] == "1" {
            for key in [
                "data_volume_limit_unit",
                "data_volume_limit_size",
                "data_volume_alert_percent",
            ] {
                pairs.push((key, form.get(key).cloned().unwrap_or_default()));
            }
        }
        self.write("DATA_LIMIT_SETTING", &pairs).await?;
        self.read(
            "data_volume_limit_switch,data_volume_limit_unit,data_volume_limit_size,data_volume_alert_percent,wan_auto_clear_flow_data_switch,traffic_clear_date",
        )
        .await
    }

    async fn traffic_set_limit(&self, params: &Value) -> Outcome {
        let enabled = match integer(params, "enabled") {
            Ok(Some(v @ (0 | 1))) => v,
            _ => return Outcome::Invalid("enabled must be 0 or 1".into()),
        };
        let mut unit = "data";
        let mut size = String::new();
        let mut ratio = String::new();
        if enabled == 1 {
            unit = match integer(params, "type") {
                Ok(Some(1)) => "data",
                Ok(Some(2)) => "time",
                _ => return Outcome::Invalid("type must be 1 (data) or 2 (time)".into()),
            };
            size = match text(params, "value") {
                Some(v)
                    if !v.is_empty()
                        && v.len() <= 15
                        && v.bytes().all(|b| b.is_ascii_digit())
                        && v != "0" =>
                {
                    v.to_owned()
                }
                _ => match integer(params, "value") {
                    Ok(Some(v)) if v > 0 => v.to_string(),
                    _ => return Outcome::Invalid("value must be a positive number".into()),
                },
            };
            ratio = match integer(params, "ratio") {
                Ok(Some(v)) if (0..=100).contains(&v) => v.to_string(),
                Ok(None) => "80".into(),
                _ => return Outcome::Invalid("ratio must be 0 through 100".into()),
            };
        }
        let result = self
            .data_limit_write(|form| {
                form.insert("data_volume_limit_switch".into(), enabled.to_string());
                if enabled == 1 {
                    form.insert("data_volume_limit_unit".into(), unit.into());
                    form.insert("data_volume_limit_size".into(), size.clone());
                    form.insert("data_volume_alert_percent".into(), ratio.clone());
                }
            })
            .await;
        match result {
            Ok(after) => {
                let on = after
                    .get("data_volume_limit_switch")
                    .and_then(Value::as_str)
                    == Some("1");
                let verified = on == (enabled == 1)
                    && (enabled == 0
                        || after.get("data_volume_limit_size").and_then(Value::as_str)
                            == Some(size.as_str()));
                Outcome::Ok(json!({"enabled":enabled,"verified":verified}))
            }
            Err(error) => Outcome::Failed(error),
        }
    }

    async fn traffic_set_clear_day(&self, params: &Value) -> Outcome {
        let day = match integer(params, "day") {
            Ok(Some(day)) if (1..=31).contains(&day) => day,
            _ => return Outcome::Invalid("day must be 1 through 31".into()),
        };
        let enabled = match integer(params, "enabled") {
            Ok(Some(v @ (0 | 1))) => v,
            Ok(None) => 1,
            _ => return Outcome::Invalid("enabled must be 0 or 1".into()),
        };
        let wanted = if enabled == 1 { "on" } else { "off" };
        let result = self
            .data_limit_write(|form| {
                form.insert("traffic_clear_date".into(), day.to_string());
                form.insert("wan_auto_clear_flow_data_switch".into(), wanted.into());
            })
            .await;
        match result {
            Ok(after) => Outcome::Ok(json!({"day":day,"enabled":enabled,"verified":
                after.get("traffic_clear_date").and_then(Value::as_str) == Some(day.to_string().as_str())
                && after.get("wan_auto_clear_flow_data_switch").and_then(Value::as_str) == Some(wanted)})),
            Err(error) => Outcome::Failed(error),
        }
    }

    /// DHCP server settings. The LAN address and netmask are fixed: the
    /// firmware's local GoAhead only answers to `192.168.0.1`, which is also
    /// how this daemon reaches its OEM channel.
    async fn lan_set(&self, params: &Value) -> Outcome {
        let current = |key: &'static str| async move { self.field(key).await.unwrap_or_default() };
        let ip = current("lan_ipaddr").await;
        let netmask = current("lan_netmask").await;
        if ip.is_empty() || netmask.is_empty() {
            return Outcome::Failed("current LAN settings unavailable".into());
        }
        if text(params, "ip").is_some_and(|v| v != ip)
            || text(params, "netmask").is_some_and(|v| v != netmask)
        {
            return Outcome::Invalid("the U50S LAN address and netmask cannot be changed".into());
        }
        let disabled = match integer(params, "dhcp_disabled") {
            Ok(Some(v @ (0 | 1))) => Some(v),
            Ok(None) => None,
            _ => return Outcome::Invalid("dhcp_disabled must be 0 or 1".into()),
        };
        let server = match disabled {
            Some(v) => v == 0,
            None => current("dhcpEnabled").await == "1",
        };
        let addr = |key: &str, fallback: String| -> Result<String, String> {
            let value = text(params, key).map(str::to_owned).unwrap_or(fallback);
            let parsed: std::net::Ipv4Addr = value
                .parse()
                .map_err(|_| format!("{key} must be an IPv4 address"))?;
            let (a, n, m) = (
                u32::from(parsed),
                u32::from(
                    ip.parse::<std::net::Ipv4Addr>()
                        .map_err(|_| "bad LAN address")?,
                ),
                u32::from(
                    netmask
                        .parse::<std::net::Ipv4Addr>()
                        .map_err(|_| "bad LAN netmask")?,
                ),
            );
            if a & m != n & m || a == n {
                return Err(format!("{key} must be another address in the LAN subnet"));
            }
            Ok(value)
        };
        let mut pairs = vec![
            ("lanIp", ip.clone()),
            ("lanNetmask", netmask.clone()),
            (
                "lanDhcpType",
                if server { "SERVER" } else { "DISABLE" }.to_owned(),
            ),
        ];
        if server {
            let start = match addr("dhcp_start", current("dhcpStart").await) {
                Ok(v) => v,
                Err(e) => return Outcome::Invalid(e),
            };
            let end = match addr("dhcp_end", current("dhcpEnd").await) {
                Ok(v) => v,
                Err(e) => return Outcome::Invalid(e),
            };
            if u32::from(
                start
                    .parse::<std::net::Ipv4Addr>()
                    .unwrap_or(std::net::Ipv4Addr::UNSPECIFIED),
            ) > u32::from(
                end.parse::<std::net::Ipv4Addr>()
                    .unwrap_or(std::net::Ipv4Addr::BROADCAST),
            ) {
                return Outcome::Invalid("dhcp_start must not be above dhcp_end".into());
            }
            let hours = match integer(params, "lease_seconds") {
                Ok(Some(seconds))
                    if seconds >= 3600 && seconds % 3600 == 0 && seconds / 3600 <= 720 =>
                {
                    seconds / 3600
                }
                Ok(Some(_)) => {
                    return Outcome::Invalid(
                        "lease_seconds must be whole hours, 3600 to 2592000".into(),
                    );
                }
                Ok(None) => current("dhcpLease_hour").await.parse().unwrap_or(24),
                Err(e) => return Outcome::Invalid(e),
            };
            pairs.push(("dhcpStart", start));
            pairs.push(("dhcpEnd", end));
            pairs.push(("dhcpLease", hours.to_string()));
        }
        pairs.push(("dhcp_reboot_flag", "1".into()));
        pairs.push(("mac_ip_reset", "0".into()));
        let wanted_start = pairs
            .iter()
            .find(|(k, _)| *k == "dhcpStart")
            .map(|(_, v)| v.clone());
        if let Err(error) = self.write("DHCP_SETTING", &pairs).await {
            return Outcome::Failed(error);
        }
        let enabled_now = self.field("dhcpEnabled").await;
        let start_now = self.field("dhcpStart").await;
        let verified = enabled_now.as_deref() == Some(if server { "1" } else { "0" })
            && wanted_start.is_none_or(|w| start_now.as_deref() == Some(w.as_str()));
        Outcome::Ok(json!({"dhcp_enabled":server,"verified":verified}))
    }

    async fn lan_set_mtu(&self, params: &Value) -> Outcome {
        let mtu = match integer(params, "mtu") {
            Ok(Some(v)) if (576..=1500).contains(&v) => v,
            _ => return Outcome::Invalid("mtu must be 576 through 1500".into()),
        };
        let mss = mtu - 40;
        if let Err(error) = self
            .write(
                "SET_DEVICE_MTU",
                &[("mtu", mtu.to_string()), ("tcp_mss", mss.to_string())],
            )
            .await
        {
            return Outcome::Failed(error);
        }
        let verified = self.field("mtu").await.as_deref() == Some(mtu.to_string().as_str());
        Outcome::Ok(json!({"mtu":mtu,"tcp_mss":mss,"verified":verified}))
    }

    /// Master Wi-Fi switch (`SET_WIFI_INFO`); the state is read back from
    /// `wifi_onoff_state`.
    async fn wifi_set_module(&self, params: &Value) -> Outcome {
        let enabled = match integer(params, "enabled") {
            Ok(Some(v @ (0 | 1))) => v,
            _ => return Outcome::Invalid("enabled must be 0 or 1".into()),
        };
        if let Err(error) = self
            .write("SET_WIFI_INFO", &[("wifiEnabled", enabled.to_string())])
            .await
        {
            return Outcome::Failed(error);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        let verified =
            self.field("wifi_onoff_state").await.as_deref() == Some(enabled.to_string().as_str());
        Outcome::Ok(json!({"enabled":enabled,"verified":verified}))
    }

    /// `roaming` maps to the OEM connection mode; `enabled` to connect/disconnect.
    /// The current dial mode is preserved when only roaming changes.
    async fn cellular_set(&self, params: &Value) -> Outcome {
        let enabled = match integer(params, "enabled") {
            Ok(Some(v)) if v == 0 || v == 1 => Some(v),
            Ok(None) => None,
            _ => return Outcome::Invalid("enabled must be 0 or 1".into()),
        };
        let roaming = match integer(params, "roaming") {
            Ok(Some(v)) if v == 0 || v == 1 => Some(v),
            Ok(None) => None,
            _ => return Outcome::Invalid("roaming must be 0 or 1".into()),
        };
        let connect_mode = match text(params, "connect_mode") {
            None => None,
            Some("auto" | "auto_dial") => Some("auto_dial"),
            Some("manual" | "manual_dial") => Some("manual_dial"),
            Some(_) => return Outcome::Invalid("connect_mode must be auto or manual".into()),
        };
        if enabled.is_none() && roaming.is_none() && connect_mode.is_none() {
            return Outcome::Invalid("no cellular fields supplied".into());
        }
        let mut result = json!({});
        if roaming.is_some() || connect_mode.is_some() {
            let current = match self.read("dial_mode,roam_setting_option").await {
                Ok(fields) => fields,
                Err(error) => return Outcome::Failed(error),
            };
            let dial = connect_mode.map(str::to_owned).unwrap_or_else(|| {
                match current.get("dial_mode").and_then(Value::as_str) {
                    Some("manual_dial") => "manual_dial".into(),
                    _ => "auto_dial".into(),
                }
            });
            let roam = match roaming {
                Some(1) => "on",
                Some(_) => "off",
                None => match current.get("roam_setting_option").and_then(Value::as_str) {
                    Some("on") => "on",
                    _ => "off",
                },
            };
            if let Err(error) = self
                .write(
                    "SET_CONNECTION_MODE",
                    &[
                        ("ConnectionMode", dial),
                        ("roam_setting_option", roam.into()),
                    ],
                )
                .await
            {
                return Outcome::Failed(error);
            }
            // Read back what the firmware accepted.
            let readback = self.field("roam_setting_option").await;
            result["roaming"] = json!(readback.as_deref() == Some("on"));
            result["verified"] = json!(readback.as_deref() == Some(roam));
        }
        match enabled {
            Some(1) => {
                if let Err(error) = self.write("CONNECT_NETWORK", &[]).await {
                    return Outcome::Failed(error);
                }
                result["enabled"] = json!(true);
            }
            Some(_) => {
                if let Err(error) = self.write("DISCONNECT_NETWORK", &[]).await {
                    return Outcome::Failed(error);
                }
                result["enabled"] = json!(false);
            }
            None => {}
        }
        Outcome::Ok(result)
    }

    async fn network_set_mode(&self, params: &Value) -> Outcome {
        // U50S bearer preferences (MU5002 WebUI): 4G+5G, 5G only, 4G only.
        let mode = match text(params, "mode").and_then(network_mode) {
            Some(mode) => mode,
            None => {
                return Outcome::Invalid(
                    "mode must be 4G_AND_5G/auto, Only_LTE/4G or Only_5G/5G".into(),
                );
            }
        };
        if let Err(error) = self
            .write(
                "SET_BEARER_PREFERENCE",
                &[("BearerPreference", mode.into())],
            )
            .await
        {
            return Outcome::Failed(error);
        }
        let verified = self
            .wait_field("net_select", |value| network_mode(value) == Some(mode))
            .await;
        if !verified {
            return Outcome::Failed(
                "OEM network mode write was accepted but readback did not match".into(),
            );
        }
        Outcome::Ok(json!({"result":"success","mode":mode,"verified":true}))
    }

    async fn sim_set_slot(&self, params: &Value) -> Outcome {
        let slot = match integer(params, "slot") {
            Ok(Some(slot @ (1 | 2))) => slot,
            _ => return Outcome::Invalid("slot must be 1 or 2".into()),
        };
        if let Err(error) = self
            .write(
                "SWITCH_SIMCARD_SLOT",
                &[("simcard_active_slot", slot.to_string())],
            )
            .await
        {
            return Outcome::Failed(error);
        }
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if self.field("simcard_active_slot").await.as_deref() == Some(&slot.to_string()) {
                return Outcome::Ok(json!({"slot":slot,"verified":true}));
            }
        }
        Outcome::Failed(format!("SIM slot {slot} did not become active"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_ids_use_the_oem_semicolon_list() {
        assert_eq!(id_list(&json!({"ids":[1, "22"]}), "ids").unwrap(), "1;22;");
        assert_eq!(id_list(&json!({"ids":"3;4,"}), "ids").unwrap(), "3;4;");
        assert_eq!(id_list(&json!({"ids":7}), "ids").unwrap(), "7;");
        for bad in [
            json!({}),
            json!({"ids":[]}),
            json!({"ids":["1;reboot"]}),
            json!({"ids":["-1"]}),
            json!({"ids":[null]}),
        ] {
            assert!(id_list(&bad, "ids").is_err(), "{bad}");
        }
        let too_many: Vec<u32> = (0..65).collect();
        assert!(id_list(&json!({"ids":too_many}), "ids").is_err());
    }

    #[test]
    fn integer_params_are_strict() {
        assert_eq!(integer(&json!({"a":"5"}), "a").unwrap(), Some(5));
        assert_eq!(integer(&json!({}), "a").unwrap(), None);
        assert!(integer(&json!({"a":"x"}), "a").is_err());
        assert!(integer(&json!({"a":1.5}), "a").is_err());
    }
}

#[cfg(test)]
mod band_mode_tests {
    use super::*;
    #[test]
    fn band_inputs_accept_csv_arrays_and_auto_but_reject_bad_values() {
        assert_eq!(
            band_list(&json!({"bands":"41,1,3,3"}), "bands", 64).unwrap(),
            vec![1, 3, 41]
        );
        assert_eq!(
            band_list(&json!({"bands":[41,1,3,3]}), "bands", 64).unwrap(),
            vec![1, 3, 41]
        );
        for value in [json!(""), json!([])] {
            assert!(
                band_list(&json!({"bands":value}), "bands", 64)
                    .unwrap()
                    .is_empty()
            );
        }
        for value in [
            json!([-1]),
            json!([0]),
            json!([65]),
            json!([1.5]),
            json!([true]),
            json!("1;reboot"),
        ] {
            assert!(band_list(&json!({"bands":value}), "bands", 64).is_err());
        }
        assert!(band_list(&json!({}), "bands", 64).is_err());
        assert_eq!(lte_mask(&[1, 3, 41]), "0x10000000005");
    }
    #[test]
    fn canonical_network_modes_and_common_aliases() {
        for value in ["4G_AND_5G", "WL_AND_5G", "auto"] {
            assert_eq!(network_mode(value), Some("4G_AND_5G"));
        }
        for value in ["Only_LTE", "4G", "LTE"] {
            assert_eq!(network_mode(value), Some("Only_LTE"));
        }
        for value in ["Only_5G", "5G", "NR"] {
            assert_eq!(network_mode(value), Some("Only_5G"));
        }
        for value in ["5G_NSA", "garbage", "rm -rf"] {
            assert_eq!(network_mode(value), None);
        }
    }
}
