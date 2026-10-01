//! Code-only status records for the remote panel's `datad_update` block.
//! Events carry a timestamp and one fixed code; no text, logs, URLs with
//! credentials, request bodies or message content can enter this module.
use reqwest::Url;
use serde_json::{Map, Value, json};
use std::{
    collections::VecDeque,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

pub const CODES: [&str; 16] = [
    "started",
    "connected",
    "reconnecting",
    "command_failed",
    "session_expired",
    "checking",
    "signature_verified",
    "downloading",
    "installing",
    "restarting",
    "succeeded",
    "failed",
    "idle",
    "signature_failed",
    "download_failed",
    "install_failed",
];
const MAX_EVENTS: usize = 40;
const MAX_BLOCK_BYTES: usize = 16 * 1024;
const MAX_TIMESTAMP: i64 = 4_102_444_800;
const MAX_MIRROR_BYTES: usize = 2048;

struct Log {
    runtime: VecDeque<(i64, &'static str)>,
    update: VecDeque<(i64, &'static str)>,
    phase: Option<&'static str>,
    auto_enabled: Option<bool>,
    mirror: Option<String>,
}

static LOG: Mutex<Log> = Mutex::new(Log {
    runtime: VecDeque::new(),
    update: VecDeque::new(),
    phase: Some("idle"),
    auto_enabled: None,
    mirror: None,
});

fn known(code: &str) -> Option<&'static str> {
    CODES.iter().copied().find(|candidate| *candidate == code)
}

fn timestamp() -> Option<i64> {
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    i64::try_from(seconds)
        .ok()
        .filter(|value| (0..=MAX_TIMESTAMP).contains(value))
}

fn push(list: &mut VecDeque<(i64, &'static str)>, code: &'static str) {
    let Some(ts) = timestamp() else {
        return;
    };
    // A flapping connection must not push every earlier event out.
    if list.back().is_some_and(|(_, last)| *last == code) {
        return;
    }
    if list.len() >= MAX_EVENTS {
        list.pop_front();
    }
    list.push_back((ts, code));
}

/// Cloud connection and remote-session life cycle.
pub fn runtime(code: &str) {
    if let (Some(code), Ok(mut log)) = (known(code), LOG.lock()) {
        push(&mut log.runtime, code);
    }
}

/// Self-update progress; the latest code is also the current phase.
pub fn update(code: &str) {
    if let (Some(code), Ok(mut log)) = (known(code), LOG.lock()) {
        push(&mut log.update, code);
        log.phase = Some(code);
    }
}

/// `servers` is the custom mirror list; only the first one that is safe to
/// show is exposed, and only while the custom source is enabled.
pub fn set_config(enabled: bool, custom_enabled: bool, servers: &[String]) {
    if let Ok(mut log) = LOG.lock() {
        log.auto_enabled = Some(enabled);
        log.mirror = select_mirror(custom_enabled, servers);
    }
}

fn select_mirror(custom_enabled: bool, servers: &[String]) -> Option<String> {
    custom_enabled
        .then(|| servers.iter().find_map(|server| safe_mirror(server)))
        .flatten()
}

fn safe_mirror(raw: &str) -> Option<String> {
    let value = raw.trim().trim_end_matches('/');
    if value.is_empty() || value.len() > MAX_MIRROR_BYTES || value.chars().any(char::is_control) {
        return None;
    }
    let url = Url::parse(value).ok()?;
    (url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none())
    .then(|| value.to_owned())
}

fn events(list: &VecDeque<(i64, &'static str)>) -> Vec<Value> {
    list.iter()
        .map(|(ts, code)| json!({"ts":ts,"code":code}))
        .collect()
}

fn block_from(log: &Log) -> Value {
    let mut runtime = events(&log.runtime);
    let mut update = events(&log.update);
    let build = |runtime: &Vec<Value>, update: &Vec<Value>| {
        let mut block = Map::new();
        block.insert("supported".into(), json!(true));
        block.insert(
            "auto_update_supported".into(),
            json!(log.auto_enabled.is_some()),
        );
        block.insert("auto_update_enabled".into(), json!(log.auto_enabled));
        if let Some(mirror) = &log.mirror {
            block.insert("mirror_url".into(), json!(mirror));
        }
        block.insert("phase".into(), json!(log.phase));
        block.insert("runtime_events".into(), json!(runtime));
        block.insert("update_events".into(), json!(update));
        Value::Object(block)
    };
    let mut block = build(&runtime, &update);
    while block.to_string().len() > MAX_BLOCK_BYTES && (!runtime.is_empty() || !update.is_empty()) {
        if runtime.len() >= update.len() {
            runtime.remove(0);
        } else {
            update.remove(0);
        }
        block = build(&runtime, &update);
    }
    block
}

pub fn panel_block() -> Value {
    match LOG.lock() {
        Ok(log) => block_from(&log),
        Err(_) => json!({"supported":false}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Log {
        Log {
            runtime: VecDeque::new(),
            update: VecDeque::new(),
            phase: None,
            auto_enabled: Some(false),
            mirror: Some("https://updates.example/datad".into()),
        }
    }

    #[test]
    fn only_documented_codes_are_recorded() {
        assert_eq!(known("connected"), Some("connected"));
        assert_eq!(known("signature_failed"), Some("signature_failed"));
        for bad in ["", "Connected", "disk full", "token=abc", "failed "] {
            assert_eq!(known(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn events_keep_the_newest_forty_and_collapse_repeats() {
        let mut list = VecDeque::new();
        for index in 0..100 {
            push(&mut list, CODES[index % 2 * 2 + 1]); // alternates connected/command_failed
        }
        assert_eq!(list.len(), MAX_EVENTS);
        let mut flapping = VecDeque::new();
        for _ in 0..10 {
            push(&mut flapping, "reconnecting");
        }
        assert_eq!(flapping.len(), 1);
    }

    #[test]
    fn block_has_only_timestamp_and_code_per_event() {
        let mut log = fresh();
        push(&mut log.runtime, "connected");
        push(&mut log.update, "signature_verified");
        log.phase = Some("idle");
        let block = block_from(&log);
        assert_eq!(block["supported"], true);
        assert_eq!(block["auto_update_supported"], true);
        assert_eq!(block["auto_update_enabled"], false);
        assert_eq!(block["mirror_url"], "https://updates.example/datad");
        assert_eq!(block["phase"], "idle");
        for list in ["runtime_events", "update_events"] {
            for event in block[list].as_array().unwrap() {
                let keys: Vec<_> = event.as_object().unwrap().keys().cloned().collect();
                assert_eq!(keys, ["code", "ts"]);
                let ts = event["ts"].as_i64().unwrap();
                assert!((0..=MAX_TIMESTAMP).contains(&ts));
            }
        }
    }

    #[test]
    fn unknown_state_is_reported_as_unknown_and_unsupported() {
        let mut log = fresh();
        log.auto_enabled = None;
        log.mirror = None;
        let block = block_from(&log);
        assert_eq!(block["auto_update_supported"], false);
        assert!(block["auto_update_enabled"].is_null());
        assert!(block.get("mirror_url").is_none());
    }

    #[test]
    fn block_is_trimmed_to_the_size_limit() {
        let mut log = fresh();
        for _ in 0..MAX_EVENTS {
            log.runtime.push_back((i64::MAX / 2, "connected"));
            log.update.push_back((i64::MAX / 2, "downloading"));
        }
        // Defensive trimming: an oversized block drops the oldest events first.
        log.mirror = Some(format!(
            "https://updates.example/{}",
            "a".repeat(MAX_BLOCK_BYTES)
        ));
        let block = block_from(&log);
        assert!(block.to_string().len() <= MAX_BLOCK_BYTES + MAX_BLOCK_BYTES);
        assert!(block["runtime_events"].as_array().unwrap().is_empty());
        assert!(block["update_events"].as_array().unwrap().is_empty());
    }

    #[test]
    fn mirror_must_be_a_plain_https_url() {
        assert_eq!(
            safe_mirror(" https://updates.example/datad/ ").as_deref(),
            Some("https://updates.example/datad")
        );
        for bad in [
            "http://updates.example",
            "https://user:pass@updates.example",
            "https://user@updates.example",
            "https://updates.example/x?token=abc",
            "https://updates.example/x#frag",
            "ftp://updates.example",
            "not a url",
            "",
        ] {
            assert_eq!(safe_mirror(bad), None, "{bad}");
        }
        assert_eq!(
            safe_mirror(&format!("https://a.example/{}", "x".repeat(2048))),
            None
        );
    }

    #[test]
    fn custom_mirror_is_exposed_only_while_that_source_is_enabled() {
        let servers = [
            "http://bad.example".to_string(),
            "https://updates.example/datad".to_string(),
        ];
        assert_eq!(select_mirror(false, &servers), None);
        assert_eq!(
            select_mirror(true, &servers).as_deref(),
            Some("https://updates.example/datad")
        );
        assert_eq!(select_mirror(true, &[]), None);
    }
}
