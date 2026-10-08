//! Authenticated U50S OEM GoAhead bridge. No credentials are persisted; the
//! daemon's own local session is derived from the device's stored admin hash.
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
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, RwLock, RwLockReadGuard};
use zeroize::{Zeroize, Zeroizing};

const MAX_REPLY: usize = 64 * 1024;
const SESSION_LIFETIME: Duration = Duration::from_secs(15 * 60);
const LOGIN_INTERVAL: Duration = Duration::from_secs(3);
const PAUSED_ERROR: &str = "已暂停官方后台访问，请等待自动恢复或在 App 中手动恢复";

#[derive(Default)]
pub(crate) struct AccessPause {
    paused: bool,
    deadline: Option<Instant>,
    resume_at: Option<u64>,
}

impl AccessPause {
    fn active(&self) -> bool {
        self.paused
            && self
                .deadline
                .is_none_or(|deadline| Instant::now() < deadline)
    }

    fn status(&self) -> Value {
        let paused = self.active();
        let remaining = self.deadline.map(|deadline| {
            let duration = deadline.saturating_duration_since(Instant::now());
            duration.as_secs() + u64::from(duration.subsec_nanos() > 0)
        });
        json!({"supported":true,"paused":paused,
            "remaining_seconds":if paused { remaining } else { Some(0) },
            "resume_at":if paused { self.resume_at } else { None },
            "restart_resumes":true})
    }
}
/// GoAhead answers these hidden-page actions only after a second,
/// developer-option login (same admin password, own LD challenge, plus AD).
/// Mirrors the checklist table embedded in `zte_topsw_goahead`.
const DEVELOPER_GATED: &[&str] = &[
    "BAND_SELECT",
    "BAND_SELECT_EX",
    "WAN_PERFORM_NR5G_BAND_LOCK",
    "WAN_PERFORM_NR5G_SANSA_BAND_LOCK",
    "WAN_OPERATE_MODE_SET",
    "SCAN_NR5G_NEIGHBOR_CELL",
    "IF_UPGRADE",
    "ALG_SETTING",
    "OFFLINE_DEVICE_REMOVE",
    "STK_PROCESS",
];

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
    write_gate: Mutex<()>,
    access: RwLock<AccessPause>,
}
struct Session {
    token: String,
    cookie: String,
    first: Zeroizing<String>,
    expires: Instant,
}

fn sha256_hex(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes())).to_ascii_uppercase()
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
                write_gate: Mutex::new(()),
                access: RwLock::new(AccessPause::default()),
            }),
        })
    }
    pub(crate) async fn access_permit(&self) -> Result<RwLockReadGuard<'_, AccessPause>, String> {
        let guard = self.inner.access.read().await;
        if guard.active() {
            return Err(PAUSED_ERROR.into());
        }
        Ok(guard)
    }

    pub async fn access_status(&self) -> Value {
        self.inner.access.read().await.status()
    }

    /// Wait for in-flight HTTP exchanges before acknowledging the pause.
    /// Never send LOGOUT: it could invalidate the user's official WebUI.
    pub async fn set_access(&self, params: &Value) -> Result<Value, String> {
        let object = params.as_object().ok_or("params must be an object")?;
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "paused" | "duration_seconds"))
        {
            return Err("only paused and duration_seconds are accepted".into());
        }
        let paused = params["paused"].as_bool().ok_or("paused must be boolean")?;
        let duration = match params.get("duration_seconds") {
            None => 0,
            Some(value) => value
                .as_u64()
                .filter(|n| *n <= 3600)
                .ok_or("duration_seconds must be 0 (manual) or 1..3600")?,
        };
        if !paused && duration != 0 {
            return Err("resume does not accept a duration".into());
        }
        let mut guard = self.inner.access.write().await;
        *guard = AccessPause {
            paused,
            deadline: (paused && duration > 0)
                .then(|| Instant::now() + Duration::from_secs(duration)),
            resume_at: (paused && duration > 0).then(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    + duration
            }),
        };
        if paused {
            self.drop_session().await;
        }
        Ok(guard.status())
    }
    fn request(&self, request: reqwest::RequestBuilder, cookie: &str) -> reqwest::RequestBuilder {
        let origin = format!("http://{}", self.inner.host);
        let request = request
            .header(HOST, self.inner.host.as_str())
            .header(REFERER, format!("{origin}/"))
            .header("Origin", &origin)
            .header("X-Requested-With", "XMLHttpRequest")
            .header(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
            );
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
        self.get_query(keys, cookie, params, true).await
    }
    async fn get_query(
        &self,
        keys: &str,
        cookie: &str,
        params: &BTreeMap<String, String>,
        multi_data: bool,
    ) -> Result<(Value, String), String> {
        let _permit = self.access_permit().await?;
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
            query.append_pair("cmd", keys);
            if multi_data {
                query.append_pair("multi_data", "1");
            }
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
        let _permit = self.access_permit().await?;
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
    /// The WebUI proves knowledge of `sha256(password)` (upper-case hex) with
    /// `sha256(first + LD)`. The firmware stores exactly that first hash as
    /// `admin_Password`, so the daemon (root, same device) can open its own
    /// session without ever seeing the password.
    async fn login_with_first_hash(&self, first: Zeroizing<String>) -> Result<Value, String> {
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
        let proof = sha256_hex(&format!("{}{ld}", first.as_str()));
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
        let (status, status_cookie) = self.get("loginfo", &cookie).await?;
        let cookie = if status_cookie.is_empty() {
            cookie
        } else {
            cookies_from_two(&cookie, &status_cookie)
        };
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
            first,
            expires: Instant::now() + SESSION_LIFETIME,
        });
        Ok(json!({"ok":true,"token":token,"expires_in":SESSION_LIFETIME.as_secs()}))
    }
    async fn authorized_cookie(&self, token: &str) -> Result<String, String> {
        self.authorized_session(token)
            .await
            .map(|(cookie, _)| cookie)
    }
    async fn authorized_session(&self, token: &str) -> Result<(String, Zeroizing<String>), String> {
        let guard = self.inner.session.lock().await;
        let Some(session) = guard.as_ref() else {
            return Err("OEM login required".into());
        };
        if Instant::now() >= session.expires || !same_token(&session.token, token) {
            return Err("OEM session expired or invalid".into());
        }
        Ok((session.cookie.clone(), session.first.clone()))
    }
    /// The write challenge digest: `AD = SHA256(SHA256(wa_inner_version +
    /// cr_version) + RD)` with a fresh RD challenge.
    async fn access_id(&self, cookie: &str) -> Result<String, String> {
        let (versions, _) = self.get("wa_inner_version,cr_version", cookie).await?;
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
        let (rd, _) = self.get("RD", cookie).await?;
        let Some(rd) = rd
            .get("RD")
            .and_then(Value::as_str)
            .filter(|v| v.len() == 64)
        else {
            return Err("OEM write challenge unavailable".into());
        };
        Ok(sha256_hex(&format!(
            "{}{rd}",
            sha256_hex(&format!("{version}{cr}"))
        )))
    }
    /// Elevate the session for developer-gated actions. The proof mirrors the
    /// normal login (`SHA256(stored admin hash + LD)`) but goes to
    /// `DEVELOPER_OPTION_LOGIN` with its own AD; the elevation is verified
    /// through the `developer_option_loginfo` status read.
    async fn ensure_developer(
        &self,
        cookie: &str,
        first: &Zeroizing<String>,
    ) -> Result<(), String> {
        let (status, _) = self.get("developer_option_loginfo", cookie).await?;
        if status
            .get("developer_option_loginfo")
            .and_then(Value::as_str)
            == Some("ok")
        {
            return Ok(());
        }
        let (challenge, _) = self.get("LD", cookie).await?;
        let Some(ld) = challenge
            .get("LD")
            .and_then(Value::as_str)
            .filter(|s| s.len() == 64)
        else {
            return Err("developer login challenge unavailable".into());
        };
        let proof = sha256_hex(&format!("{}{ld}", first.as_str()));
        let ad = self.access_id(cookie).await?;
        let form = vec![
            ("isTest".into(), "false".into()),
            ("goformId".into(), "DEVELOPER_OPTION_LOGIN".into()),
            ("password".into(), proof),
            ("AD".into(), ad),
        ];
        let (result, _) = self.post(&form, cookie).await?;
        if result.get("result").and_then(Value::as_str) != Some("0") {
            return Err("developer login rejected".into());
        }
        let (status, _) = self.get("developer_option_loginfo", cookie).await?;
        if status
            .get("developer_option_loginfo")
            .and_then(Value::as_str)
            != Some("ok")
        {
            return Err("developer login not confirmed".into());
        }
        Ok(())
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
        // RD belongs to the OEM session: concurrent writes must not replace
        // each other's challenge between reading it and submitting AD.
        let _write = self.inner.write_gate.lock().await;
        let (cookie, first) = self.authorized_session(token).await?;
        if DEVELOPER_GATED.contains(&goform_id) {
            self.ensure_developer(&cookie, &first).await?;
        }
        // The OEM WebUI sends `isTest` first and `AD` last; some goahead
        // builds are sensitive to this order.
        let mut form: Vec<(String, String)> = vec![("isTest".into(), "false".into())];
        // The OEM WebUI explicitly exempts SET_WEB_LANGUAGE from AD. All
        // other write IDs use a fresh RD challenge tied to firmware versions.
        let ad = if goform_id != "SET_WEB_LANGUAGE" {
            self.access_id(&cookie).await?
        } else {
            String::new()
        };
        form.push(("goformId".into(), goform_id.into()));
        if goform_id == "SET_WEB_LANGUAGE" {
            let Some(language) = params.get("Language").and_then(Value::as_str) else {
                return Err("missing language".into());
            };
            if language.len() < 2
                || language.len() > 16
                || !language
                    .bytes()
                    .next()
                    .is_some_and(|b| b.is_ascii_alphabetic())
                || !language
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
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
        // AD goes last, matching the WebUI's form order.
        if !ad.is_empty() {
            form.push(("AD".into(), ad));
        }
        let write_cookie = if goform_id == "SET_WEB_LANGUAGE" {
            ""
        } else {
            &cookie
        };
        let (result, refreshed_cookie) = self.post(&form, write_cookie).await?;
        if !refreshed_cookie.is_empty() {
            let mut guard = self.inner.session.lock().await;
            if let Some(session) = guard.as_mut()
                && same_token(&session.token, token)
            {
                session.cookie = cookies_from_two(&session.cookie, &refreshed_cookie);
            }
        }
        if result.get("result").and_then(Value::as_str) != Some("success") {
            return Err("OEM action rejected".into());
        }
        let verified = if goform_id == "SET_WEB_LANGUAGE" {
            let (readback, _) = self.get("Language", "").await?;
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
    /// Token of the daemon's own OEM session, logging in from the stored admin
    /// hash when there is none or it is about to expire.
    pub async fn local_token(&self) -> Result<String, String> {
        // Do not read credentials or enter a login retry while paused.
        drop(self.access_permit().await?);
        {
            let guard = self.inner.session.lock().await;
            if let Some(session) = guard.as_ref()
                && Instant::now() + Duration::from_secs(30) < session.expires
            {
                return Ok(session.token.clone());
            }
        }
        let program =
            std::env::var("ZWRT_DATAD_U50_CFG_BIN").unwrap_or_else(|_| "/usr/bin/cfg".into());
        let raw = crate::command::run(&program, ["get", "admin_Password"], Duration::from_secs(2))
            .await
            .map_err(|_| "admin credential unavailable".to_string())?;
        let stored = Zeroizing::new(String::from_utf8_lossy(&raw).trim().to_owned());
        if stored.len() != 64
            || !stored
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'A'..=b'F'))
        {
            return Err("admin credential has an unexpected format".into());
        }
        let reply = self.login_with_first_hash(stored).await?;
        reply
            .get("token")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "OEM login failed".into())
    }

    /// Read OEM fields with the local session; one transparent re-login when
    /// the firmware expired it.
    pub async fn local_read(
        &self,
        keys: &str,
        params: &BTreeMap<String, String>,
    ) -> Result<Value, String> {
        for attempt in 0..2 {
            let token = self.local_token().await?;
            match self.read(&token, keys, params).await {
                Ok(value) => return Ok(value.get("fields").cloned().unwrap_or(Value::Null)),
                Err(error) if attempt == 0 => {
                    self.drop_session().await;
                    let _ = error;
                }
                Err(error) => return Err(error),
            }
        }
        Err("OEM read failed".into())
    }

    /// Single-command read (no `multi_data`), as the WebUI does for paged
    /// resources such as `sms_data_total`; returns the reply object as is.
    pub async fn local_read_single(
        &self,
        cmd: &str,
        params: &BTreeMap<String, String>,
    ) -> Result<Value, String> {
        for attempt in 0..2 {
            let token = self.local_token().await?;
            let cookie = self.authorized_cookie(&token).await?;
            // GoAhead can return a successful empty SMS list after its cookie
            // was replaced by another WebUI login. Never baseline that as an
            // empty inbox: verify the OEM session and renew once before reads.
            if cmd.starts_with("sms_")
                && self
                    .get("loginfo", &cookie)
                    .await?
                    .0
                    .get("loginfo")
                    .and_then(Value::as_str)
                    != Some("ok")
            {
                self.drop_session().await;
                if attempt == 0 {
                    continue;
                }
                return Err("OEM SMS session unavailable".into());
            }
            match self.get_query(cmd, &cookie, params, false).await {
                Ok((value, _)) => return Ok(value),
                Err(error) if attempt == 0 => {
                    self.drop_session().await;
                    let _ = error;
                }
                Err(error) => return Err(error),
            }
        }
        Err("OEM read failed".into())
    }

    /// Perform an allow-listed OEM action with the local session.
    pub async fn local_write(
        &self,
        goform_id: &str,
        params: &Map<String, Value>,
    ) -> Result<Value, String> {
        let token = self.local_token().await?;
        match self.write(&token, goform_id, params, true).await {
            // "OEM action rejected" is what the goahead returns when its cookie
            // expired mid-session — the same recoverable condition as the
            // explicit session/login errors. Drop and retry once.
            Err(error)
                if error.contains("session")
                    || error.contains("login")
                    || error.contains("OEM action rejected") =>
            {
                self.drop_session().await;
                let token = self.local_token().await?;
                self.write(&token, goform_id, params, true).await
            }
            other => other,
        }
    }

    async fn drop_session(&self) {
        *self.inner.session.lock().await = None;
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

    fn bridge() -> Bridge {
        Bridge::new(
            Client::new(),
            Url::parse("http://127.0.0.1:1/").unwrap(),
            "127.0.0.1".into(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn access_pause_blocks_reads_writes_login_and_drops_only_local_session() {
        let bridge = bridge();
        *bridge.inner.session.lock().await = Some(Session {
            token: "fixture".into(),
            cookie: "zte_web_cookie=fixture".into(),
            first: Zeroizing::new("A".repeat(64)),
            expires: Instant::now() + SESSION_LIFETIME,
        });
        assert_eq!(bridge.access_status().await["paused"], false);
        let status = bridge
            .set_access(&json!({"paused":true,"duration_seconds":300}))
            .await
            .unwrap();
        assert_eq!(status["paused"], true);
        assert_eq!(status["remaining_seconds"], 300);
        assert!(status["resume_at"].as_u64().is_some());
        assert!(bridge.inner.session.lock().await.is_none());
        // A dead endpoint ensures these fail at the gate, not in HTTP or cfg.
        assert_eq!(bridge.local_token().await.unwrap_err(), PAUSED_ERROR);
        assert_eq!(
            bridge.get("loginfo", "existing-cookie").await.unwrap_err(),
            PAUSED_ERROR
        );
        assert_eq!(
            bridge.post(&[], "existing-cookie").await.unwrap_err(),
            PAUSED_ERROR
        );
        assert_eq!(
            bridge
                .local_read("sms_capacity_info", &BTreeMap::new())
                .await
                .unwrap_err(),
            PAUSED_ERROR
        );
        let status = bridge.set_access(&json!({"paused":false})).await.unwrap();
        assert_eq!(status["paused"], false);
        assert_eq!(status["remaining_seconds"], 0);
        assert!(bridge.access_permit().await.is_ok());
    }

    #[tokio::test]
    async fn access_pause_manual_extension_and_expiration() {
        let bridge = bridge();
        let manual = bridge.set_access(&json!({"paused":true})).await.unwrap();
        assert!(manual["remaining_seconds"].is_null());
        assert!(manual["resume_at"].is_null());
        assert!(bridge.access_permit().await.is_err());
        let timed = bridge
            .set_access(&json!({"paused":true,"duration_seconds":600}))
            .await
            .unwrap();
        assert_eq!(timed["remaining_seconds"], 600);
        bridge.inner.access.write().await.deadline = Some(Instant::now() - Duration::from_secs(1));
        // Expiration must work without any App polling/status request.
        assert!(bridge.access_permit().await.is_ok());
        let expired = bridge.access_status().await;
        assert_eq!(expired["paused"], false);
        assert_eq!(expired["remaining_seconds"], 0);
        assert!(expired["resume_at"].is_null());
    }

    #[tokio::test]
    async fn access_pause_invalid_input_leaves_state_unchanged() {
        let bridge = bridge();
        bridge.set_access(&json!({"paused":true})).await.unwrap();
        for params in [
            json!(null),
            json!({}),
            json!({"paused":"false"}),
            json!({"paused":false,"duration_seconds":300}),
            json!({"paused":true,"duration_seconds":-1}),
            json!({"paused":true,"duration_seconds":3601}),
            json!({"paused":true,"duration_seconds":1.5}),
            json!({"paused":true,"duration_seconds":"300"}),
            json!({"paused":true,"duration_seconds":null}),
            json!({"paused":false,"unexpected":true}),
        ] {
            assert!(bridge.set_access(&params).await.is_err());
            assert_eq!(bridge.access_status().await["paused"], true);
        }
    }

    #[tokio::test]
    async fn access_pause_waits_for_inflight_http_and_prevents_later_requests() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Notify;
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let count = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let app = axum::Router::new().fallback({
            let (entered, release, count) = (entered.clone(), release.clone(), count.clone());
            move || {
                let (entered, release, count) = (entered.clone(), release.clone(), count.clone());
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    entered.notify_one();
                    release.notified().await;
                    axum::Json(json!({"loginfo":"ok"}))
                }
            }
        });
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let bridge = Bridge::new(
            Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
            url,
            "127.0.0.1".into(),
        )
        .unwrap();
        let request = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.get("loginfo", "").await }
        });
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        let mut pause = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.set_access(&json!({"paused":true})).await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut pause)
                .await
                .is_err()
        );
        release.notify_one();
        assert!(request.await.unwrap().is_ok());
        assert!(
            tokio::time::timeout(Duration::from_secs(2), pause)
                .await
                .unwrap()
                .unwrap()
                .is_ok()
        );
        assert_eq!(bridge.get("loginfo", "").await.unwrap_err(), PAUSED_ERROR);
        assert_eq!(bridge.post(&[], "").await.unwrap_err(), PAUSED_ERROR);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[test]
    fn token_and_key_validation() {
        assert_eq!(
            sha256_hex("abc"),
            "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD"
        );
        assert!(valid_key("SET_DEVICE_LED"));
        assert!(!valid_key("a;reboot"));
        assert!(same_token("abc", "abc"));
        assert!(!same_token("abc", "abd"));
        assert!(u50_oem_ids::IDS.contains(&"SET_DEVICE_LED"));
        assert!(!u50_oem_ids::IDS.contains(&"LOGIN"));
    }
}
