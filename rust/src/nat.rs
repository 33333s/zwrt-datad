//! IPv4 NAT type detection over STUN: RFC 5780 behaviour discovery, reported
//! with the classic RFC 3489 names (full cone ... symmetric, NAT1-NAT4).
//!
//! The probe runs on the device itself over the default route. The device's
//! own INPUT firewall drops the STUN replies that come from a changed address,
//! so a plain socket only sees the device's combined result. Each probe socket
//! therefore also has a packet capture (AF_PACKET, before netfilter) that sees
//! what the carrier NAT let through: the main result is the carrier's NAT, and
//! `device` is what remains after the device firewall.
use crate::stun;
use serde_json::{Value, json};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::LazyLock,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{net::UdpSocket, sync::Mutex};

/// Servers that answer from mainland China come first. The first four support
/// RFC 5780 (OTHER-ADDRESS and CHANGE-REQUEST); the last two only report the
/// mapped address and serve as a second endpoint for the mapping comparison.
pub const DEFAULT_SERVERS: [&str; 6] = [
    "stun:stun.miwifi.com:3478",
    "stun:stun.voipgate.com:3478",
    "stun:stun.fitauto.ru:3478",
    "stun:stun.nextcloud.com:443",
    "stun:stun.cloudflare.com:3478",
    "stun:stun.chat.bilibili.com:3478",
];
pub const MAX_SERVERS: usize = 6;
/// Upper bound for one complete detection, DNS included.
const BUDGET: Duration = Duration::from_secs(20);
/// Waits between retransmissions of one request.
#[cfg(not(test))]
const WAITS: [Duration; 2] = [Duration::from_millis(600), Duration::from_millis(1000)];
#[cfg(test)]
const WAITS: [Duration; 2] = [Duration::from_millis(150), Duration::from_millis(150)];
/// How many extra RFC 5780 servers may confirm the port-only filtering test:
/// some servers never answer CHANGE-PORT alone.
const PORT_FILTER_SERVERS: usize = 2;

/// After a WAN IPv4 change, how long the addresses must hold before the
/// automatic detection runs; another change restarts the wait.
const SETTLE: Duration = Duration::from_secs(30);
/// Waits before each retry of a failed automatic detection while the
/// addresses stay the same; after the last one the failure stands.
const RETRIES: [Duration; 3] = [
    Duration::from_secs(60),
    Duration::from_secs(300),
    Duration::from_secs(900),
];
/// The classic names NMS accepts in telemetry.
const TYPES: [&str; 9] = [
    "full_cone",
    "restricted_cone",
    "port_restricted_cone",
    "symmetric",
    "open",
    "firewalled",
    "no_nat",
    "udp_blocked",
    "unknown",
];

static LAST: LazyLock<Mutex<Option<Value>>> = LazyLock::new(|| Mutex::new(None));
static RUNNING: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
/// The WAN IPv4 addresses the watcher last saw; each detection records them.
static WAN: LazyLock<std::sync::Mutex<Vec<String>>> = LazyLock::new(Default::default);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Behaviour {
    EndpointIndependent,
    AddressDependent,
    AddressAndPortDependent,
    /// Not endpoint independent; the server could not tell which kind.
    Dependent,
    Unknown,
}

impl Behaviour {
    fn name(self) -> &'static str {
        match self {
            Self::EndpointIndependent => "endpoint_independent",
            Self::AddressDependent => "address_dependent",
            Self::AddressAndPortDependent => "address_and_port_dependent",
            Self::Dependent => "dependent",
            Self::Unknown => "unknown",
        }
    }
}

/// Classic type name and NAT level (1 = full cone ... 4 = symmetric).
fn classify(nat: bool, mapping: Behaviour, filtering: Behaviour) -> (&'static str, Option<u8>) {
    use Behaviour::*;
    if !nat {
        return match filtering {
            EndpointIndependent => ("open", None),
            Unknown => ("no_nat", None),
            _ => ("firewalled", None),
        };
    }
    match (mapping, filtering) {
        (AddressDependent | AddressAndPortDependent | Dependent, _) => ("symmetric", Some(4)),
        (EndpointIndependent, EndpointIndependent) => ("full_cone", Some(1)),
        (EndpointIndependent, AddressDependent) => ("restricted_cone", Some(2)),
        (EndpointIndependent, AddressAndPortDependent) => ("port_restricted_cone", Some(3)),
        _ => ("unknown", None),
    }
}

/// Private and carrier-grade (100.64.0.0/10) addresses: a NAT in front of
/// such a WAN address belongs to the carrier.
fn carrier_range(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    ip.is_private() || (a == 100 && (64..128).contains(&b))
}

fn public(address: SocketAddr) -> bool {
    match address.ip() {
        IpAddr::V4(ip) => {
            !(carrier_range(ip)
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_multicast())
        }
        IpAddr::V6(_) => false,
    }
}

/// After a captured reply, how long the socket may still receive the same
/// datagram; a reply the firewall passes arrives within microseconds.
const GRACE: Duration = Duration::from_millis(100);

#[derive(Clone)]
struct Reply {
    mapped: SocketAddr,
    other: Option<SocketAddr>,
    source: SocketAddr,
}

/// Source address and payload of an IPv4/UDP packet (network header first),
/// as an AF_PACKET datagram socket delivers it. Fragments are ignored.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn udp_payload(packet: &[u8]) -> Option<(SocketAddr, &[u8])> {
    let header = usize::from(packet.first()? & 0x0f) * 4;
    if packet[0] >> 4 != 4 || header < 20 || packet.len() < header + 8 || packet[9] != 17 {
        return None;
    }
    if u16::from_be_bytes([packet[6], packet[7]]) & 0x3fff != 0 {
        return None;
    }
    let total = usize::from(u16::from_be_bytes([packet[2], packet[3]])).min(packet.len());
    let source = Ipv4Addr::new(packet[12], packet[13], packet[14], packet[15]);
    let port = u16::from_be_bytes([packet[header], packet[header + 1]]);
    let length = usize::from(u16::from_be_bytes([packet[header + 4], packet[header + 5]]));
    let end = (header + length).min(total);
    let payload = packet.get(header + 8..end)?;
    Some((SocketAddr::new(IpAddr::V4(source), port), payload))
}

#[cfg(target_os = "linux")]
mod capture {
    use super::{Reply, udp_payload};
    use crate::stun;
    use std::{
        mem::size_of,
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
    };
    use tokio::io::unix::AsyncFd;

    /// Classic BPF for an AF_PACKET datagram socket (offsets start at the IPv4
    /// header): accept unfragmented UDP to `port`, drop everything else.
    pub(super) fn program(port: u16) -> Vec<libc::sock_filter> {
        let op = |code: u16, jt: u8, jf: u8, k: u32| libc::sock_filter { code, jt, jf, k };
        vec![
            op(0x30, 0, 0, 9),               // ldb [9]: protocol
            op(0x15, 0, 6, 17),              // jeq UDP, else drop
            op(0x28, 0, 0, 6),               // ldh [6]: flags + fragment offset
            op(0x45, 4, 0, 0x1fff),          // jset fragment offset: drop
            op(0xb1, 0, 0, 0),               // ldxb 4*([0]&0xf): header length
            op(0x48, 0, 0, 2),               // ldh [x+2]: UDP destination port
            op(0x15, 0, 1, u32::from(port)), // jeq port, else drop
            op(0x06, 0, 0, 0x0004_0000),     // accept
            op(0x06, 0, 0, 0),               // drop
        ]
    }

    /// Copies of the UDP datagrams addressed to one local port, taken before
    /// netfilter, so replies the device's INPUT firewall drops are seen too.
    pub(super) struct Capture {
        fd: AsyncFd<OwnedFd>,
    }

    impl Capture {
        /// None without CAP_NET_RAW or AF_PACKET support.
        pub(super) fn open(port: u16) -> Option<Self> {
            // Protocol 0 receives nothing until the filter is in place and the
            // socket is bound to IPv4.
            let raw = unsafe {
                libc::socket(
                    libc::AF_PACKET,
                    libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                    0,
                )
            };
            if raw < 0 {
                return None;
            }
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            let filter = program(port);
            let fprog = libc::sock_fprog {
                len: filter.len() as u16,
                filter: filter.as_ptr() as *mut libc::sock_filter,
            };
            let attached = unsafe {
                libc::setsockopt(
                    raw,
                    libc::SOL_SOCKET,
                    libc::SO_ATTACH_FILTER,
                    (&fprog as *const libc::sock_fprog).cast(),
                    size_of::<libc::sock_fprog>() as libc::socklen_t,
                )
            };
            if attached != 0 {
                return None;
            }
            let mut address: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
            address.sll_family = libc::AF_PACKET as u16;
            address.sll_protocol = (libc::ETH_P_IP as u16).to_be();
            let bound = unsafe {
                libc::bind(
                    raw,
                    (&address as *const libc::sockaddr_ll).cast(),
                    size_of::<libc::sockaddr_ll>() as libc::socklen_t,
                )
            };
            if bound != 0 {
                return None;
            }
            Some(Self {
                fd: AsyncFd::new(fd).ok()?,
            })
        }

        /// The next captured reply to `transaction`; callers bound the wait.
        pub(super) async fn next(&self, transaction: &[u8; 12]) -> Option<Reply> {
            let mut buffer = vec![0u8; 2048];
            loop {
                let mut guard = self.fd.readable().await.ok()?;
                let mut from: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
                let received = guard.try_io(|inner| {
                    let mut length = size_of::<libc::sockaddr_ll>() as libc::socklen_t;
                    let size = unsafe {
                        libc::recvfrom(
                            inner.as_raw_fd(),
                            buffer.as_mut_ptr().cast(),
                            buffer.len(),
                            0,
                            (&mut from as *mut libc::sockaddr_ll).cast(),
                            &mut length,
                        )
                    };
                    if size < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(size as usize)
                    }
                });
                let size = match received {
                    Ok(Ok(size)) => size,
                    Ok(Err(_)) => return None,
                    Err(_would_block) => continue,
                };
                if from.sll_pkttype == libc::PACKET_OUTGOING {
                    continue;
                }
                let Some((source, payload)) = udp_payload(&buffer[..size]) else {
                    continue;
                };
                if let Some(stun::Response::Mapped { mapped, other }) =
                    stun::parse(payload, transaction)
                {
                    return Some(Reply {
                        mapped,
                        other,
                        source,
                    });
                }
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod capture {
    use super::Reply;

    /// Packet capture needs Linux AF_PACKET; elsewhere the carrier view is absent.
    pub(super) struct Capture;

    impl Capture {
        pub(super) fn open(_port: u16) -> Option<Self> {
            None
        }
        pub(super) async fn next(&self, _transaction: &[u8; 12]) -> Option<Reply> {
            None
        }
    }
}

use capture::Capture;

/// What one request got back: through the socket (past the device firewall)
/// and on the wire (what the carrier NAT delivered to the device).
#[derive(Default)]
struct Seen {
    socket: Option<Reply>,
    wire: Option<Reply>,
}

impl Seen {
    fn view(&self, wire: bool) -> Option<&Reply> {
        if wire {
            self.wire.as_ref()
        } else {
            self.socket.as_ref()
        }
    }
}

/// One binding request with retransmissions, watched on the socket and, when
/// given, on the capture. Error responses and silence both give nothing.
async fn query_seen(
    socket: &UdpSocket,
    capture: Option<&Capture>,
    target: SocketAddr,
    change: u8,
) -> Seen {
    let transaction: [u8; 12] = rand::random();
    let packet = stun::request(transaction, change);
    let mut wire: Option<Reply> = None;
    for wait in WAITS {
        if socket.send_to(&packet, target).await.is_err() {
            break;
        }
        let mut deadline = tokio::time::Instant::now() + wait;
        let mut buffer = [0u8; 1024];
        loop {
            let listen = capture.filter(|_| wire.is_none());
            let captured = async move {
                match listen {
                    Some(capture) => capture.next(&transaction).await,
                    None => std::future::pending::<Option<Reply>>().await,
                }
            };
            tokio::select! {
                received = tokio::time::timeout_at(deadline, socket.recv_from(&mut buffer)) => {
                    let Ok(Ok((size, source))) = received else { break };
                    match stun::parse(&buffer[..size], &transaction) {
                        Some(stun::Response::Mapped { mapped, other }) => {
                            let reply = Reply { mapped, other, source };
                            return Seen { socket: Some(reply.clone()), wire: Some(reply) };
                        }
                        Some(stun::Response::Error) => return Seen::default(),
                        None => {}
                    }
                }
                Some(reply) = captured => {
                    wire = Some(reply);
                    deadline = deadline.min(tokio::time::Instant::now() + GRACE);
                }
            }
        }
        if wire.is_some() {
            break;
        }
    }
    Seen { socket: None, wire }
}

async fn query(socket: &UdpSocket, target: SocketAddr, change: u8) -> Option<Reply> {
    query_seen(socket, None, target, change).await.socket
}

async fn fresh_socket() -> Option<UdpSocket> {
    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.ok()
}

/// The source address the kernel picks towards `target` (no packet is sent).
async fn local_ip(target: SocketAddr) -> Option<IpAddr> {
    let socket = fresh_socket().await?;
    socket.connect(target).await.ok()?;
    Some(socket.local_addr().ok()?.ip())
}

async fn resolve(servers: &[(String, u16)]) -> Vec<(String, SocketAddr)> {
    let lookups = servers.iter().map(|(host, port)| async move {
        let found = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::lookup_host((host.as_str(), *port)),
        )
        .await
        .ok()?
        .ok()?
        .find(SocketAddr::is_ipv4)?;
        Some((format!("{host}:{port}"), found))
    });
    futures_util::future::join_all(lookups)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// A server's alternate address, when it is usable for behaviour tests: public
/// and differing from the primary in both IP and port.
fn alternate(server: SocketAddr, reply: &Reply) -> Option<SocketAddr> {
    // Tests run their RFC 5780 server on 127.0.0.1 and 127.0.0.2.
    let usable = |other: &SocketAddr| public(*other) || (cfg!(test) && other.ip().is_loopback());
    reply
        .other
        .filter(|other| usable(other) && other.ip() != server.ip() && other.port() != server.port())
}

/// A server that answered on its own socket. The socket has only talked to
/// the server's primary address, so it is fit for filtering tests.
struct Probe {
    label: String,
    server: SocketAddr,
    socket: UdpSocket,
    capture: Option<Capture>,
    reply: Reply,
}

impl Probe {
    async fn open(label: &str, server: SocketAddr, capture: bool) -> Option<Self> {
        let socket = fresh_socket().await?;
        let port = socket.local_addr().ok()?.port();
        let capture = if capture { Capture::open(port) } else { None };
        let reply = query(&socket, server, 0).await?;
        Some(Self {
            label: label.into(),
            server,
            socket,
            capture,
            reply,
        })
    }
    fn alternate(&self) -> Option<SocketAddr> {
        alternate(self.server, &self.reply)
    }
    fn port(&self) -> u16 {
        self.socket
            .local_addr()
            .map(|a| a.port())
            .unwrap_or_default()
    }
}

/// Port-only filtering verdict of one server: Some(true) when the answer came
/// from the changed port, Some(false) when nothing came, None when the server
/// ignored the request (it answered from the same port).
fn port_verdict(reply: Option<&Reply>, server: SocketAddr) -> Option<bool> {
    match reply {
        Some(reply) if reply.source.ip() == server.ip() && reply.source.port() != server.port() => {
            Some(true)
        }
        Some(_) => None,
        None => Some(false),
    }
}

/// Filtering behaviour from the change-IP-and-port answer and the port-only
/// answers, seen on the socket (`wire` false) or on the capture (`wire` true).
fn evaluate(
    both: &Seen,
    ports: &[(Seen, SocketAddr)],
    server: SocketAddr,
    wire: bool,
) -> Behaviour {
    match both.view(wire) {
        Some(reply) if reply.source.ip() != server.ip() => return Behaviour::EndpointIndependent,
        Some(_) => return Behaviour::Unknown,
        None => {}
    }
    let mut verdict = None;
    for (seen, server) in ports {
        if let Some(result) = port_verdict(seen.view(wire), *server) {
            verdict = Some(verdict.unwrap_or(false) || result);
        }
    }
    match verdict {
        Some(true) => Behaviour::AddressDependent,
        Some(false) => Behaviour::AddressAndPortDependent,
        None => Behaviour::Unknown,
    }
}

/// Whether a view still lacks a verdict that more port-only tests could give.
fn undecided(both: &Seen, ports: &[(Seen, SocketAddr)], wire: bool) -> bool {
    both.view(wire).is_none()
        && !ports
            .iter()
            .any(|(seen, server)| port_verdict(seen.view(wire), *server) == Some(true))
}

/// Filtering behaviour through `main` (which must have an alternate address)
/// on the socket, and on the wire when the probes capture; the other RFC 5780
/// probes confirm a silent port-only test, since some servers never answer
/// CHANGE-PORT alone.
async fn filtering(main: &Probe, others: &[Probe]) -> (Behaviour, Option<Behaviour>) {
    let server = main.server;
    let wire = main.capture.is_some();
    let both = query_seen(
        &main.socket,
        main.capture.as_ref(),
        server,
        stun::CHANGE_IP | stun::CHANGE_PORT,
    )
    .await;
    let mut ports: Vec<(Seen, SocketAddr)> = Vec::new();
    let needed = |ports: &[(Seen, SocketAddr)]| {
        undecided(&both, ports, false) || (wire && undecided(&both, ports, true))
    };
    if needed(&ports) {
        let seen = query_seen(
            &main.socket,
            main.capture.as_ref(),
            server,
            stun::CHANGE_PORT,
        )
        .await;
        ports.push((seen, server));
    }
    for other in others.iter().filter(|probe| probe.alternate().is_some()) {
        if !needed(&ports) {
            break;
        }
        let seen = query_seen(
            &other.socket,
            other.capture.as_ref(),
            other.server,
            stun::CHANGE_PORT,
        )
        .await;
        ports.push((seen, other.server));
    }
    let device = evaluate(&both, &ports, server, false);
    (device, wire.then(|| evaluate(&both, &ports, server, true)))
}

/// Opens `count` RFC 5780 probes (servers with an alternate address),
/// skipping servers that do not answer; plain servers are kept only when
/// nothing better answers, as the second endpoint for the mapping test.
async fn open_probes(resolved: &[(String, SocketAddr)], count: usize, capture: bool) -> Vec<Probe> {
    let mut probes: Vec<Probe> = Vec::new();
    for (label, server) in resolved {
        if probes
            .iter()
            .filter(|probe| probe.alternate().is_some())
            .count()
            >= count
        {
            break;
        }
        if let Some(probe) = Probe::open(label, *server, capture).await {
            probes.push(probe);
        }
    }
    // The wire view needs every probe captured, or a missing capture would
    // read as silence.
    if probes.iter().any(|probe| probe.capture.is_none()) {
        for probe in &mut probes {
            probe.capture = None;
        }
    }
    probes
}

/// `capture` enables the carrier view (packet capture needs root).
async fn run(servers: &[(String, u16)], capture: bool) -> Value {
    let started = Instant::now();
    let resolved = resolve(servers).await;
    if resolved.is_empty() {
        return json!({"type":"unknown","nat_level":Value::Null,"error":"dns_failed"});
    }
    let mut probes = open_probes(&resolved, PORT_FILTER_SERVERS + 1, capture).await;
    if probes.is_empty() {
        return json!({"type":"udp_blocked","nat_level":Value::Null,"error":"no_stun_answer","duration_ms":started.elapsed().as_millis() as u64});
    }
    let index = probes
        .iter()
        .position(|probe| probe.alternate().is_some())
        .unwrap_or(0);
    let main = probes.swap_remove(index);
    let other = main.alternate();
    let mapped = main.reply.mapped;
    let local = local_ip(main.server).await;
    let nat = local != Some(mapped.ip());

    // Filtering first: the mapping tests below contact the alternate address
    // and would open the filter for it.
    let (device_filtering, carrier_filtering) = if other.is_some() {
        filtering(&main, &probes).await
    } else {
        (
            Behaviour::Unknown,
            main.capture.as_ref().map(|_| Behaviour::Unknown),
        )
    };

    let mut mapping = if nat {
        Behaviour::Unknown
    } else {
        Behaviour::EndpointIndependent
    };
    if nat && let Some(other) = other {
        let alternate_ip = SocketAddr::new(other.ip(), main.server.port());
        if let Some(second) = query(&main.socket, alternate_ip, 0).await {
            mapping = if second.mapped == mapped {
                Behaviour::EndpointIndependent
            } else {
                match query(&main.socket, other, 0).await {
                    Some(third) if third.mapped == second.mapped => Behaviour::AddressDependent,
                    Some(_) => Behaviour::AddressAndPortDependent,
                    None => Behaviour::Dependent,
                }
            };
        }
    }
    if mapping == Behaviour::Unknown
        && let Some(second) = probes
            .iter()
            .find(|probe| probe.server.ip() != main.server.ip())
    {
        // Without an alternate address, compare the mapping towards another
        // server from the same socket.
        if let Some(reply) = query(&main.socket, second.server, 0).await {
            mapping = if reply.mapped == mapped {
                Behaviour::EndpointIndependent
            } else {
                Behaviour::Dependent
            };
        }
    }

    let filtering = carrier_filtering.unwrap_or(device_filtering);
    let (kind, level) = classify(nat, mapping, filtering);
    let (device_kind, device_level) = classify(nat, mapping, device_filtering);
    let local_port = main.port();
    let carrier_nat = match local {
        Some(IpAddr::V4(ip)) => nat && carrier_range(ip),
        _ => false,
    };
    json!({
        "type":kind,
        "nat_level":level,
        "nat":nat,
        "carrier_nat":carrier_nat,
        "mapping":mapping.name(),
        "filtering":filtering.name(),
        "measured_at":if carrier_filtering.is_some() { "carrier" } else { "device" },
        "device":{"type":device_kind,"nat_level":device_level,"filtering":device_filtering.name()},
        "public_address":mapped.to_string(),
        "local_address":local.map(|ip| SocketAddr::new(ip, local_port).to_string()),
        "port_preserved":mapped.port() == local_port,
        "server":main.label,
        "rfc5780":other.is_some(),
        "duration_ms":started.elapsed().as_millis() as u64,
    })
}

/// The last finished detection, or null before the first one, and the WAN
/// IPv4 addresses now; the result belongs to the addresses in its `wan_ipv4`.
pub async fn status() -> Value {
    json!({
        "running":RUNNING.try_lock().is_err(),
        "wan_ipv4":WAN.lock().unwrap().clone(),
        "last":LAST.lock().await.clone(),
    })
}

/// Runs one detection; a second concurrent request is refused rather than
/// queued, since both would share the same NAT state anyway.
pub async fn detect(servers: Vec<(String, u16)>) -> Result<Value, &'static str> {
    measure(&servers, "request").await
}

async fn measure(servers: &[(String, u16)], trigger: &str) -> Result<Value, &'static str> {
    let Ok(_running) = RUNNING.try_lock() else {
        return Err("operation_busy");
    };
    let wan = WAN.lock().unwrap().clone();
    let mut result = tokio::time::timeout(BUDGET, run(servers, true))
        .await
        .unwrap_or_else(|_| json!({"type":"unknown","nat_level":Value::Null,"error":"timeout"}));
    result["checked_at"] = json!(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default()
    );
    result["trigger"] = json!(trigger);
    result["wan_ipv4"] = json!(wan);
    *LAST.lock().await = Some(result.clone());
    Ok(result)
}

pub fn default_servers() -> Vec<(String, u16)> {
    DEFAULT_SERVERS
        .iter()
        .filter_map(|server| stun::parse_server(server))
        .collect()
}

/// Sorted IPv4 addresses of `wan4` in the snapshot's `interfaces`.
pub fn wan_ipv4(interfaces: Option<&Value>) -> Vec<String> {
    let mut addresses: Vec<String> = interfaces
        .and_then(|interfaces| interfaces.pointer("/wan4/ipv4"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("address")?.as_str()?.parse::<Ipv4Addr>().ok())
        .map(|ip| ip.to_string())
        .collect();
    addresses.sort();
    addresses.dedup();
    addresses
}

/// Detects the NAT type again whenever the WAN IPv4 addresses change and
/// keeps the result until the next change. There is no periodic detection.
pub struct Watcher {
    servers: Vec<(String, u16)>,
    seen: Vec<String>,
    since: Instant,
    /// Automatic detections run for `seen`.
    attempts: usize,
    retry_at: Option<Instant>,
}

impl Watcher {
    pub fn new(servers: Vec<(String, u16)>) -> Self {
        Self {
            servers,
            seen: Vec::new(),
            since: Instant::now(),
            attempts: 0,
            retry_at: None,
        }
    }

    /// `cached` is the cached result's `wan_ipv4` and whether it failed;
    /// `busy` is a speed test or another detection in progress, which only
    /// defers the detection.
    fn due(
        &mut self,
        addresses: Vec<String>,
        cached: Option<(Value, bool)>,
        busy: bool,
        now: Instant,
    ) -> bool {
        // Without IPv4 the old result stays; the next address decides.
        if addresses.is_empty() {
            return false;
        }
        if addresses != self.seen {
            self.seen = addresses;
            self.since = now;
            self.attempts = 0;
            self.retry_at = None;
            return false;
        }
        if now.duration_since(self.since) < SETTLE {
            return false;
        }
        if cached.is_some_and(|(wan, failed)| !failed && wan == json!(self.seen)) {
            return false;
        }
        if self.attempts > RETRIES.len() || busy {
            return false;
        }
        self.retry_at.is_none_or(|at| now >= at)
    }

    /// Records a finished automatic detection; a failure schedules the next
    /// retry, if any is left.
    fn finished(&mut self, failed: bool, now: Instant) {
        self.attempts += 1;
        self.retry_at = RETRIES
            .get(self.attempts - 1)
            .filter(|_| failed)
            .map(|wait| now + *wait);
    }

    pub async fn tick(&mut self, addresses: Vec<String>, speedtest_running: bool) {
        if !addresses.is_empty() {
            *WAN.lock().unwrap() = addresses.clone();
        }
        let cached = LAST
            .lock()
            .await
            .as_ref()
            .map(|result| (result["wan_ipv4"].clone(), result.get("error").is_some()));
        let busy = speedtest_running || RUNNING.try_lock().is_err();
        if !self.due(addresses, cached, busy, Instant::now()) {
            return;
        }
        // operation_busy: a requested detection started meanwhile; the next
        // tick looks at its result.
        if let Ok(result) = measure(&self.servers, "wan_change").await {
            self.finished(result.get("error").is_some(), Instant::now());
        }
    }
}

/// The cached result as NMS telemetry (`telemetry/device.nat`), or None
/// before the first detection. Only these keys are sent.
pub async fn telemetry() -> Option<Value> {
    LAST.lock().await.as_ref().map(summary)
}

fn summary(result: &Value) -> Value {
    let kind = result["type"]
        .as_str()
        .filter(|kind| TYPES.contains(kind))
        .unwrap_or("unknown");
    // A failed detection has no verdict; a private or 100.64/10 WAN address
    // still shows the carrier NAT.
    let carrier_nat = result["carrier_nat"].as_bool().unwrap_or_else(|| {
        result["wan_ipv4"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|ip| ip.as_str()?.parse::<Ipv4Addr>().ok())
            .any(carrier_range)
    });
    let mut out = json!({
        "type":kind,
        "carrier_nat":carrier_nat,
        "checked_at":result["checked_at"].as_u64().unwrap_or_default(),
    });
    if let Some(level) = result["nat_level"]
        .as_u64()
        .filter(|level| (1..=4).contains(level))
    {
        out["nat_level"] = json!(level);
    }
    if let Some(error) = result["error"].as_str() {
        out["error"] = json!(error.to_ascii_lowercase());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn behaviour_pairs_map_to_the_classic_names() {
        use Behaviour::*;
        assert_eq!(
            classify(true, EndpointIndependent, EndpointIndependent),
            ("full_cone", Some(1))
        );
        assert_eq!(
            classify(true, EndpointIndependent, AddressDependent),
            ("restricted_cone", Some(2))
        );
        assert_eq!(
            classify(true, EndpointIndependent, AddressAndPortDependent),
            ("port_restricted_cone", Some(3))
        );
        for mapping in [AddressDependent, AddressAndPortDependent, Dependent] {
            assert_eq!(classify(true, mapping, Unknown), ("symmetric", Some(4)));
        }
        assert_eq!(
            classify(true, EndpointIndependent, Unknown),
            ("unknown", None)
        );
        assert_eq!(classify(true, Unknown, Unknown), ("unknown", None));
        assert_eq!(
            classify(false, EndpointIndependent, EndpointIndependent),
            ("open", None)
        );
        assert_eq!(
            classify(false, EndpointIndependent, AddressAndPortDependent),
            ("firewalled", None)
        );
        assert_eq!(
            classify(false, EndpointIndependent, Unknown),
            ("no_nat", None)
        );
    }

    #[derive(Clone, Copy)]
    enum Filter {
        Open,
        PortChangeOnly,
        Closed,
    }

    /// An RFC 5780 server on 127.0.0.1/127.0.0.2 x two ports that pretends
    /// the client sits behind a NAT: it reports a mapped address in
    /// 198.51.100.0/24 (constant, or per destination when `symmetric`) and
    /// drops the changed-address replies `filter` would not pass.
    async fn mock_server(symmetric: bool, filter: Filter) -> Option<SocketAddr> {
        // RFC 5780 layout: both IPs listen on the same two ports.
        let primary = UdpSocket::bind(("127.0.0.1", 0)).await.ok()?;
        let alternate_port = UdpSocket::bind(("127.0.0.1", 0)).await.ok()?;
        let ports = [
            primary.local_addr().ok()?.port(),
            alternate_port.local_addr().ok()?.port(),
        ];
        let sockets = vec![
            primary,
            alternate_port,
            UdpSocket::bind(("127.0.0.2", ports[0])).await.ok()?,
            UdpSocket::bind(("127.0.0.2", ports[1])).await.ok()?,
        ];
        // Index: 0 = ip1/port1, 1 = ip1/port2, 2 = ip2/port1, 3 = ip2/port2.
        let sockets: Vec<std::sync::Arc<UdpSocket>> =
            sockets.into_iter().map(std::sync::Arc::new).collect();
        let addresses: Vec<SocketAddr> = sockets
            .iter()
            .map(|socket| socket.local_addr().unwrap())
            .collect();
        for index in 0..4 {
            let sockets = sockets.clone();
            let addresses = addresses.clone();
            tokio::spawn(async move {
                let mut buffer = [0u8; 512];
                while let Ok((size, client)) = sockets[index].recv_from(&mut buffer).await {
                    let packet = &buffer[..size];
                    if size < 20 {
                        continue;
                    }
                    let change = if size >= 28 && packet[20..22] == [0, 3] {
                        packet[27]
                    } else {
                        0
                    };
                    let allowed = match filter {
                        _ if change == 0 => true,
                        Filter::Open => true,
                        Filter::PortChangeOnly => change == stun::CHANGE_PORT,
                        Filter::Closed => false,
                    };
                    if !allowed {
                        continue;
                    }
                    let ip_flip = if change & stun::CHANGE_IP != 0 { 2 } else { 0 };
                    let port_flip = if change & stun::CHANGE_PORT != 0 {
                        1
                    } else {
                        0
                    };
                    let responder = (index ^ ip_flip) ^ port_flip;
                    let port = if symmetric {
                        40000 + index as u16
                    } else {
                        40000
                    };
                    let mut body = Vec::new();
                    for (kind, address) in [
                        (0x0001u16, SocketAddr::from(([198, 51, 100, 1], port))),
                        (0x802c, addresses[3]),
                    ] {
                        let SocketAddr::V4(v4) = address else {
                            unreachable!()
                        };
                        body.extend_from_slice(&kind.to_be_bytes());
                        body.extend_from_slice(&8u16.to_be_bytes());
                        body.extend_from_slice(&[0, 1]);
                        body.extend_from_slice(&v4.port().to_be_bytes());
                        body.extend_from_slice(&v4.ip().octets());
                    }
                    let mut reply = vec![0x01, 0x01];
                    reply.extend_from_slice(&(body.len() as u16).to_be_bytes());
                    reply.extend_from_slice(&packet[4..20]);
                    reply.extend(body);
                    let _ = sockets[responder].send_to(&reply, client).await;
                }
            });
        }
        Some(addresses[0])
    }

    #[tokio::test]
    async fn detection_names_each_simulated_nat() {
        for (symmetric, filter, expected) in [
            (false, Filter::Open, "full_cone"),
            (false, Filter::PortChangeOnly, "restricted_cone"),
            (false, Filter::Closed, "port_restricted_cone"),
            (true, Filter::Closed, "symmetric"),
        ] {
            let Some(server) = mock_server(symmetric, filter).await else {
                // macOS has no 127.0.0.2 unless an alias is configured.
                eprintln!("skipped: cannot bind the second loopback address");
                return;
            };
            let result = run(&[("127.0.0.1".into(), server.port())], false).await;
            assert_eq!(result["type"], expected, "{result}");
            assert_eq!(result["nat"], true, "{result}");
            assert_eq!(result["rfc5780"], true, "{result}");
            // Without capture (tests are not root) the socket view is all there is.
            assert_eq!(result["measured_at"], "device", "{result}");
            assert_eq!(result["device"]["type"], expected, "{result}");
            assert_eq!(
                result["public_address"].as_str().unwrap().split(':').next(),
                Some("198.51.100.1")
            );
        }
        // Nothing answers at all.
        let silent = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let port = silent.local_addr().unwrap().port();
        let result = run(&[("127.0.0.1".into(), port)], false).await;
        assert_eq!(result["type"], "udp_blocked", "{result}");
    }

    /// Drives a watcher in 5 s ticks like the server does. `wan(t)` gives the
    /// addresses at t seconds, `busy(t)` a running detection or speed test,
    /// `fails(n)` whether detection n (0-based) fails. Returns the times of
    /// the detections.
    fn simulate(
        until: u64,
        wan: impl Fn(u64) -> Vec<String>,
        busy: impl Fn(u64) -> bool,
        fails: impl Fn(usize) -> bool,
    ) -> Vec<u64> {
        let start = Instant::now();
        let mut watcher = Watcher::new(Vec::new());
        let mut cached: Option<(Value, bool)> = None;
        let mut runs = Vec::new();
        for t in (0..=until).step_by(5) {
            let now = start + Duration::from_secs(t);
            let addresses = wan(t);
            if watcher.due(addresses.clone(), cached.clone(), busy(t), now) {
                let failed = fails(runs.len());
                runs.push(t);
                cached = Some((json!(addresses), failed));
                watcher.finished(failed, now);
            }
        }
        runs
    }

    fn ips(list: &[&str]) -> Vec<String> {
        list.iter().map(|ip| ip.to_string()).collect()
    }

    #[test]
    fn watcher_detects_once_per_wan_change() {
        // First IPv4 after start, 30 s later; nothing more while it holds.
        let first = simulate(
            7200,
            |t| if t < 20 { ips(&[]) } else { ips(&["10.1.2.3"]) },
            |_| false,
            |_| false,
        );
        assert_eq!(first, [50]);
        // A changed address is detected again.
        let changed = simulate(
            7200,
            |t| {
                if t < 1000 {
                    ips(&["10.1.2.3"])
                } else {
                    ips(&["10.9.8.7"])
                }
            },
            |_| false,
            |_| false,
        );
        assert_eq!(changed, [30, 1030]);
        // Changes within 30 s restart the wait: one detection, for the last one.
        let flapping = simulate(
            600,
            |t| match t {
                0..100 => ips(&["10.1.2.3"]),
                100..115 => ips(&["10.1.2.4"]),
                115..130 => ips(&["10.1.2.5"]),
                _ => ips(&["10.1.2.6"]),
            },
            |_| false,
            |_| false,
        );
        assert_eq!(flapping, [30, 160]);
        // Losing IPv4 keeps the result; the same address back is no change,
        // a new one is.
        let outage = simulate(
            2000,
            |t| match t {
                0..500 => ips(&["10.1.2.3"]),
                500..600 => ips(&[]),
                600..1000 => ips(&["10.1.2.3"]),
                1000..1100 => ips(&[]),
                _ => ips(&["10.4.5.6"]),
            },
            |_| false,
            |_| false,
        );
        assert_eq!(outage, [30, 1130]);
    }

    #[test]
    fn watcher_retries_failures_three_times() {
        let wan = |_| ips(&["10.1.2.3"]);
        // Every attempt fails (udp_blocked or timeout): 3 retries 1, 5 and
        // 15 min apart, then the failure stands.
        let failing = simulate(20_000, wan, |_| false, |_| true);
        assert_eq!(failing, [30, 90, 390, 1290]);
        // A retry that succeeds ends the series.
        let recovers = simulate(20_000, wan, |_| false, |run| run < 2);
        assert_eq!(recovers, [30, 90, 390]);
        // A new address starts a fresh series.
        let moved = simulate(
            20_000,
            |t| {
                if t < 5000 {
                    ips(&["10.1.2.3"])
                } else {
                    ips(&["10.4.5.6"])
                }
            },
            |_| false,
            |_| true,
        );
        assert_eq!(moved, [30, 90, 390, 1290, 5030, 5090, 5390, 6290]);
    }

    #[test]
    fn watcher_defers_while_busy() {
        let wan = |_| ips(&["10.1.2.3"]);
        // A speed test or another detection from 20 s to 200 s: the
        // detection runs right after it ends instead of being dropped.
        let deferred = simulate(1000, wan, |t| (20..200).contains(&t), |_| false);
        assert_eq!(deferred, [200]);
        // A retry falling inside a busy window also waits for its end.
        let retry = simulate(2000, wan, |t| (80..120).contains(&t), |run| run == 0);
        assert_eq!(retry, [30, 120]);
    }

    #[test]
    fn wan_addresses_come_sorted_from_the_snapshot() {
        let interfaces = json!({"wan4":{"ipv4":[
            {"address":"10.9.8.7"},{"address":"not-an-ip"},{"address":"2001:db8::1"},
            {"address":"10.1.2.3"},{"address":"10.9.8.7"}
        ]}});
        assert_eq!(wan_ipv4(Some(&interfaces)), ["10.1.2.3", "10.9.8.7"]);
        assert!(wan_ipv4(Some(&json!({}))).is_empty());
        assert!(wan_ipv4(None).is_empty());
    }

    #[test]
    fn telemetry_carries_only_the_nms_keys() {
        let result = json!({
            "type":"full_cone","nat_level":1,"nat":true,"carrier_nat":true,
            "mapping":"endpoint_independent","filtering":"endpoint_independent",
            "measured_at":"carrier",
            "device":{"type":"port_restricted_cone","nat_level":3,"filtering":"address_and_port_dependent"},
            "public_address":"203.0.113.9:40123","local_address":"10.1.2.3:51000",
            "port_preserved":false,"server":"stun.example.com:3478","rfc5780":true,
            "duration_ms":1650,"checked_at":1760000000u64,"trigger":"wan_change",
            "wan_ipv4":["10.1.2.3"]
        });
        assert_eq!(
            summary(&result),
            json!({"type":"full_cone","nat_level":1,"carrier_nat":true,"checked_at":1760000000u64})
        );
        // Open public WAN: no level.
        assert_eq!(
            summary(
                &json!({"type":"open","nat_level":null,"nat":false,"carrier_nat":false,
                "public_address":"121.0.0.9:3000","checked_at":5,"wan_ipv4":["121.0.0.9"]})
            ),
            json!({"type":"open","carrier_nat":false,"checked_at":5})
        );
        // Failures carry the error code; carrier_nat follows the WAN address.
        assert_eq!(
            summary(
                &json!({"type":"udp_blocked","nat_level":null,"error":"no_stun_answer",
                "duration_ms":3000,"checked_at":7,"trigger":"wan_change","wan_ipv4":["10.1.2.3"]})
            ),
            json!({"type":"udp_blocked","carrier_nat":true,"checked_at":7,"error":"no_stun_answer"})
        );
        assert_eq!(
            summary(&json!({"type":"unknown","nat_level":null,"error":"timeout",
                "checked_at":8,"wan_ipv4":["121.0.0.9"]})),
            json!({"type":"unknown","carrier_nat":false,"checked_at":8,"error":"timeout"})
        );
        // Anything outside the agreed names is reported as unknown.
        assert_eq!(
            summary(&json!({"type":"weird","nat_level":9,"checked_at":9,"wan_ipv4":[]}))["type"],
            "unknown"
        );
    }

    #[tokio::test]
    async fn watcher_records_the_wan_with_the_result() {
        let silent = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let port = silent.local_addr().unwrap().port();
        let wan = ips(&["10.1.2.3"]);
        let mut watcher = Watcher::new(vec![("127.0.0.1".into(), port)]);
        watcher.seen = wan.clone();
        watcher.since = Instant::now().checked_sub(SETTLE).unwrap();
        // A running speed test defers it.
        watcher.tick(wan.clone(), true).await;
        assert_eq!(watcher.attempts, 0);
        watcher.tick(wan.clone(), false).await;
        let status = status().await;
        assert_eq!(status["wan_ipv4"], json!(["10.1.2.3"]));
        let last = &status["last"];
        assert_eq!(last["type"], "udp_blocked", "{last}");
        assert_eq!(last["trigger"], "wan_change");
        assert_eq!(last["wan_ipv4"], json!(["10.1.2.3"]));
        assert_eq!(watcher.attempts, 1);
        let retry = watcher.retry_at.unwrap();
        // Not retried before the back-off ends.
        watcher.tick(wan, false).await;
        assert_eq!(watcher.attempts, 1);
        assert_eq!(watcher.retry_at, Some(retry));
        let reported = telemetry().await.unwrap();
        assert_eq!(reported["type"], "udp_blocked");
        assert_eq!(reported["error"], "no_stun_answer");
        assert_eq!(reported["carrier_nat"], true);
    }

    fn ipv4_udp(source: [u8; 4], port: u16, payload: &[u8], flags: u16) -> Vec<u8> {
        let mut packet = vec![0x45, 0];
        packet.extend_from_slice(&((28 + payload.len()) as u16).to_be_bytes());
        packet.extend_from_slice(&[0, 0]);
        packet.extend_from_slice(&flags.to_be_bytes());
        packet.extend_from_slice(&[64, 17, 0, 0]);
        packet.extend_from_slice(&source);
        packet.extend_from_slice(&[10, 0, 0, 2]);
        packet.extend_from_slice(&port.to_be_bytes());
        packet.extend_from_slice(&40000u16.to_be_bytes());
        packet.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        packet.extend_from_slice(&[0, 0]);
        packet.extend_from_slice(payload);
        packet
    }

    #[test]
    fn captured_packets_yield_source_and_payload() {
        let packet = ipv4_udp([111, 206, 174, 2], 3479, b"stun", 0x4000);
        let (source, payload) = udp_payload(&packet).unwrap();
        assert_eq!(source, "111.206.174.2:3479".parse().unwrap());
        assert_eq!(payload, b"stun");
        // Trailing link padding past the IP length is not payload.
        let mut padded = packet.clone();
        padded.extend_from_slice(&[0; 6]);
        assert_eq!(udp_payload(&padded).unwrap().1, b"stun");
        // Fragments, other protocols and short or non-IPv4 packets are ignored.
        assert!(udp_payload(&ipv4_udp([1, 2, 3, 4], 1, b"x", 0x2000)).is_none());
        let mut tcp = packet.clone();
        tcp[9] = 6;
        assert!(udp_payload(&tcp).is_none());
        assert!(udp_payload(&packet[..20]).is_none());
        let mut v6 = packet;
        v6[0] = 0x60;
        assert!(udp_payload(&v6).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn capture_filter_matches_unfragmented_udp_to_the_port() {
        let program = capture::program(40000);
        assert_eq!(program.len(), 9);
        // Every jump lands inside the program.
        for (index, op) in program.iter().enumerate() {
            if op.code & 0x07 == 0x05 {
                assert!(index + 1 + usize::from(op.jt) < program.len());
                assert!(index + 1 + usize::from(op.jf) < program.len());
            }
        }
        assert_eq!(program[6].k, 40000);
    }

    #[test]
    fn carrier_and_public_ranges() {
        for ip in [
            "10.1.2.3",
            "100.64.0.1",
            "100.127.255.254",
            "192.168.0.1",
            "172.16.0.1",
        ] {
            assert!(carrier_range(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["100.63.0.1", "100.128.0.1", "111.206.174.3"] {
            assert!(!carrier_range(ip.parse().unwrap()), "{ip}");
        }
        assert!(public("111.206.174.3:3479".parse().unwrap()));
        assert!(!public("10.48.0.11:443".parse().unwrap()));
        assert!(!public("0.0.0.0:3478".parse().unwrap()));
    }
}
