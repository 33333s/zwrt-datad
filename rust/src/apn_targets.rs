//! APN configuration targets of multi-modem devices (MU5252/TopFlow).
//!
//! `101` and `201` are the vendor's APN namespaces for the external modems
//! (`4G2` and `4G1`), not physical SIM slots. Account names and passwords are
//! never copied out of the vendor replies.
use crate::state;
use serde_json::{Value, json};
use std::{
    future::Future,
    sync::atomic::{AtomicBool, Ordering},
};

pub const MAX_TARGETS_BYTES: usize = 24 * 1024;
const PROFILE_LIMIT: usize = 64;

/// `(slot_id, external single-configuration modem)`; 1 → 5G, 101 → 4G2, 201 → 4G1.
pub const TARGETS: [(i64, bool); 3] = [(1, false), (101, true), (201, true)];

static MULTI_MODEM: AtomicBool = AtomicBool::new(false);

/// Tests that depend on the process-wide multi-modem flag serialise here.
#[cfg(test)]
pub static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Set by the state collector when the template has external modems.
pub fn set_multi_modem(enabled: bool) {
    MULTI_MODEM.store(enabled, Ordering::Relaxed);
}

pub fn multi_modem() -> bool {
    MULTI_MODEM.load(Ordering::Relaxed)
}

pub fn is_external(slot: i64) -> bool {
    TARGETS
        .iter()
        .any(|(id, external)| *id == slot && *external)
}

/// `Ok(None)` keeps the legacy single-modem call; an unavailable or malformed
/// `slot_id` is an error and must never fall back to the main modem.
///
/// Slot 1 is the main modem on every device. Single-modem devices have no
/// other target, so `slot_id: 1` there is the plain legacy call (clients that
/// always send a target keep working); any other value is refused.
pub fn slot_param(params: &Value) -> Result<Option<i64>, String> {
    let Some(value) = params.get("slot_id") else {
        return Ok(None);
    };
    let slot = value
        .as_i64()
        .ok_or_else(|| "slot_id must be an integer".to_string())?;
    if !multi_modem() {
        return if slot == TARGETS[0].0 {
            Ok(None)
        } else {
            Err("slot_id is not an available APN target".into())
        };
    }
    if !TARGETS.iter().any(|(id, _)| *id == slot) {
        return Err("slot_id is not an available APN target".into());
    }
    Ok(Some(slot))
}

/// Adds the vendor `slotId` argument; the legacy call stays untouched.
pub fn with_slot(mut args: Value, slot: Option<i64>) -> Value {
    if let (Some(slot), Some(fields)) = (slot, args.as_object_mut()) {
        fields.insert("slotId".into(), json!(slot));
    }
    args
}

pub fn profiles(reply: &Value) -> Vec<Value> {
    reply
        .get("apnListArray")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(PROFILE_LIMIT)
        .filter_map(|item| {
            let id = item.get("profileId")?.as_str()?;
            if id.is_empty()
                || id.len() > 64
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-.".contains(&byte))
            {
                return None;
            }
            let limited = |name: &str, max: usize| -> String {
                item.get(name)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .chars()
                    .take(max)
                    .collect()
            };
            Some(json!({
                "id":id,"name":limited("profilename",64),"apn":limited("wanapn",128),
                "pdp_type":item.get("pdpType").and_then(Value::as_i64).filter(|v|(0..=3).contains(v)),
                "auth_mode":item.get("pppAuthMode").and_then(Value::as_i64).filter(|v|(0..=3).contains(v)),
                "enabled":item.get("isEnable").and_then(Value::as_bool)==Some(true)
            }))
        })
        .collect()
}

/// Reviewed APN configuration of one target; `None` when any read fails.
pub async fn config(slot: Option<i64>) -> Option<Value> {
    config_from(slot, &vendor_read).await
}

async fn vendor_read(method: &'static str, args: Value) -> Result<Value, String> {
    state::ubus("zwrt_apn_object", method, args).await
}

async fn config_from<F, Fut>(slot: Option<i64>, read: &F) -> Option<Value>
where
    F: Fn(&'static str, Value) -> Fut,
    Fut: Future<Output = Result<Value, String>>,
{
    let call = |method: &'static str| read(method, with_slot(json!({}), slot));
    let (mode, automatic, manual, enabled) = tokio::join!(
        call("get_apn_mode"),
        call("getAutoApnList"),
        call("getManuApnList"),
        call("get_enabled_manu_apn_id")
    );
    let (Ok(mode), Ok(automatic), Ok(manual), Ok(enabled)) = (mode, automatic, manual, enabled)
    else {
        return None;
    };
    let mode = mode
        .get("apn_mode")
        .and_then(Value::as_i64)
        .filter(|v| *v == 0 || *v == 1)?;
    let enabled_id = enabled
        .get("profileId")
        .and_then(Value::as_str)
        .filter(|v| v.len() <= 64)
        .unwrap_or("");
    Some(json!({"mode":mode,"enabled_id":enabled_id,
        "automatic":profiles(&automatic),"manual":profiles(&manual)}))
}

fn target_entry(slot: i64, config: Option<Value>) -> Value {
    let config = match config {
        Some(Value::Object(mut fields)) => {
            fields.insert("writable".into(), json!(true));
            Value::Object(fields)
        }
        _ => json!({"writable":false,"mode":0,"enabled_id":"","automatic":[],"manual":[]}),
    };
    json!({"slot_id":slot,"config":config})
}

/// Drops profiles (automatic first, then the longest list) until the block
/// fits the panel limit.
fn fit(targets: &mut [Value]) {
    while serde_json::to_vec(&json!({"targets":targets})).map_or(0, |raw| raw.len())
        > MAX_TARGETS_BYTES
    {
        let mut longest: Option<(usize, &str, usize)> = None;
        for (index, target) in targets.iter().enumerate() {
            for list in ["automatic", "manual"] {
                let len = target["config"][list].as_array().map_or(0, Vec::len);
                if len > 0 && longest.is_none_or(|(_, _, best)| len > best) {
                    longest = Some((index, list, len));
                }
            }
        }
        let Some((index, list, _)) = longest else {
            return;
        };
        if let Some(items) = targets[index]["config"][list].as_array_mut() {
            items.pop();
        }
    }
}

/// `{"targets":[...]}` for the panel stream; `None` on single-modem devices.
pub async fn panel_targets() -> Option<Value> {
    if !multi_modem() {
        return None;
    }
    Some(targets_from(&vendor_read).await)
}

async fn targets_from<F, Fut>(read: &F) -> Value
where
    F: Fn(&'static str, Value) -> Fut,
    Fut: Future<Output = Result<Value, String>>,
{
    let reads = tokio::join!(
        config_from(Some(TARGETS[0].0), read),
        config_from(Some(TARGETS[1].0), read),
        config_from(Some(TARGETS[2].0), read)
    );
    let mut targets: Vec<Value> = [reads.0, reads.1, reads.2]
        .into_iter()
        .zip(TARGETS)
        .map(|(config, (slot, _))| target_entry(slot, config))
        .collect();
    fit(&mut targets);
    json!({"targets":targets})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_never_copy_account_secrets() {
        let raw = json!({"apnListArray":[
            {"profileId":"profile-1","profilename":"Carrier","wanapn":"internet",
             "username":"must-not-leak","password":"must-not-leak","pdpType":2,"pppAuthMode":1,"isEnable":true},
            {"profileId":"../bad","profilename":"x","wanapn":"y"}
        ]});
        let filtered = profiles(&raw);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["apn"], "internet");
        assert!(!filtered[0].to_string().contains("must-not-leak"));
    }

    #[test]
    fn slot_param_rejects_everything_outside_the_known_targets() {
        let _guard = TEST_LOCK.blocking_lock();
        set_multi_modem(true);
        assert_eq!(slot_param(&json!({})), Ok(None));
        for slot in [1, 101, 201] {
            assert_eq!(slot_param(&json!({"slot_id":slot})), Ok(Some(slot)));
        }
        for bad in [
            json!(0),
            json!(2),
            json!(102),
            json!(-1),
            json!(1.5),
            json!("101"),
            json!(true),
            json!(null),
        ] {
            assert!(slot_param(&json!({"slot_id":bad})).is_err(), "{bad}");
        }
        // Single-modem devices: slot 1 is the main modem (the legacy call), nothing else exists.
        set_multi_modem(false);
        assert_eq!(slot_param(&json!({"slot_id":1})), Ok(None));
        assert_eq!(slot_param(&json!({})), Ok(None));
        for bad in [
            json!(0),
            json!(2),
            json!(101),
            json!(201),
            json!("1"),
            json!(1.5),
            json!(true),
        ] {
            assert!(slot_param(&json!({"slot_id":bad})).is_err(), "{bad}");
        }
        set_multi_modem(true);
    }

    #[test]
    fn external_modems_are_single_configuration_targets() {
        assert!(!is_external(1));
        assert!(is_external(101));
        assert!(is_external(201));
        assert!(!is_external(7));
    }

    #[test]
    fn slot_is_translated_to_the_vendor_argument() {
        assert_eq!(
            with_slot(json!({"a":1}), Some(101)),
            json!({"a":1,"slotId":101})
        );
        assert_eq!(with_slot(json!({"a":1}), None), json!({"a":1}));
    }

    #[test]
    fn unreadable_targets_are_not_writable_and_readable_ones_are() {
        let missing = target_entry(201, None);
        assert_eq!(missing["config"]["writable"], false);
        assert_eq!(missing["config"]["manual"], json!([]));
        let present = target_entry(
            101,
            Some(json!({"mode":1,"enabled_id":"p1","automatic":[],"manual":[]})),
        );
        assert_eq!(present["config"]["writable"], true);
        assert_eq!(present["slot_id"], 101);
    }

    #[test]
    fn oversized_blocks_are_trimmed_to_the_panel_limit() {
        let many = |prefix: &str| -> Vec<Value> {
            (0..64)
                .map(|i| {
                    json!({"id":format!("{prefix}{i}"),"name":"n".repeat(64),"apn":"a".repeat(128),
                    "pdp_type":1,"auth_mode":0,"enabled":false})
                })
                .collect()
        };
        let mut targets: Vec<Value> = TARGETS
            .iter()
            .map(|(slot, _)| {
                json!({"slot_id":slot,"config":{"writable":true,"mode":1,"enabled_id":"p0",
                    "automatic":many("a"),"manual":many("m")}})
            })
            .collect();
        assert!(
            serde_json::to_vec(&json!({"targets":&targets}))
                .unwrap()
                .len()
                > MAX_TARGETS_BYTES
        );
        fit(&mut targets);
        assert!(
            serde_json::to_vec(&json!({"targets":&targets}))
                .unwrap()
                .len()
                <= MAX_TARGETS_BYTES
        );
        assert_eq!(targets.len(), 3);
        assert!(targets.iter().all(|t| t["config"]["writable"] == true));
    }

    #[tokio::test]
    async fn targets_are_read_per_slot_and_unreadable_ones_are_not_writable() {
        use std::sync::Mutex;
        let seen = Mutex::new(Vec::new());
        let read = |method: &'static str, args: Value| {
            seen.lock().unwrap().push((method, args.clone()));
            async move {
                let slot = args["slotId"].as_i64();
                if slot == Some(201) {
                    return Err("exit status: 1".to_string());
                }
                Ok(match method {
                    "get_apn_mode" => json!({"apn_mode":1}),
                    "get_enabled_manu_apn_id" => {
                        json!({"profileId":format!("p{}", slot.unwrap_or(0))})
                    }
                    _ => json!({"apnListArray":[{"profileId":format!("p{}", slot.unwrap_or(0)),
                        "profilename":"internet","wanapn":"internet","username":"must-not-leak",
                        "password":"must-not-leak","pdpType":1,"pppAuthMode":0,"isEnable":true}]}),
                })
            }
        };
        let block = targets_from(&read).await;
        let targets = block["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 3);
        assert_eq!(targets[0]["slot_id"], 1);
        assert_eq!(targets[1]["slot_id"], 101);
        assert_eq!(targets[2]["slot_id"], 201);
        assert_eq!(targets[0]["config"]["writable"], true);
        assert_eq!(targets[1]["config"]["enabled_id"], "p101");
        assert_eq!(targets[1]["config"]["manual"][0]["apn"], "internet");
        assert_eq!(targets[2]["config"]["writable"], false);
        assert_eq!(targets[2]["config"]["manual"], json!([]));
        assert!(!block.to_string().contains("must-not-leak"));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 12);
        assert!(seen.iter().all(|(_, args)| args["slotId"].is_i64()));
    }

    #[tokio::test]
    async fn legacy_config_has_no_slot_argument() {
        let read = |_: &'static str, args: Value| async move {
            assert!(args.get("slotId").is_none());
            Ok(json!({"apn_mode":0,"apnListArray":[]}))
        };
        let config = config_from(None, &read).await.unwrap();
        assert_eq!(config["mode"], 0);
        assert!(config.get("writable").is_none());
    }
}
