//! Dedicated authenticated file RPC. No file bytes enter telemetry or the
//! existing panel-control channel, and no local listener is exposed.
use crate::{
    cloud::{Config, websocket_tls},
    files,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::watch;
use tokio_tungstenite::{
    Connector, connect_async_tls_with_config,
    tungstenite::{
        Message, client::IntoClientRequest, http::HeaderValue, protocol::WebSocketConfig,
    },
};

pub(crate) const PROTOCOL: &str = "nms-datad-files-v1";
const MAX_MESSAGE: usize = 256 * 1024;
const MAX_REGULAR_REQUEST: usize = 64 * 1024;
const MAX_REQUESTS: usize = 16384;
const MAX_QUEUED: usize = 4;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    #[serde(rename = "type")]
    kind: String,
    protocol_version: u8,
    request_id: String,
    action: String,
    params: Value,
    confirmed: bool,
}

fn parse_request(raw: &str) -> Option<Request> {
    if raw.len() > MAX_MESSAGE {
        return None;
    }
    let request: Request = serde_json::from_str(raw).ok()?;
    if request.kind != "file_request"
        || request.protocol_version != 1
        || request.request_id.len() != 32
        || !request
            .request_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || !request.action.starts_with("files.")
        || request.action.len() > 64
        || !request.params.is_object()
        || (request.action != "files.upload.chunk" && raw.len() > MAX_REGULAR_REQUEST)
    {
        return None;
    }
    Some(request)
}

#[derive(Default)]
struct Queue {
    used: HashSet<String>,
    pending: VecDeque<Request>,
}
impl Queue {
    fn admit(&mut self, request: Request) -> bool {
        if self.pending.len() >= MAX_QUEUED
            || self.used.len() >= MAX_REQUESTS
            || !self.used.insert(request.request_id.clone())
        {
            return false;
        }
        self.pending.push_back(request);
        true
    }
}

fn reply(id: &str, result: files::Result<Value>) -> Option<Message> {
    let value = match result {
        Ok(result) if result.is_object() => {
            json!({"type":"file_result","protocol_version":1,"request_id":id,"ok":true,"result":result})
        }
        Ok(_) => return None,
        Err(error) => {
            json!({"type":"file_result","protocol_version":1,"request_id":id,"ok":false,"code":error.code})
        }
    };
    let raw = serde_json::to_string(&value).ok()?;
    (raw.len() <= MAX_MESSAGE).then(|| Message::Text(raw.into()))
}

struct CancelOnExit(Arc<AtomicBool>);
impl Drop for CancelOnExit {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

pub(crate) async fn run(
    config: &Config,
    url: &str,
    token: &str,
    data_dir: PathBuf,
    ttl: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    if *shutdown.borrow()
        || ttl.is_zero()
        || ttl > Duration::from_secs(3600)
        || !config.enabled
        || !config.remote_enabled
        || !config.remote_panel_control_enabled
    {
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
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE))
        .write_buffer_size(0)
        .max_write_buffer_size(MAX_MESSAGE * 2);
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
    let Ok(mut session) = files::Session::new(&data_dir) else {
        return;
    };
    session.set_deadline(
        Instant::now() + deadline.saturating_duration_since(tokio::time::Instant::now()),
    );
    let cancel = session.cancellation_token();
    let _cancel_on_exit = CancelOnExit(cancel.clone());
    if socket
        .send(Message::Text(
            json!({"type":"ready","protocol_version":1})
                .to_string()
                .into(),
        ))
        .await
        .is_err()
    {
        return;
    }
    let mut queue = Queue::default();
    let mut ping = tokio::time::interval(Duration::from_secs(15));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ping.tick().await;
    loop {
        let request = if let Some(request) = queue.pending.pop_front() {
            request
        } else {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return,
                _ = shutdown.changed() => return,
                _ = ping.tick() => {
                    if socket.send(Message::Ping(Vec::new().into())).await.is_err() { return; }
                    continue;
                },
                incoming = socket.next() => match incoming {
                    Some(Ok(Message::Text(raw))) => {
                        let Some(value)=parse_request(&raw) else {return;};
                        if !queue.admit(value) {return;}
                        queue.pending.pop_front().unwrap()
                    },
                    Some(Ok(Message::Ping(raw))) => { if socket.send(Message::Pong(raw)).await.is_err() { return; } continue; },
                    Some(Ok(Message::Pong(_))) => continue,
                    _ => return,
                }
            }
        };
        let handle = tokio::runtime::Handle::current();
        let mut operation = tokio::task::spawn_blocking(move || {
            let result = handle.block_on(session.execute(
                &request.action,
                request.params,
                request.confirmed,
            ));
            (session, request.request_id, result)
        });
        let (next_session, id, result) = loop {
            tokio::select! {
                result = &mut operation => match result { Ok(value) => break value, Err(_) => return },
                _ = tokio::time::sleep_until(deadline) => {cancel.store(true, Ordering::Release); return;},
                _ = shutdown.changed() => {cancel.store(true, Ordering::Release); return;},
                _ = ping.tick() => {
                    if socket.send(Message::Ping(Vec::new().into())).await.is_err() { return; }
                },
                incoming = socket.next() => match incoming {
                    Some(Ok(Message::Text(raw))) => {
                        let Some(value)=parse_request(&raw) else {cancel.store(true, Ordering::Release); return;};
                        if !queue.admit(value) {cancel.store(true, Ordering::Release); return;}
                    },
                    Some(Ok(Message::Ping(raw))) => { if socket.send(Message::Pong(raw)).await.is_err() { return; } },
                    Some(Ok(Message::Pong(_))) => {},
                    _ => {cancel.store(true, Ordering::Release); return;},
                }
            }
        };
        session = next_session;
        let Some(message) = reply(&id, result) else {
            return;
        };
        if socket.send(message).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(action: &str, params: Value) -> String {
        json!({"type":"file_request","protocol_version":1,"request_id":"a".repeat(32),"action":action,"params":params,"confirmed":false}).to_string()
    }
    #[test]
    fn parser_keeps_regular_requests_small_and_transfer_frames_bounded() {
        assert!(parse_request(&request("files.list", json!({"path":"/data"}))).is_some());
        assert!(
            parse_request(&request(
                "files.upload.chunk",
                json!({"transfer_id":"b".repeat(32),"offset":0,"data":"A".repeat(174764)})
            ))
            .is_some()
        );
        assert!(
            parse_request(&request(
                "files.list",
                json!({"path":"a".repeat(MAX_REGULAR_REQUEST)})
            ))
            .is_none()
        );
        assert!(
            parse_request(&request(
                "files.upload.chunk",
                json!({"data":"A".repeat(MAX_MESSAGE)})
            ))
            .is_none()
        );
        let valid = request("files.status", json!({}));
        for bad in [
            valid.replace("file_request", "control"),
            valid.replace("\"protocol_version\":1", "\"protocol_version\":2"),
            valid.replace(&"a".repeat(32), &"A".repeat(32)),
            valid.replace("\"params\":{}", "\"params\":[]"),
            valid.replace("\"params\":{}", "\"params\":{},\"token\":\"extra\""),
        ] {
            assert!(parse_request(&bad).is_none());
        }
    }
    #[test]
    fn replies_do_not_expose_raw_errors_or_oversize_data() {
        let id = "a".repeat(32);
        let Message::Text(raw) = reply(&id, Err("files_version_conflict".into())).unwrap() else {
            panic!("not text")
        };
        let value: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["code"], "files_version_conflict");
        assert!(value.get("result").is_none());
        assert!(reply(&id, Ok(json!({"data":"x".repeat(MAX_MESSAGE)}))).is_none());
        assert!(reply(&id, Ok(json!(["unreviewed shape"]))).is_none());
    }
    #[test]
    fn exiting_session_sets_the_cooperative_cancel_flag() {
        let flag = Arc::new(AtomicBool::new(false));
        {
            let _guard = CancelOnExit(flag.clone());
        }
        assert!(flag.load(Ordering::Acquire));
    }
    #[test]
    fn simultaneous_requests_queue_in_order_without_replaying_an_id() {
        let mut queue = Queue::default();
        for i in 0..MAX_QUEUED {
            let raw =
                request("files.status", json!({})).replace(&"a".repeat(32), &format!("{i:032x}"));
            assert!(queue.admit(parse_request(&raw).unwrap()));
        }
        let extra = request("files.status", json!({}))
            .replace(&"a".repeat(32), &format!("{:032x}", MAX_QUEUED));
        assert!(!queue.admit(parse_request(&extra).unwrap()));
        for i in 0..MAX_QUEUED {
            assert_eq!(
                queue.pending.pop_front().unwrap().request_id,
                format!("{i:032x}")
            );
        }
        let old =
            request("files.status", json!({})).replace(&"a".repeat(32), &format!("{:032x}", 0));
        assert!(!queue.admit(parse_request(&old).unwrap()));
        assert!(queue.admit(parse_request(&extra).unwrap()));
    }

    #[tokio::test]
    async fn a_disabled_files_session_does_not_connect() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("wss://{}/fixture", listener.local_addr().unwrap());
        let (_shutdown, rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            run(
                &Config::default(),
                &url,
                &"a".repeat(64),
                std::env::temp_dir(),
                Duration::from_secs(60),
                rx,
            )
            .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "disabled session opened a network connection"
        );
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)]
    async fn reverse_tls_auth_protocol_concurrent_requests_and_disable_cleanup() {
        use rustls::{ServerConfig, pki_types::PrivateKeyDer};
        use tokio::net::TcpListener;
        use tokio_rustls::TlsAcceptor;
        use tokio_tungstenite::accept_hdr_async;
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let fixture = Fixture(
            std::fs::canonicalize(std::env::temp_dir())
                .unwrap()
                .join(format!("files-wss-test-{:032x}", rand::random::<u128>())),
        );
        let payload = fixture.0.join("payload");
        std::fs::create_dir_all(&payload).unwrap();
        std::fs::write(payload.join("source.bin"), vec![42u8; 16 * 1024 * 1024]).unwrap();
        let mut probe = files::Session::new(&fixture.0).unwrap();
        let stat = probe
            .execute("files.stat", json!({"path":payload}), false)
            .await
            .unwrap();
        let version = stat["entry"]["version"].clone();
        let _ = rustls::crypto::ring::default_provider().install_default();
        let certificate = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
        let config = Config {
            enabled: true,
            remote_enabled: true,
            remote_panel_control_enabled: true,
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
            "wss://{}/api/remote/device/files-fixture",
            listener.local_addr().unwrap()
        );
        let (shutdown, rx) = watch::channel(false);
        let data = fixture.0.clone();
        let task = tokio::spawn(async move {
            run(
                &config,
                &url,
                &"a".repeat(64),
                data,
                Duration::from_secs(20),
                rx,
            )
            .await
        });
        let mut socket=tokio::time::timeout(Duration::from_secs(10),async{
            let (stream,_)=listener.accept().await.unwrap();
            let tls=acceptor.accept(stream).await.unwrap();
            accept_hdr_async(tls,|request:&tokio_tungstenite::tungstenite::handshake::server::Request,mut response:tokio_tungstenite::tungstenite::handshake::server::Response|{
                assert_eq!(request.headers().get("authorization").unwrap().to_str().unwrap(),format!("Bearer {}","a".repeat(64)));
                assert_eq!(request.headers().get("x-nms-target-port").unwrap(),"0");
                assert_eq!(request.headers().get("sec-websocket-protocol").unwrap(),PROTOCOL);
                response.headers_mut().insert("sec-websocket-protocol",HeaderValue::from_static(PROTOCOL));
                Ok(response)
            }).await.unwrap()
        }).await.unwrap();
        let ready = socket.next().await.unwrap().unwrap();
        let Message::Text(raw) = ready else {
            panic!("not ready")
        };
        assert_eq!(
            serde_json::from_str::<Value>(&raw).unwrap(),
            json!({"type":"ready","protocol_version":1})
        );
        for i in 1..=4 {
            // A real compression keeps the worker occupied while subsequent
            // reads arrive; these must queue rather than disconnect the owner.
            let mut value: Value = serde_json::from_str(&request(
                if i == 1 {
                    "files.compress"
                } else {
                    "files.status"
                },
                if i == 1 {
                    json!({"path":payload,"expected_version":version})
                } else {
                    json!({})
                },
            ))
            .unwrap();
            value["request_id"] = json!(format!("{i:032x}"));
            value["confirmed"] = json!(i == 1);
            let raw = value.to_string();
            socket.send(Message::Text(raw.into())).await.unwrap();
        }
        let results = tokio::time::timeout(Duration::from_secs(10), async {
            let mut ids = Vec::new();
            while ids.len() < 4 {
                match socket.next().await.unwrap().unwrap() {
                    Message::Text(raw) => {
                        let value: Value = serde_json::from_str(&raw).unwrap();
                        assert_eq!(value["ok"], true);
                        if value["request_id"] == format!("{:032x}", 1) {
                            assert_eq!(value["result"]["entry"]["kind"], "file");
                        } else {
                            assert_eq!(value["result"]["supported"], true);
                        }
                        ids.push(value["request_id"].as_str().unwrap().to_owned());
                    }
                    Message::Ping(raw) => socket.send(Message::Pong(raw)).await.unwrap(),
                    _ => {}
                }
            }
            ids
        })
        .await
        .unwrap();
        assert_eq!(
            results,
            (1..=4).map(|i| format!("{i:032x}")).collect::<Vec<_>>()
        );
        shutdown.send_replace(true);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }
}
