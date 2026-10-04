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
    reply: Option<(Value, Vec<u8>)>,
}

async fn browser(
    to_hub: mpsc::Sender<Value>,
    mut from_device: mpsc::Receiver<Value>,
    seen: mpsc::Sender<Seen>,
    request: Option<Vec<u8>>,
) {
    let socket = StdUdp::bind("127.0.0.1:0").unwrap();
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
                    if let Some(request) = &request {
                        assert!(channel.write(true, request).unwrap());
                    }
                }
                Output::Event(Event::ChannelData(data)) => {
                    if data.binary {
                        let (header, payload) = frame::decode(&data.data).unwrap();
                        report.reply = Some((header, payload.to_vec()));
                    } else {
                        report.pong = serde_json::from_slice(&data.data).ok();
                    }
                    let done =
                        report.pong.is_some() && (request.is_none() || report.reply.is_some());
                    if done {
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

async fn session(request: Option<Vec<u8>>) -> (Seen, Option<String>, bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "ws://{}/api/remote/device/session?transport=mesh",
        listener.local_addr().unwrap()
    );
    let (browser_to_hub, hub_from_browser) = mpsc::channel(32);
    let (hub_to_browser, browser_from_device) = mpsc::channel(32);
    let (checked_tx, checked_rx) = oneshot::channel();
    tokio::spawn(hub(listener, hub_from_browser, hub_to_browser, checked_tx));
    let (seen_tx, mut seen_rx) = mpsc::channel(1);
    tokio::spawn(browser(
        browser_to_hub,
        browser_from_device,
        seen_tx,
        request,
    ));
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
                80,
                Duration::from_secs(60),
                stop_rx,
                true,
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

#[tokio::test(flavor = "multi_thread")]
async fn the_device_answers_the_offer_and_serves_the_channel() {
    let request = frame::encode(&json!({"t":"nope","id":3}), b"").unwrap();
    let (seen, auth, origin) = session(Some(request)).await;
    assert_eq!(seen.pong, Some(json!({"t":"pong","n":42})));
    let (header, _) = seen.reply.expect("reply to the unsupported request");
    assert_eq!(header["id"], 3);
    assert_eq!(header["ok"], false);
    assert_eq!(header["error"], "unsupported");
    assert_eq!(
        auth.as_deref(),
        Some(format!("Bearer {}", "t".repeat(64)).as_str())
    );
    assert!(!origin, "the device must not send an Origin header");
}
