//! Read-only modem DIAG transport over QRTR for the U50 runtime.
//!
//! Verified on a U50 Pro (sdxlemur): the modem's DIAG CMD service is qrtr
//! service `0x1001`, instance `1` (node and port are reassigned at every boot
//! and must be discovered through the qrtr name service). A bare diag command
//! sent as one QRTR DATA packet is answered with the NHDLC wrapper
//! `7e 01 <len:le16> <payload> <7e>`. Only the version probe is used here; log
//! masks and streams are deliberately not touched, so the modem's diag state
//! is never modified. Every step is time-boxed and the state loop only pays
//! for a refresh once per minute.
use serde_json::{Map, Value, json};
use std::{
    io,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const AF_QIPCRTR: i32 = 42;
const SOCK_DGRAM: i32 = 2;
const QRTR_TYPE_HELLO: u32 = 2;
const QRTR_TYPE_NEW_SERVER: u32 = 4;
const QRTR_TYPE_NEW_LOOKUP: u32 = 10;
const QRTR_NS_NODE: u32 = 0xFFFF_FFFF;
const QRTR_NS_PORT: u32 = 0xFFFF_FFFE;
const DIAG_SERVICE: u32 = 0x1001;
const DIAG_INSTANCE_CMD: u32 = 1;
const DIAG_CMD_VER_F: u8 = 0x00;

/// One refresh per minute; the probe itself is bound well under a second.
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const RECV_TIMEOUT: Duration = Duration::from_millis(120);
const DISCOVERY_BUDGET: usize = 256;

/// Send/receive surface, injectable for fixture tests.
trait DiagIo {
    fn send_to(&mut self, buf: &[u8], node: u32, port: u32) -> io::Result<()>;
    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<(usize, u32, u32)>;
}

/// 20-byte v5.4 qrtr control packet: cmd, service, instance, node, port.
fn ctrl_packet(cmd: u32, service: u32, instance: u32, node: u32, port: u32) -> [u8; 20] {
    let mut out = [0u8; 20];
    out[0..4].copy_from_slice(&cmd.to_le_bytes());
    out[4..8].copy_from_slice(&service.to_le_bytes());
    out[8..12].copy_from_slice(&instance.to_le_bytes());
    out[12..16].copy_from_slice(&node.to_le_bytes());
    out[16..20].copy_from_slice(&port.to_le_bytes());
    out
}

fn ctrl_field(pkt: &[u8], at: usize) -> Option<u32> {
    let bytes: [u8; 4] = pkt.get(at..at + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

/// Strip the NHDLC wrapper the modem answers with. The length field counts
/// the payload only; the trailing 0x7e is not part of it.
fn unwrap_nhdlc(pkt: &[u8]) -> Option<&[u8]> {
    if pkt.len() < 5 || pkt[0] != 0x7e || pkt[1] != 0x01 {
        return None;
    }
    let len = u16::from_le_bytes([pkt[2], pkt[3]]) as usize;
    let end = 4usize.checked_add(len)?;
    if pkt.len() != end + 1 || pkt[end] != 0x7e {
        return None;
    }
    Some(&pkt[4..end])
}

/// Fields of the DIAG_VER_F reply: command echo, two 19-byte `date+time`
/// stamps (the first is reported) and the printable build name (e.g.
/// `Sep 15 2025 23:47:41` / `olympic.`).
fn parse_ver(payload: &[u8]) -> Option<(String, String)> {
    let payload = payload.strip_prefix(&[DIAG_CMD_VER_F])?;
    // Two fixed-width stamps, e.g. `Sep 15 2025` + `23:47:41` twice over.
    let stamp = std::str::from_utf8(payload.get(0..19)?)
        .ok()
        .map(str::trim)
        .filter(|v| v.len() == 19 && v.chars().all(|c| !c.is_ascii_control()))?;
    let second = std::str::from_utf8(payload.get(19..38)?).ok()?;
    if second != stamp {
        return None;
    }
    let stamp = format!("{} {}", &stamp[..11], &stamp[11..]);
    let rest = &payload[38..];
    let name_end = rest
        .iter()
        .position(|b| !b.is_ascii_graphic())
        .unwrap_or(rest.len());
    let name = std::str::from_utf8(&rest[..name_end])
        .ok()
        .map(str::trim)
        .filter(|v| !v.is_empty())?;
    Some((stamp, name.to_owned()))
}

struct Probed {
    fields: Map<String, Value>,
    at: Instant,
}

static CACHE: OnceLock<Mutex<Option<Probed>>> = OnceLock::new();

/// The `u50_diag` state block, refreshed at most once per minute.
pub fn block() -> Value {
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(probed) = guard.as_ref()
        && probed.at.elapsed() < REFRESH_INTERVAL
    {
        return Value::Object(probed.fields.clone());
    }
    let mut io = RealIo::new();
    let fields = probe(&mut io);
    *guard = Some(Probed {
        fields: fields.clone(),
        at: Instant::now(),
    });
    Value::Object(fields)
}

fn unavailable(reason: &str) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("available".into(), json!(false));
    out.insert("source".into(), json!("qrtr_diag_cmd"));
    out.insert("reason".into(), json!(reason));
    out
}

fn probe(io: &mut dyn DiagIo) -> Map<String, Value> {
    // Hello makes every node re-announce its services; the wildcard lookup
    // then brings the whole table back to this socket.
    if io
        .send_to(&ctrl_packet(QRTR_TYPE_HELLO, 0, 0, 0, 0)[..4], QRTR_NS_NODE, QRTR_NS_PORT)
        .is_err()
    {
        return unavailable("qrtr_hello_failed");
    }
    if io
        .send_to(
            &ctrl_packet(QRTR_TYPE_NEW_LOOKUP, 0, 0, 0, 0),
            QRTR_NS_NODE,
            QRTR_NS_PORT,
        )
        .is_err()
    {
        return unavailable("qrtr_lookup_failed");
    }
    let Some((node, port)) = discover_diag_service(io) else {
        return unavailable("diag_service_not_found");
    };
    let Some(reply) = ver_probe(io, node, port) else {
        return unavailable("ver_no_reply");
    };
    let mut out = unavailable("");
    out.insert("available".into(), json!(true));
    match parse_ver(&reply) {
        Some((date, name)) => {
            out.insert("node".into(), json!(node));
            out.insert("port".into(), json!(port));
            out.insert("modem_build_date".into(), json!(date));
            out.insert("modem_build_name".into(), json!(name));
        }
        // Reachability stands on its own; a surprising reply shape is not a
        // reason to hide the working transport.
        None => {
            out.insert("ver_reply_len".into(), json!(reply.len()));
        }
    }
    out
}

/// The modem's DIAG CMD service address from the name-service table. A read
/// timeout after the announcement burst means the table ended without a match.
fn discover_diag_service(io: &mut dyn DiagIo) -> Option<(u32, u32)> {
    let mut buf = [0u8; 512];
    for _ in 0..DISCOVERY_BUDGET {
        let (n, _, _) = io.recv_from(&mut buf).ok()?;
        if n >= 20
            && ctrl_field(&buf, 0) == Some(QRTR_TYPE_NEW_SERVER)
            && ctrl_field(&buf, 4) == Some(DIAG_SERVICE)
            && ctrl_field(&buf, 8) == Some(DIAG_INSTANCE_CMD)
            && let (Some(node), Some(port)) = (ctrl_field(&buf, 12), ctrl_field(&buf, 16))
            // The name service emits a node 0 / port 0 placeholder before
            // the real entries; skip it.
            && node != 0
            && port != 0
        {
            return Some((node, port));
        }
    }
    None
}

fn ver_probe(io: &mut dyn DiagIo, node: u32, port: u32) -> Option<Vec<u8>> {
    io.send_to(&[DIAG_CMD_VER_F], node, port).ok()?;
    let mut buf = [0u8; 4096];
    // The name-service table the discovery burst brought in is usually still
    // queued ahead of the reply; drain it instead of failing on it. The
    // receive timeout ends the loop once the queue runs dry.
    for _ in 0..512 {
        match io.recv_from(&mut buf) {
            Ok((n, from_node, from_port)) if from_node == node && from_port == port => {
                return unwrap_nhdlc(&buf[..n]).map(<[u8]>::to_vec);
            }
            Err(_) => return None,
            Ok(_) => continue,
        }
    }
    None
}

#[repr(C)]
struct SockaddrQrtr {
    family: u16,
    pad: u16,
    node: u32,
    port: u32,
}

struct RealIo {
    fd: i32,
}

impl RealIo {
    fn new() -> Self {
        // Safety: socket(2) with constant arguments.
        let fd = unsafe { libc::socket(AF_QIPCRTR, SOCK_DGRAM, 0) };
        if fd >= 0 {
            let tv = libc::timeval {
                tv_sec: RECV_TIMEOUT.as_secs() as libc::time_t,
                tv_usec: (RECV_TIMEOUT.subsec_micros()) as libc::suseconds_t,
            };
            // Safety: valid fd and timeval for the duration of the call.
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_RCVTIMEO,
                    (&tv as *const libc::timeval).cast(),
                    std::mem::size_of::<libc::timeval>() as u32,
                );
            }
        }
        Self { fd }
    }
}

impl Drop for RealIo {
    fn drop(&mut self) {
        if self.fd >= 0 {
            // Safety: closing a descriptor we own and never share.
            unsafe { libc::close(self.fd) };
        }
    }
}

impl DiagIo for RealIo {
    fn send_to(&mut self, buf: &[u8], node: u32, port: u32) -> io::Result<()> {
        let addr = SockaddrQrtr {
            family: AF_QIPCRTR as u16,
            pad: 0,
            node,
            port,
        };
        // Safety: valid fd and sockaddr for the duration of the call.
        let n = unsafe {
            libc::sendto(
                self.fd,
                buf.as_ptr().cast(),
                buf.len(),
                0,
                (&addr as *const SockaddrQrtr).cast(),
                std::mem::size_of::<SockaddrQrtr>() as u32,
            )
        };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<(usize, u32, u32)> {
        let mut addr = SockaddrQrtr {
            family: 0,
            pad: 0,
            node: 0,
            port: 0,
        };
        let mut len = std::mem::size_of::<SockaddrQrtr>() as u32;
        // Safety: valid fd, sockaddr and length for the duration of the call.
        let n = unsafe {
            libc::recvfrom(
                self.fd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                0,
                (&mut addr as *mut SockaddrQrtr).cast(),
                &mut len,
            )
        };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok((n as usize, addr.node, addr.port))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One scripted receive: payload plus sender, or the error to raise.
    type Scripted = Result<(Vec<u8>, u32, u32), io::Error>;

    /// Hands out scripted replies in order; an exhausted script times out.
    struct FakeIo {
        sent: Vec<(Vec<u8>, u32, u32)>,
        replies: Vec<Scripted>,
    }

    impl FakeIo {
        fn timeout() -> io::Error {
            io::Error::new(io::ErrorKind::WouldBlock, "timeout")
        }
    }

    impl DiagIo for FakeIo {
        fn send_to(&mut self, buf: &[u8], node: u32, port: u32) -> io::Result<()> {
            self.sent.push((buf.to_vec(), node, port));
            Ok(())
        }
        fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<(usize, u32, u32)> {
            if self.replies.is_empty() {
                return Err(Self::timeout());
            }
            match self.replies.remove(0) {
                Ok((data, node, port)) => {
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    Ok((n, node, port))
                }
                Err(e) => Err(e),
            }
        }
    }

    fn server_entry(service: u32, instance: u32, node: u32, port: u32) -> Vec<u8> {
        ctrl_packet(QRTR_TYPE_NEW_SERVER, service, instance, node, port).to_vec()
    }

    #[test]
    fn ctrl_packet_is_v54_layout() {
        let pkt = ctrl_packet(QRTR_TYPE_NEW_LOOKUP, 0x1001, 1, 3, 17);
        assert_eq!(&pkt[0..4], &10u32.to_le_bytes());
        assert_eq!(&pkt[4..8], &0x1001u32.to_le_bytes());
        assert_eq!(&pkt[8..12], &1u32.to_le_bytes());
        assert_eq!(&pkt[12..16], &3u32.to_le_bytes());
        assert_eq!(&pkt[16..20], &17u32.to_le_bytes());
    }

    #[test]
    fn nhdlc_unwrap_is_strict() {
        let frame = [0x7e, 0x01, 0x02, 0x00, 0xaa, 0xbb, 0x7e];
        assert_eq!(unwrap_nhdlc(&frame), Some(&[0xaa, 0xbb][..]));
        assert_eq!(unwrap_nhdlc(&[0x7e, 0x02, 0x02, 0x00, 0xaa, 0xbb, 0x7e]), None);
        assert_eq!(unwrap_nhdlc(&[0x7e, 0x01, 0x03, 0x00, 0xaa, 0xbb, 0x7e]), None);
        assert_eq!(unwrap_nhdlc(&[0x7e, 0x01, 0x02, 0x00, 0xaa, 0xbb, 0x00]), None);
        assert_eq!(unwrap_nhdlc(&[0x7e, 0x01]), None);
    }

    #[test]
    fn ver_fields_match_the_device_reply() {
        let mut payload = vec![0x00];
        payload.extend_from_slice(b"Sep 15 202523:47:41");
        payload.extend_from_slice(b"Sep 15 202523:47:41");
        payload.extend_from_slice(b"olympic.");
        payload.extend_from_slice(&[0x00, 0x00, 0xff, 0x64, 0x00, 0x00, 0x01, 0x99]);
        let (date, name) = parse_ver(&payload).expect("parse");
        assert_eq!(date, "Sep 15 2025 23:47:41");
        assert_eq!(name, "olympic.");
        assert!(parse_ver(&payload[1..]).is_none());
        assert!(parse_ver(&[0x00]).is_none());
    }

    #[test]
    fn discovery_picks_the_diag_cmd_service_and_skips_placeholders() {
        let mut ver = vec![0x7e, 0x01, 55, 0x00, 0x00];
        ver.extend_from_slice(b"Sep 15 202523:47:41");
        ver.extend_from_slice(b"Sep 15 202523:47:41");
        ver.extend_from_slice(b"olympic.");
        ver.extend_from_slice(&[0x00, 0x00, 0xff, 0x64, 0x00, 0x00, 0x01, 0x99]);
        ver.push(0x7e);
        let mut io = FakeIo {
            sent: Vec::new(),
            replies: vec![
                Ok((server_entry(DIAG_SERVICE, DIAG_INSTANCE_CMD, 0, 0), 2, 16406)),
                Ok((server_entry(0x1000, 1, 2, 16391), 2, 16406)),
                Ok((server_entry(DIAG_SERVICE, 3, 3, 21), 2, 16406)),
                Ok((server_entry(DIAG_SERVICE, DIAG_INSTANCE_CMD, 3, 17), 2, 16406)),
                Ok((ver, 3, 17)),
                Err(FakeIo::timeout()),
            ],
        };
        let fields = probe(&mut io);
        assert_eq!(fields["available"], json!(true));
        assert_eq!(fields["node"], json!(3));
        assert_eq!(fields["port"], json!(17));
        assert_eq!(fields["reason"], json!(""));
        assert_eq!(fields["modem_build_date"], json!("Sep 15 2025 23:47:41"));
        assert_eq!(fields["modem_build_name"], json!("olympic."));
        // Hello goes out as the 4-byte command, the lookup as the full packet,
        // the version probe as a bare one-byte command.
        assert_eq!(io.sent[0].0, vec![2u8, 0, 0, 0]);
        assert_eq!(io.sent[1].0.len(), 20);
        assert_eq!(io.sent[1].1, 0xFFFF_FFFF);
        assert_eq!(io.sent[2], (vec![0x00], 3, 17));
    }

    #[test]
    fn unexpected_ver_shape_still_reports_reachability() {
        let frame = vec![0x7e, 0x01, 0x02, 0x00, 0x00, 0x42, 0x7e];
        let mut io = FakeIo {
            sent: Vec::new(),
            replies: vec![
                Ok((server_entry(DIAG_SERVICE, DIAG_INSTANCE_CMD, 3, 17), 2, 16406)),
                Ok((frame, 3, 17)),
                Err(FakeIo::timeout()),
            ],
        };
        let fields = probe(&mut io);
        assert_eq!(fields["available"], json!(true));
        assert_eq!(fields["ver_reply_len"], json!(2));
        assert!(!fields.contains_key("modem_build_name"));
    }

    #[test]
    fn missing_service_reports_reason() {
        let mut io = FakeIo {
            sent: Vec::new(),
            replies: vec![Err(FakeIo::timeout())],
        };
        let fields = probe(&mut io);
        assert_eq!(fields["available"], json!(false));
        assert_eq!(fields["reason"], json!("diag_service_not_found"));
    }
}
