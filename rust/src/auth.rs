use crate::{elapsed, state};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read, time::Duration};

const SESSION_TTL: u64 = 12 * 60 * 60;
const SESSION_LIMIT: usize = 16;

#[derive(Clone)]
struct Session {
    token: String,
    deadline: Duration,
}

#[derive(Default)]
pub struct Sessions(Vec<Session>);

impl Sessions {
    pub fn validate(&mut self, token: &str) -> bool {
        let now = elapsed::now();
        self.0.retain(|s| s.deadline > now);
        if let Some(session) = self.0.iter_mut().find(|s| secure_eq(&s.token, token)) {
            session.deadline = now.saturating_add(Duration::from_secs(SESSION_TTL));
            true
        } else {
            false
        }
    }

    pub fn issue(&mut self) -> std::io::Result<(String, u64)> {
        let mut bytes = [0u8; 24];
        File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        let token = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let now = elapsed::now();
        let deadline = now.saturating_add(Duration::from_secs(SESSION_TTL));
        let expires_at = (elapsed::wall_epoch().max(0) as u64).saturating_add(SESSION_TTL);
        self.0.retain(|s| s.deadline > now);
        if self.0.len() >= SESSION_LIMIT {
            let oldest = self
                .0
                .iter()
                .enumerate()
                .min_by_key(|(_, s)| s.deadline)
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.0.remove(oldest);
        }
        self.0.push(Session {
            token: token.clone(),
            deadline,
        });
        Ok((token, expires_at))
    }
}

pub async fn verify_password(username: &str, password: &str) -> bool {
    if username.is_empty() || username.len() > 256 || password.len() > 256 {
        return false;
    }
    let Ok(info) = state::ubus("zwrt_web", "web_login_info", json!({})).await else {
        return false;
    };
    let Some(salt) = info.get("zte_web_sault").and_then(Value::as_str) else {
        return false;
    };
    if salt.is_empty() || salt.len() > 256 {
        return false;
    }
    let first = format!("{:X}", Sha256::digest(password.as_bytes()));
    let response = format!(
        "{:X}",
        Sha256::digest([first.as_bytes(), salt.as_bytes()].concat())
    );
    let Ok(reply) = state::ubus(
        "zwrt_web",
        "web_login",
        json!({"username":username,"password":response}),
    )
    .await
    else {
        return false;
    };
    indicates_success(&reply)
}

pub async fn verify_webtoken(token: &str, mode: i64, remote: &str, tag: &str) -> bool {
    if token.is_empty() || token.len() > 4096 || tag.is_empty() || tag.len() > 256 {
        return false;
    }
    let args = json!({"webtoken":token,"zmode":mode,"web_remote_addr":remote,"z-tag":tag});
    for method in ["webtoken_check", "web_security_check"] {
        if let Ok(reply) = state::ubus("zwrt_web", method, args.clone()).await
            && indicates_success(&reply)
        {
            return true;
        }
    }
    false
}

fn indicates_success(value: &Value) -> bool {
    for key in ["webtoken", "token", "auth_token", "secondary_auth_token"] {
        if value
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty())
        {
            return true;
        }
    }
    for key in ["success", "ok", "web_login_flag", "login_flag"] {
        if let Some(value) = value.get(key) {
            return truthy(value);
        }
    }
    for key in ["code", "errno", "ret", "result"] {
        if let Some(value) = value.get(key) {
            if let Some(number) = value.as_i64() {
                return number == 0;
            }
            if let Some(text) = value.as_str() {
                if let Ok(number) = text.parse::<i64>() {
                    return number == 0;
                }
                return success_text(text);
            }
        }
    }
    ["status", "state", "msg", "message"]
        .iter()
        .filter_map(|key| value.get(*key).and_then(Value::as_str))
        .any(success_text)
        || ["username", "user"]
            .iter()
            .filter_map(|key| value.get(*key).and_then(Value::as_str))
            .any(|v| !v.is_empty())
}

fn truthy(value: &Value) -> bool {
    value.as_bool().unwrap_or(false)
        || value.as_i64() == Some(1)
        || value.as_str().is_some_and(|v| v == "1" || success_text(v))
}
fn success_text(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "success" | "ok" | "true" | "pass" | "passed" | "logged" | "logined" | "done"
    )
}
fn secure_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |diff, (x, y)| diff | (x ^ y))
            == 0
}
pub fn token_reply(token: String, expires_at: u64) -> Value {
    json!({"ok":true,"token_type":"Bearer","access_token":token,"expires_in":SESSION_TTL,"expires_at":expires_at})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn response_shapes() {
        assert!(indicates_success(&json!({"result":0})));
        assert!(!indicates_success(&json!({"result":1})));
        let mut sessions = Sessions::default();
        let (token, _) = sessions.issue().unwrap();
        assert_eq!(token.len(), 48);
        assert!(sessions.validate(&token));
    }

    #[test]
    fn session_ttl_ignores_eight_hour_wall_steps_and_includes_suspend() {
        for step in [-8 * 3600, 8 * 3600] {
            elapsed::with_clock(Duration::from_secs(100), 1_780_000_000, || {
                let mut sessions = Sessions::default();
                let (token, expires_at) = sessions.issue().unwrap();
                assert_eq!(expires_at, 1_780_000_000 + SESSION_TTL);
                elapsed::advance(Duration::ZERO, step);
                assert_eq!(
                    sessions.0[0].deadline,
                    Duration::from_secs(100 + SESSION_TTL)
                );
                // BOOTTIME advances during suspend even without request activity.
                elapsed::advance(Duration::from_secs(SESSION_TTL), 0);
                assert!(!sessions.validate(&token));
            });
        }
    }

    #[test]
    fn active_session_refresh_is_twelve_elapsed_hours_not_wall_hours() {
        elapsed::with_clock(Duration::ZERO, 1_780_000_000, || {
            let mut sessions = Sessions::default();
            let (token, _) = sessions.issue().unwrap();
            elapsed::advance(Duration::from_secs(60), -8 * 3600);
            assert!(sessions.validate(&token));
            elapsed::advance(Duration::from_secs(SESSION_TTL), 8 * 3600);
            assert!(!sessions.validate(&token));
        });
    }
}
