//! What the data channel serves: the device web UI over HTTP (`router_web`).
//! Frames reach `handle` in arrival order; every reply goes back through `out`,
//! so an unsupported or refused request is answered, never silently dropped.
use super::{
    Mode, frame,
    http::{self, Body, BodyKind, PIECE},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::{Notify, mpsc},
    task::JoinSet,
};

/// At most this many requests are in flight per session.
const MAX_IN_FLIGHT: usize = 16;
/// Unacknowledged response bytes per request.
const WINDOW: u64 = 1024 * 1024;
/// Request body bytes held in memory for the whole session.
const MAX_BUFFERED: usize = 32 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HEAD_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Flow control and cancellation shared with one request task.
struct Window {
    acked: AtomicU64,
    cancelled: AtomicBool,
    notify: Notify,
}

impl Window {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            acked: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
        })
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
        self.notify.notify_one();
    }

    async fn cancelled(&self) {
        while !self.cancelled.load(Ordering::SeqCst) {
            self.notify.notified().await;
        }
    }

    /// Waits until fewer than `WINDOW` bytes are unacknowledged.
    async fn room(&self, sent: u64) {
        while sent.saturating_sub(self.acked.load(Ordering::SeqCst)) > WINDOW {
            self.notify.notified().await;
        }
    }
}

struct Flight {
    /// Request body still to come, if any.
    body: Option<mpsc::UnboundedSender<Vec<u8>>>,
    received: u64,
    expected: u64,
    window: Arc<Window>,
    done: Arc<AtomicBool>,
    /// Request body bytes of this request that are queued but not yet written.
    held: Arc<AtomicUsize>,
}

pub struct Service {
    /// The one device port this session may reach, if it serves the web UI.
    port: Option<u16>,
    out: mpsc::Sender<Vec<u8>>,
    tasks: JoinSet<()>,
    flights: HashMap<u64, Flight>,
    buffered: Arc<AtomicUsize>,
}

impl Service {
    pub fn new(mode: Mode, out: mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            port: match mode {
                Mode::Web(port) => Some(port),
                Mode::Files => None,
            },
            out,
            tasks: JoinSet::new(),
            flights: HashMap::new(),
            buffered: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn handle(&mut self, header: Value, payload: &[u8]) {
        self.flights
            .retain(|_, flight| !flight.done.load(Ordering::SeqCst));
        // Finished tasks are reaped so the set does not grow with every request.
        while self.tasks.try_join_next().is_some() {}
        let Some(id) = header.get("id").and_then(Value::as_u64) else {
            return;
        };
        let Some(port) = self.port else {
            // Files sessions have no direct service: every request is refused so
            // the browser carries on over the relay.
            return reply(&self.out, id, "unsupported");
        };
        match header.get("t").and_then(Value::as_str) {
            Some("http") => self.open(id, port, &header, payload),
            Some("http.body") => self.more_body(id, payload),
            Some("http.ack") => {
                if let (Some(flight), Some(bytes)) = (
                    self.flights.get(&id),
                    header.get("bytes").and_then(Value::as_u64),
                ) {
                    flight
                        .window
                        .acked
                        .fetch_add(bytes.min(u64::from(u32::MAX)), Ordering::SeqCst);
                    flight.window.notify.notify_waiters();
                    flight.window.notify.notify_one();
                }
            }
            Some("http.cancel") => {
                if let Some(flight) = self.flights.remove(&id) {
                    flight.window.cancel();
                }
            }
            _ => reply(&self.out, id, "unsupported"),
        }
    }

    fn open(&mut self, id: u64, port: u16, header: &Value, payload: &[u8]) {
        if self.flights.contains_key(&id) {
            return reply(&self.out, id, "bad_request");
        }
        if self.flights.len() >= MAX_IN_FLIGHT {
            return reply(&self.out, id, "busy");
        }
        let request = match http::parse_request(header, port) {
            Ok(request) => request,
            Err(error) => return reply(&self.out, id, error),
        };
        if payload.len() as u64 > request.body_len {
            return reply(&self.out, id, "bad_request");
        }
        if self.buffered.load(Ordering::SeqCst) + payload.len() > MAX_BUFFERED {
            return reply(&self.out, id, "too_large");
        }
        self.buffered.fetch_add(payload.len(), Ordering::SeqCst);
        let held = Arc::new(AtomicUsize::new(payload.len()));
        let (body, body_rx) = if request.body_len > payload.len() as u64 {
            let (tx, rx) = mpsc::unbounded_channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let window = Window::new();
        let done = Arc::new(AtomicBool::new(false));
        self.flights.insert(
            id,
            Flight {
                body,
                received: payload.len() as u64,
                expected: request.body_len,
                window: window.clone(),
                done: done.clone(),
                held: held.clone(),
            },
        );
        let job = Job {
            id,
            port,
            request,
            initial: payload.to_vec(),
            body_rx,
            window,
            out: self.out.clone(),
            buffered: self.buffered.clone(),
            held,
        };
        self.tasks.spawn(async move {
            job.run().await;
            done.store(true, Ordering::SeqCst);
        });
    }

    fn more_body(&mut self, id: u64, payload: &[u8]) {
        let Some(flight) = self.flights.get_mut(&id) else {
            return;
        };
        flight.received += payload.len() as u64;
        let over_buffer = self.buffered.load(Ordering::SeqCst) + payload.len() > MAX_BUFFERED;
        if flight.received > flight.expected || over_buffer {
            let flight = self.flights.remove(&id).expect("flight exists");
            flight.window.cancel();
            return reply(
                &self.out,
                id,
                if over_buffer {
                    "too_large"
                } else {
                    "bad_request"
                },
            );
        }
        self.buffered.fetch_add(payload.len(), Ordering::SeqCst);
        flight.held.fetch_add(payload.len(), Ordering::SeqCst);
        if let Some(body) = &flight.body {
            let _ = body.send(payload.to_vec());
        }
        if flight.received == flight.expected {
            flight.body = None;
        }
    }
}

fn reply(out: &mpsc::Sender<Vec<u8>>, id: u64, error: &str) {
    if let Some(frame) = frame::encode(&json!({"id":id,"ok":false,"error":error}), b"") {
        let _ = out.try_send(frame);
    }
}

struct Job {
    id: u64,
    port: u16,
    request: http::Request,
    initial: Vec<u8>,
    body_rx: Option<mpsc::UnboundedReceiver<Vec<u8>>>,
    window: Arc<Window>,
    out: mpsc::Sender<Vec<u8>>,
    buffered: Arc<AtomicUsize>,
    held: Arc<AtomicUsize>,
}

impl Job {
    /// Gives `bytes` of queued request body back to the session budget.
    fn release(&self, bytes: usize) {
        let taken = bytes.min(self.held.load(Ordering::SeqCst));
        self.held.fetch_sub(taken, Ordering::SeqCst);
        let _ = self
            .buffered
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                Some(v.saturating_sub(taken))
            });
    }

    async fn run(mut self) {
        let window = self.window.clone();
        let (id, out) = (self.id, self.out.clone());
        let outcome = tokio::select! {
            result = self.exchange() => result,
            _ = window.cancelled() => Ok(()),
        };
        // Whatever of the request body was never written no longer counts.
        self.release(usize::MAX);
        if let Err(error) = outcome
            && let Some(frame) = frame::encode(&json!({"id":id,"ok":false,"error":error}), b"")
        {
            let _ = out.send(frame).await;
        }
    }

    async fn send(&self, header: Value, payload: &[u8]) -> Result<(), &'static str> {
        let frame = frame::encode(&header, payload).ok_or("bad_response")?;
        self.out.send(frame).await.map_err(|_| "channel_closed")
    }

    async fn exchange(&mut self) -> Result<(), &'static str> {
        let stream = tokio::time::timeout(
            CONNECT_TIMEOUT,
            TcpStream::connect(("127.0.0.1", self.port)),
        )
        .await
        .map_err(|_| "upstream_unreachable")?
        .map_err(|_| "upstream_unreachable")?;
        let (read, mut write) = stream.into_split();
        write
            .write_all(&http::head_bytes(&self.request, self.port))
            .await
            .map_err(|_| "upstream_unreachable")?;
        write
            .write_all(&self.initial)
            .await
            .map_err(|_| "upstream_unreachable")?;
        self.release(self.initial.len());
        let mut body_rx = self.body_rx.take();
        if let Some(rx) = body_rx.as_mut() {
            while let Some(chunk) = tokio::time::timeout(IDLE_TIMEOUT, rx.recv())
                .await
                .map_err(|_| "request_timeout")?
            {
                write
                    .write_all(&chunk)
                    .await
                    .map_err(|_| "upstream_closed")?;
                self.release(chunk.len());
            }
        }
        let mut reader = BufReader::new(read);
        let head_only = self.request.method == "HEAD";
        let head = tokio::time::timeout(HEAD_TIMEOUT, http::read_head(&mut reader, head_only))
            .await
            .map_err(|_| "upstream_timeout")??;
        let headers: Vec<Value> = head
            .headers
            .iter()
            .map(|(name, value)| json!([name, value]))
            .collect();
        self.send(json!({"t":"http.head","id":self.id,"status":head.status,"headers":headers,"last":head.body == BodyKind::None}), b"")
            .await?;
        if head.body == BodyKind::None {
            return Ok(());
        }
        let mut body = Body::new(reader, head.body);
        let mut sent = 0u64;
        loop {
            let piece = tokio::time::timeout(IDLE_TIMEOUT, body.next())
                .await
                .map_err(|_| "upstream_timeout")??;
            let Some(piece) = piece else {
                // Chunked and close-delimited bodies only end on the next read.
                return self
                    .send(json!({"t":"http.body","id":self.id,"last":true}), b"")
                    .await;
            };
            debug_assert!(piece.len() <= PIECE);
            self.window.room(sent).await;
            sent += piece.len() as u64;
            self.send(
                json!({"t":"http.body","id":self.id,"last":body.finished()}),
                &piece,
            )
            .await?;
            if body.finished() {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::timeout,
    };

    /// Scripted upstream: records every request head+body and answers with `reply`.
    async fn upstream(reply: Vec<u8>) -> (u16, Arc<Mutex<Vec<Vec<u8>>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let store = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let (reply, store) = (reply.clone(), store.clone());
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0u8; 8192];
                    // Read the head, then the announced body.
                    loop {
                        let read = socket.read(&mut buffer).await.unwrap_or(0);
                        if read == 0 {
                            break;
                        }
                        request.extend_from_slice(&buffer[..read]);
                        let text = String::from_utf8_lossy(&request).to_string();
                        if let Some(end) = text.find("\r\n\r\n") {
                            let length = text
                                .lines()
                                .find_map(|l| l.strip_prefix("Content-Length: "))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            if request.len() >= end + 4 + length {
                                break;
                            }
                        }
                    }
                    store.lock().unwrap().push(request);
                    let _ = socket.write_all(&reply).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (port, seen)
    }

    fn get(id: u64, path: &str) -> Value {
        json!({"t":"http","id":id,"method":"GET","path":path,"headers":[["accept","*/*"]],"body_len":0})
    }

    async fn frames_for(rx: &mut mpsc::Receiver<Vec<u8>>, id: u64) -> (Vec<Value>, Vec<u8>) {
        let mut headers = Vec::new();
        let mut body = Vec::new();
        loop {
            let data = timeout(Duration::from_secs(10), rx.recv())
                .await
                .expect("frame in time")
                .expect("open");
            let (header, payload) = frame::decode(&data).unwrap();
            if header["id"] != id {
                continue;
            }
            body.extend_from_slice(payload);
            let last = header["last"] == true || header["ok"] == false;
            headers.push(header);
            if last {
                return (headers, body);
            }
        }
    }

    #[tokio::test]
    async fn a_get_returns_the_head_and_the_exact_body() {
        let (port, seen) = upstream(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\nContent-Length: 5\r\n\r\nhello".to_vec()).await;
        let (out, mut rx) = mpsc::channel(64);
        let mut service = Service::new(Mode::Web(port), out);
        service.handle(get(1, "/x?y=1"), b"");
        let (frames, body) = frames_for(&mut rx, 1).await;
        assert_eq!(frames[0]["t"], "http.head");
        assert_eq!(frames[0]["status"], 200);
        assert_eq!(
            frames[0]["headers"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|h| h[0] == "set-cookie")
                .count(),
            2
        );
        assert_eq!(frames[0]["last"], false);
        assert_eq!(body, b"hello");
        assert_eq!(frames.last().unwrap()["last"], true);
        let request = String::from_utf8(seen.lock().unwrap()[0].clone()).unwrap();
        assert!(request.starts_with(&format!(
            "GET /x?y=1 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
        )));
        assert!(request.contains("Connection: close"));
    }

    #[tokio::test]
    async fn a_body_split_across_frames_reaches_the_device_byte_exact() {
        let (port, seen) = upstream(b"HTTP/1.1 204 No Content\r\n\r\n".to_vec()).await;
        let (out, mut rx) = mpsc::channel(64);
        let mut service = Service::new(Mode::Web(port), out);
        let body: Vec<u8> = (0..300_000u32).map(|n| (n * 7 % 251) as u8).collect();
        let (first, rest) = body.split_at(128 * 1024);
        let header = json!({"t":"http","id":9,"method":"POST","path":"/api","headers":[["content-type","application/octet-stream"]],"body_len":body.len()});
        service.handle(header, first);
        for chunk in rest.chunks(64 * 1024) {
            service.handle(json!({"t":"http.body","id":9}), chunk);
        }
        let (frames, _) = frames_for(&mut rx, 9).await;
        assert_eq!(frames[0]["status"], 204);
        assert_eq!(frames[0]["last"], true, "no body: the head ends the call");
        let request = seen.lock().unwrap()[0].clone();
        let split = request.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        assert!(
            String::from_utf8_lossy(&request[..split])
                .contains(&format!("Content-Length: {}", body.len()))
        );
        assert_eq!(&request[split..], &body[..]);
        assert_eq!(
            service.buffered.load(Ordering::SeqCst),
            0,
            "the body budget is given back"
        );
    }

    #[tokio::test]
    async fn chunked_responses_are_reframed() {
        let (port, _) = upstream(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n".to_vec()).await;
        let (out, mut rx) = mpsc::channel(64);
        let mut service = Service::new(Mode::Web(port), out);
        service.handle(get(2, "/c"), b"");
        let (frames, body) = frames_for(&mut rx, 2).await;
        assert_eq!(body, b"abcdef");
        assert!(
            frames[0]["headers"]
                .as_array()
                .unwrap()
                .iter()
                .all(|h| h[0] != "transfer-encoding")
        );
        assert_eq!(frames.last().unwrap()["last"], true);
    }

    #[tokio::test]
    async fn responses_stop_at_one_mebibyte_until_the_browser_acknowledges() {
        let payload: Vec<u8> = (0..3 * 1024 * 1024u32 + 17)
            .map(|n| (n * 31 + 7) as u8)
            .collect();
        let reply = [
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                payload.len()
            )
            .into_bytes(),
            payload.clone(),
        ]
        .concat();
        let (port, _) = upstream(reply).await;
        let (out, mut rx) = mpsc::channel(256);
        let mut service = Service::new(Mode::Web(port), out);
        service.handle(get(5, "/big"), b"");
        let mut received = Vec::new();
        let mut unacked = 0u64;
        let mut peak = 0u64;
        let mut stalled = false;
        let mut finished = false;
        while !finished {
            match timeout(Duration::from_millis(600), rx.recv()).await {
                Ok(Some(data)) => {
                    let (header, payload) = frame::decode(&data).unwrap();
                    assert!(payload.len() <= PIECE);
                    received.extend_from_slice(payload);
                    unacked += payload.len() as u64;
                    peak = peak.max(unacked);
                    finished = header["last"] == true;
                }
                Ok(None) => panic!("closed"),
                Err(_) => {
                    // Nothing more arrives until bytes are acknowledged.
                    stalled = true;
                    assert!(unacked <= WINDOW + PIECE as u64, "{unacked}");
                    service.handle(json!({"t":"http.ack","id":5,"bytes":unacked}), b"");
                    unacked = 0;
                }
            }
        }
        assert!(stalled, "the window never closed");
        assert!(peak <= WINDOW + PIECE as u64);
        assert_eq!(received, payload, "byte-exact through the window");
    }

    #[tokio::test]
    async fn cancel_closes_the_upstream_connection_and_frees_the_slot() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000000\r\n\r\n")
                .await;
            // Keep sending until the device hangs up.
            let chunk = vec![1u8; 16 * 1024];
            while socket.write_all(&chunk).await.is_ok() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            let _ = closed_tx.send(());
        });
        let (out, mut rx) = mpsc::channel(256);
        let mut service = Service::new(Mode::Web(port), out);
        service.handle(get(3, "/stream"), b"");
        let _ = frames_for_first(&mut rx).await;
        service.handle(json!({"t":"http.cancel","id":3}), b"");
        timeout(Duration::from_secs(5), closed_rx)
            .await
            .expect("upstream saw the hang-up")
            .unwrap();
        assert!(service.flights.is_empty());
    }

    async fn frames_for_first(rx: &mut mpsc::Receiver<Vec<u8>>) -> Value {
        let data = timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        frame::decode(&data).unwrap().0
    }

    #[tokio::test]
    async fn refused_requests_are_answered_with_a_code() {
        let (out, mut rx) = mpsc::channel(64);
        let mut service = Service::new(Mode::Web(80), out);
        for (header, error) in [
            (
                json!({"t":"http","id":1,"method":"GET","path":"/x","headers":[],"port":8080}),
                "target_not_allowed",
            ),
            (
                json!({"t":"http","id":2,"method":"GET","path":"http://evil/","headers":[]}),
                "target_not_allowed",
            ),
            (
                json!({"t":"http","id":3,"method":"CONNECT","path":"/x","headers":[]}),
                "method_not_allowed",
            ),
            (json!({"t":"files.upload.chunk","id":4}), "unsupported"),
            (json!({"t":"nonsense","id":5}), "unsupported"),
        ] {
            service.handle(header, b"");
            let reply = frames_for_first(&mut rx).await;
            assert_eq!(
                (reply["ok"].clone(), reply["error"].clone()),
                (json!(false), json!(error))
            );
        }
        // No id: nothing to answer.
        service.handle(json!({"t":"http"}), b"");
        assert!(rx.try_recv().is_err());
        // An unreachable device port is reported, not hung on.
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);
        let mut service = Service::new(Mode::Web(dead_port), service.out.clone());
        service.handle(get(6, "/"), b"");
        assert_eq!(
            frames_for_first(&mut rx).await["error"],
            "upstream_unreachable"
        );
    }

    #[tokio::test]
    async fn at_most_sixteen_requests_run_at_once_and_dropping_the_service_stops_them() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let hung_up = Arc::new(AtomicUsize::new(0));
        let (a, h) = (accepted.clone(), hung_up.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                a.fetch_add(1, Ordering::SeqCst);
                let h = h.clone();
                tokio::spawn(async move {
                    let mut buffer = [0u8; 4096];
                    // Never answers; ends when the device closes its side.
                    while socket.read(&mut buffer).await.unwrap_or(0) > 0 {}
                    h.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        let (out, mut rx) = mpsc::channel(64);
        let mut service = Service::new(Mode::Web(port), out);
        for id in 0..16 {
            service.handle(get(id, "/hang"), b"");
        }
        service.handle(get(99, "/one-too-many"), b"");
        assert_eq!(frames_for_first(&mut rx).await["error"], "busy");
        service.handle(get(3, "/duplicate"), b"");
        assert_eq!(frames_for_first(&mut rx).await["error"], "bad_request");
        timeout(Duration::from_secs(5), async {
            while accepted.load(Ordering::SeqCst) < 16 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        drop(service);
        timeout(Duration::from_secs(5), async {
            while hung_up.load(Ordering::SeqCst) < 16 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("every upstream connection was closed with the service");
    }
}
