//! On-demand state stream for an authenticated NMS panel session. No local
//! HTTP port, datad bearer token, or UFI process is exposed to the cloud.
use crate::{
    cloud::{Config, websocket_tls},
    model::Snapshot,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use std::time::Duration;
use tokio::sync::watch;
use tokio_tungstenite::{
    Connector, connect_async_tls_with_config,
    tungstenite::{
        Message, client::IntoClientRequest, http::HeaderValue, protocol::WebSocketConfig,
    },
};

pub(crate) const PROTOCOL: &str = "nms-datad-panel-v1";
const MAX_STATE_BYTES: usize = 192 * 1024;
const EXPOSED_BLOCKS: &[&str] = &[
    "net",
    "neighbor",
    "battery",
    "power",
    "sms",
    "traffic",
    "wlan",
    "nfc",
    "sim",
    "modems",
    "aggregation",
    "multiwan",
    "cooling",
    "dhcp",
    "device",
    "system",
    "thermal",
    "usb",
    "runtime",
    "qos",
    "clients",
    "sample_interval_ms",
];

fn panel_snapshot(snapshot: &Snapshot) -> Value {
    let mut view = Map::new();
    view.insert("ts".into(), json!(snapshot.ts));
    view.insert("datad".into(), json!(snapshot.datad));
    for name in EXPOSED_BLOCKS {
        if let Some(value) = snapshot.fields.get(*name) {
            view.insert((*name).to_owned(), value.clone());
        }
    }
    Value::Object(view)
}

fn state_message(snapshot: &Snapshot) -> Option<Message> {
    let raw = serde_json::to_vec(&json!({
        "type": "state", "protocol_version": 1, "snapshot": panel_snapshot(snapshot)
    }))
    .ok()?;
    if raw.len() > MAX_STATE_BYTES {
        return None;
    }
    Some(Message::Text(String::from_utf8(raw).ok()?.into()))
}

pub(crate) async fn run(
    state_rx: watch::Receiver<Snapshot>,
    config: &Config,
    url: &str,
    token: &str,
    ttl: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    if *shutdown.borrow() || ttl.is_zero() {
        return;
    }
    let deadline = tokio::time::Instant::now() + ttl;
    let Ok(mut request) = url.into_client_request() else {
        return;
    };
    let Ok(auth) = HeaderValue::from_str(&format!("Bearer {token}")) else {
        return;
    };
    request.headers_mut().insert("authorization", auth);
    request
        .headers_mut()
        .insert("x-nms-target-port", HeaderValue::from_static("0"));
    request
        .headers_mut()
        .insert("sec-websocket-protocol", HeaderValue::from_static(PROTOCOL));
    let Ok(tls) = websocket_tls(config) else {
        return;
    };
    let limits = WebSocketConfig::default()
        .max_message_size(Some(4 * 1024))
        .max_frame_size(Some(4 * 1024))
        .write_buffer_size(0)
        .max_write_buffer_size(2 * MAX_STATE_BYTES);
    let connect =
        connect_async_tls_with_config(request, Some(limits), false, Some(Connector::Rustls(tls)));
    let (mut socket, response) = tokio::select! {
        result = tokio::time::timeout(ttl.min(Duration::from_secs(10)), connect) => {
            match result { Ok(Ok(value)) => value, _ => return }
        },
        _ = shutdown.changed() => return,
    };
    if response
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        != Some(PROTOCOL)
    {
        return;
    }
    if socket
        .send(Message::Text(
            r#"{"type":"ready","protocol_version":1}"#.into(),
        ))
        .await
        .is_err()
    {
        return;
    }
    let mut last = state_rx.borrow().clone();
    let Some(first) = state_message(&last) else {
        return;
    };
    if socket.send(first).await.is_err() {
        return;
    }
    let mut sample = tokio::time::interval(Duration::from_secs(1));
    sample.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ping = tokio::time::interval(Duration::from_secs(15));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    sample.tick().await;
    ping.tick().await;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return,
            _ = shutdown.changed() => return,
            _ = sample.tick() => {
                let next = state_rx.borrow().clone();
                if next != last {
                    let Some(message) = state_message(&next) else { return; };
                    if socket.send(message).await.is_err() { return; }
                    last = next;
                }
            },
            _ = ping.tick() => {
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() { return; }
            },
            incoming = socket.next() => match incoming {
                Some(Ok(Message::Ping(data))) => {
                    if socket.send(Message::Pong(data)).await.is_err() { return; }
                },
                Some(Ok(Message::Pong(_))) => {},
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                // This first protocol version is deliberately read-only.
                Some(Ok(_)) => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DatadVersion;

    #[test]
    fn only_reviewed_state_blocks_leave_the_device() {
        let mut fields = Map::new();
        fields.insert("net".into(), json!({"type":"SA"}));
        fields.insert("future_secret".into(), json!({"token":"must-not-leak"}));
        fields.insert(
            "interfaces".into(),
            json!({"cellular":{"password":"must-not-leak"}}),
        );
        let snapshot = Snapshot {
            ts: 7,
            datad: DatadVersion::default(),
            fields,
        };
        let message = state_message(&snapshot).unwrap();
        let Message::Text(raw) = message else {
            panic!("expected a text frame")
        };
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["snapshot"]["net"]["type"], "SA");
        assert!(parsed["snapshot"].get("future_secret").is_none());
        assert!(parsed["snapshot"].get("interfaces").is_none());
        assert_eq!(parsed["protocol_version"], 1);
    }

    #[test]
    fn oversized_state_is_rejected_without_truncation() {
        let mut fields = Map::new();
        fields.insert("sms".into(), json!({"text":"x".repeat(MAX_STATE_BYTES)}));
        let snapshot = Snapshot {
            ts: 1,
            datad: DatadVersion::default(),
            fields,
        };
        assert!(state_message(&snapshot).is_none());
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)] // Tungstenite handshake callback has a large error type.
    async fn authenticated_wss_streams_changed_state_and_rejects_input() {
        use rustls::{ServerConfig, pki_types::PrivateKeyDer};
        use std::sync::Arc;
        use tokio::net::TcpListener;
        use tokio_rustls::TlsAcceptor;
        use tokio_tungstenite::accept_hdr_async;

        let _ = rustls::crypto::ring::default_provider().install_default();
        let certificate = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
        let config = Config {
            ca_pem: certificate.cert.pem(),
            ..Default::default()
        };
        let tls = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.cert.der().clone()],
                PrivateKeyDer::Pkcs8(certificate.key_pair.serialize_der().into()),
            )
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(tls));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "wss://{}/api/remote/device/panel-fixture",
            listener.local_addr().unwrap()
        );
        let mut fields = Map::new();
        fields.insert("net".into(), json!({"type":"LTE"}));
        let (sender, receiver) = watch::channel(Snapshot {
            ts: 1,
            datad: DatadVersion::default(),
            fields,
        });
        let (_shutdown_sender, shutdown) = watch::channel(false);
        let task = tokio::spawn(async move {
            run(
                receiver,
                &config,
                &url,
                &"a".repeat(64),
                Duration::from_secs(5),
                shutdown,
            )
            .await;
        });
        let (tcp, _) = listener.accept().await.unwrap();
        let tls = acceptor.accept(tcp).await.unwrap();
        let mut ws = accept_hdr_async(tls, |request: &tokio_tungstenite::tungstenite::handshake::server::Request, mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
            assert_eq!(request.headers()["authorization"],format!("Bearer {}","a".repeat(64)));
            assert_eq!(request.headers()["x-nms-target-port"],"0");
            assert_eq!(request.headers()["sec-websocket-protocol"],PROTOCOL);
            response.headers_mut().insert("sec-websocket-protocol",HeaderValue::from_static(PROTOCOL));
            Ok(response)
        }).await.unwrap();
        let ready = ws.next().await.unwrap().unwrap();
        assert!(matches!(ready,Message::Text(ref body) if body.contains("\"ready\"")));
        let first = ws.next().await.unwrap().unwrap();
        assert!(matches!(first,Message::Text(ref body) if body.contains("\"LTE\"")));
        sender.send_modify(|snapshot| {
            snapshot.ts = 2;
            snapshot.fields.insert("net".into(), json!({"type":"SA"}));
        });
        let second = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(second,Message::Text(ref body) if body.contains("\"SA\"")));
        ws.send(Message::Text(r#"{"type":"control"}"#.into()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
    }
}
