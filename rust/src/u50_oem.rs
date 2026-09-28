//! Authenticated U50S OEM GoAhead bridge. No credentials are persisted.
use crate::u50_oem_ids;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{RngCore, rngs::OsRng};
use reqwest::{
    Client, Url,
    header::{COOKIE, HOST, HeaderMap, REFERER, SET_COOKIE},
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use zeroize::{Zeroize, Zeroizing};

const MAX_REPLY: usize = 64 * 1024;
const SESSION_LIFETIME: Duration = Duration::from_secs(15 * 60);
const LOGIN_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Clone)]
pub struct Bridge {
    inner: Arc<Inner>,
}
struct Inner {
    client: Client,
    get_url: Url,
    set_url: Url,
    host: String,
    session: Mutex<Option<Session>>,
    last_login: Mutex<Option<Instant>>,
}
struct Session {
    token: String,
    cookie: String,
    expires: Instant,
}

fn sha256_hex(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
fn valid_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_')
}
fn same_token(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left
            .bytes()
            .zip(right.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}
fn cookies(headers: &HeaderMap, previous: &str) -> String {
    let mut values: Vec<String> = previous
        .split("; ")
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .collect();
    for header in headers.get_all(SET_COOKIE) {
        let Ok(raw) = header.to_str() else { continue };
        let Some(pair) = raw.split(';').next() else {
            continue;
        };
        let Some((name, _)) = pair.split_once('=') else {
            continue;
        };
        if !valid_key(name) || pair.len() > 1024 || pair.chars().any(char::is_control) {
            continue;
        }
        values.retain(|old| old.split_once('=').is_none_or(|(n, _)| n != name));
        values.push(pair.to_owned());
    }
    let joined = values.join("; ");
    if joined.len() <= 4096 {
        joined
    } else {
        String::new()
    }
}

impl Bridge {
    pub fn new(client: Client, mut get_url: Url, host: String) -> Result<Self, String> {
        if host.parse::<std::net::IpAddr>().is_err() {
            return Err("invalid OEM Host".into());
        }
        let mut set_url = get_url.clone();
        get_url.set_path("/goform/goform_get_cmd_process");
        set_url.set_path("/goform/goform_set_cmd_process");
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                get_url,
                set_url,
                host,
                session: Mutex::new(None),
                last_login: Mutex::new(None),
            }),
        })
    }
    fn request(&self, request: reqwest::RequestBuilder, cookie: &str) -> reqwest::RequestBuilder {
        let request = request
            .header(HOST, self.inner.host.as_str())
            .header(REFERER, format!("http://{}/", self.inner.host))
            .header("X-Requested-With", "XMLHttpRequest");
        if cookie.is_empty() {
            request
        } else {
            request.header(COOKIE, cookie)
        }
    }
    async fn response(&self, mut response: reqwest::Response) -> Result<(Value, String), String> {
        if !response.status().is_success() {
            return Err("OEM HTTP failure".into());
        }
        let cookie = cookies(response.headers(), "");
        if response
            .content_length()
            .is_some_and(|n| n > MAX_REPLY as u64)
        {
            return Err("OEM response too large".into());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| "OEM body failure")? {
            if bytes.len().saturating_add(chunk.len()) > MAX_REPLY {
                return Err("OEM response too large".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| "OEM invalid JSON")?;
        if !value.is_object() {
            return Err("OEM invalid response".into());
        }
        Ok((value, cookie))
    }
    async fn get(&self, keys: &str, cookie: &str) -> Result<(Value, String), String> {
        self.get_with_params(keys, cookie, &BTreeMap::new()).await
    }
    async fn get_with_params(
        &self,
        keys: &str,
        cookie: &str,
        params: &BTreeMap<String, String>,
    ) -> Result<(Value, String), String> {
        if keys.len() > 1024 || keys.split(',').count() > 32 || !keys.split(',').all(valid_key) {
            return Err("invalid OEM read keys".into());
        }
        if params.len() > 16 {
            return Err("too many OEM read parameters".into());
        }
        let mut total = 0usize;
        for (key, value) in params {
            if !valid_key(key)
                || matches!(key.as_str(), "cmd" | "multi_data")
                || value.len() > 512
                || value.chars().any(char::is_control)
            {
                return Err("invalid OEM read parameter".into());
            }
            total += key.len() + value.len();
            if total > 2048 {
                return Err("OEM read parameters too large".into());
            }
        }
        let mut url = self.inner.get_url.clone();
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("cmd", keys)
                .append_pair("multi_data", "1");
            for (key, value) in params {
                query.append_pair(key, value);
            }
        }
        let request = self.request(self.inner.client.get(url), cookie);
        let response = request.send().await.map_err(|_| "OEM read failed")?;
        self.response(response).await
    }
    async fn post(
        &self,
        form: &[(String, String)],
        cookie: &str,
    ) -> Result<(Value, String), String> {
        let request = self.request(
            self.inner
                .client
                .post(self.inner.set_url.clone())
                .form(form),
            cookie,
        );
        let response = request.send().await.map_err(|_| "OEM write failed")?;
        self.response(response).await
    }
    pub async fn login(&self, password: String) -> Result<Value, String> {
        let password = Zeroizing::new(password);
        if password.is_empty() || password.len() > 256 || password.chars().any(char::is_control) {
            return Err("invalid password".into());
        }
        {
            let mut last = self.inner.last_login.lock().await;
            if last.is_some_and(|at| at.elapsed() < LOGIN_INTERVAL) {
                return Err("login rate limited".into());
            }
            *last = Some(Instant::now());
        }
        let (challenge, first_cookie) = self.get("LD", "").await?;
        let Some(ld) = challenge
            .get("LD")
            .and_then(Value::as_str)
            .filter(|s| s.len() == 64)
        else {
            return Err("OEM login challenge unavailable".into());
        };
        // Current U50S WebUI WEB_ATTR_IF_SUPPORT_SHA256=2.
        let first = sha256_hex(&password);
        let proof = sha256_hex(&format!("{first}{ld}"));
        let form = vec![
            ("goformId".into(), "LOGIN".into()),
            ("isTest".into(), "false".into()),
            ("password".into(), proof),
        ];
        let (result, second_cookie) = self.post(&form, &first_cookie).await?;
        let accepted = matches!(
            result.get("result").and_then(Value::as_str),
            Some("0" | "4")
        );
        if !accepted {
            return Err("OEM login rejected".into());
        }
        let cookie = cookies_from_two(&first_cookie, &second_cookie);
        let (status, _) = self.get("loginfo", &cookie).await?;
        if status.get("loginfo").and_then(Value::as_str) != Some("ok") {
            return Err("OEM login not confirmed".into());
        }
        let mut random = [0u8; 32];
        OsRng.fill_bytes(&mut random);
        let token = URL_SAFE_NO_PAD.encode(random);
        random.zeroize();
        *self.inner.session.lock().await = Some(Session {
            token: token.clone(),
            cookie,
            expires: Instant::now() + SESSION_LIFETIME,
        });
        Ok(json!({"ok":true,"token":token,"expires_in":SESSION_LIFETIME.as_secs()}))
    }
    async fn authorized_cookie(&self, token: &str) -> Result<String, String> {
        let guard = self.inner.session.lock().await;
        let Some(session) = guard.as_ref() else {
            return Err("OEM login required".into());
        };
        if Instant::now() >= session.expires || !same_token(&session.token, token) {
            return Err("OEM session expired or invalid".into());
        }
        Ok(session.cookie.clone())
    }
    pub async fn read(
        &self,
        token: &str,
        keys: &str,
        params: &BTreeMap<String, String>,
    ) -> Result<Value, String> {
        let cookie = self.authorized_cookie(token).await?;
        let (value, _) = self.get_with_params(keys, &cookie, params).await?;
        Ok(json!({"ok":true,"fields":value}))
    }
    pub async fn write(
        &self,
        token: &str,
        goform_id: &str,
        params: &Map<String, Value>,
        confirm: bool,
    ) -> Result<Value, String> {
        if !confirm {
            return Err("explicit confirmation required".into());
        }
        if !u50_oem_ids::IDS.contains(&goform_id) {
            return Err("unsupported OEM action".into());
        }
        if params.len() > 32 {
            return Err("too many OEM parameters".into());
        }
        let cookie = self.authorized_cookie(token).await?;
        let mut form = vec![
            ("goformId".into(), goform_id.into()),
            ("isTest".into(), "false".into()),
        ];
        // The OEM WebUI explicitly exempts SET_WEB_LANGUAGE from AD. All
        // other write IDs use a fresh RD challenge tied to firmware versions.
        if goform_id != "SET_WEB_LANGUAGE" {
            let (versions, _) = self.get("wa_inner_version,cr_version", &cookie).await?;
            let version = versions
                .get("wa_inner_version")
                .and_then(Value::as_str)
                .unwrap_or("");
            let cr = versions
                .get("cr_version")
                .and_then(Value::as_str)
                .unwrap_or("");
            if version.is_empty() {
                return Err("OEM version challenge unavailable".into());
            }
            let (rd, _) = self.get("RD", &cookie).await?;
            let Some(rd) = rd
                .get("RD")
                .and_then(Value::as_str)
                .filter(|v| v.len() == 64)
            else {
                return Err("OEM write challenge unavailable".into());
            };
            let ad = sha256_hex(&format!("{}{rd}", sha256_hex(&format!("{version}{cr}"))));
            form.push(("AD".into(), ad));
        }
        if goform_id == "SET_WEB_LANGUAGE" {
            let Some(language) = params.get("Language").and_then(Value::as_str) else {
                return Err("missing language".into());
            };
            if language.is_empty() || language.len() > 16 || !valid_key(language) {
                return Err("invalid language".into());
            }
        }
        let mut total = 0usize;
        for (key, value) in params {
            if !valid_key(key) || matches!(key.as_str(), "goformId" | "AD" | "isTest") {
                return Err("invalid OEM parameter name".into());
            }
            let value = match value {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => {
                    if *b {
                        "1".into()
                    } else {
                        "0".into()
                    }
                }
                _ => return Err("invalid OEM parameter value".into()),
            };
            if value.len() > 2048 || value.chars().any(char::is_control) {
                return Err("invalid OEM parameter value".into());
            }
            total += key.len() + value.len();
            if total > 8192 {
                return Err("OEM parameters too large".into());
            }
            form.push((key.clone(), value));
        }
        let (result, _) = self.post(&form, &cookie).await?;
        if result.get("result").and_then(Value::as_str) != Some("success") {
            return Err("OEM action rejected".into());
        }
        let verified = if goform_id == "SET_WEB_LANGUAGE" {
            let (readback, _) = self.get("Language", &cookie).await?;
            let expected = params.get("Language").and_then(Value::as_str).unwrap_or("");
            if readback.get("Language").and_then(Value::as_str) != Some(expected) {
                return Err("OEM readback mismatch".into());
            }
            true
        } else {
            false
        };
        Ok(json!({"ok":true,"action":goform_id,"vendor_result":"success","verified":verified}))
    }
    pub async fn logout(&self, token: &str) -> Result<Value, String> {
        let _ = self.authorized_cookie(token).await?;
        *self.inner.session.lock().await = None;
        Ok(json!({"ok":true}))
    }
}
fn cookies_from_two(first: &str, second: &str) -> String {
    let mut values: Vec<String> = first
        .split("; ")
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .collect();
    for pair in second.split("; ").filter(|v| !v.is_empty()) {
        if let Some((name, _)) = pair.split_once('=') {
            values.retain(|old| old.split_once('=').is_none_or(|(n, _)| n != name));
            values.push(pair.to_owned());
        }
    }
    values.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn token_and_key_validation() {
        assert!(valid_key("SET_DEVICE_LED"));
        assert!(!valid_key("a;reboot"));
        assert!(same_token("abc", "abc"));
        assert!(!same_token("abc", "abd"));
        assert!(u50_oem_ids::IDS.contains(&"SET_DEVICE_LED"));
        assert!(!u50_oem_ids::IDS.contains(&"LOGIN"));
    }
}
