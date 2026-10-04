//! A real browser against the device end: `cargo test browser_interop -- --ignored --nocapture`
//! serves a page that plays NMS's browser role (offer, ICE, the `nms` channel, a 3 MiB
//! download through the HTTP proxy) over a rendezvous hub on 127.0.0.1. Open the printed
//! URL in Chrome; the test passes when the page reports a byte-exact download.
use super::*;
use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::Html,
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::{net::TcpListener, sync::Mutex};

const PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>mesh interop</title><pre id="log">starting</pre>
<script>
const say = (m) => { document.getElementById('log').textContent += '\n' + m; fetch('/log', {method:'POST', body:String(m)}); };
const frame = (h, p = new Uint8Array(0)) => { const j = new TextEncoder().encode(JSON.stringify(h)); const b = new Uint8Array(4 + j.length + p.length); new DataView(b.buffer).setUint32(0, j.length); b.set(j, 4); b.set(p, 4 + j.length); return b; };
const parse = (buf) => { const n = new DataView(buf).getUint32(0); return [JSON.parse(new TextDecoder().decode(new Uint8Array(buf, 4, n))), new Uint8Array(buf, 4 + n)]; };
const hex = (buf) => [...new Uint8Array(buf)].map(x => x.toString(16).padStart(2, '0')).join('');
const ws = new WebSocket('ws://' + location.host + '/browser');
let pc, dc, started = false;
ws.onmessage = async (e) => {
  const m = JSON.parse(e.data);
  if (m.type === 'hub' && m.peer === 'up' && !started) { started = true; start(); }
  else if (m.type === 'signal') {
    const d = m.data;
    if (d.type === 'answer') await pc.setRemoteDescription({type: 'answer', sdp: d.sdp});
    else if (d.type === 'ice' && d.candidate) await pc.addIceCandidate(d.candidate).catch(err => say('addIceCandidate: ' + err));
  }
};
async function start() {
  const q = new URLSearchParams(location.search);
  pc = new RTCPeerConnection({iceServers: q.get('stun') ? [{urls: q.get('stun')}] : []});
  dc = pc.createDataChannel('nms');
  dc.binaryType = 'arraybuffer';
  pc.onicecandidate = (e) => { if (e.candidate) ws.send(JSON.stringify({type: 'ice', candidate: e.candidate})); };
  pc.oniceconnectionstatechange = () => say('ice ' + pc.iceConnectionState);
  const pending = new Map();
  dc.onmessage = (e) => {
    if (typeof e.data === 'string') { say('text ' + e.data); return; }
    const [h, p] = parse(e.data);
    const slot = pending.get(h.id);
    if (!slot) return;
    if (h.t === 'http.head') slot.head = h;
    if (h.t === 'http.body') { slot.chunks.push(p); slot.bytes += p.length; dc.send(frame({t: 'http.ack', id: h.id, bytes: p.length})); }
    if (h.ok === false) { slot.error = h.error; }
    if (h.last === true || h.ok === false) slot.done(slot);
  };
  const request = (id, path) => new Promise((done) => { pending.set(id, {chunks: [], bytes: 0, done}); dc.send(frame({t: 'http', id, method: 'GET', path, headers: [['accept', '*/*']], body_len: 0})); });
  dc.onopen = async () => {
    say('channel open');
    dc.send(JSON.stringify({t: 'ping', n: 42}));
    const big = await request(1, q.get('path') || '/big.bin');
    const all = new Uint8Array(big.bytes); let o = 0; for (const c of big.chunks) { all.set(c, o); o += c.length; }
    const digest = hex(await crypto.subtle.digest('SHA-256', all));
    const missing = await request(2, '/missing');
    const refused = await request(3, 'http://evil.example/');
    await fetch('/result', {method: 'POST', body: JSON.stringify({status: big.head && big.head.status, bytes: big.bytes, sha256: digest, missing: missing.head && missing.head.status, refused: refused.error})});
    say('done');
  };
  const offer = await pc.createOffer();
  await pc.setLocalDescription(offer);
  ws.send(JSON.stringify({type: 'offer', sdp: offer.sdp}));
}
</script>"#;

#[derive(Clone, Default)]
struct Hub {
    browser: Arc<Mutex<Option<mpsc::Sender<String>>>>,
    device: Arc<Mutex<Option<mpsc::Sender<String>>>>,
    result: Arc<Mutex<Option<mpsc::Sender<Value>>>>,
}

async fn serve(hub: Hub, socket: WebSocket, role: &'static str) {
    let (mut write, mut read) = socket.split();
    let (tx, mut rx) = mpsc::channel::<String>(64);
    let (mine, theirs) = if role == "browser" {
        (hub.browser.clone(), hub.device.clone())
    } else {
        (hub.device.clone(), hub.browser.clone())
    };
    *mine.lock().await = Some(tx.clone());
    let up = json!({"type":"hub","v":1,"peer":"up"}).to_string();
    if let Some(other) = theirs.lock().await.clone() {
        let _ = tx.send(up.clone()).await;
        let _ = other.send(up).await;
    } else {
        let _ = tx
            .send(json!({"type":"hub","v":1,"peer":"down"}).to_string())
            .await;
    }
    let writer = tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            if write.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });
    while let Some(Ok(message)) = read.next().await {
        if let Message::Text(text) = message {
            let Ok(data) = serde_json::from_str::<Value>(text.as_str()) else {
                continue;
            };
            if let Some(other) = theirs.lock().await.clone() {
                let _ = other
                    .send(json!({"type":"signal","data":data}).to_string())
                    .await;
            }
        }
    }
    writer.abort();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a real browser"]
async fn browser_interop() {
    let size = 3 * 1024 * 1024 + 17;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let web = listener.local_addr().unwrap().port();
    let body: Vec<u8> = (0..size).map(|n| ((n * 31 + 7) & 255) as u8).collect();
    let expected = format!("{:x}", Sha256::digest(&body));
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let body = body.clone();
            tokio::spawn(async move {
                let mut request = vec![0u8; 8192];
                let read = socket.read(&mut request).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&request[..read]).to_string();
                let reply = if head.starts_with("GET /missing") {
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()
                } else {
                    [
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len())
                            .into_bytes(),
                        body,
                    ]
                    .concat()
                };
                let _ = socket.write_all(&reply).await;
            });
        }
    });
    let hub = Hub::default();
    let (result_tx, mut result_rx) = mpsc::channel(1);
    *hub.result.lock().await = Some(result_tx);
    let app = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route(
            "/browser",
            get(
                |State(hub): State<Hub>, upgrade: WebSocketUpgrade| async move {
                    upgrade.on_upgrade(move |socket| serve(hub, socket, "browser"))
                },
            ),
        )
        .route(
            "/device",
            get(
                |State(hub): State<Hub>, upgrade: WebSocketUpgrade| async move {
                    upgrade.on_upgrade(move |socket| serve(hub, socket, "device"))
                },
            ),
        )
        .route(
            "/log",
            post(|body: String| async move {
                eprintln!("page: {body}");
            }),
        )
        .route(
            "/result",
            post(|State(hub): State<Hub>, body: String| async move {
                if let Some(tx) = hub.result.lock().await.clone() {
                    let _ = tx
                        .send(serde_json::from_str(&body).unwrap_or(Value::Null))
                        .await;
                }
            }),
        )
        .with_state(hub);
    let fixed: u16 = std::env::var("MESH_HUB_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let hub_listener = TcpListener::bind(("127.0.0.1", fixed)).await.unwrap();
    let hub_port = hub_listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(hub_listener, app).await.unwrap() });
    let params = Params {
        v: 1,
        stun: vec![],
        udp_ports: "49160-49223".into(),
        ttl_seconds: 600,
        rendezvous_url: format!("ws://127.0.0.1:{hub_port}/device"),
    };
    let remote_device = std::env::var("MESH_NO_DEVICE").is_ok();
    let (_stop_tx, stop_rx) = watch::channel(false);
    let config = Config::default();
    if !remote_device {
        tokio::spawn(async move {
            run(
                &config,
                &params,
                &"t".repeat(64),
                Mode::Web(web),
                Duration::from_secs(600),
                stop_rx,
                false,
            )
            .await;
        });
    }
    eprintln!("OPEN IN THE BROWSER: http://127.0.0.1:{hub_port}/");
    let result = tokio::time::timeout(Duration::from_secs(300), result_rx.recv())
        .await
        .expect("the browser never reported")
        .unwrap();
    eprintln!("RESULT: {result}");
    if remote_device {
        // A real device serves its own pages: the report is the evidence.
        return;
    }
    assert_eq!(result["status"], 200);
    assert_eq!(result["bytes"], size as u64);
    assert_eq!(
        result["sha256"],
        expected.as_str(),
        "byte-exact through a real browser"
    );
    assert_eq!(result["missing"], 404);
    assert_eq!(result["refused"], "target_not_allowed");
}

/// The device half on a real device: `MESH_HUB=ws://127.0.0.1:38000/device MESH_WEB_PORT=80
/// MESH_STUN=stun:stun.cloudflare.com:3478 MESH_SECONDS=240 mesh-tests device_side --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "runs the device end against a hub from the environment"]
async fn device_side() {
    let hub = std::env::var("MESH_HUB").expect("MESH_HUB");
    let web: u16 = std::env::var("MESH_WEB_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(80);
    let seconds: u64 = std::env::var("MESH_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(240);
    let stun: Vec<String> = std::env::var("MESH_STUN").into_iter().collect();
    let params = Params {
        v: 1,
        stun,
        udp_ports: "49160-49223".into(),
        ttl_seconds: seconds,
        rendezvous_url: hub,
    };
    let (_stop, stop_rx) = watch::channel(false);
    run(
        &Config::default(),
        &params,
        &"t".repeat(64),
        Mode::Web(web),
        Duration::from_secs(seconds),
        stop_rx,
        false,
    )
    .await;
}
