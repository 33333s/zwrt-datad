//! Opt-in SMS forwarding over fixed HTTPS POST, authenticated SMTP or the device's own SIM.
//! This does not execute UFI's legacy curl_text or copy its local database.
use crate::{model::Snapshot, reboot_schedule, sms, smtp_forward};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::{Client, Url, header::DATE, redirect::Policy};
use ring::hmac;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, DirBuilder, OpenOptions, Permissions},
    io::Write,
    net::{IpAddr, SocketAddr},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

const FILE_NAME: &str = "sms-forward.json";
const MAX_FILE_BYTES: u64 = 128 * 1024;
const MAX_SEEN: usize = 1024;
const MAX_SMS_TARGETS: usize = 3;
const MAX_SMS_DAILY_SENDS: u16 = 60;
const MAX_SMS_UNITS: usize = 280;
const MAX_POWER_DAILY_SENDS: u16 = 60;
const MAX_BLACKLIST_PHONES: usize = 64;
const MAX_BLACKLIST_KEYWORDS: usize = 32;
const MAX_KEYWORD_BYTES: usize = 128;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    schema: u8,
    enabled: bool,
    method: String,
    webhook_url: String,
    dingtalk_webhook: String,
    dingtalk_secret: String,
    #[serde(default)]
    sms_to_phone: Vec<String>,
    #[serde(default)]
    sms_quota_date: String,
    #[serde(default)]
    sms_quota_used: u16,
    #[serde(default)]
    smtp: smtp_forward::Settings,
    #[serde(default)]
    power_forward_enabled: bool,
    #[serde(default)]
    power_last_percent: Option<i64>,
    #[serde(default)]
    power_last_charging: Option<i64>,
    #[serde(default)]
    power_quota_date: String,
    #[serde(default)]
    power_quota_used: u16,
    #[serde(default)]
    blacklist_phone: Vec<String>,
    #[serde(default)]
    blacklist_keywords: Vec<String>,
    #[serde(default)]
    nickname: String,
    #[serde(default)]
    smtp_forward_device_info: bool,
    #[serde(default)]
    dingtalk_forward_device_info: bool,
    #[serde(default)]
    sms_forward_device_info: bool,
    seen: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema: 1,
            enabled: false,
            method: "webhook".into(),
            webhook_url: String::new(),
            dingtalk_webhook: String::new(),
            dingtalk_secret: String::new(),
            sms_to_phone: Vec::new(),
            sms_quota_date: String::new(),
            sms_quota_used: 0,
            smtp: smtp_forward::Settings::default(),
            power_forward_enabled: false,
            power_last_percent: None,
            power_last_charging: None,
            power_quota_date: String::new(),
            power_quota_used: 0,
            blacklist_phone: Vec::new(),
            blacklist_keywords: Vec::new(),
            nickname: String::new(),
            smtp_forward_device_info: false,
            dingtalk_forward_device_info: false,
            sms_forward_device_info: false,
            seen: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub enabled: bool,
    pub method: String,
    pub webhook_url: Option<String>,
    pub dingtalk_webhook: Option<String>,
    pub dingtalk_secret: Option<String>,
    pub sms_to_phone: Option<Vec<String>>,
    pub smtp: Option<smtp_forward::Update>,
    pub power_forward_enabled: Option<bool>,
    pub blacklist_phone: Option<Vec<String>>,
    pub blacklist_keywords: Option<Vec<String>>,
    pub nickname: Option<String>,
    pub smtp_forward_device_info: Option<bool>,
    pub dingtalk_forward_device_info: Option<bool>,
    pub sms_forward_device_info: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Sms,
    Power,
    Device,
}

#[derive(Clone)]
pub struct Message {
    from: String,
    text: String,
    date: String,
    kind: Kind,
}

pub struct Destination {
    method: String,
    webhook_url: String,
    dingtalk_webhook: String,
    dingtalk_secret: String,
    sms_to_phone: Vec<String>,
    smtp: smtp_forward::Settings,
    include_device_info: bool,
    nickname: String,
    device_info: Option<DeviceInfo>,
}

#[derive(Clone, Debug)]
struct DeviceInfo {
    model: String,
    nickname: String,
    firmware: String,
    daily_flow: String,
    monthly_flow: String,
    battery_level: String,
    battery_temp: String,
    cpu_temp: String,
    cpu_usage: String,
    mem_usage: String,
    uptime: String,
    datad_version: String,
    msisdn: String,
}

fn safe_info_text(value: Option<&Value>, max_bytes: usize) -> String {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| {
            !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
        })
        .unwrap_or_default()
        .to_owned()
}

fn measured(value: Option<&Value>, min: f64, max: f64, suffix: &str) -> String {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && (min..=max).contains(value))
        .map(|value| format!("{value:.1}{suffix}"))
        .unwrap_or_default()
}

fn total_bytes(value: Option<&Value>, rx: &str, tx: &str) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let Some(rx) = value.get(rx).and_then(Value::as_u64) else {
        return String::new();
    };
    let Some(tx) = value.get(tx).and_then(Value::as_u64) else {
        return String::new();
    };
    let Some(total) = rx.checked_add(tx).filter(|total| *total > 0) else {
        return String::new();
    };
    for (unit, name) in [
        (1024_u64.pow(4), "TB"),
        (1024_u64.pow(3), "GB"),
        (1024_u64.pow(2), "MB"),
        (1024, "KB"),
    ] {
        if total >= unit {
            return format!("{:.2} {name}", total as f64 / unit as f64);
        }
    }
    format!("{total} B")
}

impl DeviceInfo {
    fn from_snapshot(snapshot: &Snapshot, nickname: &str) -> Self {
        let device = snapshot.fields.get("device");
        let system = snapshot.fields.get("system");
        let battery = snapshot.fields.get("battery");
        let thermal = snapshot.fields.get("thermal");
        let traffic = snapshot.fields.get("traffic");
        let sim = snapshot.fields.get("sim");
        let market = safe_info_text(device.and_then(|item| item.get("market_name")), 80);
        let model = if market.is_empty() {
            safe_info_text(device.and_then(|item| item.get("model_name")), 80)
        } else {
            market
        };
        let msisdn = safe_info_text(sim.and_then(|item| item.get("msisdn")), 32);
        let msisdn = if valid_phone(&msisdn) {
            msisdn
        } else {
            String::new()
        };
        let uptime = system
            .and_then(|item| item.get("uptime"))
            .and_then(Value::as_u64)
            .filter(|seconds| *seconds <= 100 * 365 * 24 * 3600)
            .map(|seconds| {
                let days = seconds / 86_400;
                let hours = seconds / 3_600 % 24;
                let minutes = seconds / 60 % 60;
                if days > 0 {
                    format!("{days}天 {hours}小时 {minutes}分")
                } else {
                    format!("{hours}小时 {minutes}分")
                }
            })
            .unwrap_or_default();
        Self {
            model,
            nickname: nickname.to_owned(),
            firmware: safe_info_text(system.and_then(|item| item.get("sw_version")), 128),
            daily_flow: total_bytes(traffic, "day_rx_bytes", "day_tx_bytes"),
            monthly_flow: total_bytes(traffic, "month_rx_bytes", "month_tx_bytes"),
            battery_level: battery
                .and_then(|item| item.get("percent"))
                .and_then(Value::as_u64)
                .filter(|percent| *percent <= 100)
                .map(|percent| format!("{percent}%"))
                .unwrap_or_default(),
            battery_temp: measured(
                battery.and_then(|item| item.get("temp")),
                -40.0,
                120.0,
                "°C",
            ),
            cpu_temp: measured(
                thermal.and_then(|item| item.get("cpu_celsius")),
                -40.0,
                150.0,
                "°C",
            ),
            cpu_usage: measured(
                system.and_then(|item| item.get("cpu_usage")),
                0.0,
                100.0,
                "%",
            ),
            mem_usage: measured(
                system.and_then(|item| item.get("mem_used_pct")),
                0.0,
                100.0,
                "%",
            ),
            uptime,
            datad_version: snapshot.datad.version.to_owned(),
            msisdn,
        }
    }

    fn name(&self) -> String {
        match (self.model.is_empty(), self.nickname.is_empty()) {
            (false, false) if self.model != self.nickname => {
                format!("{} ({})", self.model, self.nickname)
            }
            (false, _) => self.model.clone(),
            (_, false) => self.nickname.clone(),
            _ => String::new(),
        }
    }

    fn details(&self) -> String {
        let mut lines = vec!["设备信息".to_string()];
        let name = self.name();
        for (label, value) in [
            ("设备名称", name.as_str()),
            ("固件版本", self.firmware.as_str()),
            ("当日用量", self.daily_flow.as_str()),
            ("本月用量", self.monthly_flow.as_str()),
            ("电池电量", self.battery_level.as_str()),
            ("电池温度", self.battery_temp.as_str()),
            ("CPU温度", self.cpu_temp.as_str()),
            ("CPU占用", self.cpu_usage.as_str()),
            ("内存占用", self.mem_usage.as_str()),
            ("开机时长", self.uptime.as_str()),
            ("datad版本", self.datad_version.as_str()),
        ] {
            if !value.is_empty() {
                lines.push(format!("{label}: {value}"));
            }
        }
        if lines.len() == 1 {
            lines.push("暂不可用".into());
        }
        lines.join("\n")
    }

    fn sms_line(&self) -> String {
        [
            self.model.as_str(),
            self.battery_level.as_str(),
            self.msisdn.as_str(),
        ]
        .into_iter()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
    }
}

impl Destination {
    pub fn attach_device_info(&mut self, snapshot: &Snapshot) {
        if self.include_device_info {
            self.device_info = Some(DeviceInfo::from_snapshot(snapshot, &self.nickname));
        }
    }

    pub fn force_device_info(&mut self, snapshot: &Snapshot) {
        self.include_device_info = true;
        self.attach_device_info(snapshot);
    }
}

impl Message {
    pub fn test() -> Self {
        Self {
            from: "NMS".into(),
            text: "zwrt-datad 短信转发测试".into(),
            date: String::new(),
            kind: Kind::Sms,
        }
    }

    pub fn device_info() -> Self {
        Self {
            from: "DeviceInfo".into(),
            text: String::new(),
            date: String::new(),
            kind: Kind::Device,
        }
    }
}

pub struct Forwarder {
    path: PathBuf,
    config: Config,
    last_result: &'static str,
    power_last_result: &'static str,
}

pub(crate) fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || a == 100 && (64..=127).contains(&b)
                || a == 169 && b == 254
                || a == 172 && (16..=31).contains(&b)
                || a == 192 && (b == 0 || b == 168 || b == 88)
                || a == 198 && (b == 18 || b == 19 || b == 51 && c == 100)
                || a == 203 && b == 0 && c == 113)
        }
        IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(mapped));
            }
            let parts = ip.segments();
            (parts[0] & 0xe000) == 0x2000 && !(parts[0] == 0x2001 && parts[1] == 0x0db8)
        }
    }
}

fn destination(raw: &str, method: &str) -> Result<Url, String> {
    if raw.len() > 2048 || raw.chars().any(char::is_control) {
        return Err("invalid_destination".into());
    }
    let url = Url::parse(raw).map_err(|_| "invalid_destination")?;
    let host = url.host_str().ok_or("invalid_destination")?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || host.parse::<IpAddr>().is_ok()
        || !host.contains('.')
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || method == "dingtalk" && (host != "oapi.dingtalk.com" || url.path() != "/robot/send")
    {
        return Err("invalid_destination".into());
    }
    Ok(url)
}

fn valid_phone(value: &str) -> bool {
    let digits = value.strip_prefix('+').unwrap_or(value);
    (3..=32).contains(&value.len())
        && !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
}

fn phone_matches(a: &str, b: &str) -> bool {
    a.trim_start_matches('+') == b.trim_start_matches('+')
}

fn valid_keyword(value: &str) -> bool {
    !value.is_empty()
        && value == value.trim()
        && value.len() <= MAX_KEYWORD_BYTES
        && !value.chars().any(char::is_control)
}

fn valid_nickname(value: &str) -> bool {
    value.len() <= 255 && value == value.trim() && !value.chars().any(char::is_control)
}

fn blacklisted(config: &Config, message: &Message) -> bool {
    config
        .blacklist_phone
        .iter()
        .any(|phone| phone_matches(phone, &message.from))
        || config
            .blacklist_keywords
            .iter()
            .any(|keyword| message.text.contains(keyword))
}

pub fn power_state(value: Option<&Value>) -> Option<(i64, i64)> {
    let battery = value?;
    let percent = battery.get("percent")?.as_i64()?;
    let charging = battery.get("charging")?.as_i64()?;
    if !(0..=100).contains(&percent) || !(1..=4).contains(&charging) {
        return None;
    }
    Some((percent, charging))
}

fn charge_label(charging: i64) -> &'static str {
    match charging {
        1 => "充电中",
        2 => "放电中",
        3 => "未在充电",
        4 => "已充满",
        _ => "未知",
    }
}

fn valid_config(config: &Config) -> bool {
    config.schema == 1
        && matches!(
            config.method.as_str(),
            "webhook" | "dingtalk" | "sms" | "smtp"
        )
        && config.dingtalk_secret.len() <= 256
        && !config.dingtalk_secret.chars().any(char::is_control)
        && config.seen.len() <= MAX_SEEN
        && config.sms_to_phone.len() <= MAX_SMS_TARGETS
        && config.sms_to_phone.iter().all(|phone| valid_phone(phone))
        && config
            .sms_to_phone
            .iter()
            .map(|phone| phone.trim_start_matches('+'))
            .collect::<HashSet<_>>()
            .len()
            == config.sms_to_phone.len()
        && config.blacklist_phone.len() <= MAX_BLACKLIST_PHONES
        && config
            .blacklist_phone
            .iter()
            .all(|phone| valid_phone(phone))
        && config
            .blacklist_phone
            .iter()
            .map(|phone| phone.trim_start_matches('+'))
            .collect::<HashSet<_>>()
            .len()
            == config.blacklist_phone.len()
        && config.blacklist_keywords.len() <= MAX_BLACKLIST_KEYWORDS
        && config
            .blacklist_keywords
            .iter()
            .all(|word| valid_keyword(word))
        && config
            .blacklist_keywords
            .iter()
            .collect::<HashSet<_>>()
            .len()
            == config.blacklist_keywords.len()
        && valid_nickname(&config.nickname)
        && config.sms_quota_used <= MAX_SMS_DAILY_SENDS
        && config.power_quota_used <= MAX_POWER_DAILY_SENDS
        && (config.power_quota_date.is_empty()
            || reboot_schedule::valid_date(&config.power_quota_date))
        && match (config.power_last_percent, config.power_last_charging) {
            (None, None) => true,
            (Some(percent), Some(charging)) => {
                (0..=100).contains(&percent) && (1..=4).contains(&charging)
            }
            _ => false,
        }
        && config.smtp.safe()
        && (config.sms_quota_date.is_empty() || reboot_schedule::valid_date(&config.sms_quota_date))
        && config
            .seen
            .iter()
            .all(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        && (config.webhook_url.is_empty() || destination(&config.webhook_url, "webhook").is_ok())
        && (config.dingtalk_webhook.is_empty()
            || destination(&config.dingtalk_webhook, "dingtalk").is_ok())
        && (!config.enabled
            || match config.method.as_str() {
                "webhook" => !config.webhook_url.is_empty(),
                "dingtalk" => !config.dingtalk_webhook.is_empty(),
                "sms" => !config.sms_to_phone.is_empty(),
                "smtp" => config.smtp.configured(),
                _ => false,
            })
}

fn messages(snapshot: &Value) -> Result<Vec<(String, Message)>, String> {
    if snapshot.get("stale").and_then(Value::as_bool) != Some(false)
        || snapshot.get("truncated").and_then(Value::as_bool) != Some(false)
    {
        return Err("sms_list_unavailable".into());
    }
    let list = snapshot
        .get("list")
        .and_then(Value::as_array)
        .filter(|list| list.len() <= 512)
        .ok_or("sms_list_unavailable")?;
    let mut output = Vec::with_capacity(list.len());
    for item in list.iter().rev() {
        let tag = item
            .get("tag")
            .and_then(Value::as_i64)
            .ok_or("sms_list_unavailable")?;
        if tag != 0 && tag != 1 {
            continue;
        }
        let id = item
            .get("id")
            .and_then(Value::as_i64)
            .ok_or("sms_list_unavailable")?;
        let from = item
            .get("num")
            .and_then(Value::as_str)
            .ok_or("sms_list_unavailable")?;
        let text = item
            .get("text")
            .and_then(Value::as_str)
            .ok_or("sms_list_unavailable")?;
        let date = item
            .get("date")
            .and_then(Value::as_str)
            .ok_or("sms_list_unavailable")?;
        if from.len() > 64 || text.len() > 4096 || date.len() > 64 {
            return Err("sms_list_unavailable".into());
        }
        let mut digest = Sha256::new();
        digest.update(id.to_be_bytes());
        for field in [from, date, text] {
            digest.update((field.len() as u32).to_be_bytes());
            digest.update(field.as_bytes());
        }
        output.push((
            format!("{:x}", digest.finalize()),
            Message {
                from: from.into(),
                text: text.into(),
                date: date.into(),
                kind: Kind::Sms,
            },
        ));
    }
    Ok(output)
}

fn delivery_result_code(result: &Result<(), String>) -> &'static str {
    match result {
        Ok(()) => "sent",
        Err(error) if error == "clock_unavailable" => "clock_unavailable",
        Err(error) if error == "source_unavailable" => "source_unavailable",
        Err(error) if error == "rate_limited" => "rate_limited",
        Err(error) if error == "self_forward_blocked" => "self_forward_blocked",
        Err(error) if error == "message_too_long" => "message_too_long",
        Err(error) if error == "smtp_auth_failed" => "smtp_auth_failed",
        Err(error) if error == "forward_storage_failed" => "storage_failed",
        Err(_) => "delivery_failed",
    }
}

impl Forwarder {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join(FILE_NAME);
        let config = if path.exists() {
            fs::symlink_metadata(&path)
                .ok()
                .filter(|metadata| metadata.is_file() && metadata.len() <= MAX_FILE_BYTES)
                .and_then(|_| fs::read(&path).ok())
                .and_then(|raw| {
                    let raw = Zeroizing::new(raw);
                    serde_json::from_slice::<Config>(&raw).ok()
                })
                .filter(valid_config)
        } else {
            Some(Config::default())
        };
        let failed = config.is_none();
        Self {
            path,
            config: config.unwrap_or_default(),
            last_result: if failed { "invalid_config" } else { "idle" },
            power_last_result: if failed { "invalid_config" } else { "idle" },
        }
    }

    pub fn status(&self) -> Value {
        let today = reboot_schedule::local_clock().map(|clock| clock.date);
        let remaining = if today.as_deref() == Some(&self.config.sms_quota_date) {
            MAX_SMS_DAILY_SENDS - self.config.sms_quota_used
        } else {
            MAX_SMS_DAILY_SENDS
        };
        let power_remaining = if today.as_deref() == Some(&self.config.power_quota_date) {
            MAX_POWER_DAILY_SENDS - self.config.power_quota_used
        } else {
            MAX_POWER_DAILY_SENDS
        };
        json!({"supported":true,"enabled":self.config.enabled,"method":self.config.method,
            "webhook_configured":!self.config.webhook_url.is_empty(),
            "dingtalk_configured":!self.config.dingtalk_webhook.is_empty(),
            "dingtalk_secret_configured":!self.config.dingtalk_secret.is_empty(),
            "sms_configured":!self.config.sms_to_phone.is_empty(),
            "sms_daily_remaining":remaining,
            "smtp_configured":self.config.smtp.configured(),
            "smtp_supported":true,
            "power_forward_enabled":self.config.power_forward_enabled,
            "power_daily_remaining":power_remaining,
            "power_last_result":self.power_last_result,
            "rules_supported":true,
            "blacklist_phone_count":self.config.blacklist_phone.len(),
            "blacklist_keywords_count":self.config.blacklist_keywords.len(),
            "nickname_supported":true,
            "nickname":self.config.nickname,
            "device_info_supported":true,
            "smtp_forward_device_info":self.config.smtp_forward_device_info,
            "dingtalk_forward_device_info":self.config.dingtalk_forward_device_info,
            "sms_forward_device_info":self.config.sms_forward_device_info,
            "last_result":self.last_result})
    }

    pub fn requires_baseline(&self, input: &Update) -> bool {
        input.enabled
            && (!self.config.enabled
                || input.method != self.config.method
                || self.last_result == "invalid_config")
    }

    fn save(&self, next: &Config) -> Result<(), String> {
        let dir = self.path.parent().ok_or("forward_storage_failed")?;
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|_| "forward_storage_failed")?;
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "forward_storage_failed")?
            .as_nanos();
        let temporary = dir.join(format!("{FILE_NAME}.tmp.{}.{}", std::process::id(), suffix));
        let result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(|_| "forward_storage_failed")?;
            let mut raw =
                Zeroizing::new(serde_json::to_vec(next).map_err(|_| "forward_storage_failed")?);
            raw.push(b'\n');
            if raw.len() as u64 > MAX_FILE_BYTES {
                return Err("forward_storage_failed");
            }
            file.write_all(&raw).map_err(|_| "forward_storage_failed")?;
            file.set_permissions(Permissions::from_mode(0o600))
                .map_err(|_| "forward_storage_failed")?;
            file.sync_all().map_err(|_| "forward_storage_failed")?;
            fs::rename(&temporary, &self.path).map_err(|_| "forward_storage_failed")
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result.map_err(str::to_string)
    }

    pub fn update(
        &mut self,
        input: Update,
        snapshot: Option<&Value>,
        battery: Option<&Value>,
    ) -> Result<Value, String> {
        let power_change = input.power_forward_enabled;
        let mut next = self.config.clone();
        next.enabled = input.enabled;
        next.method = input.method;
        let reset_power_baseline = power_change.is_some()
            || next.enabled && !self.config.enabled && next.power_forward_enabled;
        if let Some(url) = input.webhook_url {
            next.webhook_url = url;
        }
        if let Some(url) = input.dingtalk_webhook {
            next.dingtalk_webhook = url;
        }
        if let Some(secret) = input.dingtalk_secret {
            next.dingtalk_secret = secret;
        }
        if let Some(phones) = input.sms_to_phone {
            next.sms_to_phone = phones;
        }
        if let Some(smtp) = input.smtp {
            next.smtp.apply(smtp);
        }
        if let Some(phones) = input.blacklist_phone {
            next.blacklist_phone = phones;
        }
        if let Some(keywords) = input.blacklist_keywords {
            next.blacklist_keywords = keywords;
        }
        if let Some(nickname) = input.nickname {
            next.nickname = nickname;
        }
        if let Some(enabled) = input.smtp_forward_device_info {
            next.smtp_forward_device_info = enabled;
        }
        if let Some(enabled) = input.dingtalk_forward_device_info {
            next.dingtalk_forward_device_info = enabled;
        }
        if let Some(enabled) = input.sms_forward_device_info {
            next.sms_forward_device_info = enabled;
        }
        if let Some(enabled) = power_change {
            let baseline = power_state(battery);
            if enabled && baseline.is_none() {
                return Err("power_unavailable".into());
            }
            next.power_forward_enabled = enabled;
            (next.power_last_percent, next.power_last_charging) = baseline
                .map_or((None, None), |(percent, charging)| {
                    (Some(percent), Some(charging))
                });
        } else if reset_power_baseline {
            (next.power_last_percent, next.power_last_charging) = power_state(battery)
                .map_or((None, None), |(percent, charging)| {
                    (Some(percent), Some(charging))
                });
        }
        if !valid_config(&next) {
            return Err("invalid_forward_config".into());
        }
        if next.enabled
            && (!self.config.enabled
                || next.method != self.config.method
                || self.last_result == "invalid_config")
        {
            let current = messages(snapshot.ok_or("sms_list_unavailable")?)?;
            let mut seen: HashSet<_> = next.seen.iter().cloned().collect();
            for (hash, _) in current {
                if seen.insert(hash.clone()) {
                    next.seen.push(hash);
                }
            }
            if next.seen.len() > MAX_SEEN {
                next.seen.drain(..next.seen.len() - MAX_SEEN);
            }
        }
        self.save(&next)?;
        self.config = next;
        self.last_result = "idle";
        if reset_power_baseline {
            self.power_last_result = "idle";
        }
        Ok(self.status())
    }

    /// Persist the fingerprint before delivery: a crash after remote success
    /// cannot re-send SMS content on restart. Failed delivery is not retried.
    pub fn next(&mut self, snapshot: &Value) -> Result<Option<Message>, String> {
        if !self.config.enabled {
            return Ok(None);
        }
        let mut seen: HashSet<_> = self.config.seen.iter().cloned().collect();
        let mut next = self.config.clone();
        let mut deliverable = None;
        for (hash, message) in messages(snapshot)? {
            if !seen.insert(hash.clone()) {
                continue;
            }
            next.seen.push(hash);
            if next.seen.len() > MAX_SEEN {
                next.seen.drain(..next.seen.len() - MAX_SEEN);
            }
            if !blacklisted(&next, &message) {
                deliverable = Some(message);
                break;
            }
        }
        if next.seen == self.config.seen {
            return Ok(None);
        }
        self.save(&next)?;
        self.config = next;
        Ok(deliverable)
    }

    pub fn mark_delivery(&mut self, result: &Result<(), String>) {
        self.last_result = delivery_result_code(result);
    }

    pub fn mark_power_delivery(&mut self, result: &Result<(), String>) {
        self.power_last_result = delivery_result_code(result);
    }

    pub fn mark_source_unavailable(&mut self) {
        if self.config.enabled {
            self.last_result = "source_unavailable";
        }
    }

    pub fn mark_storage_failed(&mut self) {
        self.last_result = "storage_failed";
    }

    pub fn delivery(&self) -> Destination {
        let include_device_info = match self.config.method.as_str() {
            "smtp" => self.config.smtp_forward_device_info,
            "dingtalk" => self.config.dingtalk_forward_device_info,
            "sms" => self.config.sms_forward_device_info,
            _ => false,
        };
        Destination {
            method: self.config.method.clone(),
            webhook_url: self.config.webhook_url.clone(),
            dingtalk_webhook: self.config.dingtalk_webhook.clone(),
            dingtalk_secret: self.config.dingtalk_secret.clone(),
            sms_to_phone: self.config.sms_to_phone.clone(),
            smtp: self.config.smtp.clone(),
            include_device_info,
            nickname: self.config.nickname.clone(),
            device_info: None,
        }
    }

    pub fn device_info_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Store the new battery baseline and the daily quota before any external
    /// delivery. Reboots and failed sends cannot replay an old power event.
    pub fn observe_power(&mut self, battery: Option<&Value>) -> Result<Option<Message>, String> {
        let current = power_state(battery);
        let previous = match (
            self.config.power_last_percent,
            self.config.power_last_charging,
        ) {
            (Some(percent), Some(charging)) => Some((percent, charging)),
            _ => None,
        };
        if current == previous {
            return Ok(None);
        }
        let mut next = self.config.clone();
        (next.power_last_percent, next.power_last_charging) = current
            .map_or((None, None), |(percent, charging)| {
                (Some(percent), Some(charging))
            });
        if !self.config.power_forward_enabled || !self.config.enabled {
            self.config.power_last_percent = next.power_last_percent;
            self.config.power_last_charging = next.power_last_charging;
            self.power_last_result = "idle";
            return Ok(None);
        }
        let Some((percent, charging)) = current else {
            self.save(&next)?;
            self.config = next;
            if self.config.power_forward_enabled && self.config.enabled {
                self.power_last_result = "source_unavailable";
            }
            return Ok(None);
        };
        if previous.is_none() {
            self.save(&next)?;
            self.config = next;
            self.power_last_result = "idle";
            return Ok(None);
        }
        let Some(clock) = reboot_schedule::local_clock() else {
            self.save(&next)?;
            self.config = next;
            self.power_last_result = "clock_unavailable";
            return Ok(None);
        };
        if next.power_quota_date != clock.date {
            next.power_quota_date = clock.date.clone();
            next.power_quota_used = 0;
        }
        if next.power_quota_used >= MAX_POWER_DAILY_SENDS {
            self.save(&next)?;
            self.config = next;
            self.power_last_result = "rate_limited";
            return Ok(None);
        }
        next.power_quota_used += 1;
        self.save(&next)?;
        self.config = next;
        let (old_percent, old_charging) = previous.expect("checked above");
        let text = if old_charging != charging {
            format!("充电状态：{}\n当前电量：{percent}%", charge_label(charging))
        } else {
            format!(
                "电量变化：{:+}%（{old_percent}% → {percent}%）\n充电状态：{}",
                percent - old_percent,
                charge_label(charging)
            )
        };
        self.power_last_result = "idle";
        Ok(Some(Message {
            from: "PowerMonitor".into(),
            text,
            date: format!("{} {}", clock.date, clock.time),
            kind: Kind::Power,
        }))
    }

    pub fn reserve_sms(&mut self, message: &Message) -> Result<(), String> {
        if self.config.method != "sms" {
            return Ok(());
        }
        let count = self
            .config
            .sms_to_phone
            .iter()
            .filter(|phone| !phone_matches(phone, &message.from))
            .count();
        if count == 0 {
            return Err("self_forward_blocked".into());
        }
        let date = reboot_schedule::local_clock()
            .ok_or("clock_unavailable")?
            .date;
        let mut next = self.config.clone();
        if next.sms_quota_date != date {
            next.sms_quota_date = date;
            next.sms_quota_used = 0;
        }
        if usize::from(next.sms_quota_used) + count > usize::from(MAX_SMS_DAILY_SENDS) {
            return Err("rate_limited".into());
        }
        next.sms_quota_used += count as u16;
        self.save(&next)?;
        self.config = next;
        Ok(())
    }
}

async fn public_client(url: &Url) -> Result<Client, String> {
    let host = url.host_str().ok_or("invalid_destination")?;
    let port = url.port_or_known_default().ok_or("invalid_destination")?;
    let resolved = tokio::time::timeout(
        Duration::from_secs(4),
        tokio::net::lookup_host((host, port)),
    )
    .await
    .map_err(|_| "destination_unavailable")?
    .map_err(|_| "destination_unavailable")?;
    let addresses: Vec<_> = resolved.collect();
    if addresses.is_empty() || addresses.iter().any(|address| !public_ip(address.ip())) {
        return Err("invalid_destination".into());
    }
    pinned_client(host, &addresses)
}

fn pinned_client(host: &str, addresses: &[SocketAddr]) -> Result<Client, String> {
    Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(10))
        .resolve_to_addrs(host, addresses)
        .build()
        .map_err(|_| "destination_unavailable".into())
}

async fn trusted_timestamp_ms(origin: &str) -> Result<String, String> {
    let mut url = destination(origin, "webhook").map_err(|_| "clock_unavailable")?;
    url.set_path("/api/health");
    url.set_query(None);
    let client = public_client(&url).await.map_err(|_| "clock_unavailable")?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| "clock_unavailable")?;
    if !response.status().is_success() {
        return Err("clock_unavailable".into());
    }
    let date = response
        .headers()
        .get(DATE)
        .ok_or("clock_unavailable")?
        .to_str()
        .map_err(|_| "clock_unavailable")?;
    timestamp_from_http_date(date)
}

fn timestamp_from_http_date(date: &str) -> Result<String, String> {
    let time = httpdate::parse_http_date(date).map_err(|_| "clock_unavailable")?;
    Ok(time
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "clock_unavailable")?
        .as_millis()
        .to_string())
}

fn with_device_info(mut body: String, info: Option<&DeviceInfo>) -> String {
    if let Some(info) = info {
        body.push_str("\n\n");
        body.push_str(&info.details());
    }
    body
}

fn smtp_content(message: &Message, info: Option<&DeviceInfo>) -> (&'static str, String) {
    if message.kind == Kind::Device {
        (
            "设备信息",
            info.map(DeviceInfo::details)
                .unwrap_or_else(|| "设备信息暂不可用".into()),
        )
    } else if message.kind == Kind::Power {
        ("电源状态通知", with_device_info(message.text.clone(), info))
    } else {
        (
            "新短信通知",
            with_device_info(format!("来自 {}:\n{}", message.from, message.text), info),
        )
    }
}

fn dingtalk_content(message: &Message, info: Option<&DeviceInfo>) -> String {
    if message.kind == Kind::Device {
        return info
            .map(DeviceInfo::details)
            .unwrap_or_else(|| "设备信息暂不可用".into());
    }
    let content = if message.kind == Kind::Power {
        format!("电源状态通知\n{}", message.text)
    } else {
        format!("短信来自 {}\n{}", message.from, message.text)
    };
    with_device_info(content, info)
}

pub async fn deliver(
    destination_config: &Destination,
    time_origin: &str,
    message: &Message,
) -> Result<(), String> {
    if destination_config.method == "sms" {
        return deliver_sms(destination_config, message).await;
    }
    if destination_config.method == "smtp" {
        let (subject, body) = smtp_content(message, destination_config.device_info.as_ref());
        return smtp_forward::send(&destination_config.smtp, subject, &body).await;
    }
    let method = destination_config.method.as_str();
    let mut url = destination(
        if method == "dingtalk" {
            &destination_config.dingtalk_webhook
        } else {
            &destination_config.webhook_url
        },
        method,
    )?;
    let body = if method == "dingtalk" {
        if !destination_config.dingtalk_secret.is_empty() {
            let timestamp = trusted_timestamp_ms(time_origin).await?;
            let text = format!("{timestamp}\n{}", destination_config.dingtalk_secret);
            let key = hmac::Key::new(
                hmac::HMAC_SHA256,
                destination_config.dingtalk_secret.as_bytes(),
            );
            let signature = STANDARD.encode(hmac::sign(&key, text.as_bytes()).as_ref());
            url.query_pairs_mut()
                .append_pair("timestamp", &timestamp)
                .append_pair("sign", &signature);
        }
        let content = dingtalk_content(message, destination_config.device_info.as_ref());
        json!({"msgtype":"text","text":{"content":content}})
    } else if message.kind == Kind::Device {
        let info = destination_config
            .device_info
            .as_ref()
            .ok_or("source_unavailable")?;
        json!({"kind":"device","text":info.details(),"date":message.date})
    } else if message.kind == Kind::Power {
        json!({"kind":"power","from":message.from,"text":message.text,"date":message.date})
    } else {
        json!({"from":message.from,"text":message.text,"date":message.date})
    };
    let client = public_client(&url).await?;
    post_payload(&client, url, &body, method).await
}

fn sms_message_hex(message: &Message, info: Option<&DeviceInfo>) -> Result<String, String> {
    let mut body = if message.kind == Kind::Device {
        let line = info.map(DeviceInfo::sms_line).unwrap_or_default();
        if line.is_empty() {
            return Err("source_unavailable".into());
        }
        format!("设备信息:\n{line}")
    } else if message.kind == Kind::Power {
        format!("电源状态通知:\n{}", message.text)
    } else {
        format!("来自 {}:\n{}", message.from, message.text)
    };
    if message.kind != Kind::Device
        && let Some(info) = info
    {
        let line = info.sms_line();
        if !line.is_empty() {
            body.push('\n');
            body.push_str(&line);
        }
    }
    let units: Vec<_> = body.encode_utf16().collect();
    if units.is_empty() || units.len() > MAX_SMS_UNITS {
        return Err("message_too_long".into());
    }
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(units.len() * 4);
    for unit in units {
        write!(&mut hex, "{unit:04X}").map_err(|_| "delivery_failed")?;
    }
    Ok(hex)
}

async fn deliver_sms(destination_config: &Destination, message: &Message) -> Result<(), String> {
    let message_hex = sms_message_hex(message, destination_config.device_info.as_ref())?;
    let clock = reboot_schedule::local_clock().ok_or("clock_unavailable")?;
    let sms_time = format!(
        "{};{};{};{};{};00;+;0",
        &clock.date[2..4],
        &clock.date[5..7],
        &clock.date[8..10],
        &clock.time[0..2],
        &clock.time[3..5]
    );
    let mut sent = 0;
    for phone in &destination_config.sms_to_phone {
        if phone_matches(phone, &message.from) {
            continue;
        }
        let params =
            json!({"sender":"host","number":phone,"message_hex":message_hex,"sms_time":sms_time});
        sms::send(&params).await.map_err(|_| "delivery_failed")?;
        sent += 1;
    }
    if sent == 0 {
        return Err("self_forward_blocked".into());
    }
    Ok(())
}

async fn post_payload(client: &Client, url: Url, body: &Value, method: &str) -> Result<(), String> {
    let request = serde_json::to_vec(body).map_err(|_| "delivery_failed")?;
    let mut response = client
        .post(url)
        .header("content-type", "application/json")
        .body(request)
        .send()
        .await
        .map_err(|_| "delivery_failed")?;
    if !response.status().is_success() || response.content_length().is_some_and(|len| len > 4096) {
        return Err("delivery_failed".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "delivery_failed")? {
        if bytes.len() + chunk.len() > 4096 {
            return Err("delivery_failed".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    if method == "dingtalk"
        && serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|value| value.get("errcode").and_then(Value::as_i64))
            != Some(0)
    {
        return Err("delivery_failed".into());
    }
    Ok(())
}

pub async fn baseline() -> Result<Value, String> {
    sms::snapshot().await.ok_or("sms_list_unavailable".into())
}

pub async fn fresh_baseline() -> Result<Value, String> {
    sms::invalidate();
    baseline().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::post};
    use std::net::Ipv4Addr;
    use std::sync::{Arc, Mutex};
    #[test]
    fn destinations_and_ips_are_restricted() {
        assert!(destination("https://example.com/hook", "webhook").is_ok());
        assert!(
            destination(
                "https://oapi.dingtalk.com/robot/send?access_token=redacted",
                "dingtalk"
            )
            .is_ok()
        );
        for url in [
            "http://example.com/x",
            "https://127.0.0.1/x",
            "https://user:pass@example.com/x",
            "https://localhost/x",
            "https://example.local/x",
            "https://example.com/x#fragment",
        ] {
            assert!(destination(url, "webhook").is_err(), "{url}");
        }
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.1.1",
            "192.168.0.1",
            "100.64.0.1",
            "169.254.1.1",
            "203.0.113.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(public_ip(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        assert!(public_ip("2606:4700:4700::1111".parse().unwrap()));
    }
    #[test]
    fn signed_webhook_time_uses_trusted_http_date_not_device_clock() {
        assert_eq!(
            timestamp_from_http_date("Tue, 29 Sep 2026 04:02:23 GMT").unwrap(),
            "1790654543000"
        );
        assert!(timestamp_from_http_date("not-a-date").is_err());
    }
    #[test]
    fn only_incoming_sms_are_forwarding_candidates() {
        let sent = json!({"stale":false,"truncated":false,"list":[{"id":3,"num":"10086","date":"09-29 11:00","tag":3,"text":"outgoing"}]});
        assert!(messages(&sent).unwrap().is_empty());
        let unknown = json!({"stale":false,"truncated":false,"list":[{"id":4,"num":"10086","date":"09-29 11:00","text":"unknown"}]});
        assert!(messages(&unknown).is_err());
    }
    #[test]
    fn blacklist_filters_incoming_sms_and_persists_skipped_fingerprints() {
        let dir = std::env::temp_dir().join(format!(
            "datad-sms-rules-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut manager = Forwarder::load(&dir);
        let update = |phones, keywords| Update {
            enabled: true,
            method: "webhook".into(),
            webhook_url: Some("https://example.com/hook".into()),
            dingtalk_webhook: None,
            dingtalk_secret: None,
            sms_to_phone: None,
            smtp: None,
            power_forward_enabled: None,
            blacklist_phone: Some(phones),
            blacklist_keywords: Some(keywords),
            nickname: None,
            smtp_forward_device_info: None,
            dingtalk_forward_device_info: None,
            sms_forward_device_info: None,
        };
        let empty = json!({"stale":false,"truncated":false,"list":[]});
        let status = manager
            .update(
                update(vec!["+10086".into()], vec!["验证码".into(), "敏感".into()]),
                Some(&empty),
                None,
            )
            .unwrap();
        assert_eq!(status["blacklist_phone_count"], 1);
        assert_eq!(status["blacklist_keywords_count"], 2);
        assert!(status.to_string().len() < 768);
        assert!(!status.to_string().contains("10086"));
        assert!(!status.to_string().contains("验证码"));
        let fresh = json!({"stale":false,"truncated":false,"list":[
            {"id":3,"num":"10010","date":"09-29 11:03","tag":1,"text":"普通通知"},
            {"id":2,"num":"10010","date":"09-29 11:02","tag":1,"text":"验证码123"},
            {"id":1,"num":"10086","date":"09-29 11:01","tag":1,"text":"服务消息"}
        ]});
        let message = manager.next(&fresh).unwrap().unwrap();
        assert_eq!(message.text, "普通通知");
        assert!(manager.next(&fresh).unwrap().is_none());
        let mut reloaded = Forwarder::load(&dir);
        reloaded
            .update(update(Vec::new(), Vec::new()), None, None)
            .unwrap();
        assert!(reloaded.next(&fresh).unwrap().is_none());
        assert_eq!(reloaded.status()["blacklist_phone_count"], 0);
        assert_eq!(reloaded.status()["blacklist_keywords_count"], 0);
        assert_eq!(
            fs::metadata(dir.join(FILE_NAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(
            reloaded
                .update(
                    update(vec!["10086".into(), "+10086".into()], Vec::new()),
                    None,
                    None
                )
                .is_err()
        );
        assert!(
            reloaded
                .update(
                    update(Vec::new(), vec!["字".repeat(MAX_KEYWORD_BYTES)]),
                    None,
                    None
                )
                .is_err()
        );
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn phone_forwarding_validates_targets_length_and_persistent_daily_quota() {
        let dir = std::env::temp_dir().join(format!(
            "datad-phone-forward-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut manager = Forwarder::load(&dir);
        let old = json!({"stale":false,"truncated":false,"list":[]});
        manager
            .update(
                Update {
                    enabled: true,
                    method: "sms".into(),
                    webhook_url: None,
                    dingtalk_webhook: None,
                    dingtalk_secret: None,
                    sms_to_phone: Some(vec!["+10086".into(), "10010".into()]),
                    smtp: None,
                    power_forward_enabled: None,
                    blacklist_phone: None,
                    blacklist_keywords: None,
                    nickname: None,
                    smtp_forward_device_info: None,
                    dingtalk_forward_device_info: None,
                    sms_forward_device_info: None,
                },
                Some(&old),
                None,
            )
            .unwrap();
        let message = Message {
            from: "10086".into(),
            text: "测试".into(),
            date: String::new(),
            kind: Kind::Sms,
        };
        assert_eq!(
            sms_message_hex(&message, None),
            Ok("676581EA002000310030003000380036003A000A6D4B8BD5".into())
        );
        manager.reserve_sms(&message).unwrap();
        assert_eq!(
            manager.status()["sms_daily_remaining"],
            MAX_SMS_DAILY_SENDS - 1
        );
        let reloaded = Forwarder::load(&dir);
        assert_eq!(
            reloaded.status()["sms_daily_remaining"],
            MAX_SMS_DAILY_SENDS - 1
        );
        let mut invalid = reloaded.config.clone();
        invalid.sms_to_phone = vec!["+10086".into(), "10086".into()];
        assert!(!valid_config(&invalid));
        invalid.sms_to_phone = vec!["10086;reboot".into()];
        assert!(!valid_config(&invalid));
        let long = Message {
            from: "10086".into(),
            text: "字".repeat(MAX_SMS_UNITS),
            date: String::new(),
            kind: Kind::Sms,
        };
        assert!(sms_message_hex(&long, None).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pre_phone_schema_loads_with_forwarding_disabled() {
        let dir = std::env::temp_dir().join(format!(
            "datad-phone-migrate-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(FILE_NAME),br#"{"schema":1,"enabled":false,"method":"webhook","webhook_url":"","dingtalk_webhook":"","dingtalk_secret":"","seen":[]}"#).unwrap();
        let manager = Forwarder::load(&dir);
        assert_eq!(manager.status()["enabled"], false);
        assert_eq!(manager.status()["sms_configured"], false);
        assert_eq!(manager.status()["smtp_configured"], false);
        assert_eq!(manager.status()["power_forward_enabled"], false);
        assert_eq!(
            manager.status()["power_daily_remaining"],
            MAX_POWER_DAILY_SENDS
        );
        assert_eq!(manager.status()["last_result"], "idle");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn power_events_baseline_before_delivery_and_do_not_replay_after_restart() {
        let dir = std::env::temp_dir().join(format!(
            "datad-power-forward-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut manager = Forwarder::load(&dir);
        let first = json!({"percent":50,"charging":1});
        let second = json!({"percent":51,"charging":1});
        let third = json!({"percent":52,"charging":1});
        let full = json!({"percent":52,"charging":4});
        let sms_baseline = json!({"stale":false,"truncated":false,"list":[]});
        assert!(power_state(Some(&first)).is_some());
        assert!(power_state(Some(&json!({"percent":100,"charging":0}))).is_none());
        assert!(power_state(Some(&json!({"percent":101,"charging":4}))).is_none());
        assert!(manager.observe_power(Some(&first)).unwrap().is_none());
        assert!(!dir.join(FILE_NAME).exists());
        let settings = |enabled, power_forward_enabled| Update {
            enabled,
            method: "webhook".into(),
            webhook_url: Some("https://example.com/hook".into()),
            dingtalk_webhook: None,
            dingtalk_secret: None,
            sms_to_phone: None,
            smtp: None,
            power_forward_enabled,
            blacklist_phone: None,
            blacklist_keywords: None,
            nickname: None,
            smtp_forward_device_info: None,
            dingtalk_forward_device_info: None,
            sms_forward_device_info: None,
        };
        assert_eq!(
            manager
                .update(settings(false, Some(true)), None, None)
                .unwrap_err(),
            "power_unavailable"
        );
        assert_eq!(
            manager
                .update(settings(false, Some(true)), None, Some(&first))
                .unwrap()["power_forward_enabled"],
            true
        );
        assert!(manager.observe_power(Some(&second)).unwrap().is_none());
        manager
            .update(settings(true, None), Some(&sms_baseline), Some(&second))
            .unwrap();
        assert!(manager.observe_power(Some(&second)).unwrap().is_none());
        let event = manager.observe_power(Some(&third)).unwrap().unwrap();
        assert_eq!(event.kind, Kind::Power);
        assert!(event.text.contains("+1%"));
        let (subject, body) = smtp_content(&event, None);
        assert_eq!(subject, "电源状态通知");
        assert_eq!(body, event.text);
        let phone_hex = sms_message_hex(&event, None).unwrap();
        let units: Vec<u16> = phone_hex
            .as_bytes()
            .chunks_exact(4)
            .map(|digits| u16::from_str_radix(std::str::from_utf8(digits).unwrap(), 16).unwrap())
            .collect();
        assert!(
            String::from_utf16(&units)
                .unwrap()
                .starts_with("电源状态通知:")
        );
        assert_eq!(manager.status()["power_daily_remaining"], 59);
        let mut restarted = Forwarder::load(&dir);
        assert!(restarted.observe_power(Some(&third)).unwrap().is_none());
        let event = restarted.observe_power(Some(&full)).unwrap().unwrap();
        assert!(event.text.contains("已充满"));
        assert_eq!(restarted.status()["power_daily_remaining"], 58);
        restarted
            .update(settings(false, None), None, Some(&full))
            .unwrap();
        assert!(
            restarted
                .observe_power(Some(&json!({"percent":53,"charging":4})))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            fs::metadata(dir.join(FILE_NAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn power_events_fail_closed_without_battery_or_after_daily_limit() {
        let dir = std::env::temp_dir().join(format!(
            "datad-power-limit-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut manager = Forwarder::load(&dir);
        let first = json!({"percent":90,"charging":2});
        let mut config = manager.config.clone();
        config.enabled = true;
        config.webhook_url = "https://example.com/hook".into();
        config.power_forward_enabled = true;
        config.power_last_percent = Some(90);
        config.power_last_charging = Some(2);
        config.power_quota_date = reboot_schedule::local_clock().unwrap().date;
        config.power_quota_used = MAX_POWER_DAILY_SENDS;
        manager.save(&config).unwrap();
        manager.config = config;
        assert!(
            manager
                .observe_power(Some(&json!({"percent":91,"charging":2})))
                .unwrap()
                .is_none()
        );
        assert_eq!(manager.status()["power_last_result"], "rate_limited");
        assert!(manager.observe_power(None).unwrap().is_none());
        assert_eq!(manager.status()["power_last_result"], "source_unavailable");
        assert!(manager.observe_power(Some(&first)).unwrap().is_none());
        assert_eq!(manager.status()["power_last_result"], "idle");
        let status = Forwarder::load(&dir).status();
        assert_eq!(status["power_daily_remaining"], 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn smtp_credentials_stay_in_private_config_and_never_enter_panel_status() {
        let dir = std::env::temp_dir().join(format!(
            "datad-smtp-forward-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut manager = Forwarder::load(&dir);
        let update = |smtp| Update {
            enabled: false,
            method: "smtp".into(),
            webhook_url: None,
            dingtalk_webhook: None,
            dingtalk_secret: None,
            sms_to_phone: None,
            smtp: Some(smtp),
            power_forward_enabled: None,
            blacklist_phone: None,
            blacklist_keywords: None,
            nickname: None,
            smtp_forward_device_info: None,
            dingtalk_forward_device_info: None,
            sms_forward_device_info: None,
        };
        let status = manager
            .update(
                update(smtp_forward::Update {
                    host: "smtp.example.com".into(),
                    port: 465,
                    username: "sender@example.com".into(),
                    password: Some("fixture-app-password".into()),
                    to: "recipient@example.net".into(),
                }),
                None,
                None,
            )
            .unwrap();
        assert_eq!(status["enabled"], false);
        assert_eq!(status["smtp_configured"], true);
        for secret in [
            "fixture-app-password",
            "sender@example.com",
            "recipient@example.net",
        ] {
            assert!(!status.to_string().contains(secret));
        }
        assert_eq!(
            fs::metadata(dir.join(FILE_NAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let mut reloaded = Forwarder::load(&dir);
        assert_eq!(reloaded.status()["smtp_configured"], true);
        reloaded
            .update(
                update(smtp_forward::Update {
                    host: String::new(),
                    port: 0,
                    username: String::new(),
                    password: Some(String::new()),
                    to: String::new(),
                }),
                None,
                None,
            )
            .unwrap();
        assert_eq!(reloaded.status()["smtp_configured"], false);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn enabling_baselines_history_and_marks_new_sms_before_delivery() {
        let dir = std::env::temp_dir().join(format!(
            "datad-forward-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut manager = Forwarder::load(&dir);
        let old = json!({"stale":false,"truncated":false,"list":[{"id":1,"num":"10001","date":"09-29 11:00","tag":1,"text":"old"}]});
        assert!(
            manager
                .update(
                    Update {
                        enabled: true,
                        method: "webhook".into(),
                        webhook_url: Some("https://example.com/hook".into()),
                        dingtalk_webhook: None,
                        dingtalk_secret: None,
                        sms_to_phone: None,
                        smtp: None,
                        power_forward_enabled: None,
                        blacklist_phone: None,
                        blacklist_keywords: None,
                        nickname: None,
                        smtp_forward_device_info: None,
                        dingtalk_forward_device_info: None,
                        sms_forward_device_info: None
                    },
                    Some(&old),
                    None
                )
                .is_ok()
        );
        assert!(!manager.status().to_string().contains("example.com"));
        assert!(manager.next(&old).unwrap().is_none());
        let fresh = json!({"stale":false,"truncated":false,"list":[{"id":2,"num":"10002","date":"09-29 11:01","tag":1,"text":"new"},{"id":1,"num":"10001","date":"09-29 11:00","tag":1,"text":"old"}]});
        let next = manager.next(&fresh).unwrap().unwrap();
        assert_eq!(next.text, "new");
        assert!(manager.next(&fresh).unwrap().is_none());
        let mut reloaded = Forwarder::load(&dir);
        assert!(reloaded.next(&fresh).unwrap().is_none());
        assert_eq!(
            fs::metadata(dir.join(FILE_NAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn nickname_migrates_persists_and_clears_without_enabling_forwarding() {
        let dir = std::env::temp_dir().join(format!(
            "datad-forward-nickname-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(FILE_NAME),
            br#"{"schema":1,"enabled":false,"method":"webhook","webhook_url":"","dingtalk_webhook":"","dingtalk_secret":"","seen":[]}"#,
        )
        .unwrap();
        let mut manager = Forwarder::load(&dir);
        assert_eq!(manager.status()["nickname"], "");
        assert_eq!(manager.status()["nickname_supported"], true);
        let update = |nickname: String| {
            serde_json::from_value::<Update>(json!({
                "enabled":false,"method":"webhook","nickname":nickname
            }))
            .unwrap()
        };
        for bad in [
            " padded ".to_string(),
            "line\nbreak".into(),
            "a".repeat(256),
        ] {
            assert_eq!(
                manager.update(update(bad), None, None).unwrap_err(),
                "invalid_forward_config"
            );
        }
        let status = manager
            .update(update("客厅 U60".into()), None, None)
            .unwrap();
        assert_eq!(status["nickname"], "客厅 U60");
        assert_eq!(status["enabled"], false);
        assert_eq!(
            fs::metadata(dir.join(FILE_NAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let mut reloaded = Forwarder::load(&dir);
        assert_eq!(reloaded.status()["nickname"], "客厅 U60");
        assert_eq!(
            reloaded.update(update(String::new()), None, None).unwrap()["nickname"],
            ""
        );
        assert_eq!(Forwarder::load(&dir).status()["nickname"], "");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn device_info_flags_are_independent_off_by_default_and_persist() {
        let dir = std::env::temp_dir().join(format!(
            "datad-forward-info-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut manager = Forwarder::load(&dir);
        for key in [
            "smtp_forward_device_info",
            "dingtalk_forward_device_info",
            "sms_forward_device_info",
        ] {
            assert_eq!(manager.status()[key], false);
        }
        let update = |method: &str, flag: &str, enabled: bool| {
            let mut value = json!({"enabled":false,"method":method});
            value[flag] = json!(enabled);
            serde_json::from_value::<Update>(value).unwrap()
        };
        manager
            .update(update("smtp", "smtp_forward_device_info", true), None, None)
            .unwrap();
        assert_eq!(manager.status()["enabled"], false);
        assert!(manager.delivery().include_device_info);
        manager
            .update(update("sms", "sms_forward_device_info", true), None, None)
            .unwrap();
        assert!(manager.delivery().include_device_info);
        let mut reloaded = Forwarder::load(&dir);
        assert_eq!(reloaded.status()["smtp_forward_device_info"], true);
        assert_eq!(reloaded.status()["sms_forward_device_info"], true);
        assert_eq!(reloaded.status()["dingtalk_forward_device_info"], false);
        reloaded
            .update(
                update("webhook", "sms_forward_device_info", false),
                None,
                None,
            )
            .unwrap();
        assert!(!reloaded.delivery().include_device_info);
        assert_eq!(reloaded.status()["sms_forward_device_info"], false);
        assert_eq!(
            fs::metadata(dir.join(FILE_NAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn device_info_uses_bounded_status_without_leaking_identifiers_to_mail_or_dingtalk() {
        let fields = json!({
            "device":{"market_name":"U60 Pro"},
            "system":{"sw_version":"B28","uptime":3600,"cpu_usage":17,"mem_used_pct":52,
                "imei":"863500074315883"},
            "sim":{"msisdn":"+8613800000000","iccid":"8986000000000000000","imsi":"460000000000001"},
            "battery":{"percent":66,"temp":32.5},
            "thermal":{"cpu_celsius":41.0},
            "traffic":{"day_rx_bytes":1024,"day_tx_bytes":1024,
                "month_rx_bytes":1048576,"month_tx_bytes":1048576}
        });
        let snapshot = Snapshot {
            ts: 0,
            datad: crate::model::DatadVersion::default(),
            fields: fields.as_object().unwrap().clone(),
        };
        let info = DeviceInfo::from_snapshot(&snapshot, "客厅");
        let details = info.details();
        for expected in [
            "U60 Pro (客厅)",
            "B28",
            "2.00 KB",
            "2.00 MB",
            "66%",
            "41.0°C",
        ] {
            assert!(details.contains(expected), "missing {expected}");
        }
        for secret in [
            "863500074315883",
            "8986000000000000000",
            "460000000000001",
            "+8613800000000",
        ] {
            assert!(!details.contains(secret), "identifier in verbose status");
        }
        assert_eq!(info.sms_line(), "U60 Pro 66% +8613800000000");
        let message = Message::test();
        let (_, mail) = smtp_content(&message, Some(&info));
        assert!(mail.starts_with("来自 NMS:\nzwrt-datad 短信转发测试"));
        assert!(mail.contains("设备信息\n设备名称: U60 Pro (客厅)"));
        let dingtalk = dingtalk_content(&message, Some(&info));
        assert!(dingtalk.starts_with("短信来自 NMS\nzwrt-datad 短信转发测试"));
        assert!(dingtalk.contains("设备信息\n设备名称: U60 Pro (客厅)"));
        assert!(!dingtalk.contains("+8613800000000"));
        let hex = sms_message_hex(&message, Some(&info)).unwrap();
        let units = (0..hex.len())
            .step_by(4)
            .map(|index| u16::from_str_radix(&hex[index..index + 4], 16).unwrap())
            .collect::<Vec<_>>();
        let sms = String::from_utf16(&units).unwrap();
        assert!(sms.ends_with("U60 Pro 66% +8613800000000"));
        assert!(!sms.contains("863500074315883"));
        let mut unsafe_fields = fields.as_object().unwrap().clone();
        unsafe_fields.insert("sim".into(), json!({"msisdn":"10086\r\nBcc:target"}));
        let unsafe_snapshot = Snapshot {
            fields: unsafe_fields,
            ..snapshot
        };
        assert!(
            !DeviceInfo::from_snapshot(&unsafe_snapshot, "")
                .sms_line()
                .contains("Bcc")
        );
    }

    #[test]
    fn corrupt_config_disables_delivery_without_exposing_secrets() {
        let dir = std::env::temp_dir().join(format!(
            "datad-forward-corrupt-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(FILE_NAME), br#"{"schema":1,"enabled":true,"method":"webhook","webhook_url":"http://127.0.0.1/private","dingtalk_webhook":"","dingtalk_secret":"must-not-leak","seen":[]}"#).unwrap();
        let mut manager = Forwarder::load(&dir);
        assert_eq!(manager.status()["enabled"], false);
        assert_eq!(manager.status()["last_result"], "invalid_config");
        assert!(!manager.status().to_string().contains("must-not-leak"));
        assert!(
            manager
                .next(&json!({"stale":false,"truncated":false,"list":[]}))
                .unwrap()
                .is_none()
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn fixed_post_sends_only_bounded_json_and_rejects_business_errors() {
        let received = Arc::new(Mutex::new(Vec::<Value>::new()));
        let sink = received.clone();
        let router = Router::new()
            .route(
                "/hook",
                post(move |Json(body): Json<Value>| {
                    let sink = sink.clone();
                    async move {
                        sink.lock().unwrap().push(body);
                        Json(json!({"errcode":0}))
                    }
                }),
            )
            .route("/error", post(|| async { Json(json!({"errcode":310000})) }))
            .route("/large", post(|| async { "x".repeat(5000) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let payload = json!({"from":"10001","text":"synthetic test","date":"09-29 11:00"});
        post_payload(
            &client,
            format!("http://{addr}/hook").parse().unwrap(),
            &payload,
            "webhook",
        )
        .await
        .unwrap();
        assert_eq!(
            received.lock().unwrap().as_slice(),
            std::slice::from_ref(&payload)
        );
        let pinned = pinned_client(
            "example.test",
            &[SocketAddr::from(([127, 0, 0, 1], 1)), addr],
        )
        .unwrap();
        post_payload(
            &pinned,
            format!("http://example.test:{}/hook", addr.port())
                .parse()
                .unwrap(),
            &payload,
            "webhook",
        )
        .await
        .unwrap();
        assert_eq!(received.lock().unwrap().len(), 2);
        assert!(
            post_payload(
                &client,
                format!("http://{addr}/error").parse().unwrap(),
                &json!({}),
                "dingtalk"
            )
            .await
            .is_err()
        );
        assert!(
            post_payload(
                &client,
                format!("http://{addr}/large").parse().unwrap(),
                &json!({}),
                "webhook"
            )
            .await
            .is_err()
        );
        server.abort();
    }
}
