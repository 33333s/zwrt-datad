//! Vendor web session (`zwrt_web`) that NMS can re-establish with the device
//! back-end password. The password is hashed for one `web_login` call and is
//! never stored, logged or returned; only the opaque session id stays in
//! process memory so a later expiry can be reported as `device_session_expired`.
use crate::state;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use zeroize::Zeroize;

const MAX_PASSWORD_BYTES: usize = 1024;
const MAX_ATTEMPTS: usize = 3;
const ATTEMPT_WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginError {
    Invalid,
    InvalidCredentials,
    RateLimited,
    Unavailable,
}

impl LoginError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Invalid => "invalid_parameter",
            Self::InvalidCredentials => "invalid_credentials",
            Self::RateLimited => "device_session_rate_limited",
            Self::Unavailable => "device_session_unavailable",
        }
    }
}

struct Session {
    id: String,
    /// Whether the vendor session id is visible to `session list`; without
    /// that the session state can only be reported as unknown.
    checkable: bool,
}

struct State {
    session: Option<Session>,
    expired: bool,
    attempts: VecDeque<Instant>,
}

static STATE: Mutex<State> = Mutex::const_new(State {
    session: None,
    expired: false,
    attempts: VecDeque::new(),
});

pub fn valid_password(password: &str) -> bool {
    !password.is_empty()
        && password.len() <= MAX_PASSWORD_BYTES
        && !password.contains(['\n', '\r', '\0'])
}

/// `{"password":"..."}` and nothing else.
pub fn password_param(params: &Value) -> Option<&str> {
    let fields = params.as_object().filter(|fields| fields.len() == 1)?;
    fields.get("password")?.as_str()
}

fn upper_sha256(input: &[u8]) -> String {
    format!("{:X}", Sha256::digest(input))
}

fn text_or_number(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn locked(info: &Value) -> bool {
    info.get("login_fail_lock_lefttime")
        .and_then(text_or_number)
        .is_some_and(|seconds| seconds > 0)
}

fn session_id(reply: &Value) -> Option<&str> {
    reply
        .get("ubus_rpc_session")
        .and_then(Value::as_str)
        .filter(|id| (8..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// `Some(false)` only when ubus positively reports the session as not found
/// (exit status 252); any other failure leaves the state unknown.
async fn session_exists(id: &str) -> Option<bool> {
    match state::ubus("session", "list", json!({"ubus_rpc_session":id})).await {
        Ok(value) => Some(value.get("ubus_rpc_session").and_then(Value::as_str) == Some(id)),
        Err(error) if error.contains("exit status: 252") => Some(false),
        Err(_) => None,
    }
}

pub async fn login(password: &str) -> Result<(), LoginError> {
    if !valid_password(password) {
        return Err(LoginError::Invalid);
    }
    let mut state = STATE.lock().await;
    let now = Instant::now();
    state
        .attempts
        .retain(|attempt| now.duration_since(*attempt) < ATTEMPT_WINDOW);
    if state.attempts.len() >= MAX_ATTEMPTS {
        return Err(LoginError::RateLimited);
    }
    state.attempts.push_back(now);
    let info = state::ubus("zwrt_web", "web_login_info", json!({}))
        .await
        .map_err(|_| LoginError::Unavailable)?;
    if locked(&info) {
        return Err(LoginError::RateLimited);
    }
    let salt = info
        .get("zte_web_sault")
        .and_then(Value::as_str)
        .filter(|salt| !salt.is_empty() && salt.len() <= 256)
        .ok_or(LoginError::Unavailable)?;
    let mut first = upper_sha256(password.as_bytes());
    let mut response = upper_sha256([first.as_bytes(), salt.as_bytes()].concat().as_slice());
    first.zeroize();
    let reply = state::ubus("zwrt_web", "web_login", json!({"password":response})).await;
    response.zeroize();
    let accepted = match &reply {
        Ok(value) if value.get("result").and_then(text_or_number) == Some(0) => {
            session_id(value).map(str::to_owned)
        }
        _ => None,
    };
    let Some(id) = accepted else {
        let refused = matches!(&reply, Ok(value) if value.get("result").is_some());
        let lockout = state::ubus("zwrt_web", "web_login_info", json!({}))
            .await
            .is_ok_and(|info| locked(&info));
        return Err(if lockout {
            LoginError::RateLimited
        } else if refused {
            LoginError::InvalidCredentials
        } else {
            LoginError::Unavailable
        });
    };
    let checkable = session_exists(&id).await == Some(true);
    state.session = Some(Session { id, checkable });
    state.expired = false;
    Ok(())
}

/// `Some(true)`: the session NMS established is still valid. `Some(false)`:
/// it was replaced or timed out. `None`: not established by this process or
/// not verifiable; unknown must not be shown as logged in or logged out.
pub async fn active() -> Option<bool> {
    let mut state = STATE.lock().await;
    if state.expired {
        return Some(false);
    }
    let session = state.session.as_ref()?;
    if !session.checkable {
        return None;
    }
    match session_exists(&session.id).await {
        Some(true) => Some(true),
        Some(false) => {
            state.session = None;
            state.expired = true;
            Some(false)
        }
        None => None,
    }
}

/// True only when a session established here is known to be gone. Devices
/// where no login ever happened keep the legacy behaviour.
pub async fn expired() -> bool {
    active().await == Some(false)
}

pub async fn panel_status() -> Value {
    json!({"supported":true,"reauth_supported":true,"active":active().await})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_shape_follows_the_panel_contract() {
        assert!(valid_password(" keeps spaces "));
        assert!(valid_password(&"p".repeat(1024)));
        for bad in ["", "line\nbreak", "cr\rhere", "nul\0byte"] {
            assert!(!valid_password(bad), "{bad:?}");
        }
        assert!(!valid_password(&"p".repeat(1025)));
        assert!(!valid_password(&"é".repeat(513)));
        assert_eq!(password_param(&json!({"password":"x"})), Some("x"));
        assert_eq!(password_param(&json!({"password":"x","extra":1})), None);
        assert_eq!(password_param(&json!({"password":1})), None);
        assert_eq!(password_param(&json!({})), None);
    }

    #[test]
    fn lockout_and_session_shapes_are_strict() {
        assert!(locked(&json!({"login_fail_lock_lefttime":"300"})));
        assert!(locked(&json!({"login_fail_lock_lefttime":12})));
        assert!(!locked(&json!({"login_fail_lock_lefttime":"0"})));
        assert!(!locked(&json!({"login_fail_lock_lefttime":""})));
        assert!(!locked(&json!({})));
        assert_eq!(
            session_id(&json!({"ubus_rpc_session":"abcDEF0123456789"})),
            Some("abcDEF0123456789")
        );
        assert_eq!(session_id(&json!({"ubus_rpc_session":"short"})), None);
        assert_eq!(
            session_id(&json!({"ubus_rpc_session":"has space 123456"})),
            None
        );
        assert_eq!(
            session_id(&json!({"ubus_rpc_session":"../../etc/passwd/x"})),
            None
        );
    }

    #[test]
    fn login_errors_use_the_documented_codes() {
        assert_eq!(LoginError::InvalidCredentials.code(), "invalid_credentials");
        assert_eq!(
            LoginError::RateLimited.code(),
            "device_session_rate_limited"
        );
        assert_eq!(LoginError::Unavailable.code(), "device_session_unavailable");
        assert_eq!(LoginError::Invalid.code(), "invalid_parameter");
    }
}
