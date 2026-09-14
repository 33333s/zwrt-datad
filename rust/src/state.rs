use crate::{command, model::Snapshot};
use serde_json::{Map, Value, json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub async fn collect() -> Snapshot {
    let mut fields = Map::new();
    let system = ubus("system", "info", json!({})).await;
    fields.insert(
        "system".into(),
        system.unwrap_or_else(|e| json!({"available":false,"error":e})),
    );
    let network = ubus("network.interface", "dump", json!({})).await;
    fields.insert(
        "network".into(),
        network.unwrap_or_else(|e| json!({"available":false,"error":e})),
    );
    Snapshot {
        ts: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
        datad: Default::default(),
        fields,
    }
}

pub async fn ubus(service: &str, method: &str, args: Value) -> Result<Value, String> {
    validate_name(service)?;
    validate_name(method)?;
    if !args.is_object() {
        return Err("args must be an object".into());
    }
    let body = serde_json::to_string(&args).map_err(|e| e.to_string())?;
    let raw = command::run(
        "/usr/bin/ubus",
        ["call", service, method, &body],
        Duration::from_secs(8),
    )
    .await
    .map_err(|e| e.to_string())?;
    serde_json::from_slice(&raw).map_err(|e| format!("invalid ubus JSON: {e}"))
}

pub async fn ubus_list(verbose: bool) -> Result<Value, String> {
    let args: Vec<&str> = if verbose {
        vec!["-v", "list"]
    } else {
        vec!["list"]
    };
    let raw = command::run("/usr/bin/ubus", args, Duration::from_secs(8))
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({"ok":true,"verbose":verbose,"output":String::from_utf8_lossy(&raw)}))
}

fn validate_name(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err("invalid ubus name".into());
    }
    Ok(())
}
