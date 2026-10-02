//! Direct ubusd client over its unix socket, replacing one `ubus call`
//! fork/exec per request. Approach after the MU5250 fork by faying
//! (github.com/faying/zte-u60-pro-mu5250-data-service, MIT): one connection,
//! HELLO/LOOKUP/INVOKE/DATA/STATUS, a lookup cache, and at most one request in
//! flight for the whole process.
//!
//! The socket is the default; `ZWRT_DATAD_UBUS=cli` keeps `ubus call`, and the
//! CLI is also used whenever the socket cannot be used before a request is
//! sent. Results and error texts mirror the CLI so callers behave the same: an
//! OK reply without data is `{}`, and a non-zero status reads like the CLI
//! exit status (`exit status: 252` for NOT_FOUND).
use serde_json::{Map, Number, Value, json};
use std::{
    collections::HashMap,
    future::Future,
    sync::{
        OnceLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::{Mutex, Notify},
    time::{Instant, timeout_at},
};

const SOCKET: &str = "/var/run/ubus/ubus.sock";
const TIMEOUT: Duration = Duration::from_secs(8);
const MAX_MSG: usize = 1 << 20;
const MAX_DEPTH: usize = 32;

const ID_MASK: u32 = 0x7f00_0000;
const LEN_MASK: u32 = 0x00ff_ffff;
const EXTENDED: u32 = 0x8000_0000;

// enum ubus_msg_type
const HELLO: u8 = 0;
const STATUS: u8 = 1;
const DATA: u8 = 2;
const LOOKUP: u8 = 4;
const INVOKE: u8 = 5;
// enum ubus_msg_attr
const ATTR_STATUS: u8 = 1;
const ATTR_OBJPATH: u8 = 2;
const ATTR_OBJID: u8 = 3;
const ATTR_METHOD: u8 = 4;
const ATTR_DATA: u8 = 7;
// enum ubus_msg_status
const NOT_FOUND: u32 = 4;
// enum blobmsg_type
const T_UNSPEC: u8 = 0;
const T_ARRAY: u8 = 1;
const T_TABLE: u8 = 2;
const T_STRING: u8 = 3;
const T_INT64: u8 = 4;
const T_INT32: u8 = 5;
const T_INT16: u8 = 6;
const T_INT8: u8 = 7;
const T_DOUBLE: u8 = 8;

fn mode() -> &'static str {
    static MODE: OnceLock<String> = OnceLock::new();
    MODE.get_or_init(|| std::env::var("ZWRT_DATAD_UBUS").unwrap_or_default())
}

/// The socket backend is the default; `ZWRT_DATAD_UBUS=cli` forces the
/// `ubus call` path.
pub fn enabled() -> bool {
    mode() != "cli"
}

/// `ZWRT_DATAD_UBUS=socket`: never fall back to the CLI.
pub fn strict() -> bool {
    mode() == "socket"
}

fn socket_path() -> String {
    std::env::var("ZWRT_DATAD_UBUS_SOCKET").unwrap_or_else(|_| SOCKET.into())
}

// ------------------------------------------------------------------ blob

const fn align(n: usize) -> usize {
    (n + 3) & !3
}

struct Attr<'a> {
    id: u8,
    extended: bool,
    data: &'a [u8],
}

fn parse_attrs(mut rest: &[u8]) -> Result<Vec<Attr<'_>>, String> {
    let mut out = Vec::new();
    while !rest.is_empty() {
        let head: [u8; 4] = rest
            .get(..4)
            .and_then(|b| b.try_into().ok())
            .ok_or("truncated blob attribute")?;
        let id_len = u32::from_be_bytes(head);
        let len = (id_len & LEN_MASK) as usize;
        if len < 4 || len > rest.len() {
            return Err("malformed blob attribute length".into());
        }
        out.push(Attr {
            id: ((id_len & ID_MASK) >> 24) as u8,
            extended: id_len & EXTENDED != 0,
            data: &rest[4..len],
        });
        rest = &rest[align(len).min(rest.len())..];
    }
    Ok(out)
}

fn put_attr(out: &mut Vec<u8>, id: u8, extended: bool, payload: &[u8]) {
    let len = 4 + payload.len();
    let mut id_len = (u32::from(id) << 24) | len as u32;
    if extended {
        id_len |= EXTENDED;
    }
    out.extend_from_slice(&id_len.to_be_bytes());
    out.extend_from_slice(payload);
    out.resize(out.len() + align(len) - len, 0);
}

fn put_string(out: &mut Vec<u8>, id: u8, text: &str) {
    let mut payload = text.as_bytes().to_vec();
    payload.push(0);
    put_attr(out, id, false, &payload);
}

fn put_field(out: &mut Vec<u8>, kind: u8, name: &str, data: &[u8]) {
    let header = align(2 + name.len() + 1);
    let mut payload = Vec::with_capacity(header + data.len());
    payload.extend_from_slice(&(name.len() as u16).to_be_bytes());
    payload.extend_from_slice(name.as_bytes());
    payload.resize(header, 0);
    payload.extend_from_slice(data);
    put_attr(out, kind, true, &payload);
}

/// JSON → blobmsg with the ubus CLI's type choices (`blobmsg_add_json_element`).
fn put_json(out: &mut Vec<u8>, name: &str, value: &Value) {
    match value {
        Value::Null => put_field(out, T_UNSPEC, name, &[]),
        Value::Bool(flag) => put_field(out, T_INT8, name, &[u8::from(*flag)]),
        Value::Number(number) => match number.as_i64() {
            Some(int) => match i32::try_from(int) {
                Ok(small) => put_field(out, T_INT32, name, &small.to_be_bytes()),
                Err(_) => put_field(out, T_INT64, name, &int.to_be_bytes()),
            },
            None => put_field(
                out,
                T_DOUBLE,
                name,
                &number.as_f64().unwrap_or(0.0).to_bits().to_be_bytes(),
            ),
        },
        Value::String(text) => {
            let mut data = text.as_bytes().to_vec();
            data.push(0);
            put_field(out, T_STRING, name, &data);
        }
        Value::Array(items) => {
            let mut inner = Vec::new();
            for item in items {
                put_json(&mut inner, "", item);
            }
            put_field(out, T_ARRAY, name, &inner);
        }
        Value::Object(map) => put_field(out, T_TABLE, name, &table(map)),
    }
}

fn table(map: &Map<String, Value>) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in map {
        put_json(&mut out, name, value);
    }
    out
}

/// blobmsg → JSON as `blobmsg_format_json` prints it (INT8 is a boolean).
fn field(attr: &Attr<'_>, depth: usize) -> Result<(String, Value), String> {
    if !attr.extended || attr.data.len() < 3 {
        return Err("malformed blobmsg field".into());
    }
    let name_len = usize::from(u16::from_be_bytes([attr.data[0], attr.data[1]]));
    let header = align(2 + name_len + 1);
    if header > attr.data.len() {
        return Err("malformed blobmsg name".into());
    }
    let name = String::from_utf8_lossy(&attr.data[2..2 + name_len]).into_owned();
    let data = &attr.data[header..];
    let fixed = |size: usize| -> Result<&[u8], String> {
        (data.len() == size)
            .then_some(data)
            .ok_or_else(|| format!("blobmsg type {} has length {}", attr.id, data.len()))
    };
    let value = match attr.id {
        T_UNSPEC => Value::Null,
        T_ARRAY | T_TABLE if depth >= MAX_DEPTH => return Err("blobmsg nested too deep".into()),
        T_ARRAY => Value::Array(
            parse_attrs(data)?
                .iter()
                .map(|item| field(item, depth + 1).map(|(_, value)| value))
                .collect::<Result<_, _>>()?,
        ),
        T_TABLE => Value::Object(object(data, depth + 1)?),
        T_STRING => {
            let end = data.iter().position(|b| *b == 0).unwrap_or(data.len());
            Value::String(String::from_utf8_lossy(&data[..end]).into_owned())
        }
        T_INT64 => json!(i64::from_be_bytes(fixed(8)?.try_into().unwrap())),
        T_INT32 => json!(i32::from_be_bytes(fixed(4)?.try_into().unwrap())),
        T_INT16 => json!(i16::from_be_bytes(fixed(2)?.try_into().unwrap())),
        T_INT8 => Value::Bool(fixed(1)?[0] != 0),
        T_DOUBLE => Number::from_f64(f64::from_bits(u64::from_be_bytes(
            fixed(8)?.try_into().unwrap(),
        )))
        .map_or(Value::Null, Value::Number),
        other => return Err(format!("unknown blobmsg type {other}")),
    };
    Ok((name, value))
}

fn object(data: &[u8], depth: usize) -> Result<Map<String, Value>, String> {
    let mut map = Map::new();
    for attr in parse_attrs(data)? {
        let (name, value) = field(&attr, depth)?;
        map.insert(name, value);
    }
    Ok(map)
}

fn object_value(data: &[u8]) -> Result<Value, String> {
    object(data, 0).map(Value::Object)
}

fn message_attr<'a>(attrs: &'a [Attr<'a>], id: u8) -> Option<&'a Attr<'a>> {
    attrs.iter().rev().find(|attr| attr.id == id)
}

fn frame(kind: u8, seq: u16, peer: u32, body: &[u8]) -> Vec<u8> {
    let mut out = vec![0, kind];
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&peer.to_be_bytes());
    put_attr(&mut out, 0, false, body);
    out
}

struct Frame {
    kind: u8,
    seq: u16,
    peer: u32,
    body: Vec<u8>,
}

async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Frame, String> {
    let mut head = [0_u8; 12];
    reader
        .read_exact(&mut head)
        .await
        .map_err(|e| format!("ubus read: {e}"))?;
    let len = (u32::from_be_bytes([head[8], head[9], head[10], head[11]]) & LEN_MASK) as usize;
    if !(4..=MAX_MSG).contains(&len) {
        return Err("ubus message length out of range".into());
    }
    let mut body = vec![0_u8; len - 4];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|e| format!("ubus read: {e}"))?;
    Ok(Frame {
        kind: head[1],
        seq: u16::from_be_bytes([head[2], head[3]]),
        peer: u32::from_be_bytes([head[4], head[5], head[6], head[7]]),
        body,
    })
}

// ---------------------------------------------------------------- client

enum Failure {
    /// The request never reached ubusd; falling back to the CLI is safe.
    NotSent(String),
    /// Sent (or possibly sent): never repeat it.
    Error(String),
    /// No complete reply before the deadline; the request may have run.
    TimedOut,
    Status(u32),
    /// LOOKUP found no such object; the CLI exits with the positive code.
    Missing,
}

#[derive(Default)]
struct Client {
    stream: Option<UnixStream>,
    seq: u16,
    ids: HashMap<String, u32>,
}

impl Client {
    fn next_seq(&mut self) -> u16 {
        self.seq = self.seq.wrapping_add(1).max(1);
        self.seq
    }

    async fn connect(&mut self, path: &str, deadline: Instant) -> Result<(), Failure> {
        if self.stream.is_some() {
            return Ok(());
        }
        let mut stream = timeout_at(deadline, UnixStream::connect(path))
            .await
            .map_err(|_| Failure::NotSent("ubus connect timed out".into()))?
            .map_err(|e| Failure::NotSent(format!("ubus connect: {e}")))?;
        let hello = timeout_at(deadline, read_frame(&mut stream))
            .await
            .map_err(|_| Failure::NotSent("ubus hello timed out".into()))?
            .map_err(Failure::NotSent)?;
        if hello.kind != HELLO {
            return Err(Failure::NotSent("ubus did not greet".into()));
        }
        STATS.connects.fetch_add(1, Ordering::Relaxed);
        self.stream = Some(stream);
        Ok(())
    }

    /// One request; replies are matched on seq and peer like libubus does.
    /// A late reply to an abandoned request can never match a later seq.
    async fn request(
        &mut self,
        path: &str,
        kind: u8,
        peer: u32,
        body: &[u8],
        deadline: Instant,
    ) -> Result<Vec<Vec<u8>>, Failure> {
        let reused = self.stream.is_some();
        self.connect(path, deadline).await?;
        let seq = self.next_seq();
        let bytes = frame(kind, seq, peer, body);
        // The stream is owned by this request until a complete reply arrives.
        // If the caller is cancelled mid-frame, the stream is dropped with the
        // future and the next request starts on a fresh connection.
        let mut stream = self.stream.take().expect("connected");
        match timeout_at(deadline, stream.write_all(&bytes)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                // A dead cached connection (ubusd restarted) means nothing was
                // delivered; object ids may all have changed.
                self.ids.clear();
                return Err(if reused {
                    Failure::NotSent(format!("ubus write: {error}"))
                } else {
                    Failure::Error(format!("ubus write: {error}"))
                });
            }
            Err(_) => return Err(Failure::TimedOut),
        }
        let mut data = Vec::new();
        loop {
            let reply = match timeout_at(deadline, read_frame(&mut stream)).await {
                Ok(Ok(reply)) => reply,
                Ok(Err(error)) => {
                    self.ids.clear();
                    return Err(Failure::Error(error));
                }
                Err(_) => return Err(Failure::TimedOut),
            };
            if reply.seq != seq || reply.peer != peer {
                continue;
            }
            match reply.kind {
                DATA => data.push(reply.body),
                STATUS => {
                    let attrs = parse_attrs(&reply.body).map_err(Failure::Error)?;
                    let code = message_attr(&attrs, ATTR_STATUS)
                        .and_then(|attr| attr.data.try_into().ok())
                        .map(u32::from_be_bytes)
                        .ok_or_else(|| Failure::Error("ubus status missing".into()))?;
                    self.stream = Some(stream);
                    return if code == 0 {
                        Ok(data)
                    } else {
                        Err(Failure::Status(code))
                    };
                }
                _ => {}
            }
        }
    }

    async fn lookup(
        &mut self,
        path: &str,
        object: &str,
        deadline: Instant,
    ) -> Result<u32, Failure> {
        STATS.lookups.fetch_add(1, Ordering::Relaxed);
        let mut body = Vec::new();
        put_string(&mut body, ATTR_OBJPATH, object);
        // A lookup has no side effects, so any transport failure may fall back.
        let replies = match self.request(path, LOOKUP, 0, &body, deadline).await {
            Err(Failure::Error(error)) => return Err(Failure::NotSent(error)),
            Err(Failure::TimedOut) => return Err(Failure::NotSent("ubus lookup timed out".into())),
            Err(Failure::Status(NOT_FOUND)) => return Err(Failure::Missing),
            other => other?,
        };
        for reply in &replies {
            let attrs = parse_attrs(reply).map_err(Failure::NotSent)?;
            let path = message_attr(&attrs, ATTR_OBJPATH).map(|attr| {
                let end = attr
                    .data
                    .iter()
                    .position(|b| *b == 0)
                    .unwrap_or(attr.data.len());
                &attr.data[..end]
            });
            let id = message_attr(&attrs, ATTR_OBJID)
                .and_then(|attr| attr.data.try_into().ok())
                .map(u32::from_be_bytes);
            if let (Some(path), Some(id)) = (path, id)
                && path == object.as_bytes()
            {
                self.ids.insert(object.into(), id);
                return Ok(id);
            }
        }
        Err(Failure::Missing)
    }

    async fn call(
        &mut self,
        path: &str,
        object: &str,
        method: &str,
        args: &[u8],
        timeout: Duration,
    ) -> Result<Value, Failure> {
        let deadline = Instant::now() + timeout;
        let mut relooked = false;
        loop {
            let id = match self.ids.get(object) {
                Some(id) => *id,
                None => self.lookup(path, object, deadline).await?,
            };
            let mut body = Vec::new();
            put_attr(&mut body, ATTR_OBJID, false, &id.to_be_bytes());
            put_string(&mut body, ATTR_METHOD, method);
            put_attr(&mut body, ATTR_DATA, false, args);
            match self.request(path, INVOKE, id, &body, deadline).await {
                Ok(replies) => {
                    for reply in &replies {
                        let attrs = parse_attrs(reply).map_err(Failure::Error)?;
                        if let Some(data) = message_attr(&attrs, ATTR_DATA) {
                            return object_value(data.data).map_err(Failure::Error);
                        }
                    }
                    return Ok(json!({}));
                }
                // The object may have re-registered under a new id. Retry only
                // when the id really changed, so a service that answers
                // NOT_FOUND for its own reasons is never invoked twice.
                Err(Failure::Status(NOT_FOUND)) if !relooked => {
                    relooked = true;
                    self.ids.remove(object);
                    match self.lookup(path, object, deadline).await {
                        Ok(new_id) if new_id != id => {}
                        _ => return Err(Failure::Status(NOT_FOUND)),
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
}

// -------------------------------------------------------------- executor
//
// Every ubus request in the process goes through one connection, one at a
// time. State collection runs in the `collecting` scope and gives way to
// interactive requests (controls, panels, schedules) waiting behind it; a
// collection read that times out puts its object on a short cooldown so a
// stuck vendor service cannot stall every round.

tokio::task_local! {
    static COLLECTING: ();
}

/// Runs a state collection round on the low-priority lane.
pub async fn collecting<F: Future>(future: F) -> F::Output {
    COLLECTING.scope((), future).await
}

fn in_collection() -> bool {
    COLLECTING.try_with(|_| ()).is_ok()
}

const MAX_INTERACTIVE_WAITING: usize = 8;
const MISSING_TTL: Duration = Duration::from_secs(10);

struct Executor {
    /// `None`: `ZWRT_DATAD_UBUS_SOCKET` or the system socket.
    path: Option<String>,
    timeout: Duration,
    collect_timeout: Duration,
    cooldown: Duration,
    socket_retry: Duration,
    client: Mutex<Option<Client>>,
    interactive_waiting: AtomicUsize,
    interactive_idle: Notify,
    cooldowns: std::sync::Mutex<Option<HashMap<String, std::time::Instant>>>,
    /// Objects a collection lookup just found missing (not registered on this
    /// model, or a service still starting); looked up again after `MISSING_TTL`.
    missing: std::sync::Mutex<Option<HashMap<String, std::time::Instant>>>,
    socket_down_until: std::sync::Mutex<Option<std::time::Instant>>,
}

static EXECUTOR: Executor = Executor::new(
    None,
    TIMEOUT,
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(30),
);

/// Holds an interactive place in the queue until the connection is taken.
struct Ticket<'a>(&'a Executor);

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        if self.0.interactive_waiting.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.interactive_idle.notify_waiters();
        }
    }
}

impl Executor {
    const fn new(
        path: Option<String>,
        timeout: Duration,
        collect_timeout: Duration,
        cooldown: Duration,
        socket_retry: Duration,
    ) -> Self {
        Self {
            path,
            timeout,
            collect_timeout,
            cooldown,
            socket_retry,
            client: Mutex::const_new(None),
            interactive_waiting: AtomicUsize::new(0),
            interactive_idle: Notify::const_new(),
            cooldowns: std::sync::Mutex::new(None),
            socket_down_until: std::sync::Mutex::new(None),
            missing: std::sync::Mutex::new(None),
        }
    }

    fn path(&self) -> String {
        self.path.clone().unwrap_or_else(socket_path)
    }

    fn ticket(&self) -> Option<Ticket<'_>> {
        if self.interactive_waiting.fetch_add(1, Ordering::AcqRel) >= MAX_INTERACTIVE_WAITING {
            self.interactive_waiting.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Ticket(self))
    }

    async fn yield_to_interactive(&self) {
        loop {
            let idle = self.interactive_idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if self.interactive_waiting.load(Ordering::Acquire) == 0 {
                return;
            }
            idle.await;
        }
    }

    fn cooling_down(&self, object: &str) -> bool {
        let Ok(mut guard) = self.cooldowns.lock() else {
            return false;
        };
        let map = guard.get_or_insert_with(HashMap::new);
        let now = std::time::Instant::now();
        map.retain(|_, until| *until > now);
        map.contains_key(object)
    }

    fn start_cooldown(&self, object: &str) {
        if let Ok(mut guard) = self.cooldowns.lock() {
            guard
                .get_or_insert_with(HashMap::new)
                .insert(object.into(), std::time::Instant::now() + self.cooldown);
        }
    }

    fn known_missing(&self, object: &str) -> bool {
        let Ok(mut guard) = self.missing.lock() else {
            return false;
        };
        let map = guard.get_or_insert_with(HashMap::new);
        let now = std::time::Instant::now();
        map.retain(|_, until| *until > now);
        map.contains_key(object)
    }

    fn remember_missing(&self, object: &str) {
        if let Ok(mut guard) = self.missing.lock() {
            guard
                .get_or_insert_with(HashMap::new)
                .insert(object.into(), std::time::Instant::now() + MISSING_TTL);
        }
    }

    fn socket_down(&self) -> bool {
        self.socket_down_until
            .lock()
            .ok()
            .and_then(|until| *until)
            .is_some_and(|until| std::time::Instant::now() < until)
    }

    fn mark_socket_down(&self, down: bool) {
        if let Ok(mut until) = self.socket_down_until.lock() {
            *until = down.then(|| std::time::Instant::now() + self.socket_retry);
        }
    }

    async fn call(
        &self,
        object: &str,
        method: &str,
        args: &Value,
    ) -> Option<Result<Value, String>> {
        let map = args.as_object()?;
        if self.socket_down() {
            STATS.fallbacks.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let payload = table(map);
        if payload.len() + 256 > MAX_MSG {
            return Some(Err("ubus args too large".into()));
        }
        let collect = in_collection();
        if collect && self.cooling_down(object) {
            STATS.skipped.fetch_add(1, Ordering::Relaxed);
            return Some(Err(format!(
                "ubus {object}: skipped after a recent timeout"
            )));
        }
        if collect && self.known_missing(object) {
            STATS.errors.fetch_add(1, Ordering::Relaxed);
            return Some(Err(status_text(NOT_FOUND, true)));
        }
        STATS.calls.fetch_add(1, Ordering::Relaxed);
        let queued = std::time::Instant::now();
        let ticket = if collect {
            None
        } else {
            match self.ticket() {
                Some(ticket) => Some(ticket),
                None => {
                    STATS.busy.fetch_add(1, Ordering::Relaxed);
                    return Some(Err("ubus busy".into()));
                }
            }
        };
        // A collection call that reaches the connection while an interactive
        // request waits hands it on and queues again, so controls never sit
        // behind collection calls that were already queued.
        let mut guard = loop {
            if collect {
                self.yield_to_interactive().await;
            }
            let guard = self.client.lock().await;
            if collect && self.interactive_waiting.load(Ordering::Acquire) > 0 {
                drop(guard);
                continue;
            }
            break guard;
        };
        drop(ticket);
        let started = std::time::Instant::now();
        STATS.wait_us.fetch_add(
            started.duration_since(queued).as_micros() as u64,
            Ordering::Relaxed,
        );
        let path = self.path();
        let timeout = if collect {
            self.collect_timeout
        } else {
            self.timeout
        };
        let client = guard.get_or_insert_with(Client::default);
        let reused = client.stream.is_some();
        let mut result = client.call(&path, object, method, &payload, timeout).await;
        // A stale connection (ubusd restarted) delivered nothing: retry once fresh.
        if reused && matches!(result, Err(Failure::NotSent(_))) {
            result = client.call(&path, object, method, &payload, timeout).await;
        }
        drop(guard);
        STATS
            .busy_us
            .fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        if !matches!(result, Err(Failure::NotSent(_))) {
            self.mark_socket_down(false);
        }
        match result {
            Ok(value) => Some(Ok(value)),
            Err(Failure::NotSent(reason)) => {
                STATS.fallbacks.fetch_add(1, Ordering::Relaxed);
                self.mark_socket_down(true);
                if let Ok(mut last) = LAST_FALLBACK.lock() {
                    *last = reason;
                }
                None
            }
            Err(Failure::TimedOut) => {
                STATS.timeouts.fetch_add(1, Ordering::Relaxed);
                if collect {
                    self.start_cooldown(object);
                }
                Some(Err(format!("{} timed out", ubus_bin())))
            }
            Err(Failure::Status(code)) => {
                STATS.errors.fetch_add(1, Ordering::Relaxed);
                Some(Err(status_text(code, false)))
            }
            Err(Failure::Missing) => {
                STATS.errors.fetch_add(1, Ordering::Relaxed);
                if collect {
                    self.remember_missing(object);
                }
                Some(Err(status_text(NOT_FOUND, true)))
            }
            Err(Failure::Error(error)) => {
                STATS.errors.fetch_add(1, Ordering::Relaxed);
                Some(Err(error))
            }
        }
    }
}

fn ubus_bin() -> String {
    std::env::var("ZWRT_DATAD_UBUS_BIN").unwrap_or_else(|_| "/bin/ubus".into())
}

/// Same text as `command::run` gives for the CLI: an invoke status exits as
/// the negated 8-bit code, an unknown object with the positive one.
fn status_text(code: u32, lookup: bool) -> String {
    let exit = if lookup {
        code & 0xff
    } else {
        (256 - (code & 0xff)) & 0xff
    };
    format!("{} exited with exit status: {exit}", ubus_bin())
}

/// `None`: the socket could not be used before anything was sent; the
/// caller runs the CLI instead.
pub async fn call(object: &str, method: &str, args: &Value) -> Option<Result<Value, String>> {
    EXECUTOR.call(object, method, args).await
}

pub struct Stats {
    calls: AtomicU64,
    fallbacks: AtomicU64,
    errors: AtomicU64,
    timeouts: AtomicU64,
    skipped: AtomicU64,
    busy: AtomicU64,
    connects: AtomicU64,
    lookups: AtomicU64,
    wait_us: AtomicU64,
    busy_us: AtomicU64,
}

static STATS: Stats = Stats {
    calls: AtomicU64::new(0),
    fallbacks: AtomicU64::new(0),
    errors: AtomicU64::new(0),
    timeouts: AtomicU64::new(0),
    skipped: AtomicU64::new(0),
    busy: AtomicU64::new(0),
    connects: AtomicU64::new(0),
    lookups: AtomicU64::new(0),
    wait_us: AtomicU64::new(0),
    busy_us: AtomicU64::new(0),
};

static LAST_FALLBACK: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
static ROUNDS: AtomicU64 = AtomicU64::new(0);
static ROUND_US: AtomicU64 = AtomicU64::new(0);
static ROUND_MAX_US: AtomicU64 = AtomicU64::new(0);
static ROUND_LAST_US: AtomicU64 = AtomicU64::new(0);

/// Duration of one state collection round (measurement aid).
pub fn record_round(elapsed: Duration) {
    let micros = elapsed.as_micros() as u64;
    ROUNDS.fetch_add(1, Ordering::Relaxed);
    ROUND_US.fetch_add(micros, Ordering::Relaxed);
    ROUND_MAX_US.fetch_max(micros, Ordering::Relaxed);
    ROUND_LAST_US.store(micros, Ordering::Relaxed);
}

pub fn stats() -> Value {
    let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    let rounds = get(&ROUNDS).max(1);
    json!({
        "rounds": get(&ROUNDS),
        "round_avg_ms": get(&ROUND_US) / rounds / 1000,
        "round_max_ms": get(&ROUND_MAX_US) / 1000,
        "round_last_ms": get(&ROUND_LAST_US) / 1000,
        "backend": if !enabled() {"cli"} else if strict() {"socket-only"} else {"socket"},
        "calls": get(&STATS.calls),
        "fallbacks": get(&STATS.fallbacks),
        "errors": get(&STATS.errors),
        "timeouts": get(&STATS.timeouts),
        "skipped": get(&STATS.skipped),
        "busy": get(&STATS.busy),
        "connects": get(&STATS.connects),
        "lookups": get(&STATS.lookups),
        "wait_ms": get(&STATS.wait_us) / 1000,
        "busy_ms": get(&STATS.busy_us) / 1000,
        "last_fallback": LAST_FALLBACK.lock().map(|v| v.clone()).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trips_through_blobmsg_like_the_cli() {
        let value = json!({"s":"text","i":42,"neg":-7,"big":5_000_000_000_i64,"f":1.5,
            "b":true,"n":null,"a":[1,"x",{"k":false}],"t":{"inner":"v"},"empty":{}});
        let encoded = table(value.as_object().unwrap());
        let decoded = object_value(&encoded).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn malformed_and_deep_blobs_are_rejected() {
        assert!(object_value(&[0x80, 0, 0, 2]).is_err());
        assert!(object_value(&[0x83, 0, 0, 0x40]).is_err());
        let mut nested = json!("leaf");
        for _ in 0..40 {
            nested = json!({"x": nested});
        }
        let encoded = table(nested.as_object().unwrap());
        assert!(object_value(&encoded).is_err());
    }

    #[test]
    fn status_reads_like_the_cli_exit_code() {
        assert_eq!(
            status_text(4, false),
            "/bin/ubus exited with exit status: 252"
        );
        assert_eq!(
            status_text(7, false),
            "/bin/ubus exited with exit status: 249"
        );
        assert_eq!(status_text(4, true), "/bin/ubus exited with exit status: 4");
    }

    use std::sync::{Arc, Mutex as StdMutex};
    use tokio::net::UnixListener;

    /// Fake ubusd. Objects: `fast` echoes args, `slow` answers after 300 ms,
    /// `gate` answers only when released, `rereg` re-registers under a new id
    /// on the first invoke. Every invoke is logged as `object.method`.
    #[derive(Default)]
    struct Fake {
        log: StdMutex<Vec<String>>,
        connections: AtomicUsize,
        gate: Notify,
        rereg_moved: std::sync::atomic::AtomicBool,
    }

    const OBJECTS: [&str; 4] = ["fast", "slow", "gate", "rereg"];

    fn object_id(fake: &Fake, name: &str) -> Option<u32> {
        let index = OBJECTS.iter().position(|o| *o == name)? as u32;
        let moved = name == "rereg" && fake.rereg_moved.load(Ordering::SeqCst);
        Some(0x100 + index + if moved { 0x50 } else { 0 })
    }

    async fn serve(fake: Arc<Fake>, mut stream: UnixStream) {
        fake.connections.fetch_add(1, Ordering::SeqCst);
        if stream.write_all(&frame(HELLO, 0, 0x77, &[])).await.is_err() {
            return;
        }
        let (mut reader, writer) = stream.into_split();
        let writer = Arc::new(Mutex::new(writer));
        loop {
            let Ok(request) = read_frame(&mut reader).await else {
                return;
            };
            let attrs = parse_attrs(&request.body).unwrap();
            let fake = fake.clone();
            let writer = writer.clone();
            let reply = |status: u32, data: Option<Vec<u8>>, peer: u32, seq: u16| async move {
                let mut writer = writer.lock().await;
                if let Some(data) = data {
                    let _ = writer.write_all(&frame(DATA, seq, peer, &data)).await;
                }
                let mut body = Vec::new();
                put_attr(&mut body, ATTR_STATUS, false, &status.to_be_bytes());
                let _ = writer.write_all(&frame(STATUS, seq, peer, &body)).await;
            };
            match request.kind {
                LOOKUP => {
                    let path = message_attr(&attrs, ATTR_OBJPATH).unwrap().data;
                    let name = String::from_utf8_lossy(&path[..path.len() - 1]).into_owned();
                    match object_id(&fake, &name) {
                        Some(id) => {
                            let mut body = Vec::new();
                            put_string(&mut body, ATTR_OBJPATH, &name);
                            put_attr(&mut body, ATTR_OBJID, false, &id.to_be_bytes());
                            reply(0, Some(body), 0, request.seq).await;
                        }
                        None => reply(NOT_FOUND, None, 0, request.seq).await,
                    }
                }
                INVOKE => {
                    let id = request.peer;
                    let index = (id & 0xff) as usize % 0x50;
                    let name = OBJECTS.get(index.min(3)).copied().unwrap_or("?");
                    let method = message_attr(&attrs, ATTR_METHOD).unwrap().data;
                    let method = String::from_utf8_lossy(&method[..method.len() - 1]).into_owned();
                    let args = object_value(message_attr(&attrs, ATTR_DATA).unwrap().data).unwrap();
                    if name == "rereg" && !fake.rereg_moved.swap(true, Ordering::SeqCst) {
                        reply(NOT_FOUND, None, id, request.seq).await;
                        continue;
                    }
                    fake.log.lock().unwrap().push(format!("{name}.{method}"));
                    let mut data = Vec::new();
                    put_attr(
                        &mut data,
                        ATTR_DATA,
                        false,
                        &table(json!({"echo":args,"id":id}).as_object().unwrap()),
                    );
                    // Replies run in their own task so a held `gate` does not
                    // block the reader (like ubusd forwarding to services).
                    tokio::spawn(async move {
                        match name {
                            "slow" => tokio::time::sleep(Duration::from_millis(300)).await,
                            "gate" => fake.gate.notified().await,
                            _ => {}
                        }
                        reply(0, Some(data), id, request.seq).await;
                    });
                }
                _ => reply(1, None, request.peer, request.seq).await,
            }
        }
    }

    async fn fake_ubusd() -> (Arc<Fake>, String, tempfile_dir::Dir) {
        let dir = tempfile_dir::Dir::new();
        let path = dir.path.join("ubus.sock").to_string_lossy().into_owned();
        let listener = UnixListener::bind(&path).unwrap();
        let fake = Arc::new(Fake::default());
        let served = fake.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(served.clone(), stream));
            }
        });
        (fake, path, dir)
    }

    mod tempfile_dir {
        pub struct Dir {
            pub path: std::path::PathBuf,
        }
        impl Dir {
            pub fn new() -> Self {
                use std::sync::atomic::{AtomicU64, Ordering};
                static NEXT: AtomicU64 = AtomicU64::new(0);
                let path = std::env::temp_dir().join(format!(
                    "datad-ubus-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::SeqCst)
                ));
                std::fs::create_dir_all(&path).unwrap();
                Self { path }
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }
    }

    fn executor(path: &str) -> Arc<Executor> {
        Arc::new(Executor::new(
            Some(path.into()),
            Duration::from_millis(150),
            Duration::from_millis(150),
            Duration::from_secs(30),
            Duration::from_secs(30),
        ))
    }

    #[tokio::test]
    async fn lookup_invoke_cache_and_reregistration() {
        let (fake, path, _dir) = fake_ubusd().await;
        let exec = executor(&path);
        let reply = exec
            .call("fast", "get", &json!({"slotId":101}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply, json!({"echo":{"slotId":101},"id":0x100}));
        exec.call("fast", "get", &json!({})).await.unwrap().unwrap();
        // The object moved: one new lookup, one retry, the new id answers.
        let reply = exec
            .call("rereg", "get", &json!({}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply["id"], 0x153);
        assert_eq!(
            *fake.log.lock().unwrap(),
            ["fast.get", "fast.get", "rereg.get"]
        );
        assert_eq!(fake.connections.load(Ordering::SeqCst), 1);
        // Unknown objects read like the CLI.
        assert_eq!(
            exec.call("nope", "x", &json!({})).await.unwrap(),
            Err(status_text(NOT_FOUND, true))
        );
    }

    #[tokio::test]
    async fn timeout_drops_the_connection_and_late_replies_never_leak() {
        let (fake, path, _dir) = fake_ubusd().await;
        let exec = executor(&path);
        let error = exec
            .call("slow", "get", &json!({}))
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.ends_with("timed out"), "{error}");
        // The late "slow" reply arrives on the dead connection; the next call
        // uses a fresh one and gets its own answer.
        tokio::time::sleep(Duration::from_millis(250)).await;
        let reply = exec
            .call("fast", "get", &json!({"n":1}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply["echo"], json!({"n":1}));
        assert_eq!(fake.connections.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn collection_timeouts_cool_down_but_interactive_calls_still_go() {
        let (fake, path, _dir) = fake_ubusd().await;
        let exec = executor(&path);
        let first = collecting(exec.call("slow", "get", &json!({})))
            .await
            .unwrap();
        assert!(first.unwrap_err().ends_with("timed out"));
        let second = collecting(exec.call("slow", "get", &json!({})))
            .await
            .unwrap();
        assert!(second.unwrap_err().contains("skipped"));
        assert_eq!(
            fake.log.lock().unwrap().len(),
            1,
            "skipped call was not sent"
        );
        // Other objects and interactive callers are unaffected.
        assert!(
            collecting(exec.call("fast", "get", &json!({})))
                .await
                .unwrap()
                .is_ok()
        );
        assert!(exec.call("slow", "get", &json!({})).await.unwrap().is_err());
        assert_eq!(fake.log.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn interactive_calls_overtake_queued_collection() {
        let (fake, path, _dir) = fake_ubusd().await;
        let exec = Arc::new(Executor::new(
            Some(path),
            Duration::from_secs(5),
            Duration::from_secs(5),
            Duration::from_secs(30),
            Duration::from_secs(30),
        ));
        let spawn = |lane_collect: bool, object: &'static str, method: &'static str| {
            let exec = exec.clone();
            tokio::spawn(async move {
                let args = json!({});
                let call = exec.call(object, method, &args);
                if lane_collect {
                    collecting(call).await
                } else {
                    call.await
                }
            })
        };
        // Collection holds the connection; two more collection calls queue.
        let held = spawn(true, "gate", "hold");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let first = spawn(true, "fast", "collect1");
        let second = spawn(true, "fast", "collect2");
        tokio::time::sleep(Duration::from_millis(50)).await;
        // A control arrives last but runs next.
        let control = spawn(false, "fast", "control");
        tokio::time::sleep(Duration::from_millis(50)).await;
        fake.gate.notify_one();
        for task in [held, first, second, control] {
            assert!(task.await.unwrap().unwrap().is_ok());
        }
        let log = fake.log.lock().unwrap().clone();
        assert_eq!(log[0], "gate.hold");
        assert_eq!(log[1], "fast.control", "{log:?}");
        assert_eq!(log.len(), 4);
    }

    #[tokio::test]
    async fn interactive_queue_is_bounded() {
        let (fake, path, _dir) = fake_ubusd().await;
        let exec = Arc::new(Executor::new(
            Some(path),
            Duration::from_secs(5),
            Duration::from_secs(5),
            Duration::from_secs(30),
            Duration::from_secs(30),
        ));
        let held = tokio::spawn({
            let exec = exec.clone();
            async move { exec.call("gate", "hold", &json!({})).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut waiting = Vec::new();
        for index in 0..MAX_INTERACTIVE_WAITING {
            let exec = exec.clone();
            waiting.push(tokio::spawn(async move {
                exec.call("fast", "queued", &json!({"i":index})).await
            }));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            exec.call("fast", "overflow", &json!({})).await,
            Some(Err("ubus busy".into()))
        );
        fake.gate.notify_one();
        assert!(held.await.unwrap().unwrap().is_ok());
        for task in waiting {
            assert!(task.await.unwrap().unwrap().is_ok());
        }
        assert_eq!(exec.interactive_waiting.load(Ordering::SeqCst), 0);
        assert!(
            !fake
                .log
                .lock()
                .unwrap()
                .contains(&"fast.overflow".to_string())
        );
    }

    #[tokio::test]
    async fn a_cancelled_call_never_desynchronises_the_connection() {
        let (fake, path, _dir) = fake_ubusd().await;
        let exec = Arc::new(Executor::new(
            Some(path),
            Duration::from_secs(5),
            Duration::from_secs(5),
            Duration::from_secs(30),
            Duration::from_secs(30),
        ));
        assert!(exec.call("fast", "warm", &json!({})).await.unwrap().is_ok());
        // The caller gives up while the request is in flight.
        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                exec.call("gate", "hold", &json!({}))
            )
            .await
            .is_err()
        );
        fake.gate.notify_one();
        let reply = exec
            .call("fast", "after", &json!({"k":"v"}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply["echo"], json!({"k":"v"}));
        assert_eq!(fake.connections.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_missing_socket_falls_back_and_is_not_retried_immediately() {
        let dir = tempfile_dir::Dir::new();
        let path = dir.path.join("absent.sock").to_string_lossy().into_owned();
        let exec = executor(&path);
        assert_eq!(exec.call("fast", "get", &json!({})).await, None);
        assert!(exec.socket_down());
        assert_eq!(exec.call("fast", "get", &json!({})).await, None);
    }

    #[tokio::test]
    async fn missing_objects_are_not_looked_up_every_collection_round() {
        let (fake, path, _dir) = fake_ubusd().await;
        let exec = executor(&path);
        for _ in 0..3 {
            assert_eq!(
                collecting(exec.call("nope", "x", &json!({}))).await,
                Some(Err(status_text(NOT_FOUND, true)))
            );
        }
        assert!(exec.known_missing("nope"));
        // Interactive callers still ask ubusd, and present objects are unaffected.
        assert!(exec.call("nope", "x", &json!({})).await.unwrap().is_err());
        for _ in 0..3 {
            assert!(
                collecting(exec.call("fast", "get", &json!({})))
                    .await
                    .unwrap()
                    .is_ok()
            );
        }
        assert!(!exec.known_missing("fast"));
        assert_eq!(fake.log.lock().unwrap().len(), 3);
    }
}
