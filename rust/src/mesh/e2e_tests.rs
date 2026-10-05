//! Device end to end over loopback: a str0m "browser" offers through a fake
//! rendezvous hub, the device answers, ICE connects over UDP, DTLS and SCTP come
//! up, and the `nms` channel answers pings and requests.
use super::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::{net::UdpSocket as StdUdp, time::Instant as StdInstant};
use str0m::{
    Candidate, Event, Input, Output, Rtc,
    change::{SdpAnswer, SdpPendingOffer},
    net::{Protocol, Receive},
};
use tokio::{net::TcpListener, sync::oneshot};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{Request, Response},
    },
};

/// What the fake browser saw from the device.
#[derive(Debug, Default)]
struct Seen {
    pong: Option<Value>,
    /// Round trip of each numbered ping, in order.
    rtts: Vec<Duration>,
    /// Every binary frame, in arrival order.
    frames: Vec<(Value, Vec<u8>)>,
}

/// What the fake browser sends once the channel opens, and how many terminal
/// replies (`last:true` or `ok:false`) it waits for before reporting.
struct Plan {
    requests: Vec<Vec<u8>>,
    replies: usize,
    /// The browser's own address; loopback unless a test wants a real interface.
    bind: std::net::IpAddr,
    /// The device advertises only loopback (default) or every real interface.
    loopback_only: bool,
    /// Extra pings after the first, one every `PING_GAP`, each timed.
    pings: u32,
}

const PING_GAP: Duration = Duration::from_millis(300);

async fn browser(
    to_hub: mpsc::Sender<Value>,
    mut from_device: mpsc::Receiver<Value>,
    seen: mpsc::Sender<Seen>,
    plan: Plan,
) {
    let socket = StdUdp::bind((plan.bind, 0)).unwrap();
    socket.set_nonblocking(true).unwrap();
    let local = socket.local_addr().unwrap();
    let socket = tokio::net::UdpSocket::from_std(socket).unwrap();
    let mut rtc = Rtc::builder().build(StdInstant::now());
    let candidate = Candidate::host(local, "udp").unwrap();
    rtc.add_local_candidate(candidate.clone());
    let mut api = rtc.sdp_api();
    let channel_id = api.add_channel("nms".into());
    let (offer, pending) = api.apply().unwrap();
    let mut pending: Option<SdpPendingOffer> = Some(pending);
    to_hub
        .send(json!({"type":"offer","sdp":offer.to_sdp_string()}))
        .await
        .unwrap();
    to_hub
        .send(json!({"type":"ice","candidate":{"candidate":candidate.to_sdp_string(),"sdpMid":"0","sdpMLineIndex":0}}))
        .await
        .unwrap();
    let mut report = Seen::default();
    let mut sent = false;
    let mut buffer = vec![0u8; 2048];
    let mut next_ping = 0u32;
    let mut ping_at = tokio::time::Instant::now() + Duration::from_secs(3600);
    let mut ping_sent: std::collections::HashMap<u64, StdInstant> = Default::default();
    loop {
        let deadline = loop {
            match rtc.poll_output().unwrap() {
                Output::Transmit(t) => {
                    let _ = socket.send_to(&t.contents, t.destination).await;
                }
                Output::Timeout(at) => break at,
                Output::Event(Event::ChannelOpen(id, _)) if id == channel_id && !sent => {
                    sent = true;
                    let mut channel = rtc.channel(id).unwrap();
                    assert!(channel.write(false, br#"{"t":"ping","n":42}"#).unwrap());
                    for request in &plan.requests {
                        assert!(channel.write(true, request).unwrap());
                    }
                    ping_at = tokio::time::Instant::now() + PING_GAP;
                }
                Output::Event(Event::ChannelData(data)) => {
                    if data.binary {
                        let (header, payload) = frame::decode(&data.data).unwrap();
                        if header["t"] == "http.body" && !payload.is_empty() {
                            // Acknowledge what was consumed, as the real browser proxy does.
                            let ack =
                                json!({"t":"http.ack","id":header["id"],"bytes":payload.len()});
                            let ack = frame::encode(&ack, b"").unwrap();
                            let _ = rtc.channel(channel_id).unwrap().write(true, &ack);
                        }
                        report.frames.push((header, payload.to_vec()));
                    } else {
                        let pong: Option<Value> = serde_json::from_slice(&data.data).ok();
                        if let Some(at) = pong
                            .as_ref()
                            .and_then(|p| p["n"].as_u64())
                            .and_then(|n| ping_sent.remove(&n))
                        {
                            report.rtts.push(at.elapsed());
                        } else {
                            report.pong = pong;
                        }
                    }
                    let terminal = report
                        .frames
                        .iter()
                        .filter(|(h, _)| h["last"] == true || h["ok"] == false)
                        .count();
                    if report.pong.is_some()
                        && terminal >= plan.replies
                        && report.rtts.len() as u32 >= plan.pings
                    {
                        let _ = seen.send(report).await;
                        return;
                    }
                }
                Output::Event(_) => {}
            }
        };
        tokio::select! {
            received = socket.recv_from(&mut buffer) => {
                let (length, source) = received.unwrap();
                if let Ok(receive) = Receive::new(Protocol::Udp, source, local, &buffer[..length]) {
                    let _ = rtc.handle_input(Input::Receive(StdInstant::now(), receive));
                }
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                let _ = rtc.handle_input(Input::Timeout(StdInstant::now()));
            }
            _ = tokio::time::sleep_until(ping_at), if next_ping < plan.pings => {
                next_ping += 1;
                let n = 1000 + next_ping;
                let text = json!({"t":"ping","n":n}).to_string();
                assert!(rtc.channel(channel_id).unwrap().write(false, text.as_bytes()).unwrap());
                ping_sent.insert(u64::from(n), StdInstant::now());
                ping_at = tokio::time::Instant::now() + PING_GAP;
            }
            message = from_device.recv() => {
                let Some(message) = message else { return };
                match message["type"].as_str() {
                    Some("answer") => {
                        if let Some(pending) = pending.take() {
                            let answer = SdpAnswer::from_sdp_string(message["sdp"].as_str().unwrap()).unwrap();
                            rtc.sdp_api().accept_answer(pending, answer).unwrap();
                        }
                    }
                    Some("ice") => {
                        if let Ok(candidate) = Candidate::from_sdp_string(message["candidate"]["candidate"].as_str().unwrap()) {
                            rtc.add_remote_candidate(candidate);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// A one-connection rendezvous hub. It tells the device the browser is up, wraps
/// browser signals as the real hub does and hands the device's replies back.
async fn hub(
    listener: TcpListener,
    mut from_browser: mpsc::Receiver<Value>,
    to_browser: mpsc::Sender<Value>,
    checked: oneshot::Sender<(Option<String>, bool)>,
) {
    let (tcp, _) = listener.accept().await.unwrap();
    let mut seen_auth = None;
    let mut origin_present = false;
    let mut checked = Some(checked);
    #[allow(clippy::result_large_err)]
    let callback = |request: &Request, response: Response| {
        seen_auth = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        origin_present = request.headers().contains_key("origin");
        Ok(response)
    };
    let socket = accept_hdr_async(tcp, callback).await.unwrap();
    if let Some(checked) = checked.take() {
        let _ = checked.send((seen_auth, origin_present));
    }
    let (mut write, mut read) = socket.split();
    write
        .send(Message::Text(
            json!({"type":"hub","v":1,"peer":"up"}).to_string().into(),
        ))
        .await
        .unwrap();
    loop {
        tokio::select! {
            message = from_browser.recv() => {
                let Some(data) = message else { return };
                let wrapped = json!({"type":"signal","data":data});
                if write.send(Message::Text(wrapped.to_string().into())).await.is_err() { return }
            }
            message = read.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    let value: Value = serde_json::from_str(text.as_str()).unwrap();
                    if to_browser.send(value).await.is_err() { return }
                }
                Some(Ok(_)) => {}
                _ => return,
            }
        }
    }
}

async fn session(plan: Plan, port: u16) -> (Seen, Option<String>, bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "ws://{}/api/remote/device/session?transport=mesh",
        listener.local_addr().unwrap()
    );
    let loopback_only = plan.loopback_only;
    let (browser_to_hub, hub_from_browser) = mpsc::channel(32);
    let (hub_to_browser, browser_from_device) = mpsc::channel(32);
    let (checked_tx, checked_rx) = oneshot::channel();
    tokio::spawn(hub(listener, hub_from_browser, hub_to_browser, checked_tx));
    let (seen_tx, mut seen_rx) = mpsc::channel(1);
    tokio::spawn(browser(browser_to_hub, browser_from_device, seen_tx, plan));
    let params = Params {
        v: 1,
        stun: vec![],
        udp_ports: "49160-49223".into(),
        ttl_seconds: 60,
        rendezvous_url: url,
    };
    let (stop_tx, stop_rx) = watch::channel(false);
    let config = Config::default();
    let device = tokio::spawn({
        let params = params.clone();
        async move {
            run(
                &config,
                &params,
                "t".repeat(64).as_str(),
                Mode::Web(port),
                Duration::from_secs(60),
                stop_rx,
                loopback_only,
            )
            .await;
        }
    });
    let seen = tokio::time::timeout(Duration::from_secs(40), seen_rx.recv())
        .await
        .expect("channel never carried data")
        .unwrap();
    let (auth, origin) = checked_rx.await.unwrap();
    // Stopping the session releases everything promptly.
    stop_tx.send_replace(true);
    tokio::time::timeout(Duration::from_secs(5), device)
        .await
        .expect("device did not stop")
        .unwrap();
    (seen, auth, origin)
}

fn plan(requests: Vec<Value>, payloads: Vec<&[u8]>, replies: usize) -> Plan {
    Plan {
        requests: requests
            .iter()
            .zip(payloads)
            .map(|(header, payload)| frame::encode(header, payload).unwrap())
            .collect(),
        replies,
        bind: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        loopback_only: true,
        pings: 0,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_device_answers_the_offer_and_serves_the_channel() {
    let (seen, auth, origin) =
        session(plan(vec![json!({"t":"nope","id":3})], vec![b""], 1), 80).await;
    assert_eq!(seen.pong, Some(json!({"t":"pong","n":42})));
    let (header, _) = &seen.frames[0];
    assert_eq!(header["id"], 3);
    assert_eq!(header["ok"], false);
    assert_eq!(header["error"], "unsupported");
    assert_eq!(
        auth.as_deref(),
        Some(format!("Bearer {}", "t".repeat(64)).as_str())
    );
    assert!(!origin, "the device must not send an Origin header");
}

/// A device web server that returns a pattern of the requested size.
async fn web(size: usize) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut request = vec![0u8; 8192];
                let read = socket.read(&mut request).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&request[..read]).to_string();
                let body: Vec<u8> = (0..size).map(|n| ((n * 31 + 7) & 255) as u8).collect();
                let reply = if head.starts_with("GET /missing") {
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()
                } else {
                    [format!("HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nSet-Cookie: a=1\r\nContent-Length: {size}\r\n\r\n").into_bytes(), body].concat()
                };
                let _ = socket.write_all(&reply).await;
            });
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread")]
async fn the_device_web_ui_is_served_over_the_channel_byte_exact() {
    let size = 3 * 1024 * 1024 + 17;
    let port = web(size).await;
    let requests = vec![
        json!({"t":"http","id":1,"method":"GET","path":"/big.bin","headers":[["accept","*/*"]],"body_len":0}),
        json!({"t":"http","id":2,"method":"GET","path":"/missing","headers":[],"body_len":0}),
        json!({"t":"http","id":3,"method":"GET","path":"/x","headers":[],"body_len":0,"port":8080}),
    ];
    let (seen, _, _) = session(plan(requests, vec![b"", b"", b""], 3), port).await;
    let big: Vec<u8> = seen
        .frames
        .iter()
        .filter(|(h, _)| h["id"] == 1 && h["t"] == "http.body")
        .flat_map(|(_, payload)| payload.clone())
        .collect();
    let expected: Vec<u8> = (0..size).map(|n| ((n * 31 + 7) & 255) as u8).collect();
    assert_eq!(big.len(), expected.len());
    assert!(
        big == expected,
        "3 MiB download through the data channel must be byte-exact"
    );
    let head = seen
        .frames
        .iter()
        .find(|(h, _)| h["id"] == 1 && h["t"] == "http.head")
        .unwrap();
    assert_eq!(head.0["status"], 200);
    let missing = seen
        .frames
        .iter()
        .find(|(h, _)| h["id"] == 2 && h["t"] == "http.head")
        .unwrap();
    assert_eq!(
        (missing.0["status"].clone(), missing.0["last"].clone()),
        (json!(404), json!(true))
    );
    let refused = seen.frames.iter().find(|(h, _)| h["id"] == 3).unwrap();
    assert_eq!(refused.0["error"], "target_not_allowed");
}

/// The same exchange over this host's real interface addresses, which is what a
/// device does (one socket per address, one port). Skipped on a host without any.
#[tokio::test(flavor = "multi_thread")]
async fn the_device_connects_over_its_real_interface_addresses() {
    let Some(address) = peer::local_addresses(false).into_iter().next() else {
        eprintln!("no non-loopback interface: skipped");
        return;
    };
    let mut plan = plan(vec![json!({"t":"nope","id":3})], vec![b""], 1);
    plan.bind = address;
    plan.loopback_only = false;
    let (seen, _, _) = session(plan, 80).await;
    assert_eq!(seen.pong, Some(json!({"t":"pong","n":42})));
    assert_eq!(seen.frames[0].0["error"], "unsupported");
}

/// Needs the network: `ZWRT_MESH_STUN_TEST=stun:stun.cloudflare.com:3478 cargo test stun_probe`.
/// The device must offer a server-reflexive candidate learned on its ICE socket.
#[tokio::test(flavor = "multi_thread")]
async fn stun_probe_yields_a_server_reflexive_candidate() {
    let Ok(server) = std::env::var("ZWRT_MESH_STUN_TEST") else {
        eprintln!("ZWRT_MESH_STUN_TEST not set: skipped");
        return;
    };
    let (host, port) = stun::parse_server(&server).expect("stun:host[:port]");
    let resolved = resolve(&[(host, port)]).await;
    assert!(!resolved.is_empty(), "STUN server did not resolve");
    let mut rtc = Rtc::builder().build(StdInstant::now());
    let mut api = rtc.sdp_api();
    api.add_channel("nms".into());
    let (offer, _) = api.apply().unwrap();
    let (signals, mut seen) = mpsc::channel(32);
    let (_remote, remote_ice) = mpsc::channel(8);
    let task = tokio::spawn(peer::run(
        offer.to_sdp_string(),
        peer::Config {
            stun: resolved,
            ports: (49160, 49223),
            loopback_only: false,
        },
        peer::Link {
            signals,
            remote_ice,
            mode: Mode::Files,
        },
    ));
    let found = timeout_srflx(&mut seen).await;
    task.abort();
    let candidate = found.expect("no srflx candidate within 8 s");
    eprintln!("srflx: {candidate}");
    assert!(candidate.contains("typ srflx"));
}

async fn timeout_srflx(seen: &mut mpsc::Receiver<Value>) -> Option<String> {
    tokio::time::timeout(Duration::from_secs(8), async {
        while let Some(message) = seen.recv().await {
            if let Some(text) = message["candidate"]["candidate"].as_str()
                && text.contains("typ srflx")
            {
                return Some(text.to_owned());
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

/// Over loopback the link adds nothing, so a pong must come straight back.
#[tokio::test(flavor = "multi_thread")]
async fn pings_are_answered_without_waiting_for_a_timer() {
    let mut plan = plan(vec![], vec![], 0);
    plan.pings = 12;
    let (seen, _, _) = session(plan, 80).await;
    let mut rtts = seen.rtts.clone();
    rtts.sort();
    eprintln!("loopback ping rtt: {rtts:?}");
    assert_eq!(rtts.len(), 12);
    // Before the fix the median was about 105 ms (the next timer or packet).
    assert!(rtts[rtts.len() / 2] < Duration::from_millis(50), "{rtts:?}");
}

/// Pongs skip the queued HTTP frames, so a running download does not hold them
/// back on the device. The browser-side round trip also includes SCTP ordering
/// and the fake browser's own load (hundreds of ms on a busy CI runner), so only
/// the device-side handling time is asserted.
#[tokio::test(flavor = "multi_thread")]
async fn pings_stay_fast_during_a_download() {
    let size = 32 * 1024 * 1024;
    let port = web(size).await;
    let requests =
        vec![json!({"t":"http","id":1,"method":"GET","path":"/big.bin","headers":[],"body_len":0})];
    let mut plan = plan(requests, vec![b""], 1);
    plan.pings = 6;
    let started = StdInstant::now();
    let (seen, _, _) = session(plan, port).await;
    let mut rtts = seen.rtts.clone();
    rtts.sort();
    eprintln!(
        "ping rtt during download: {rtts:?}, session {:?}",
        started.elapsed()
    );
    let device = crate::mesh::ping_stats().unwrap();
    eprintln!("device side: {device}");
    let body: usize = seen
        .frames
        .iter()
        .filter(|(h, _)| h["id"] == 1 && h["t"] == "http.body")
        .map(|(_, payload)| payload.len())
        .sum();
    assert_eq!(body, size);
    assert_eq!(rtts.len(), 6);
    assert!(device["median_ms"].as_f64().unwrap() < 50.0, "{device}");
}
