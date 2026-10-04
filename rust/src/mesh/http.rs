//! A small HTTP/1.1 client for the device web port, and the request/response
//! mapping of docs/P2P.md ("HTTP over the channel").
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};

/// Body frames carry at most this much payload.
pub const PIECE: usize = 64 * 1024;
const MAX_HEAD: usize = 64 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_PATH: usize = 8192;
const MAX_VALUE: usize = 8192;
/// Request bodies larger than this are refused up front.
pub const MAX_BODY: u64 = 32 * 1024 * 1024;

#[derive(Debug, PartialEq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body_len: u64,
}

fn token(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Validates the header of an `http` request frame. The error is the code sent back.
pub fn parse_request(header: &Value, session_port: u16) -> Result<Request, &'static str> {
    if let Some(port) = header.get("port")
        && port.as_u64() != Some(u64::from(session_port))
    {
        return Err("target_not_allowed");
    }
    let method = header
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !matches!(
        method,
        "GET" | "HEAD" | "POST" | "PUT" | "DELETE" | "PATCH" | "OPTIONS"
    ) {
        return Err("method_not_allowed");
    }
    let path = header
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // Origin-form only: an absolute URI or authority would name another target.
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.len() > MAX_PATH
        || path.bytes().any(|b| b <= b' ' || b == 0x7f)
    {
        return Err(if path.contains("://") || path.starts_with("//") {
            "target_not_allowed"
        } else {
            "bad_request"
        });
    }
    let body_len = match header.get("body_len") {
        None => 0,
        Some(value) => value.as_u64().ok_or("bad_request")?,
    };
    if body_len > MAX_BODY {
        return Err("body_too_large");
    }
    let list = header
        .get("headers")
        .and_then(Value::as_array)
        .ok_or("bad_request")?;
    if list.len() > MAX_HEADERS {
        return Err("bad_request");
    }
    let mut headers = Vec::with_capacity(list.len());
    for pair in list {
        let pair = pair
            .as_array()
            .filter(|p| p.len() == 2)
            .ok_or("bad_request")?;
        let (Some(name), Some(value)) = (pair[0].as_str(), pair[1].as_str()) else {
            return Err("bad_request");
        };
        if !token(name, 64)
            || value.len() > MAX_VALUE
            || value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
        {
            return Err("bad_request");
        }
        headers.push((name.to_ascii_lowercase(), value.to_owned()));
    }
    Ok(Request {
        method: method.to_owned(),
        path: path.to_owned(),
        headers,
        body_len,
    })
}

/// Headers the device sets itself or that only make sense per connection.
fn skipped(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "connection"
            | "keep-alive"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
            | "expect"
            | "proxy-authorization"
            | "proxy-connection"
    )
}

/// The request line and headers as sent to `127.0.0.1:<port>`: `Host` rewritten,
/// `origin`/`referer` rebuilt as `http://<Host>…` so the device's own CSRF
/// checks see a coherent same-device origin, one request per connection.
pub fn head_bytes(request: &Request, port: u16) -> Vec<u8> {
    let host = format!("127.0.0.1:{port}");
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {host}\r\n",
        request.method, request.path
    );
    for (name, value) in &request.headers {
        if skipped(name) {
            continue;
        }
        let value = match name.as_str() {
            // A bare "/" means "the page sent an Origin".
            "origin" => format!("http://{host}"),
            "referer" if value.starts_with('/') => format!("http://{host}{value}"),
            "referer" => continue,
            _ => value.clone(),
        };
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if request.body_len > 0 || matches!(request.method.as_str(), "POST" | "PUT" | "PATCH") {
        head.push_str(&format!("Content-Length: {}\r\n", request.body_len));
    }
    head.push_str("Connection: close\r\n\r\n");
    head.into_bytes()
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum BodyKind {
    None,
    Length(u64),
    Chunked,
    Close,
}

#[derive(Debug)]
pub struct ResponseHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: BodyKind,
}

fn hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "transfer-encoding"
            | "te"
            | "trailer"
            | "upgrade"
            | "proxy-authenticate"
            | "proxy-connection"
    )
}

/// Reads one response head (skipping `1xx` interim responses). `head_only` is
/// true for `HEAD`, which never has a body.
pub async fn read_head<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    head_only: bool,
) -> Result<ResponseHead, &'static str> {
    loop {
        let mut block = Vec::new();
        loop {
            let mut line = Vec::new();
            let read = reader
                .read_until(b'\n', &mut line)
                .await
                .map_err(|_| "upstream_unreachable")?;
            if read == 0 {
                return Err("upstream_closed");
            }
            block.extend_from_slice(&line);
            if block.len() > MAX_HEAD {
                return Err("bad_response");
            }
            if line == b"\r\n" || line == b"\n" {
                break;
            }
        }
        let text = String::from_utf8_lossy(&block);
        let mut lines = text.lines();
        let status_line = lines.next().ok_or("bad_response")?;
        let mut parts = status_line.splitn(3, ' ');
        let version = parts.next().unwrap_or_default();
        let status: u16 = parts
            .next()
            .and_then(|s| s.trim().parse().ok())
            .ok_or("bad_response")?;
        if !version.starts_with("HTTP/1.") || !(100..=599).contains(&status) {
            return Err("bad_response");
        }
        if status == 101 {
            return Err("upgrade_unsupported");
        }
        if (100..200).contains(&status) {
            continue;
        }
        let mut headers = Vec::new();
        let mut content_length = None;
        let mut chunked = false;
        for line in lines {
            if line.is_empty() {
                break;
            }
            let (name, value) = line.split_once(':').ok_or("bad_response")?;
            let (name, value) = (name.trim().to_ascii_lowercase(), value.trim().to_owned());
            match name.as_str() {
                "content-length" => {
                    let parsed: u64 = value.parse().map_err(|_| "bad_response")?;
                    if content_length
                        .replace(parsed)
                        .is_some_and(|before| before != parsed)
                    {
                        return Err("bad_response");
                    }
                }
                "transfer-encoding" => {
                    chunked = value
                        .to_ascii_lowercase()
                        .split(',')
                        .any(|v| v.trim() == "chunked");
                }
                _ => {}
            }
            if !hop_by_hop(&name) {
                headers.push((name, value));
            }
        }
        let body = if head_only || matches!(status, 204 | 304) {
            BodyKind::None
        } else if chunked {
            BodyKind::Chunked
        } else if let Some(length) = content_length {
            if length == 0 {
                BodyKind::None
            } else {
                BodyKind::Length(length)
            }
        } else {
            BodyKind::Close
        };
        return Ok(ResponseHead {
            status,
            headers,
            body,
        });
    }
}

/// Decodes a response body into pieces of at most `PIECE` bytes.
pub struct Body<R> {
    reader: BufReader<R>,
    kind: BodyKind,
    remaining: u64,
    /// Chunked only: a chunk is in progress (or the terminating chunk was seen).
    in_chunk: bool,
    finished: bool,
}

impl<R: AsyncRead + Unpin> Body<R> {
    pub fn new(reader: BufReader<R>, kind: BodyKind) -> Self {
        let remaining = if let BodyKind::Length(n) = kind { n } else { 0 };
        Self {
            reader,
            kind,
            remaining,
            in_chunk: false,
            finished: matches!(kind, BodyKind::None),
        }
    }

    /// True once the whole body is known to have been delivered.
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// The next piece, or `None` at the end of the body.
    pub async fn next(&mut self) -> Result<Option<Vec<u8>>, &'static str> {
        if self.finished {
            return Ok(None);
        }
        match self.kind {
            BodyKind::None => Ok(None),
            BodyKind::Length(_) => {
                let piece = self.read_piece(self.remaining).await?;
                self.remaining -= piece.len() as u64;
                self.finished = self.remaining == 0;
                Ok(Some(piece))
            }
            BodyKind::Close => {
                let mut buffer = vec![0u8; PIECE];
                let read = self
                    .reader
                    .read(&mut buffer)
                    .await
                    .map_err(|_| "upstream_closed")?;
                if read == 0 {
                    self.finished = true;
                    return Ok(None);
                }
                buffer.truncate(read);
                Ok(Some(buffer))
            }
            BodyKind::Chunked => {
                if !self.in_chunk {
                    let mut line = String::new();
                    self.reader
                        .read_line(&mut line)
                        .await
                        .map_err(|_| "upstream_closed")?;
                    let size = line.split(';').next().unwrap_or_default().trim();
                    let size = u64::from_str_radix(size, 16).map_err(|_| "bad_response")?;
                    if size == 0 {
                        // Trailers, until the blank line.
                        loop {
                            let mut trailer = String::new();
                            let read = self
                                .reader
                                .read_line(&mut trailer)
                                .await
                                .map_err(|_| "upstream_closed")?;
                            if read == 0 || trailer == "\r\n" || trailer == "\n" {
                                break;
                            }
                        }
                        self.finished = true;
                        return Ok(None);
                    }
                    self.remaining = size;
                    self.in_chunk = true;
                }
                let piece = self.read_piece(self.remaining).await?;
                self.remaining -= piece.len() as u64;
                if self.remaining == 0 {
                    let mut crlf = String::new();
                    self.reader
                        .read_line(&mut crlf)
                        .await
                        .map_err(|_| "upstream_closed")?;
                    self.in_chunk = false;
                }
                Ok(Some(piece))
            }
        }
    }

    async fn read_piece(&mut self, remaining: u64) -> Result<Vec<u8>, &'static str> {
        let want = usize::try_from(remaining).unwrap_or(PIECE).min(PIECE);
        let mut buffer = vec![0u8; want];
        let read = self
            .reader
            .read(&mut buffer)
            .await
            .map_err(|_| "upstream_closed")?;
        if read == 0 {
            return Err("upstream_closed");
        }
        buffer.truncate(read);
        Ok(buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    fn frame(extra: Value) -> Value {
        let mut base = json!({"t":"http","id":1,"method":"GET","path":"/a?b=1","headers":[["accept","*/*"]],"body_len":0});
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        base
    }

    #[test]
    fn requests_are_validated_before_anything_is_sent() {
        let request = parse_request(&frame(json!({})), 80).unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/a?b=1");
        for (extra, error) in [
            (json!({"method":"CONNECT"}), "method_not_allowed"),
            (json!({"method":"get"}), "method_not_allowed"),
            (json!({"path":"http://evil.example/"}), "target_not_allowed"),
            (json!({"path":"//evil.example/x"}), "target_not_allowed"),
            (json!({"path":"nope"}), "bad_request"),
            (json!({"path":"/a b"}), "bad_request"),
            (json!({"path":"/a\r\nHost: x"}), "bad_request"),
            (json!({"port":8080}), "target_not_allowed"),
            (json!({"body_len":MAX_BODY + 1}), "body_too_large"),
            (json!({"body_len":"5"}), "bad_request"),
            (json!({"headers":"x"}), "bad_request"),
            (json!({"headers":[["a"]]}), "bad_request"),
            (json!({"headers":[["bad name","v"]]}), "bad_request"),
            (json!({"headers":[["x","a\r\nb: c"]]}), "bad_request"),
        ] {
            assert_eq!(
                parse_request(&frame(extra.clone()), 80).unwrap_err(),
                error,
                "{extra}"
            );
        }
        let many: Vec<Value> = (0..65).map(|n| json!([format!("h{n}"), "v"])).collect();
        assert_eq!(
            parse_request(&frame(json!({"headers":many})), 80).unwrap_err(),
            "bad_request"
        );
        assert!(parse_request(&frame(json!({"port":80})), 80).is_ok());
    }

    #[test]
    fn the_head_is_rewritten_for_the_device() {
        let request = parse_request(&frame(json!({
            "method":"POST","path":"/api/x?y=1","body_len":5,
            "headers":[["Host","evil"],["origin","/"],["referer","/page?q=1"],["cookie","a=b"],
                ["connection","keep-alive"],["content-length","999"],["transfer-encoding","chunked"],
                ["x-custom","1"],["expect","100-continue"]]
        })), 8080).unwrap();
        let head = String::from_utf8(head_bytes(&request, 8080)).unwrap();
        assert!(head.starts_with("POST /api/x?y=1 HTTP/1.1\r\nHost: 127.0.0.1:8080\r\n"));
        assert!(head.contains("origin: http://127.0.0.1:8080\r\n"));
        assert!(head.contains("referer: http://127.0.0.1:8080/page?q=1\r\n"));
        assert!(head.contains("cookie: a=b\r\n") && head.contains("x-custom: 1\r\n"));
        assert!(head.contains("Content-Length: 5\r\n") && !head.contains("999"));
        assert!(
            !head.contains("evil")
                && !head.contains("keep-alive")
                && !head.contains("chunked")
                && !head.contains("100-continue")
        );
        assert!(head.ends_with("Connection: close\r\n\r\n"));
        // A referer that is not path-form is dropped; GET without a body has no length.
        let request = parse_request(
            &frame(json!({"headers":[["referer","https://nms.example/x"]]})),
            80,
        )
        .unwrap();
        let head = String::from_utf8(head_bytes(&request, 80)).unwrap();
        assert!(!head.contains("referer") && !head.contains("Content-Length"));
    }

    async fn read(wire: &[u8], head_only: bool) -> (Result<ResponseHead, &'static str>, Vec<u8>) {
        let (mut client, server) = tokio::io::duplex(1 << 20);
        client.write_all(wire).await.unwrap();
        drop(client);
        let mut reader = BufReader::new(server);
        let head = read_head(&mut reader, head_only).await;
        let mut body = Vec::new();
        if let Ok(head) = &head {
            let mut decoder = Body::new(reader, head.body);
            while let Ok(Some(piece)) = decoder.next().await {
                body.extend(piece);
            }
        }
        (head, body)
    }

    #[tokio::test]
    async fn responses_decode_by_content_length_chunking_and_close() {
        let (head, body) = read(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhelloEXTRA", false).await;
        let head = head.unwrap();
        assert_eq!((head.status, head.body), (200, BodyKind::Length(5)));
        assert_eq!(body, b"hello");
        assert_eq!(
            head.headers
                .iter()
                .filter(|(n, _)| n == "set-cookie")
                .count(),
            2
        );
        assert!(head.headers.iter().all(|(n, _)| n != "connection"));

        let (head, body) = read(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n4;ext=1\r\ndefg\r\n0\r\nX-T: 1\r\n\r\n", false).await;
        assert_eq!(head.unwrap().body, BodyKind::Chunked);
        assert_eq!(body, b"abcdefg");

        let (head, body) = read(b"HTTP/1.0 200 OK\r\n\r\nuntil the end", false).await;
        assert_eq!(head.unwrap().body, BodyKind::Close);
        assert_eq!(body, b"until the end");
    }

    #[tokio::test]
    async fn bodyless_statuses_interim_responses_and_head_are_handled() {
        for wire in [
            &b"HTTP/1.1 204 No Content\r\n\r\n"[..],
            b"HTTP/1.1 304 Not Modified\r\nContent-Length: 9\r\n\r\n",
            b"HTTP/1.1 302 Found\r\nLocation: /x\r\nContent-Length: 0\r\n\r\n",
        ] {
            assert_eq!(read(wire, false).await.0.unwrap().body, BodyKind::None);
        }
        assert_eq!(
            read(b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\n", true)
                .await
                .0
                .unwrap()
                .body,
            BodyKind::None
        );
        let (head, body) = read(
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\nok",
            false,
        )
        .await;
        assert_eq!(head.unwrap().status, 201);
        assert_eq!(body, b"ok");
    }

    #[tokio::test]
    async fn broken_responses_are_refused() {
        for (wire, error) in [
            (&b"garbage\r\n\r\n"[..], "bad_response"),
            (&b"HTTP/1.1 999 X\r\n\r\n"[..], "bad_response"),
            (
                &b"HTTP/1.1 200 OK\r\nContent-Length: x\r\n\r\n"[..],
                "bad_response",
            ),
            (
                &b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n"[..],
                "bad_response",
            ),
            (
                &b"HTTP/1.1 101 Switching Protocols\r\n\r\n"[..],
                "upgrade_unsupported",
            ),
            (&b""[..], "upstream_closed"),
            (
                &b"HTTP/1.1 200 OK\r\nbad header line\r\n\r\n"[..],
                "bad_response",
            ),
        ] {
            assert_eq!(read(wire, false).await.0.unwrap_err(), error);
        }
        // A short body is an error, not a silent truncation.
        let (_, body) = read(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc", false).await;
        assert_eq!(body, b"abc");
        let (mut client, server) = tokio::io::duplex(1024);
        client
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc")
            .await
            .unwrap();
        drop(client);
        let mut reader = BufReader::new(server);
        let head = read_head(&mut reader, false).await.unwrap();
        let mut decoder = Body::new(reader, head.body);
        assert!(decoder.next().await.unwrap().is_some());
        assert_eq!(decoder.next().await.unwrap_err(), "upstream_closed");
        assert!(!decoder.finished());
    }

    #[tokio::test]
    async fn large_bodies_arrive_in_bounded_pieces() {
        let payload = vec![7u8; PIECE * 2 + 5];
        let wire = [
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                payload.len()
            )
            .into_bytes(),
            payload.clone(),
        ]
        .concat();
        let (mut client, server) = tokio::io::duplex(1 << 20);
        client.write_all(&wire).await.unwrap();
        drop(client);
        let mut reader = BufReader::new(server);
        let head = read_head(&mut reader, false).await.unwrap();
        let mut decoder = Body::new(reader, head.body);
        let mut total = 0;
        while let Some(piece) = decoder.next().await.unwrap() {
            assert!(piece.len() <= PIECE);
            total += piece.len();
        }
        assert_eq!(total, payload.len());
        assert!(decoder.finished());
    }
}
