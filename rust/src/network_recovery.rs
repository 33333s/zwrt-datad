//! Opt-in, bounded network recovery. Intent and action budgets survive restarts.
use crate::{activity, model::Snapshot};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, RwLock, Weak},
    time::Duration,
};
use tokio::sync::Mutex;

pub static NETWORK_GATE: Mutex<()> = Mutex::const_new(());
type Shared = Arc<Mutex<Recovery>>;
static ACTIVE: OnceLock<RwLock<Weak<Mutex<Recovery>>>> = OnceLock::new();
pub fn install(shared: &Shared) {
    *ACTIVE
        .get_or_init(|| RwLock::new(Weak::new()))
        .write()
        .unwrap() = Arc::downgrade(shared);
}
pub fn active() -> Option<Shared> {
    ACTIVE.get()?.read().ok()?.upgrade()
}
pub fn network_action(action: &str) -> bool {
    matches!(
        action,
        "cellular.connect"
            | "cellular.disconnect"
            | "cellular.set"
            | "network.set_mode"
            | "sim.set_slot"
            | "device.reboot"
            | "device.poweroff"
    ) || action.starts_with("band.")
        || action.starts_with("cell.")
        || matches!(
            action,
            "apn.set_mode" | "apn.add" | "apn.modify" | "apn.delete" | "apn.enable"
        )
}
/// Probed in parallel; one answer from any of them means the network is up.
/// Two sites reachable from mainland China plus one that is reachable from
/// almost anywhere, so an overseas SIM, roaming or a VPN is not mistaken for
/// an outage just because the Chinese sites do not answer there.
pub const DEFAULT_PROBE_URLS: [&str; 3] = [
    "https://www.baidu.com/",
    "https://www.qq.com/",
    "https://cp.cloudflare.com/generate_204",
];
pub const MAX_PROBE_URLS: usize = 4;
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub enabled: bool,
    pub interval_seconds: u64,
    pub failure_threshold: u32,
    pub cooldown_seconds: u64,
    pub max_redials: u32,
    pub reboot_after_failures: bool,
    pub probe_urls: Vec<String>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_seconds: 60,
            failure_threshold: 3,
            cooldown_seconds: 300,
            max_redials: 3,
            reboot_after_failures: false,
            probe_urls: DEFAULT_PROBE_URLS.map(String::from).to_vec(),
        }
    }
}
impl Config {
    pub fn valid(&self) -> bool {
        let mut seen = std::collections::HashSet::new();
        [30, 60, 120].contains(&self.interval_seconds)
            && (3..=10).contains(&self.failure_threshold)
            && (180..=3600).contains(&self.cooldown_seconds)
            && (1..=5).contains(&self.max_redials)
            && (1..=MAX_PROBE_URLS).contains(&self.probe_urls.len())
            && self
                .probe_urls
                .iter()
                .all(|url| valid_probe_url(url) && seen.insert(url.to_ascii_lowercase()))
    }
}
/// An HTTPS URL on the default port with no credentials or query, whose host
/// is a public domain name or address. Only the reachability of the host is
/// ever reported, never the response.
pub fn valid_probe_url(raw: &str) -> bool {
    if raw.len() > 200
        || raw
            .bytes()
            .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
    {
        return false;
    }
    let Ok(url) = reqwest::Url::parse(raw) else {
        return false;
    };
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && matches!(url.port(), None | Some(443))
        && url.host_str().is_some_and(public_host)
}
fn public_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(ip) => {
                !(ip.is_private()
                    || ip.is_loopback()
                    || ip.is_link_local()
                    || ip.is_unspecified()
                    || ip.is_broadcast()
                    || ip.is_multicast()
                    || ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1]))
            }
            IpAddr::V6(ip) => {
                let first = ip.segments()[0];
                !(ip.is_loopback()
                    || ip.is_unspecified()
                    || ip.is_multicast()
                    || first & 0xfe00 == 0xfc00
                    || first & 0xffc0 == 0xfe80)
            }
        };
    }
    let host = host.to_ascii_lowercase();
    host.contains('.')
        && !host.ends_with('.')
        && !["localhost", "local", "lan", "internal", "home.arpa"]
            .iter()
            .any(|private| host == *private || host.ends_with(&format!(".{private}")))
}
fn probe_hosts(config: &Config) -> Vec<String> {
    config
        .probe_urls
        .iter()
        .filter_map(|raw| Some(reqwest::Url::parse(raw).ok()?.host_str()?.to_owned()))
        .collect()
}
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
struct Stored {
    config: Config,
    paused: bool,
    redials: u32,
    next_allowed: u64,
    last_reboot: u64,
    action_times: Vec<u64>,
}
pub struct Recovery {
    path: PathBuf,
    stored: Stored,
    pub generation: u64,
    failures: u32,
    successes: u32,
    pub status: &'static str,
    last_check: u64,
    pub storage_ok: bool,
    startup_until: u64,
    last_manual_check: u64,
    clock_generation: u64,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plan {
    pub generation: u64,
    pub reboot: bool,
}
impl Recovery {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("network-recovery.json");
        let loaded = activity::read::<Stored>(&path, 4096)
            .filter(|s| s.config.valid() && s.redials <= 5 && s.action_times.len() <= 5);
        let storage_ok = loaded.is_some() || !path.exists();
        Self {
            path,
            stored: loaded.unwrap_or_default(),
            generation: 0,
            failures: 0,
            successes: 0,
            status: "disabled",
            last_check: 0,
            storage_ok,
            startup_until: activity::now() + 120,
            last_manual_check: 0,
            clock_generation: crate::time_control::clock_generation(),
        }
    }
    fn store(&mut self, next: Stored) -> Result<(), String> {
        if let Err(e) = activity::save(&self.path, &next) {
            self.storage_ok = false;
            self.status = "storage_failed";
            return Err(e);
        }
        self.stored = next;
        self.storage_ok = true;
        Ok(())
    }
    pub fn config(&self) -> Config {
        self.stored.config.clone()
    }
    pub fn reserve_check(&mut self, now: u64) -> bool {
        if self.last_manual_check != 0 && now.saturating_sub(self.last_manual_check) < 10 {
            return false;
        }
        self.last_manual_check = now;
        true
    }
    pub fn view(&self) -> Value {
        json!({"config":self.stored.config,"status":if !self.storage_ok {"storage_failed"} else if !self.stored.config.enabled {"disabled"}else if self.stored.paused {"manual_pause"}else{self.status},
        "consecutive_failures":self.failures,"redials":self.stored.redials,"last_check":self.last_check,
        "next_allowed":self.stored.next_allowed.max(self.startup_until),"manual_paused":self.stored.paused,"storage_ok":self.storage_ok,
        "probe_hosts":probe_hosts(&self.stored.config),"hourly_action_limit":5,"reboot_cooldown_seconds":21600})
    }
    pub fn update(&mut self, config: Config, now: u64) -> Result<(), String> {
        if !config.valid() {
            return Err("invalid_recovery_config".into());
        }
        let mut next = self.stored.clone();
        next.config = config;
        next.next_allowed = next.next_allowed.max(now + 120);
        self.store(next)?;
        self.generation += 1;
        self.failures = 0;
        self.status = "waiting";
        Ok(())
    }
    pub fn resume(&mut self, now: u64) -> Result<(), String> {
        let mut next = self.stored.clone();
        next.paused = false;
        next.redials = 0;
        next.next_allowed = next.next_allowed.max(now + 120);
        self.store(next)?;
        self.generation += 1;
        self.failures = 0;
        self.status = "waiting";
        Ok(())
    }
    pub fn manual_intent(&mut self, action: &str, now: u64) -> Result<(), String> {
        let mut next = self.stored.clone();
        match action {
            "cellular.disconnect" | "device.poweroff" => next.paused = true,
            "cellular.connect" => next.paused = false,
            _ => {}
        }
        next.next_allowed = next.next_allowed.max(now + 120);
        // No disk churn for ordinary network reads or changes while disabled.
        if next.paused != self.stored.paused || self.stored.config.enabled {
            self.store(next)?;
        }
        self.generation += 1;
        self.failures = 0;
        self.status = "waiting";
        Ok(())
    }
    pub fn may_probe(&mut self, now: u64) -> bool {
        if crate::time_control::clock_change_in_progress() {
            self.status = "clock_unavailable";
            return false;
        }
        let clock_generation = crate::time_control::clock_generation();
        if self.clock_generation != clock_generation {
            // A corrected wall clock must not grant a fresh action budget.
            // Conservatively restart outstanding cooldowns and invalidate any plan.
            let mut next = self.stored.clone();
            next.action_times.fill(now);
            if next.last_reboot != 0 {
                next.last_reboot = now;
            }
            next.next_allowed = now.saturating_add(next.config.cooldown_seconds);
            if self.store(next).is_err() {
                return false;
            }
            self.clock_generation = clock_generation;
            self.generation += 1;
            self.startup_until = now.saturating_add(120);
            self.failures = 0;
            self.successes = 0;
        }
        if !self.stored.config.enabled || self.stored.paused || !self.storage_ok {
            return false;
        }
        if now < 1_704_067_200
            || self.stored.action_times.iter().any(|at| now < *at)
            || now < self.stored.last_reboot
        {
            self.status = "clock_unavailable";
            return false;
        }
        if now < self.startup_until {
            self.status = "waiting";
            return false;
        }
        true
    }
    pub fn hold(&mut self) {
        self.failures = 0;
        self.successes = 0;
        self.status = "waiting_cellular";
    }
    pub fn observe(&mut self, online: bool, now: u64) -> Option<Plan> {
        if !self.may_probe(now) {
            return None;
        }
        self.last_check = now;
        if online {
            self.failures = 0;
            self.successes += 1;
            self.status = "online";
            if self.successes >= 2 && self.stored.redials > 0 {
                let mut next = self.stored.clone();
                next.redials = 0;
                if self.store(next).is_err() {
                    return None;
                }
            }
            return None;
        }
        self.successes = 0;
        self.failures = self.failures.saturating_add(1);
        self.status = "offline";
        if self.failures < self.stored.config.failure_threshold {
            return None;
        }
        if now < self.stored.next_allowed {
            self.status = "cooldown";
            return None;
        }
        let mut next = self.stored.clone();
        next.action_times
            .retain(|at| now.saturating_sub(*at) < 3600);
        if next.action_times.len() >= 5 {
            self.status = "limit_reached";
            return None;
        }
        let reboot = next.redials >= next.config.max_redials;
        if reboot
            && (!next.config.reboot_after_failures
                || next.last_reboot != 0 && now.saturating_sub(next.last_reboot) < 21600)
        {
            self.status = "limit_reached";
            return None;
        }
        next.action_times.push(now);
        next.next_allowed = now + next.config.cooldown_seconds;
        if reboot {
            next.last_reboot = now;
        } else {
            next.redials += 1;
        }
        // Budget must reach disk BEFORE any disconnect or reboot.
        if self.store(next).is_err() {
            return None;
        }
        self.failures = 0;
        self.status = if reboot { "rebooting" } else { "redialing" };
        Some(Plan {
            generation: self.generation,
            reboot,
        })
    }
    pub fn allows(&self, plan: Plan) -> bool {
        self.storage_ok
            && self.stored.config.enabled
            && !self.stored.paused
            && self.generation == plan.generation
    }
}
pub fn eligible(snapshot: &Snapshot, now: u64) -> bool {
    if snapshot.ts <= 0 || now.abs_diff(snapshot.ts as u64) > 30 {
        return false;
    }
    let net = snapshot.fields.get("net").unwrap_or(&Value::Null);
    let kind = net["type"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if net["radio_off"] == true {
        return false;
    }
    [
        "LTE", "5G", "SA", "NSA", "NR", "NR5G", "NR5G-SA", "NR5G-NSA", "LTE+NR", "WCDMA", "UMTS",
        "HSPA", "4G", "3G",
    ]
    .iter()
    .any(|v| kind == *v || kind.starts_with(&format!("{v}_")))
}
/// One bounded HTTPS HEAD per URL, all at once. Any HTTP answer, including an
/// error status, proves the network path works; the first answer ends the check.
pub async fn probe(urls: &[String]) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(4))
        .timeout(Duration::from_secs(8))
        .build()
    else {
        return false;
    };
    any_reachable(urls, |url| {
        let client = client.clone();
        async move { client.head(url).send().await.is_ok() }
    })
    .await
}
async fn any_reachable<F, Fut>(urls: &[String], check: F) -> bool
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    use futures_util::{StreamExt, stream::FuturesUnordered};
    let mut pending: FuturesUnordered<_> = urls.iter().cloned().map(&check).collect();
    while let Some(reachable) = pending.next().await {
        if reachable {
            return true;
        }
    }
    false
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_probe_list_has_a_globally_reachable_address_and_is_valid() {
        let config = Config::default();
        assert!(config.valid());
        assert!(
            config
                .probe_urls
                .iter()
                .any(|u| u.contains("cloudflare.com"))
        );
        assert_eq!(
            probe_hosts(&config),
            ["www.baidu.com", "www.qq.com", "cp.cloudflare.com"]
        );
    }
    #[test]
    fn probe_urls_must_be_public_https_without_surprises() {
        for good in [
            "https://www.baidu.com/",
            "https://cp.cloudflare.com/generate_204",
            "https://example.org",
            "https://1.1.1.1/",
            "https://[2606:4700:4700::1111]/",
            "https://example.org:443/x",
        ] {
            assert!(valid_probe_url(good), "{good}");
        }
        for bad in [
            "",
            "http://example.org/",
            "ftp://example.org/",
            "https://user:pw@example.org/",
            "https://example.org/?q=1",
            "https://example.org/#x",
            "https://example.org:8443/",
            "https://example.org /x",
            "https://localhost/",
            "https://printer.local/",
            "https://nas.lan/",
            "https://router.home.arpa/",
            "https://intranet/",
            "https://192.168.0.1/",
            "https://10.0.0.1/",
            "https://172.16.0.1/",
            "https://100.64.0.1/",
            "https://127.0.0.1/",
            "https://169.254.169.254/",
            "https://0.0.0.0/",
            "https://[::1]/",
            "https://[fe80::1]/",
            "https://[fd00::1]/",
            "https://example.org./",
        ] {
            assert!(!valid_probe_url(bad), "{bad}");
        }
        assert!(!valid_probe_url(&format!(
            "https://{}.example.org/",
            "a".repeat(200)
        )));
    }
    #[test]
    fn probe_list_bounds_and_duplicates_are_enforced() {
        let with = |urls: &[&str]| Config {
            probe_urls: urls.iter().map(|u| (*u).to_owned()).collect(),
            ..Default::default()
        };
        assert!(with(&["https://example.org/"]).valid());
        assert!(!with(&[]).valid());
        assert!(!with(&["https://a.example.org/", "HTTPS://A.EXAMPLE.ORG/"]).valid());
        assert!(
            with(&[
                "https://a.example.org/",
                "https://b.example.org/",
                "https://c.example.org/",
                "https://d.example.org/"
            ])
            .valid()
        );
        assert!(
            !with(&[
                "https://a.example.org/",
                "https://b.example.org/",
                "https://c.example.org/",
                "https://d.example.org/",
                "https://e.example.org/"
            ])
            .valid()
        );
        // A saved file from before probe_urls existed gets the default list.
        let old: Config =
            serde_json::from_str(r#"{"enabled":true,"interval_seconds":60}"#).unwrap();
        assert!(old.valid());
        assert_eq!(old.probe_urls, Config::default().probe_urls);
        assert!(serde_json::from_str::<Config>(r#"{"probe_hosts":[]}"#).is_err());
    }
    #[tokio::test]
    async fn any_single_answer_means_online_and_ends_the_check_early() {
        let urls: Vec<String> = ["cn-a", "cn-b", "global"].map(String::from).to_vec();
        // Only the global address answers.
        let only_global = |url: String| async move { url == "global" };
        assert!(any_reachable(&urls, only_global).await);
        // Nothing answers.
        assert!(!any_reachable(&urls, |_| async { false }).await);
        assert!(!any_reachable(&[], |_| async { true }).await);
        // A hung probe does not delay an answer from another one.
        let started = std::time::Instant::now();
        let hung_first = |url: String| async move {
            if url == "cn-a" {
                std::future::pending::<bool>().await
            } else {
                url == "global"
            }
        };
        assert!(any_reachable(&urls, hung_first).await);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn fresh_radio_state_and_read_actions_are_not_network_intent() {
        let now = activity::now();
        let mut snapshot = Snapshot {
            ts: now as i64,
            datad: Default::default(),
            fields: serde_json::Map::new(),
        };
        for kind in ["LTE", "NR5G", "SA", "LTE_NSA", "NR5G-SA"] {
            snapshot.fields.insert("net".into(), json!({"type":kind}));
            assert!(eligible(&snapshot, now));
        }
        snapshot
            .fields
            .insert("net".into(), json!({"type":"LTE","radio_off":true}));
        assert!(!eligible(&snapshot, now));
        snapshot
            .fields
            .insert("net".into(), json!({"type":"NO_SERVICE"}));
        assert!(!eligible(&snapshot, now));
        snapshot.fields.insert("net".into(), json!({"type":"SA"}));
        assert!(!eligible(&snapshot, now + 31));
        assert!(!network_action("apn.list"));
        assert!(!network_action("wifi.status"));
        assert!(network_action("network.set_mode"));
    }
    #[test]
    fn hourly_cap_survives_intermittent_success_and_clock_rollback() {
        let (dir, mut r, now) = setup("hour");
        r.update(
            Config {
                enabled: true,
                cooldown_seconds: 180,
                ..Default::default()
            },
            now,
        )
        .unwrap();
        for i in 0..5 {
            let t = now + 121 + i * 300;
            for _ in 0..2 {
                assert!(r.observe(false, t).is_none());
            }
            assert!(r.observe(false, t).is_some());
            r.observe(true, t + 60);
            r.observe(true, t + 120);
        }
        for _ in 0..4 {
            assert!(r.observe(false, now + 2000).is_none());
        }
        assert_eq!(r.status, "limit_reached");
        assert!(r.observe(false, now + 3722).is_some());
        for _ in 0..3 {
            assert!(r.observe(false, now + 3922).is_none());
        }
        assert_eq!(r.status, "limit_reached");
        let mut reopened = Recovery::load(&dir);
        assert!(!reopened.may_probe(now - 3600));
        assert_eq!(reopened.status, "clock_unavailable");
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn explicit_clock_change_preserves_action_budgets_and_cancels_plans() {
        let (dir, mut r, now) = setup("clock-transition");
        r.stored.config.enabled = true;
        r.stored.action_times = vec![now; 5];
        r.stored.last_reboot = now;
        let plan = Plan {
            generation: r.generation,
            reboot: false,
        };
        let generation = r.clock_generation;
        crate::time_control::with_clock_state(true, generation, || {
            assert!(!r.may_probe(now + 86400));
            assert_eq!(r.stored.action_times, vec![now; 5]);
        });
        crate::time_control::with_clock_state(false, generation + 1, || {
            assert!(!r.may_probe(now + 86400));
            assert!(!r.allows(plan));
            assert_eq!(r.stored.action_times, vec![now + 86400; 5]);
            assert_eq!(r.stored.last_reboot, now + 86400);
            for _ in 0..4 {
                assert!(r.observe(false, now + 87000).is_none());
            }
            assert_eq!(r.status, "limit_reached");
        });
        assert_eq!(Recovery::load(&dir).stored.action_times.len(), 5);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn setup(name: &str) -> (PathBuf, Recovery, u64) {
        let dir =
            std::env::temp_dir().join(format!("datad-recovery-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let r = Recovery::load(&dir);
        let now = activity::now() + 121;
        (dir, r, now)
    }
    #[test]
    fn default_off_threshold_cooldown_budget_and_restart() {
        let (dir, mut r, mut now) = setup("budget");
        assert!(r.observe(false, now).is_none());
        r.update(
            Config {
                enabled: true,
                max_redials: 1,
                reboot_after_failures: true,
                ..Default::default()
            },
            now,
        )
        .unwrap();
        now += 121;
        assert!(r.observe(false, now).is_none());
        assert!(r.observe(false, now + 60).is_none());
        let p = r.observe(false, now + 120).unwrap();
        assert!(!p.reboot);
        assert!(r.observe(false, now + 180).is_none());
        assert!(r.observe(false, now + 240).is_none());
        assert!(r.observe(false, now + 300).is_none());
        let p = r.observe(false, now + 421).unwrap();
        assert!(p.reboot);
        let mut r = Recovery::load(&dir);
        for i in 1..15 {
            assert!(r.observe(false, now + 500 + i * 60).is_none());
        }
        assert_eq!(r.status, "limit_reached");
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn manual_pause_cancel_and_two_successes_before_reset() {
        let (dir, mut r, now) = setup("manual");
        r.update(
            Config {
                enabled: true,
                ..Default::default()
            },
            now,
        )
        .unwrap();
        let t = now + 121;
        for _ in 0..2 {
            assert!(r.observe(false, t).is_none());
        }
        let plan = r.observe(false, t).unwrap();
        r.manual_intent("cellular.disconnect", t).unwrap();
        assert!(!r.allows(plan));
        assert!(Recovery::load(&dir).stored.paused);
        assert!(r.observe(false, t + 900).is_none());
        r.manual_intent("cellular.connect", t + 900).unwrap();
        r.observe(true, t + 1100);
        assert_eq!(r.stored.redials, 1);
        r.observe(true, t + 1160);
        assert_eq!(r.stored.redials, 0);
        r.update(Config::default(), t + 1200).unwrap();
        assert!(!r.allows(plan));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn invalid_config_and_storage_failure_never_execute() {
        let (dir, mut r, now) = setup("failure");
        assert!(
            r.update(
                Config {
                    interval_seconds: 1,
                    ..Default::default()
                },
                now
            )
            .is_err()
        );
        r.update(
            Config {
                enabled: true,
                ..Default::default()
            },
            now,
        )
        .unwrap();
        r.path = dir.join("missing/config");
        for _ in 0..3 {
            assert!(r.observe(false, now + 121).is_none());
        }
        assert!(!r.storage_ok);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
