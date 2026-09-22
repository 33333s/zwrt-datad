use crate::state;
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rand::{RngCore, rngs::OsRng};
use reqwest::redirect::Policy;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey, pkcs1::DecodeRsaPublicKey, pkcs8::DecodePublicKey};
use serde_json::{Value, json};
use std::{
    sync::{
        LazyLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use zeroize::Zeroize;

static SESSION_KEY: LazyLock<Mutex<Option<[u8; 32]>>> = LazyLock::new(|| Mutex::new(None));
// Serialize our own reads and sends; the vendor session can also be replaced
// by its web UI, so reads must still recover from authentication failures.
static SMS_IO: Mutex<()> = Mutex::const_new(());
static CACHE: LazyLock<Mutex<ReadCache>> = LazyLock::new(|| Mutex::new(ReadCache::default()));
static REVISION: AtomicU64 = AtomicU64::new(0);
const PAGE_SIZE: usize = 64;
const MAX_PAGES: usize = 4;
const MAX_MESSAGES: usize = PAGE_SIZE * MAX_PAGES * 2;

#[derive(Default)]
struct ReadCache {
    checked: Option<Instant>,
    refreshed: Option<Instant>,
    revision: u64,
    capacity: Option<Value>,
    list: Vec<Value>,
    error: Option<String>,
    truncated: bool,
}

impl ReadCache {
    fn value(&self) -> Option<Value> {
        let capacity = self.capacity.as_ref()?;
        let number = |name| {
            capacity
                .get(name)
                .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
                .unwrap_or(0)
        };
        let mut value = json!({"unread":number("sms_dev_unread_num") + number("sms_sim_unread_num"),
            "list":self.list,"stale":self.error.is_some(),"truncated":self.truncated});
        if let Some(error) = &self.error {
            value["error"] = json!(error);
        }
        Some(value)
    }
}

pub fn invalidate() {
    REVISION.fetch_add(1, Ordering::Relaxed);
}

fn valid(number: &str, message: &str, sms_time: &str) -> bool {
    !number.is_empty()
        && number.len() <= 32
        && number
            .bytes()
            .enumerate()
            .all(|(index, byte)| byte.is_ascii_digit() || (index == 0 && byte == b'+'))
        && !message.is_empty()
        && message.len() <= 4096
        && message.len() & 1 == 0
        && message.bytes().all(|byte| byte.is_ascii_hexdigit())
        && !sms_time.is_empty()
        && sms_time.len() <= 64
        && sms_time
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b';' | b'+' | b'-'))
}

fn public_key(pem: &str) -> Result<RsaPublicKey, String> {
    // Some firmware removes every PEM newline, including after the header.
    // Decode the bounded, whitespace-normalized body without weakening DER
    // validation or confusing PKCS#1 and SubjectPublicKeyInfo containers.
    if pem.len() <= 16_384 {
        for label in ["PUBLIC KEY", "RSA PUBLIC KEY"] {
            let begin = format!("-----BEGIN {label}-----");
            let end = format!("-----END {label}-----");
            if let Some(body) = pem
                .trim()
                .strip_prefix(&begin)
                .and_then(|s| s.strip_suffix(&end))
            {
                let body: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
                if let Ok(der) = STANDARD.decode(body) {
                    let parsed = if label == "PUBLIC KEY" {
                        RsaPublicKey::from_public_key_der(&der).map_err(|_| ())
                    } else {
                        RsaPublicKey::from_pkcs1_der(&der).map_err(|_| ())
                    };
                    if let Ok(key) = parsed {
                        return Ok(key);
                    }
                }
            }
        }
    }
    Err("invalid vendor RSA public key".into())
}

async fn reset_crypto_session() {
    if let Some(mut key) = SESSION_KEY.lock().await.take() {
        key.zeroize();
    }
}

async fn ensure_crypto_session() -> Result<[u8; 32], String> {
    let mut guard = SESSION_KEY.lock().await;
    if let Some(key) = *guard {
        return Ok(key);
    }
    let reply = state::ubus("zwrt_web", "web_crt_get", json!({})).await?;
    let pem = reply
        .get("result")
        .and_then(Value::as_str)
        .ok_or("vendor RSA certificate unavailable")?;
    let public = public_key(pem)?;
    let mut key = [0_u8; 32];
    OsRng.fill_bytes(&mut key);
    let mut hex = String::with_capacity(64);
    for byte in key {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").map_err(|e| e.to_string())?;
    }
    let wrapped = public
        .encrypt(&mut OsRng, Pkcs1v15Encrypt, hex.as_bytes())
        .map_err(|_| "vendor RSA key registration failed")?;
    hex.zeroize();
    let registration = state::ubus(
        "zwrt_web",
        "web_http_enstr_set",
        json!({"web_enstr":STANDARD.encode(wrapped)}),
    )
    .await;
    if let Err(error) = registration {
        key.zeroize();
        return Err(error);
    }
    if registration
        .as_ref()
        .ok()
        .and_then(|v| v.get("result"))
        .is_some_and(|v| {
            !matches!(v.as_str(), Some("success" | "0"))
                && v.as_i64() != Some(0)
                && v.as_bool() != Some(true)
        })
    {
        key.zeroize();
        return Err("vendor RSA key registration rejected".into());
    }
    *guard = Some(key);
    Ok(key)
}

async fn read_pages() -> Result<(Vec<Value>, bool), String> {
    let mut replies = Vec::new();
    let mut truncated = false;
    for store in [1, 0] {
        let mut seen = std::collections::HashSet::new();
        for page in 0..MAX_PAGES {
            let reply = state::ubus("zwrt_wms", "zte_libwms_get_sms_data", json!({
                "page":page,"data_per_page":PAGE_SIZE,"mem_store":store,"tags":10,"order_by":"order by id desc"
            })).await?;
            let rows = reply
                .get("list")
                .or_else(|| reply.get("messages"))
                .and_then(Value::as_array)
                .ok_or("invalid vendor SMS list")?;
            let count = rows.len();
            if count > PAGE_SIZE {
                return Err("vendor SMS page exceeds limit".into());
            }
            let mut new_ids = 0;
            for row in rows {
                let id = message_id(row).ok_or("invalid vendor SMS id")?;
                new_ids += usize::from(seen.insert(id));
            }
            replies.push(reply);
            if count < PAGE_SIZE {
                break;
            }
            if new_ids == 0 || page + 1 == MAX_PAGES {
                truncated = true;
                break;
            }
        }
    }
    Ok((replies, truncated))
}

async fn load_messages() -> Result<(Vec<Value>, bool), String> {
    // Unencrypted models remain readable even when the web crypto service is
    // unavailable. An encrypted response, however, must authenticate.
    let mut key = ensure_crypto_session().await.ok();
    let (replies, truncated) = read_pages().await?;
    let result = normalize_lists(&replies, key.as_ref());
    key.zeroize();
    match result {
        Ok(list) => Ok((list, truncated)),
        Err(_) => {
            reset_crypto_session().await;
            let mut key = ensure_crypto_session().await?;
            // Re-read ciphertext after registering the replacement key. Never
            // retry a send here: retransmission could duplicate real SMS.
            let result = read_pages().await;
            let result = result.and_then(|(replies, truncated)| {
                normalize_lists(&replies, Some(&key)).map(|list| (list, truncated))
            });
            key.zeroize();
            if result.is_err() {
                reset_crypto_session().await;
            }
            result
        }
    }
}

pub async fn snapshot() -> Option<Value> {
    let mut cache = CACHE.lock().await;
    let revision = REVISION.load(Ordering::Relaxed);
    if cache.revision == revision
        && cache
            .checked
            .is_some_and(|t| t.elapsed() < Duration::from_secs(5))
    {
        return cache.value();
    }
    cache.checked = Some(Instant::now());
    let capacity = match state::ubus("zwrt_wms", "zwrt_wms_get_wms_capacity", json!({})).await {
        Ok(value) => value,
        Err(_) => {
            cache.error = Some("vendor SMS capacity unavailable".into());
            return cache.value();
        }
    };
    let refresh = cache.revision != revision
        || cache.capacity.as_ref() != Some(&capacity)
        || cache.error.is_some()
        || cache
            .refreshed
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(30));
    cache.capacity = Some(capacity);
    cache.revision = revision;
    if refresh {
        let _io = SMS_IO.lock().await;
        match load_messages().await {
            Ok((list, truncated)) => {
                cache.list = list;
                cache.truncated = truncated;
                cache.error = None;
                cache.refreshed = Some(Instant::now());
            }
            Err(error) => cache.error = Some(error),
        }
    }
    cache.value()
}

fn encrypt(key: &[u8; 32], plaintext: &str) -> Result<String, String> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| "invalid AES key")?;
    let mut nonce = [0_u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let mut encrypted = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext.as_bytes())
        .map_err(|_| "SMS envelope encryption failed")?;
    if encrypted.len() < 16 {
        return Err("SMS envelope encryption failed".into());
    }
    let tag = encrypted.split_off(encrypted.len() - 16);
    let mut envelope = Vec::with_capacity(28 + encrypted.len());
    envelope.extend_from_slice(&nonce);
    envelope.extend_from_slice(&tag);
    envelope.extend_from_slice(&encrypted);
    Ok(STANDARD.encode(envelope))
}

fn decrypt(value: &str, key: Option<&[u8; 32]>) -> Result<Option<String>, String> {
    if value.len() < 40
        || value.len() & 1 == 0 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Ok(None);
    }
    let Ok(raw) = STANDARD.decode(value) else {
        return Ok(None);
    };
    if raw.len() <= 28 {
        return Ok(None);
    }
    let key = key.ok_or("vendor SMS crypto session unavailable")?;
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| "invalid AES key")?;
    let mut input = raw[28..].to_vec();
    input.extend_from_slice(&raw[12..28]);
    cipher
        .decrypt(Nonce::from_slice(&raw[..12]), input.as_ref())
        .map(|plain| Some(String::from_utf8_lossy(&plain).into_owned()))
        .map_err(|_| "SMS envelope authentication failed".into())
}

fn utf16be_hex(value: &str) -> String {
    let digits: String = value.chars().filter(|ch| ch.is_ascii_hexdigit()).collect();
    let units: Vec<u16> = digits
        .as_bytes()
        .chunks_exact(4)
        .filter_map(|chunk| std::str::from_utf8(chunk).ok())
        .filter_map(|chunk| u16::from_str_radix(chunk, 16).ok())
        .collect();
    String::from_utf16_lossy(&units)
}

fn sms_date(value: &str) -> String {
    let fields: Vec<_> = value.split(',').collect();
    if fields.len() >= 6 {
        let parsed: Option<Vec<i64>> = fields[1..=4]
            .iter()
            .map(|field| field.parse().ok())
            .collect();
        if let Some(parts) = parsed {
            return format!(
                "{:02}-{:02} {:02}:{:02}",
                parts[0], parts[1], parts[2], parts[3]
            );
        }
    }
    String::new()
}

fn message_id(item: &Value) -> Option<i64> {
    let value = item.get("id")?;
    value.as_i64().or_else(|| value.as_str()?.parse().ok())
}

fn normalize_lists(replies: &[Value], key: Option<&[u8; 32]>) -> Result<Vec<Value>, String> {
    let mut output = Vec::new();
    let mut ids = std::collections::HashSet::new();
    let items: Vec<Value> = replies
        .iter()
        .flat_map(|reply| {
            reply
                .get("list")
                .or_else(|| reply.get("messages"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    for item in items {
        if output.len() >= MAX_MESSAGES {
            break;
        }
        let id = message_id(&item).ok_or("invalid vendor SMS id")?;
        if !ids.insert(id) {
            continue;
        }
        let number_raw = item
            .get("num")
            .or_else(|| item.get("number"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let text_raw = item
            .get("text")
            .or_else(|| item.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let number = decrypt(number_raw, key)?;
        let text = decrypt(text_raw, key)?;
        let number = number.map_or_else(|| number_raw.to_owned(), |value| utf16be_hex(&value));
        let text = utf16be_hex(text.as_deref().unwrap_or(text_raw));
        let unread = item.get("tag").and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str()?.parse::<i64>().ok())
        }) == Some(1);
        output.push(json!({
            "id":id,
            "num":number,
            "date":sms_date(item.get("date").and_then(Value::as_str).unwrap_or_default()),
            "unread":i64::from(unread),
            "text":text
        }));
    }
    Ok(output)
}

async fn send_external(
    sender: &str,
    number: &str,
    message: &str,
    sms_time: &str,
) -> Result<Value, String> {
    let default = if sender == "v3e1" {
        "http://192.168.56.1/goform/goform_set_cmd_process"
    } else {
        "http://192.168.57.1/goform/goform_set_cmd_process"
    };
    let variable = if sender == "v3e1" {
        "ZWRT_DATAD_SMS_V3E1_URL"
    } else {
        "ZWRT_DATAD_SMS_V3E2_URL"
    };
    let url = std::env::var(variable).unwrap_or_else(|_| default.into());
    let encode_plus = |value: &str| value.replace('+', "%2B");
    let form = format!(
        "goformId=SEND_SMS&Number={}&MessageBody={message}&ID=-1&encode_type=UNICODE&sms_time={}",
        encode_plus(number),
        encode_plus(sms_time)
    );
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .post(url)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form)
        .send()
        .await
        .map_err(|_| format!("{sender} SMS endpoint unavailable"))?;
    if !response.status().is_success() || response.content_length().is_some_and(|v| v > 4096) {
        return Err(format!("{sender} SMS send failed"));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|_| format!("{sender} SMS send failed"))?;
    if bytes.len() > 4096
        || serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|value| {
                value
                    .get("result")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .as_deref()
            != Some("success")
    {
        return Err(format!("{sender} SMS send failed"));
    }
    Ok(json!({"sender":sender,"status":3}))
}

async fn current_slot() -> i64 {
    state::ubus("zwrt_zte_mdm.api", "get_sim_info", json!({}))
        .await
        .ok()
        .and_then(|value| {
            value
                .get("current_sim_slot")
                .or_else(|| value.get("sim_slot"))
                .and_then(Value::as_i64)
        })
        .unwrap_or_default()
}

async fn select_slot(slot: i64) -> Result<(), String> {
    let old = if slot == 2 { 1 } else { 2 };
    let _ = state::ubus(
        "zwrt_zte_mdm.api",
        "zwrt_mdm_change_provision_session",
        json!({"active_slot":old,"active_flag":0}),
    )
    .await;
    state::ubus(
        "zwrt_zte_mdm.api",
        "zwrt_mdm_change_provision_session",
        json!({"active_slot":slot,"active_flag":1}),
    )
    .await?;
    for _ in 0..30 {
        if current_slot().await == slot {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(format!("SIM slot {slot} did not become active"))
}

async fn send_host(number: &str, message: &str, sms_time: &str) -> Result<Value, String> {
    let _io = SMS_IO.lock().await;
    reset_crypto_session().await;
    let mut key = ensure_crypto_session().await?;
    let encrypted_number = encrypt(&key, number)?;
    let encrypted_message = encrypt(&key, message)?;
    key.zeroize();
    state::ubus(
        "zwrt_wms",
        "zte_libwms_send_sms",
        json!({"number":encrypted_number,"message_body":encrypted_message,"sms_time":sms_time,"encode_type":"UNICODE","id":"-1"}),
    )
    .await?;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let Ok(reply) =
            state::ubus("zwrt_wms", "zwrt_wms_get_cmd_status", json!({"sms_cmd":4})).await
        else {
            continue;
        };
        match reply.get("sms_cmd_status_result").and_then(Value::as_i64) {
            Some(3) => return Ok(json!({"sender":"host","status":3})),
            Some(2) => return Err("zwrt_wms SMS command failed".into()),
            _ => {}
        }
    }
    Err("zwrt_wms SMS command timed out".into())
}

pub async fn send(params: &Value) -> Result<Value, (bool, String)> {
    let object = params.as_object().expect("server validates params");
    let text = |name: &str| object.get(name).and_then(Value::as_str);
    let sender = text("sender").unwrap_or("host");
    let (Some(number), Some(message), Some(sms_time)) =
        (text("number"), text("message_hex"), text("sms_time"))
    else {
        return Err((true, "missing SMS parameters".into()));
    };
    if !valid(number, message, sms_time) {
        return Err((true, "invalid SMS parameters".into()));
    }
    let result = match sender {
        "v3e1" | "v3e2" => send_external(sender, number, message, sms_time).await,
        "host" | "x75" => send_host(number, message, sms_time).await,
        "sim1" | "sim2" => {
            let slot = if sender == "sim1" { 1 } else { 2 };
            if current_slot().await != slot {
                select_slot(slot).await.map_err(|error| (false, error))?;
            }
            send_host(number, message, sms_time).await
        }
        _ => return Err((true, "invalid SMS sender".into())),
    };
    invalidate();
    result.map_err(|error| (false, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_external_shape() {
        assert!(valid("+8613800000000", "6D4B8BD5", "26;08;27;04;00;00;+;0"));
        assert!(!valid("+1;reboot", "00", "1"));
        assert!(!valid("10086", "0", "1"));
    }

    #[test]
    fn aes_envelope_layout_roundtrip() {
        let key = [7_u8; 32];
        let encoded = encrypt(&key, "6D4B8BD5").unwrap();
        let raw = STANDARD.decode(encoded).unwrap();
        assert_eq!(raw.len(), 28 + 8);
        let mut input = raw[28..].to_vec();
        input.extend_from_slice(&raw[12..28]);
        let plain = Aes256Gcm::new_from_slice(&key)
            .unwrap()
            .decrypt(Nonce::from_slice(&raw[..12]), input.as_ref())
            .unwrap();
        assert_eq!(plain, b"6D4B8BD5");
    }

    #[test]
    fn normalizes_plain_sms_and_deduplicates() {
        let replies = [
            json!({"messages":[{"id":"7","num":"10086","date":"26,08,27,04,00,00,+,0","tag":"1","text":"6D4B8BD5"}]}),
            json!({"list":[{"id":7,"number":"duplicate","content":"0041"}]}),
        ];
        let list = normalize_lists(&replies, None).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["num"], "10086");
        assert_eq!(list[0]["text"], "测试");
        assert_eq!(list[0]["date"], "08-27 04:00");
        assert_eq!(list[0]["unread"], 1);
    }

    #[test]
    fn authenticates_and_normalizes_encrypted_sms() {
        let key = [9_u8; 32];
        let number = encrypt(&key, "00310030003000380036").unwrap();
        let text = encrypt(&key, "6D4B8BD5").unwrap();
        let replies = [json!({"list":[{
            "id":8,"num":number,"date":"26,08,27,04,00,00,+,0","tag":0,"text":text
        }]})];
        assert!(normalize_lists(&replies, None).is_err());
        assert!(normalize_lists(&replies, Some(&[8_u8; 32])).is_err());
        let list = normalize_lists(&replies, Some(&key)).unwrap();
        assert_eq!(list[0]["num"], "10086");
        assert_eq!(list[0]["text"], "测试");
    }

    #[test]
    fn accepts_flat_and_wrapped_public_keys_but_rejects_junk() {
        use rsa::{
            BigUint,
            pkcs1::EncodeRsaPublicKey,
            pkcs8::{EncodePublicKey, LineEnding},
        };
        let key = RsaPublicKey::new(
            (BigUint::from(1_u8) << 2047) + BigUint::from(5_u8),
            BigUint::from(65537_u32),
        )
        .unwrap();
        for pem in [
            key.to_public_key_pem(LineEnding::LF).unwrap(),
            key.to_pkcs1_pem(LineEnding::LF).unwrap(),
        ] {
            assert_eq!(public_key(&pem).unwrap(), key);
            assert_eq!(public_key(&pem.replace('\n', "")).unwrap(), key);
            assert_eq!(public_key(&pem.replace('\n', "\r\n")).unwrap(), key);
            assert!(public_key(&format!("{pem}junk")).is_err());
            assert!(public_key(&pem.replacen("-----END", "-----BROKEN", 1)).is_err());
        }
        assert!(public_key("-----BEGIN PUBLIC KEY-----!!!-----END PUBLIC KEY-----").is_err());
        assert!(public_key(&"A".repeat(16_385)).is_err());
    }

    #[test]
    fn preserves_more_than_eight_or_thirty_two_messages() {
        let rows: Vec<_> = (0..100)
            .map(|id| json!({"id":id,"number":"10086","content":"0041"}))
            .collect();
        assert_eq!(
            normalize_lists(&[json!({"messages":rows})], None)
                .unwrap()
                .len(),
            100
        );
    }

    #[test]
    fn read_failure_keeps_last_good_list_and_reports_stale() {
        let cache = ReadCache {
            capacity: Some(json!({"sms_dev_unread_num":"1"})),
            list: vec![json!({"id":1,"text":"cached"})],
            error: Some("SMS envelope authentication failed".into()),
            ..Default::default()
        };
        let value = cache.value().unwrap();
        assert_eq!(value["list"].as_array().unwrap().len(), 1);
        assert_eq!(value["stale"], true);
        assert_eq!(value["unread"], 1);
        assert!(value["error"].is_string());
    }
}
