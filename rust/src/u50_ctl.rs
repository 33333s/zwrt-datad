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
];

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
        self.read(key)
            .await
            .ok()?
            .get(key)?
            .as_str()
            .map(str::to_owned)
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
            _ => Outcome::NotHandled,
        }
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
        let mode = match text(params, "mode") {
            Some("4G_AND_5G" | "LTE_AND_5G" | "WL_AND_5G") => "4G_AND_5G",
            Some("Only_5G") => "Only_5G",
            Some("Only_LTE") => "Only_LTE",
            _ => return Outcome::Invalid("mode must be 4G_AND_5G, Only_5G or Only_LTE".into()),
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
        let readback = self.field("net_select").await;
        Outcome::Ok(json!({"mode":mode,"verified":readback.as_deref() == Some(mode)}))
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
