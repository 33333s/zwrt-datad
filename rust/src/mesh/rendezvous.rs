//! The rendezvous WebSocket of docs/P2P.md: signalling only, never data.
//! Text frames of at most 16 KiB, at most 40 per second; the server wraps
//! what the other end sends as `{"type":"signal","data":…}` and reports the
//! other end's presence as `{"type":"hub","peer":"up"|"down"}`.
use crate::cloud::{Config, websocket_tls};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::{
    Connector, connect_async_tls_with_config,
    tungstenite::{
        Message, client::IntoClientRequest, http::HeaderValue, protocol::WebSocketConfig,
    },
};

const MAX_MESSAGE: usize = 16 * 1024;
/// Stay under the hub's 40 messages per second.
const MAX_PER_SECOND: u32 = 30;

#[derive(Debug, PartialEq)]
pub enum Event {
    PeerUp,
    PeerDown,
    Signal(Value),
}

/// Turns a text frame into an event. Anything that is not a well-formed hub or
/// signal object is ignored.
pub fn parse(text: &str) -> Option<Event> {
    if text.len() > MAX_MESSAGE {
        return None;
    }
    let value: Value = serde_json::from_str(text).ok()?;
    match value.get("type")?.as_str()? {
        "hub" => match value.get("peer")?.as_str()? {
            "up" => Some(Event::PeerUp),
            "down" => Some(Event::PeerDown),
            _ => None,
        },
        "signal" => value
            .get("data")
            .filter(|data| data.is_object())
            .cloned()
            .map(Event::Signal),
        _ => None,
    }
}

/// Keeps one rendezvous socket open while the session lives, reconnecting with
/// backoff. Returns when `stop` fires or the event receiver is gone.
pub async fn run(
    config: Config,
    url: String,
    token: String,
    events: mpsc::Sender<Event>,
    mut outbound: mpsc::Receiver<Value>,
    mut stop: watch::Receiver<bool>,
) {
    let mut delay = Duration::from_secs(1);
    loop {
        if *stop.borrow() {
            return;
        }
        let connected = session(&config, &url, &token, &events, &mut outbound, &mut stop).await;
        if *stop.borrow() || events.is_closed() {
            return;
        }
        delay = if connected {
            Duration::from_secs(1)
        } else {
            (delay * 2).min(Duration::from_secs(10))
        };
        tokio::select! {
            _ = tokio::time::sleep(delay) => {},
            _ = stop.changed() => return,
        }
    }
}

/// One connection. Returns whether the handshake succeeded.
async fn session(
    config: &Config,
    url: &str,
    token: &str,
    events: &mpsc::Sender<Event>,
    outbound: &mut mpsc::Receiver<Value>,
    stop: &mut watch::Receiver<bool>,
) -> bool {
    let Ok(mut request) = url.into_client_request() else {
        return false;
    };
    let Ok(auth) = HeaderValue::from_str(&format!("Bearer {token}")) else {
        return false;
    };
    // No Origin header: the hub only accepts the device role without one.
    request.headers_mut().insert("authorization", auth);
    let Ok(tls) = websocket_tls(config) else {
        return false;
    };
    let limits = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE))
        .write_buffer_size(0)
        .max_write_buffer_size(4 * MAX_MESSAGE);
    let connect =
        connect_async_tls_with_config(request, Some(limits), false, Some(Connector::Rustls(tls)));
    let socket = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(10), connect) => match result {
            Ok(Ok((socket, _))) => socket,
            _ => return false,
        },
        _ = stop.changed() => return false,
    };
    let (mut write, mut read) = socket.split();
    let mut window = tokio::time::Instant::now();
    let mut sent = 0u32;
    loop {
        tokio::select! {
            _ = stop.changed() => {
                let _ = write.close().await;
                return true;
            }
            message = read.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    if let Some(event) = parse(text.as_str())
                        && events.send(event).await.is_err()
                    {
                        return true;
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return true,
                Some(Ok(_)) => {}
            },
            signal = outbound.recv() => {
                let Some(signal) = signal else { return true };
                if window.elapsed() >= Duration::from_secs(1) {
                    window = tokio::time::Instant::now();
                    sent = 0;
                }
                if sent >= MAX_PER_SECOND {
                    continue;
                }
                sent += 1;
                let text = signal.to_string();
                if text.len() > MAX_MESSAGE || write.send(Message::Text(text.into())).await.is_err() {
                    return true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hub_and_signal_frames_become_events() {
        assert_eq!(
            parse(r#"{"type":"hub","v":1,"peer":"up"}"#),
            Some(Event::PeerUp)
        );
        assert_eq!(
            parse(r#"{"type":"hub","v":1,"peer":"down"}"#),
            Some(Event::PeerDown)
        );
        assert_eq!(
            parse(r#"{"type":"signal","data":{"type":"offer","sdp":"v=0"}}"#),
            Some(Event::Signal(json!({"type":"offer","sdp":"v=0"})))
        );
    }

    #[test]
    fn everything_else_is_ignored() {
        for text in [
            "",
            "nope",
            r#"{"type":"hub","peer":"sideways"}"#,
            r#"{"type":"hub"}"#,
            r#"{"type":"signal","data":"text"}"#,
            r#"{"type":"signal"}"#,
            r#"{"type":"other"}"#,
            r#"[1]"#,
        ] {
            assert_eq!(parse(text), None, "{text}");
        }
        let big = format!(
            r#"{{"type":"signal","data":{{"x":"{}"}}}}"#,
            "a".repeat(MAX_MESSAGE)
        );
        assert_eq!(parse(&big), None);
    }
}
