//! Relay node origins managed by the platform (`remote.nodes.set`).
//!
//! NMS may hand a remote session to a relay node instead of its own host. The
//! node list arrives only over the authenticated MQTT command channel, is kept
//! apart from the owner's `remote_origins` in `remote-nodes.json`, and only
//! widens which HTTPS origins a `remote.open` may connect to. Ports, services
//! and every other remote check stay as they are. The owner can switch the
//! whole list off locally; nodes saved under one platform are not trusted
//! after the device moves to another.
use crate::cloud::{Config, atomic_json, https_origin, topic};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub const MAX_NODES: usize = 8;
const FILE_NAME: &str = "remote-nodes.json";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Saved {
    revision: u64,
    nodes: Vec<String>,
    /// Origin of the platform that sent the list.
    platform: String,
    /// The owner's local switch; off means no node is trusted.
    disabled: bool,
}

/// `{"protocol_version":1,"request_id":…,"action":"remote.nodes.set","nodes":[…],"revision":N}`
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetCommand {
    protocol_version: i64,
    request_id: String,
    action: String,
    nodes: Vec<String>,
    revision: u64,
}

impl SetCommand {
    pub fn is_nodes_set(&self) -> bool {
        self.action == "remote.nodes.set"
    }
}

#[derive(Clone, Default)]
pub struct RemoteNodes {
    file: Option<PathBuf>,
    saved: Arc<Mutex<Saved>>,
}

/// One node entry: an HTTPS origin on a public-looking domain name (no IP
/// literal, no `localhost`), with no path, query or credentials.
fn node_origin(raw: &str) -> Option<Url> {
    let url = https_origin(raw).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    let domain = !host.starts_with('[')
        && host.parse::<std::net::IpAddr>().is_err()
        && host.contains('.')
        && !host.ends_with('.')
        && host != "localhost"
        && !host.ends_with(".localhost");
    domain.then_some(url)
}

fn platform_origin(config: &Config) -> Option<String> {
    https_origin(&config.platform_url)
        .ok()
        .map(|origin| origin.origin().ascii_serialization())
}

impl RemoteNodes {
    /// A missing or unreadable file starts with an empty list at revision 0.
    pub fn load(data_dir: &Path) -> Self {
        let file = data_dir.join(FILE_NAME);
        let saved = fs::read(&file)
            .ok()
            .and_then(|data| serde_json::from_slice::<Saved>(&data).ok())
            .filter(|saved| {
                saved.nodes.len() <= MAX_NODES
                    && saved.nodes.iter().all(|node| node_origin(node).is_some())
            })
            .unwrap_or_default();
        Self {
            file: Some(file),
            saved: Arc::new(Mutex::new(saved)),
        }
    }

    fn saved(&self) -> Saved {
        self.saved.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn store(&self, next: Saved) -> Result<(), String> {
        if let Some(file) = &self.file {
            atomic_json(file, &next)?;
        }
        *self.saved.lock().unwrap_or_else(|e| e.into_inner()) = next;
        Ok(())
    }

    /// Node origins trusted right now under `config`'s platform.
    pub fn active(&self, config: &Config) -> Vec<Url> {
        let saved = self.saved();
        if saved.disabled || platform_origin(config).as_deref() != Some(saved.platform.as_str()) {
            return Vec::new();
        }
        saved
            .nodes
            .iter()
            .filter_map(|node| node_origin(node))
            .collect()
    }

    pub fn origins(&self, config: &Config) -> Vec<String> {
        self.active(config)
            .iter()
            .map(|origin| origin.origin().ascii_serialization())
            .collect()
    }

    pub fn revision(&self) -> u64 {
        self.saved().revision
    }

    pub fn enabled(&self) -> bool {
        !self.saved().disabled
    }

    /// The owner's local switch. It never touches the list itself.
    pub fn set_enabled(&self, enabled: bool) -> Result<(), String> {
        let mut next = self.saved();
        if next.disabled == !enabled {
            return Ok(());
        }
        next.disabled = !enabled;
        self.store(next)
    }

    /// Read-only view for the local cloud settings page.
    pub fn public_view(&self, config: &Config) -> Value {
        let saved = self.saved();
        json!({
            "enabled":!saved.disabled,
            "revision":saved.revision,
            "nodes":saved.nodes,
            "active":self.origins(config),
        })
    }

    /// Applies `remote.nodes.set` and returns the reply for `command/result`.
    /// A malformed list is refused as a whole and changes nothing; a revision
    /// that is not newer than the saved one is acknowledged without change.
    pub fn apply(&self, config: &Config, command: &SetCommand) -> Value {
        let reply = |nodes: &RemoteNodes, ok: bool, applied: bool, error: Option<&str>| {
            let mut value = json!({
                "request_id":command.request_id,
                "action":"remote.nodes.set",
                "status":if ok {"ok"} else {"rejected"},
                "ok":ok,
                "revision":nodes.revision(),
                "nodes":nodes.origins(config),
                "enabled":nodes.enabled(),
            });
            if ok {
                value["applied"] = json!(applied);
            }
            if let Some(code) = error {
                value["error"] = json!({"code":code});
            }
            value
        };
        if command.protocol_version != 1 || !topic(&command.request_id) || !command.is_nodes_set() {
            return reply(self, false, false, Some("invalid_request"));
        }
        if command.nodes.len() > MAX_NODES {
            return reply(self, false, false, Some("too_many_nodes"));
        }
        let mut nodes: Vec<String> = Vec::new();
        for raw in &command.nodes {
            let Some(origin) = node_origin(raw) else {
                return reply(self, false, false, Some("invalid_node"));
            };
            let origin = origin.origin().ascii_serialization();
            if !nodes.contains(&origin) {
                nodes.push(origin);
            }
        }
        let Some(platform) = platform_origin(config) else {
            return reply(self, false, false, Some("platform_unset"));
        };
        let current = self.saved();
        if command.revision <= current.revision && current.platform == platform {
            return reply(self, true, false, None);
        }
        let next = Saved {
            revision: command.revision,
            nodes,
            platform,
            disabled: current.disabled,
        };
        if self.store(next).is_err() {
            return reply(self, false, false, Some("save_failed"));
        }
        reply(self, true, true, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            platform_url: "https://nms.example.com".into(),
            ..Default::default()
        }
    }

    fn set(nodes: &[&str], revision: u64) -> SetCommand {
        SetCommand {
            protocol_version: 1,
            request_id: "nodes-1".into(),
            action: "remote.nodes.set".into(),
            nodes: nodes.iter().map(|node| (*node).to_owned()).collect(),
            revision,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "datad-nodes-{name}-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_valid_list_replaces_the_old_one_and_survives_a_restart() {
        let dir = temp_dir("restart");
        let nodes = RemoteNodes::load(&dir);
        let reply = nodes.apply(
            &config(),
            &set(
                &[
                    "https://nmg.services.ericsfj.com:8443",
                    "https://a.ericsfj.com:16001/",
                ],
                3,
            ),
        );
        assert_eq!(
            reply,
            json!({"request_id":"nodes-1","action":"remote.nodes.set","status":"ok","ok":true,
                "applied":true,"revision":3,"enabled":true,
                "nodes":["https://nmg.services.ericsfj.com:8443","https://a.ericsfj.com:16001"]})
        );
        let reloaded = RemoteNodes::load(&dir);
        assert_eq!(reloaded.revision(), 3);
        assert_eq!(
            reloaded.origins(&config()),
            vec![
                "https://nmg.services.ericsfj.com:8443",
                "https://a.ericsfj.com:16001"
            ]
        );
        // Whole-list replacement.
        nodes.apply(&config(), &set(&["https://b.example.com"], 4));
        assert_eq!(
            RemoteNodes::load(&dir).origins(&config()),
            vec!["https://b.example.com"]
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_bad_entry_or_too_many_refuses_the_whole_list() {
        let nodes = RemoteNodes::default();
        nodes.apply(&config(), &set(&["https://keep.example.com"], 1));
        for (list, code) in [
            (vec!["http://plain.example.com"], "invalid_node"),
            (vec!["https://node.example.com/path"], "invalid_node"),
            (vec!["https://node.example.com/?q=1"], "invalid_node"),
            (vec!["https://user:pw@node.example.com"], "invalid_node"),
            (vec!["https://203.0.113.5:8443"], "invalid_node"),
            (vec!["https://[2001:db8::1]"], "invalid_node"),
            (vec!["https://localhost"], "invalid_node"),
            (vec!["https://node.localhost"], "invalid_node"),
            (vec!["https://*.example.com"], "invalid_node"),
            (vec!["wss://node.example.com"], "invalid_node"),
            (
                vec!["https://ok.example.com", "ftp://x.example.com"],
                "invalid_node",
            ),
        ] {
            let reply = nodes.apply(&config(), &set(&list, 9));
            assert_eq!(reply["ok"], false, "{list:?}");
            assert_eq!(reply["error"]["code"], code, "{list:?}");
            assert_eq!(reply["revision"], 1);
            assert_eq!(reply["nodes"], json!(["https://keep.example.com"]));
        }
        let nine: Vec<String> = (0..9)
            .map(|n| format!("https://n{n}.example.com"))
            .collect();
        let nine: Vec<&str> = nine.iter().map(String::as_str).collect();
        let reply = nodes.apply(&config(), &set(&nine, 9));
        assert_eq!(reply["error"]["code"], "too_many_nodes");
        assert_eq!(nodes.origins(&config()), vec!["https://keep.example.com"]);
        assert_eq!(
            nodes.apply(&config(), &set(&nine[..8], 9))["revision"],
            9,
            "eight entries are allowed"
        );
    }

    #[test]
    fn older_or_equal_revisions_are_acknowledged_and_ignored() {
        let nodes = RemoteNodes::default();
        nodes.apply(&config(), &set(&["https://a.example.com"], 5));
        for revision in [5, 4, 0] {
            let reply = nodes.apply(&config(), &set(&["https://b.example.com"], revision));
            assert_eq!(reply["ok"], true);
            assert_eq!(reply["applied"], false);
            assert_eq!(reply["revision"], 5);
            assert_eq!(reply["nodes"], json!(["https://a.example.com"]));
        }
    }

    #[test]
    fn the_local_switch_and_a_platform_change_withdraw_trust() {
        let dir = temp_dir("switch");
        let nodes = RemoteNodes::load(&dir);
        nodes.apply(&config(), &set(&["https://a.example.com"], 2));
        nodes.set_enabled(false).unwrap();
        assert!(nodes.active(&config()).is_empty());
        let reloaded = RemoteNodes::load(&dir);
        assert!(!reloaded.enabled(), "the switch persists");
        assert!(reloaded.active(&config()).is_empty());
        // The platform still updates the list while it is switched off.
        let reply = reloaded.apply(&config(), &set(&["https://b.example.com"], 3));
        assert_eq!(
            (reply["ok"].clone(), reply["nodes"].clone()),
            (json!(true), json!([]))
        );
        reloaded.set_enabled(true).unwrap();
        assert_eq!(reloaded.origins(&config()), vec!["https://b.example.com"]);
        let other = Config {
            platform_url: "https://other-nms.example.com".into(),
            ..Default::default()
        };
        assert!(
            reloaded.active(&other).is_empty(),
            "nodes belong to the platform that sent them"
        );
        // The new platform starts over even with a lower revision.
        assert_eq!(
            reloaded.apply(&other, &set(&["https://c.example.com"], 1))["applied"],
            true
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_wire_format_is_strict() {
        let wire = r#"{"protocol_version":1,"request_id":"abc","action":"remote.nodes.set","nodes":["https://nmg.services.ericsfj.com:8443"],"revision":3}"#;
        assert!(
            serde_json::from_str::<SetCommand>(wire)
                .unwrap()
                .is_nodes_set()
        );
        let extra = r#"{"protocol_version":1,"request_id":"abc","action":"remote.nodes.set","nodes":[],"revision":3,"ports":[22]}"#;
        assert!(serde_json::from_str::<SetCommand>(extra).is_err());
        let reply = RemoteNodes::default().apply(
            &config(),
            &SetCommand {
                protocol_version: 2,
                ..set(&[], 1)
            },
        );
        assert_eq!(reply["error"]["code"], "invalid_request");
    }

    #[test]
    fn the_local_settings_api_can_only_flip_the_switch() {
        use crate::cloud::Update;
        for body in [
            r#"{"remote_nodes":["https://x.example.com"]}"#,
            r#"{"nodes":["https://x.example.com"]}"#,
            r#"{"remote_node_origins":["https://x.example.com"]}"#,
        ] {
            assert!(serde_json::from_str::<Update>(body).is_err(), "{body}");
        }
        let update: Update = serde_json::from_str(r#"{"platform_nodes_enabled":false}"#).unwrap();
        assert_eq!(update.platform_nodes_enabled, Some(false));
    }
}
