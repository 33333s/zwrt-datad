//! The MQTT route assigned by the platform (`mqtt.route.set`).
//!
//! NMS gives devices whose owner is a member or an operator a second MQTT
//! endpoint on its member relay. The assignment arrives only over the
//! authenticated MQTT command channel and is kept apart from `cloud.json` in
//! `mqtt-route.json`: the configured broker stays the free endpoint and is the
//! fallback whenever the member endpoint cannot be reached. A `free` route
//! drops the member endpoint, and the device no longer tries it. An assignment
//! saved under one platform is not used after the device moves to another.
use crate::cloud::{Config, atomic_json, https_origin, topic};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

const FILE_NAME: &str = "mqtt-route.json";
pub const MEMBER: &str = "member";
pub const FREE: &str = "free";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Saved {
    revision: u64,
    route: String,
    /// The member endpoint; empty on the free route.
    broker: String,
    /// Origin of the platform that assigned the route.
    platform: String,
}

/// `{"protocol_version":1,"request_id":…,"action":"mqtt.route.set","route":"member"|"free","broker":…,"revision":N}`
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetCommand {
    protocol_version: i64,
    request_id: String,
    action: String,
    route: String,
    #[serde(default)]
    broker: String,
    revision: u64,
}

impl SetCommand {
    pub fn is_route_set(&self) -> bool {
        self.action == "mqtt.route.set"
    }
}

/// The endpoints for the next connection: the member endpoint first when one
/// is assigned, then the configured broker.
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoints {
    pub primary: String,
    pub fallback: Option<String>,
}

#[derive(Clone, Default)]
pub struct MqttRoute {
    file: Option<PathBuf>,
    saved: Arc<Mutex<Saved>>,
    on_fallback: Arc<AtomicBool>,
}

/// A member endpoint: `wss` on a public-looking domain name (no IP literal, no
/// `localhost`), with no credentials, query or fragment.
fn member_broker(raw: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    let domain = !host.starts_with('[')
        && host.parse::<std::net::IpAddr>().is_err()
        && host.contains('.')
        && !host.ends_with('.')
        && host != "localhost"
        && !host.ends_with(".localhost");
    (raw.len() <= 512
        && url.scheme() == "wss"
        && domain
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.port() != Some(0))
    .then_some(url)
}

fn platform_origin(config: &Config) -> Option<String> {
    https_origin(&config.platform_url)
        .ok()
        .map(|origin| origin.origin().ascii_serialization())
}

impl MqttRoute {
    /// A missing or unreadable file starts on the free route at revision 0.
    pub fn load(data_dir: &Path) -> Self {
        let file = data_dir.join(FILE_NAME);
        let saved = fs::read(&file)
            .ok()
            .and_then(|data| serde_json::from_slice::<Saved>(&data).ok())
            .filter(|saved| {
                (saved.route == FREE && saved.broker.is_empty())
                    || (saved.route == MEMBER && member_broker(&saved.broker).is_some())
            })
            .unwrap_or_default();
        Self {
            file: Some(file),
            saved: Arc::new(Mutex::new(saved)),
            on_fallback: Arc::default(),
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

    /// The member endpoint assigned under `config`'s platform, if any.
    fn member(&self, config: &Config) -> Option<String> {
        let saved = self.saved();
        (saved.route == MEMBER
            && platform_origin(config).as_deref() == Some(saved.platform.as_str())
            && member_broker(&saved.broker).is_some())
        .then_some(saved.broker)
    }

    pub fn endpoints(&self, config: &Config) -> Endpoints {
        match self.member(config) {
            Some(member) if member != config.broker => Endpoints {
                primary: member,
                fallback: Some(config.broker.clone()),
            },
            _ => Endpoints {
                primary: config.broker.clone(),
                fallback: None,
            },
        }
    }

    pub fn revision(&self) -> u64 {
        self.saved().revision
    }

    /// Set by the supervisor while the session runs on the fallback endpoint.
    pub fn set_on_fallback(&self, value: bool) {
        self.on_fallback.store(value, Ordering::Relaxed);
    }

    /// Reported in `device` telemetry so NMS can see what the device holds.
    pub fn report(&self, config: &Config) -> Value {
        json!({
            "route":if self.member(config).is_some() {MEMBER} else {FREE},
            "revision":self.revision(),
            "via":if self.on_fallback.load(Ordering::Relaxed) {"fallback"} else {"primary"},
        })
    }

    /// Applies `mqtt.route.set` and returns the reply for `command/result`. A
    /// revision that is not newer than the saved one is acknowledged without
    /// change; a malformed command changes nothing.
    pub fn apply(&self, config: &Config, command: &SetCommand) -> Value {
        let reply = |route: &MqttRoute, ok: bool, applied: bool, error: Option<&str>| {
            let mut value = json!({
                "request_id":command.request_id,
                "action":"mqtt.route.set",
                "status":if ok {"ok"} else {"rejected"},
                "ok":ok,
            });
            if let Some(object) = value.as_object_mut()
                && let Some(report) = route.report(config).as_object()
            {
                object.extend(report.clone());
            }
            if ok {
                value["applied"] = json!(applied);
            }
            if let Some(code) = error {
                value["error"] = json!({"code":code});
            }
            value
        };
        if command.protocol_version != 1 || !topic(&command.request_id) || !command.is_route_set() {
            return reply(self, false, false, Some("invalid_request"));
        }
        let broker = match command.route.as_str() {
            MEMBER => match member_broker(&command.broker) {
                Some(_) => command.broker.clone(),
                None => return reply(self, false, false, Some("invalid_broker")),
            },
            FREE if command.broker.is_empty() => String::new(),
            _ => return reply(self, false, false, Some("invalid_route")),
        };
        let Some(platform) = platform_origin(config) else {
            return reply(self, false, false, Some("platform_unset"));
        };
        let current = self.saved();
        if command.revision <= current.revision && current.platform == platform {
            return reply(self, true, false, None);
        }
        let next = Saved {
            revision: command.revision,
            route: command.route.clone(),
            broker,
            platform,
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

    const MEMBER_BROKER: &str = "wss://a.example.com:16001/mqtt";

    fn config() -> Config {
        Config {
            platform_url: "https://nms.example.com".into(),
            broker: "wss://nms.example.com/mqtt".into(),
            ..Default::default()
        }
    }

    fn set(route: &str, broker: &str, revision: u64) -> SetCommand {
        SetCommand {
            protocol_version: 1,
            request_id: "route-1".into(),
            action: "mqtt.route.set".into(),
            route: route.into(),
            broker: broker.into(),
            revision,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "datad-route-{name}-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_member_route_puts_the_member_endpoint_first_and_survives_a_restart() {
        let dir = temp_dir("restart");
        let route = MqttRoute::load(&dir);
        assert_eq!(
            route.endpoints(&config()),
            Endpoints {
                primary: "wss://nms.example.com/mqtt".into(),
                fallback: None
            }
        );
        let reply = route.apply(&config(), &set(MEMBER, MEMBER_BROKER, 5));
        assert_eq!(
            reply,
            json!({"request_id":"route-1","action":"mqtt.route.set","status":"ok","ok":true,
                "applied":true,"route":"member","revision":5,"via":"primary"})
        );
        let reloaded = MqttRoute::load(&dir);
        assert_eq!(
            reloaded.endpoints(&config()),
            Endpoints {
                primary: MEMBER_BROKER.into(),
                fallback: Some("wss://nms.example.com/mqtt".into())
            }
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_free_route_drops_the_member_endpoint() {
        let route = MqttRoute::default();
        route.apply(&config(), &set(MEMBER, MEMBER_BROKER, 1));
        let reply = route.apply(&config(), &set(FREE, "", 2));
        assert_eq!(reply["applied"], true);
        assert_eq!(reply["route"], "free");
        assert_eq!(
            route.endpoints(&config()),
            Endpoints {
                primary: "wss://nms.example.com/mqtt".into(),
                fallback: None
            }
        );
    }

    #[test]
    fn stale_or_bad_commands_change_nothing() {
        let route = MqttRoute::default();
        route.apply(&config(), &set(MEMBER, MEMBER_BROKER, 3));
        // Not newer: acknowledged without change.
        let stale = route.apply(&config(), &set(FREE, "", 3));
        assert_eq!(stale["applied"], false);
        assert_eq!(stale["route"], "member");
        for (command, code) in [
            (set(MEMBER, "ws://a.example.com/mqtt", 9), "invalid_broker"),
            (set(MEMBER, "wss://203.0.113.5/mqtt", 9), "invalid_broker"),
            (set(MEMBER, "wss://localhost/mqtt", 9), "invalid_broker"),
            (
                set(MEMBER, "wss://u:p@a.example.com/mqtt", 9),
                "invalid_broker",
            ),
            (
                set(MEMBER, "wss://a.example.com/mqtt?x=1", 9),
                "invalid_broker",
            ),
            (set(MEMBER, "", 9), "invalid_broker"),
            (set(FREE, MEMBER_BROKER, 9), "invalid_route"),
            (set("paid", MEMBER_BROKER, 9), "invalid_route"),
        ] {
            let reply = route.apply(&config(), &command);
            assert_eq!(reply["ok"], false, "{command:?}");
            assert_eq!(reply["error"]["code"], code, "{command:?}");
        }
        let mut wrong = set(FREE, "", 9);
        wrong.action = "remote.nodes.set".into();
        assert_eq!(
            route.apply(&config(), &wrong)["error"]["code"],
            "invalid_request"
        );
        assert_eq!(route.revision(), 3);
        assert_eq!(route.endpoints(&config()).primary, MEMBER_BROKER);
    }

    #[test]
    fn an_assignment_from_another_platform_is_ignored() {
        let route = MqttRoute::default();
        route.apply(&config(), &set(MEMBER, MEMBER_BROKER, 1));
        let moved = Config {
            platform_url: "https://other.example.com".into(),
            ..config()
        };
        assert_eq!(route.endpoints(&moved).fallback, None);
        assert_eq!(route.report(&moved)["route"], "free");
        // The new platform may assign its own route at any revision.
        assert_eq!(
            route.apply(&moved, &set(MEMBER, "wss://b.example.com/mqtt", 1))["applied"],
            true
        );
    }

    #[test]
    fn a_member_endpoint_equal_to_the_configured_broker_has_no_fallback() {
        let route = MqttRoute::default();
        route.apply(&config(), &set(MEMBER, "wss://nms.example.com/mqtt", 1));
        assert_eq!(route.endpoints(&config()).fallback, None);
    }

    #[test]
    fn the_report_says_whether_the_session_runs_on_the_fallback() {
        let route = MqttRoute::default();
        route.apply(&config(), &set(MEMBER, MEMBER_BROKER, 1));
        route.set_on_fallback(true);
        assert_eq!(
            route.report(&config()),
            json!({"route":"member","revision":1,"via":"fallback"})
        );
    }
}
