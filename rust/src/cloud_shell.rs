//! A dedicated PTY channel, never an HTTP proxy to datad's management ports.
//! Records: one kind byte (0=binary, 1=UTF-8 text), u32 BE size, then payload.
use crate::{
    cloud::{Config, websocket_tls},
    webshell::{MAX_MESSAGE_SIZE, WebShell},
};
use axum::extract::ws::Message as ShellMessage;
use futures_util::{SinkExt, StreamExt, future};
use std::{io, time::Duration};
use tokio::sync::{OwnedSemaphorePermit, watch};
use tokio_tungstenite::{
    Connector, connect_async_tls_with_config,
    tungstenite::{
        Error, Message, client::IntoClientRequest, http::HeaderValue, protocol::WebSocketConfig,
    },
};

const PROTOCOL: &str = "nms-webshell-v1";

fn invalid_record() -> Error {
    Error::Io(io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid WebShell record",
    ))
}

fn encode(message: ShellMessage) -> Result<Message, Error> {
    let (kind, bytes) = match message {
        ShellMessage::Binary(data) => (0, data.to_vec()),
        ShellMessage::Text(text) => (1, text.as_bytes().to_vec()),
        ShellMessage::Ping(data) => return Ok(Message::Ping(data)),
        ShellMessage::Pong(data) => return Ok(Message::Pong(data)),
        ShellMessage::Close(_) => return Ok(Message::Close(None)),
    };
    if bytes.len() > MAX_MESSAGE_SIZE {
        return Err(invalid_record());
    }
    let mut record = Vec::with_capacity(bytes.len() + 5);
    record.push(kind);
    record.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    record.extend_from_slice(&bytes);
    Ok(Message::Binary(record.into()))
}

fn decode(message: Message) -> Result<ShellMessage, Error> {
    let data = match message {
        Message::Binary(data) => data,
        Message::Ping(data) => return Ok(ShellMessage::Ping(data)),
        Message::Pong(data) => return Ok(ShellMessage::Pong(data)),
        Message::Close(_) => return Ok(ShellMessage::Close(None)),
        _ => return Err(invalid_record()),
    };
    if data.len() < 5 {
        return Err(invalid_record());
    }
    let size = u32::from_be_bytes(data[1..5].try_into().map_err(|_| invalid_record())?) as usize;
    if size > MAX_MESSAGE_SIZE || data.len() != size + 5 {
        return Err(invalid_record());
    }
    match data[0] {
        0 => Ok(ShellMessage::Binary(data.slice(5..))),
        1 => Ok(ShellMessage::Text(
            std::str::from_utf8(&data[5..])
                .map_err(|_| invalid_record())?
                .to_owned()
                .into(),
        )),
        _ => Err(invalid_record()),
    }
}

pub(crate) async fn run(
    shell: WebShell,
    config: &Config,
    url: &str,
    token: &str,
    ttl: Duration,
    mut shutdown: watch::Receiver<bool>,
    permit: OwnedSemaphorePermit,
) {
    if *shutdown.borrow() {
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
    // Bound allocation before decoding records, including fragmented messages.
    let limits = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_SIZE + 5))
        .max_frame_size(Some(MAX_MESSAGE_SIZE + 5))
        .write_buffer_size(0)
        .max_write_buffer_size(2 * MAX_MESSAGE_SIZE);
    let connect =
        connect_async_tls_with_config(request, Some(limits), false, Some(Connector::Rustls(tls)));
    let (socket, response) = tokio::select! {
        result = tokio::time::timeout(ttl.min(Duration::from_secs(10)), connect) => {
            match result { Ok(Ok(value)) => value, _ => return }
        },
        _ = shutdown.changed() => return,
    };
    if *shutdown.borrow()
        || response
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok())
            != Some(PROTOCOL)
    {
        return;
    }
    let socket = socket
        .with(|message| future::ready(encode(message)))
        .map(|result| result.and_then(decode));
    let stop = async move {
        if *shutdown.borrow_and_update() {
            return;
        }
        tokio::select! { _ = tokio::time::sleep_until(deadline) => {}, _ = shutdown.changed() => {} }
    };
    // Do not reconnect/replay an interactive shell after connection loss.
    // Cooperative cancellation lets the shared PTY engine terminate its group.
    shell.serve_with_cancel(socket, permit, stop).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn records_keep_binary_text_and_reject_malformed_frames() {
        let data = vec![0, 3, 27, 255];
        assert!(
            matches!(decode(encode(ShellMessage::Binary(data.clone().into())).unwrap()).unwrap(), ShellMessage::Binary(value) if value == data)
        );
        assert!(
            matches!(decode(encode(ShellMessage::Text("中文".into())).unwrap()).unwrap(), ShellMessage::Text(value) if value == "中文")
        );
        for bytes in [
            vec![],
            vec![0, 0, 0, 0, 1],
            vec![2, 0, 0, 0, 0],
            vec![1, 0, 0, 0, 1, 255],
            vec![0, 0, 0, 64, 1],
        ] {
            assert!(decode(Message::Binary(bytes.into())).is_err());
        }
        assert!(encode(ShellMessage::Binary(vec![0; MAX_MESSAGE_SIZE + 1].into())).is_err());
        assert!(decode(Message::Text("unframed".into())).is_err());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[allow(clippy::result_large_err)] // Required by Tungstenite's handshake callback.
    async fn reverse_wss_pty_resize_interrupt_and_cleanup() {
        use futures_util::{Sink, Stream};
        use rustls::{ServerConfig, pki_types::PrivateKeyDer};
        use std::sync::Arc;
        use tokio::net::TcpListener;
        use tokio_rustls::TlsAcceptor;
        use tokio_tungstenite::accept_hdr_async;
        let _ = rustls::crypto::ring::default_provider().install_default();

        async fn until<S>(ws: &mut S, marker: &str) -> String
        where
            S: Stream<Item = Result<Message, Error>> + Unpin,
        {
            tokio::time::timeout(Duration::from_secs(3), async {
                let mut output = String::new();
                while let Some(message) = ws.next().await {
                    if let ShellMessage::Binary(data) = decode(message.unwrap()).unwrap() {
                        output.push_str(&String::from_utf8_lossy(&data));
                        if output.contains(marker) {
                            return output;
                        }
                    }
                }
                panic!("terminal closed before marker {marker}");
            })
            .await
            .unwrap()
        }
        async fn input<S>(ws: &mut S, value: &[u8])
        where
            S: Sink<Message, Error = Error> + Unpin,
        {
            ws.send(encode(ShellMessage::Binary(value.to_vec().into())).unwrap())
                .await
                .unwrap();
        }
        for ending in ["disconnect", "disable", "ttl", "oversize"] {
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
                "wss://{}/api/remote/device/fixture",
                listener.local_addr().unwrap()
            );
            let shell = WebShell::new(true);
            let permit = shell.try_acquire().unwrap();
            let (shutdown, rx) = watch::channel(false);
            let run_shell = shell.clone();
            let task = tokio::spawn(async move {
                run(
                    run_shell,
                    &config,
                    &url,
                    &"a".repeat(64),
                    Duration::from_secs(5),
                    rx,
                    permit,
                )
                .await;
            });
            let (tcp, _) = listener.accept().await.unwrap();
            let tls = acceptor.accept(tcp).await.unwrap();
            let mut ws=accept_hdr_async(tls,|request: &tokio_tungstenite::tungstenite::handshake::server::Request, mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                assert_eq!(request.headers()["authorization"], format!("Bearer {}","a".repeat(64)));
                assert_eq!(request.headers()["x-nms-target-port"], "0");
                assert_eq!(request.headers()["sec-websocket-protocol"],PROTOCOL);
                response.headers_mut().insert("sec-websocket-protocol",HeaderValue::from_static(PROTOCOL));
                Ok(response)
            }).await.unwrap();
            let first = decode(ws.next().await.unwrap().unwrap()).unwrap();
            assert!(matches!(first,ShellMessage::Text(value) if value.contains("ready")));
            ws.send(Message::Ping(b"idle-probe".to_vec().into()))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                while let Some(message) = ws.next().await {
                    if matches!(message.unwrap(), Message::Pong(data) if data == b"idle-probe"[..])
                    {
                        return;
                    }
                }
                panic!("idle terminal did not answer ping");
            })
            .await
            .unwrap();
            input(&mut ws, b"printf '\\137PID:%s\\n' \"$$\"\n").await;
            let output = until(&mut ws, "_PID:").await;
            let pid: i32 = output
                .split("_PID:")
                .nth(1)
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .parse()
                .unwrap();
            ws.send(
                encode(ShellMessage::Text(
                    r#"{"type":"resize","cols":100,"rows":40}"#.into(),
                ))
                .unwrap(),
            )
            .await
            .unwrap();
            input(&mut ws, b"stty size; printf '\\137RESIZED\\n'\n").await;
            let output = until(&mut ws, "_RESIZED").await;
            assert!(output.contains("40 100"), "{output}");
            input(&mut ws, "printf '\\137UTF8:中文\\n'\n".as_bytes()).await;
            assert!(until(&mut ws, "_UTF8:中文").await.contains("_UTF8:中文"));
            input(&mut ws, b"sleep 20\n").await;
            tokio::time::sleep(Duration::from_millis(80)).await;
            input(&mut ws, &[3]).await;
            input(&mut ws, b"printf '\\137INTERRUPTED\\n'\n").await;
            until(&mut ws, "_INTERRUPTED").await;
            // A foreground command uses a different job-control process group.
            input(
                &mut ws,
                b"sh -c 'printf \"\\137CHILD:%s\\n\" \"$$\"; exec sleep 30'\n",
            )
            .await;
            let output = until(&mut ws, "_CHILD:").await;
            let child: i32 = output
                .split("_CHILD:")
                .nth(1)
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .parse()
                .unwrap();
            match ending {
                "disconnect" => {
                    ws.close(None).await.unwrap();
                }
                "disable" => {
                    shutdown.send_replace(true);
                }
                "oversize" => {
                    ws.send(Message::Binary(vec![0; MAX_MESSAGE_SIZE + 6].into()))
                        .await
                        .unwrap();
                }
                _ => {}
            }
            tokio::time::timeout(Duration::from_secs(7), task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(shell.status()["active_sessions"], 0);
            let permits: Vec<_> = (0..4)
                .map(|_| shell.try_acquire().expect("PTY permit leaked"))
                .collect();
            assert!(shell.try_acquire().is_none());
            drop(permits);
            assert!(
                !std::path::Path::new(&format!("/proc/{pid}")).exists(),
                "shell leader not reaped"
            );
            let stat = std::fs::read_to_string(format!("/proc/{child}/stat")).unwrap_or_default();
            assert!(
                stat.is_empty() || stat.split(") ").nth(1).is_some_and(|v| v.starts_with('Z')),
                "foreground command survived {ending}"
            );
        }
    }
}
