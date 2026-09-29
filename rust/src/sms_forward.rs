//! Opt-in SMS forwarding over fixed HTTPS POST. This does not execute UFI's
//! legacy curl_text or copy its local forwarding database.
use crate::sms;
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

const FILE_NAME: &str = "sms-forward.json";
const MAX_FILE_BYTES: u64 = 128 * 1024;
const MAX_SEEN: usize = 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    schema: u8,
    enabled: bool,
    method: String,
    webhook_url: String,
    dingtalk_webhook: String,
    dingtalk_secret: String,
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
            seen: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub enabled: bool,
    pub method: String,
    pub webhook_url: Option<String>,
    pub dingtalk_webhook: Option<String>,
    pub dingtalk_secret: Option<String>,
}

#[derive(Clone)]
pub struct Message {
    from: String,
    text: String,
    date: String,
}

impl Message {
    pub fn test() -> Self {
        Self {
            from: "NMS".into(),
            text: "zwrt-datad 短信转发测试".into(),
            date: String::new(),
        }
    }
}

pub struct Forwarder {
    path: PathBuf,
    config: Config,
    last_result: &'static str,
}

fn public_ip(ip: IpAddr) -> bool {
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

fn valid_config(config: &Config) -> bool {
    config.schema == 1
        && matches!(config.method.as_str(), "webhook" | "dingtalk")
        && config.dingtalk_secret.len() <= 256
        && !config.dingtalk_secret.chars().any(char::is_control)
        && config.seen.len() <= MAX_SEEN
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
            },
        ));
    }
    Ok(output)
}

impl Forwarder {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join(FILE_NAME);
        let config = if path.exists() {
            fs::symlink_metadata(&path)
                .ok()
                .filter(|metadata| metadata.is_file() && metadata.len() <= MAX_FILE_BYTES)
                .and_then(|_| fs::read(&path).ok())
                .and_then(|raw| serde_json::from_slice::<Config>(&raw).ok())
                .filter(valid_config)
        } else {
            Some(Config::default())
        };
        let failed = config.is_none();
        Self {
            path,
            config: config.unwrap_or_default(),
            last_result: if failed { "invalid_config" } else { "idle" },
        }
    }

    pub fn status(&self) -> Value {
        json!({"supported":true,"enabled":self.config.enabled,"method":self.config.method,
            "webhook_configured":!self.config.webhook_url.is_empty(),
            "dingtalk_configured":!self.config.dingtalk_webhook.is_empty(),
            "dingtalk_secret_configured":!self.config.dingtalk_secret.is_empty(),
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
            let mut raw = serde_json::to_vec(next).map_err(|_| "forward_storage_failed")?;
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

    pub fn update(&mut self, input: Update, snapshot: Option<&Value>) -> Result<Value, String> {
        let mut next = self.config.clone();
        next.enabled = input.enabled;
        next.method = input.method;
        if let Some(url) = input.webhook_url {
            next.webhook_url = url;
        }
        if let Some(url) = input.dingtalk_webhook {
            next.dingtalk_webhook = url;
        }
        if let Some(secret) = input.dingtalk_secret {
            next.dingtalk_secret = secret;
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
        Ok(self.status())
    }

    /// Persist the fingerprint before delivery: a crash after remote success
    /// cannot re-send SMS content on restart. Failed delivery is not retried.
    pub fn next(&mut self, snapshot: &Value) -> Result<Option<Message>, String> {
        if !self.config.enabled {
            return Ok(None);
        }
        let seen: HashSet<_> = self.config.seen.iter().map(String::as_str).collect();
        let Some((hash, message)) = messages(snapshot)?
            .into_iter()
            .find(|(hash, _)| !seen.contains(hash.as_str()))
        else {
            return Ok(None);
        };
        let mut next = self.config.clone();
        next.seen.push(hash);
        if next.seen.len() > MAX_SEEN {
            next.seen.drain(..next.seen.len() - MAX_SEEN);
        }
        self.save(&next)?;
        self.config = next;
        Ok(Some(message))
    }

    pub fn mark_delivery(&mut self, result: &Result<(), String>) {
        self.last_result = match result {
            Ok(()) => "sent",
            Err(error) if error == "clock_unavailable" => "clock_unavailable",
            Err(_) => "delivery_failed",
        };
    }

    pub fn mark_source_unavailable(&mut self) {
        if self.config.enabled {
            self.last_result = "source_unavailable";
        }
    }

    pub fn mark_storage_failed(&mut self) {
        self.last_result = "storage_failed";
    }

    pub fn delivery(&self) -> (String, String, String, String) {
        (
            self.config.method.clone(),
            self.config.webhook_url.clone(),
            self.config.dingtalk_webhook.clone(),
            self.config.dingtalk_secret.clone(),
        )
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

pub async fn deliver(
    method: &str,
    webhook_url: &str,
    dingtalk_url: &str,
    secret: &str,
    time_origin: &str,
    message: &Message,
) -> Result<(), String> {
    let mut url = destination(
        if method == "dingtalk" {
            dingtalk_url
        } else {
            webhook_url
        },
        method,
    )?;
    let body = if method == "dingtalk" {
        if !secret.is_empty() {
            let timestamp = trusted_timestamp_ms(time_origin).await?;
            let text = format!("{timestamp}\n{secret}");
            let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
            let signature = STANDARD.encode(hmac::sign(&key, text.as_bytes()).as_ref());
            url.query_pairs_mut()
                .append_pair("timestamp", &timestamp)
                .append_pair("sign", &signature);
        }
        json!({"msgtype":"text","text":{"content":format!("短信来自 {}\n{}",message.from,message.text)}})
    } else {
        json!({"from":message.from,"text":message.text,"date":message.date})
    };
    let client = public_client(&url).await?;
    post_payload(&client, url, &body, method).await
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
    fn enabling_baselines_history_and_marks_new_sms_before_delivery() {
        let dir = std::env::temp_dir().join(format!(
            "datad-forward-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut manager = Forwarder::load(&dir);
        let old = json!({"stale":false,"truncated":false,"list":[{"id":1,"num":"10001","date":"09-29 11:00","text":"old"}]});
        assert!(
            manager
                .update(
                    Update {
                        enabled: true,
                        method: "webhook".into(),
                        webhook_url: Some("https://example.com/hook".into()),
                        dingtalk_webhook: None,
                        dingtalk_secret: None
                    },
                    Some(&old)
                )
                .is_ok()
        );
        assert!(!manager.status().to_string().contains("example.com"));
        assert!(manager.next(&old).unwrap().is_none());
        let fresh = json!({"stale":false,"truncated":false,"list":[{"id":2,"num":"10002","date":"09-29 11:01","text":"new"},{"id":1,"num":"10001","date":"09-29 11:00","text":"old"}]});
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
