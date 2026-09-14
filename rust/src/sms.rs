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
use std::{sync::LazyLock, time::Duration};
use tokio::sync::Mutex;
use zeroize::Zeroize;

static SESSION_KEY: LazyLock<Mutex<Option<[u8; 32]>>> = LazyLock::new(|| Mutex::new(None));

fn valid(number: &str, message: &str, sms_time: &str) -> bool {
    !number.is_empty()
        && number.len() <= 32
        && number
            .bytes()
            .enumerate()
            .all(|(index, byte)| byte.is_ascii_digit() || (index == 0 && byte == b'+'))
        && !message.is_empty()
        && message.len() <= 4096
        && message.len() % 2 == 0
        && message.bytes().all(|byte| byte.is_ascii_hexdigit())
        && !sms_time.is_empty()
        && sms_time.len() <= 64
        && sms_time
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b';' | b'+' | b'-'))
}

fn public_key(pem: &str) -> Result<RsaPublicKey, String> {
    RsaPublicKey::from_public_key_pem(pem)
        .or_else(|_| RsaPublicKey::from_pkcs1_pem(pem))
        .map_err(|_| "invalid vendor RSA public key".into())
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
    if let Err(error) = state::ubus(
        "zwrt_web",
        "web_http_enstr_set",
        json!({"web_enstr":STANDARD.encode(wrapped)}),
    )
    .await
    {
        key.zeroize();
        return Err(error);
    }
    *guard = Some(key);
    Ok(key)
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
}
