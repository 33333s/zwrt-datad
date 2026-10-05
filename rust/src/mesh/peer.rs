//! One WebRTC peer connection: the answering end of the `nms` data channel.
//!
//! str0m is sans-IO, so this task owns the UDP socket and the clock. Browser
//! candidates arrive over the rendezvous socket and ours (host plus
//! server-reflexive from a STUN probe on the same socket) go back the same way.
use super::{frame, service::Service, stun};
use rand::Rng;
use serde_json::{Value, json};
use std::sync::Arc;
use std::{
    collections::VecDeque,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};
use str0m::{
    Candidate, Event, Input, Output, Rtc,
    change::SdpOffer,
    channel::ChannelId,
    net::{Protocol, Receive},
};
use tokio::{net::UdpSocket, sync::mpsc, task::JoinHandle};

const CHANNEL_LABEL: &str = "nms";
/// At most this many browser candidates are used per peer.
const MAX_REMOTE_CANDIDATES: usize = 64;
/// Frames waiting for send-buffer room; beyond this the peer is dropped.
const MAX_QUEUED_BYTES: usize = 8 * 1024 * 1024;
const LOW_WATER: usize = 64 * 1024;

pub struct Config {
    pub stun: Vec<SocketAddr>,
    pub ports: (u16, u16),
    /// Tests advertise loopback and nothing else, so they do not depend on the
    /// host's interfaces; real sessions never advertise loopback.
    pub loopback_only: bool,
}

pub struct Link {
    /// Answer and local candidates for the browser (`answer`, `ice` objects).
    pub signals: mpsc::Sender<Value>,
    /// Browser candidates (`ice` objects as they arrived).
    pub remote_ice: mpsc::Receiver<Value>,
    /// What the channel serves.
    pub mode: super::Mode,
}

/// Non-loopback IPv4 addresses of this device, for host candidates (or just
/// loopback when `loopback_only`, for tests).
pub fn local_addresses(loopback_only: bool) -> Vec<IpAddr> {
    if loopback_only {
        return vec![IpAddr::V4(Ipv4Addr::LOCALHOST)];
    }
    let mut out = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills a linked list that freeifaddrs releases below.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut cursor = list;
        while !cursor.is_null() {
            let entry = &*cursor;
            cursor = entry.ifa_next;
            let up = entry.ifa_flags & libc::IFF_UP as u32 != 0;
            let loopback = entry.ifa_flags & libc::IFF_LOOPBACK as u32 != 0;
            if !up || loopback || entry.ifa_addr.is_null() {
                continue;
            }
            if i32::from((*entry.ifa_addr).sa_family) != libc::AF_INET {
                continue;
            }
            let sin = &*(entry.ifa_addr as *const libc::sockaddr_in);
            let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            if !ip.is_unspecified() && !ip.is_link_local() && !out.contains(&IpAddr::V4(ip)) {
                out.push(IpAddr::V4(ip));
            }
        }
        libc::freeifaddrs(list);
    }
    out
}

/// One UDP socket per local address, all on the same port. str0m matches packets
/// by their destination address, which a wildcard socket cannot report.
struct Sockets {
    sockets: Vec<(SocketAddr, Arc<UdpSocket>)>,
    readers: Vec<JoinHandle<()>>,
}

impl Drop for Sockets {
    fn drop(&mut self) {
        for reader in &self.readers {
            reader.abort();
        }
    }
}

type Datagram = (SocketAddr, SocketAddr, Vec<u8>);

/// Binds every address on one port, preferring a random port of the offered
/// range. Addresses that refuse the port are left out.
async fn bind(addresses: &[IpAddr], ports: (u16, u16)) -> Option<Sockets> {
    let (low, high) = ports;
    let mut candidates: Vec<u16> = Vec::new();
    if low != 0 && low <= high {
        let span = u32::from(high - low) + 1;
        let start = rand::thread_rng().gen_range(0..span);
        candidates.extend((0..span).map(|step| low + ((start + step) % span) as u16));
    }
    candidates.push(0);
    for port in candidates {
        let first = addresses.first()?;
        let Ok(socket) = UdpSocket::bind((*first, port)).await else {
            continue;
        };
        let chosen = socket.local_addr().ok()?.port();
        let mut sockets = vec![(SocketAddr::new(*first, chosen), Arc::new(socket))];
        for ip in &addresses[1..] {
            if let Ok(socket) = UdpSocket::bind((*ip, chosen)).await {
                sockets.push((SocketAddr::new(*ip, chosen), Arc::new(socket)));
            }
        }
        return Some(Sockets {
            sockets,
            readers: Vec::new(),
        });
    }
    None
}

/// The local address the default route uses (nothing is sent), for the STUN probe.
fn primary_address() -> Option<IpAddr> {
    let probe = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    probe.connect((Ipv4Addr::new(1, 1, 1, 1), 53)).ok()?;
    probe.local_addr().ok().map(|address| address.ip())
}

fn media_id(sdp: &str) -> String {
    sdp.lines()
        .find_map(|line| line.trim().strip_prefix("a=mid:"))
        .map(|mid| mid.trim().to_owned())
        .filter(|mid| !mid.is_empty() && mid.len() <= 16)
        .unwrap_or_else(|| "0".into())
}

fn ice_message(candidate: &Candidate, mid: &str) -> Value {
    json!({"type":"ice","candidate":{
        "candidate":candidate.to_sdp_string(),"sdpMid":mid,"sdpMLineIndex":0}})
}

fn remote_candidate(message: &Value) -> Option<Candidate> {
    let text = message.get("candidate")?.get("candidate")?.as_str()?;
    if text.is_empty() || text.len() > 512 {
        return None;
    }
    // Browsers hide host addresses behind mDNS names, which cannot be parsed
    // here; those candidates are skipped and the connection uses the others.
    Candidate::from_sdp_string(text).ok()
}

/// Runs until the connection ends, the offer is refused or the task is aborted.
pub async fn run(offer: String, config: Config, mut link: Link) {
    let addresses = local_addresses(config.loopback_only);
    let Some(mut sockets) = bind(&addresses, config.ports).await else {
        return;
    };
    let (datagrams, mut inbound) = mpsc::channel::<Datagram>(256);
    for (local, socket) in &sockets.sockets {
        let (local, socket, datagrams) = (*local, socket.clone(), datagrams.clone());
        sockets.readers.push(tokio::spawn(async move {
            let mut buffer = vec![0u8; 2048];
            while let Ok((length, source)) = socket.recv_from(&mut buffer).await {
                // A full queue drops the datagram, as an overloaded network would.
                let _ = datagrams.try_send((local, source, buffer[..length].to_vec()));
            }
        }));
    }
    drop(datagrams);
    let mut rtc = Rtc::builder().build(Instant::now());
    let mid = media_id(&offer);
    let mut local_candidates = Vec::new();
    for (local, _) in &sockets.sockets {
        if let Ok(candidate) = Candidate::host(*local, "udp")
            && let Some(added) = rtc.add_local_candidate(candidate)
        {
            local_candidates.push(added.clone());
        }
    }
    let Ok(offer) = SdpOffer::from_sdp_string(&offer) else {
        return;
    };
    let Ok(answer) = rtc.sdp_api().accept_offer(offer) else {
        return;
    };
    if link
        .signals
        .send(json!({"type":"answer","sdp":answer.to_sdp_string()}))
        .await
        .is_err()
    {
        return;
    }
    for candidate in &local_candidates {
        let _ = link.signals.send(ice_message(candidate, &mid)).await;
    }

    // STUN probes go out from this very socket; the first answers decide the
    // server-reflexive candidates.
    let mut probes: Vec<([u8; 12], SocketAddr)> = config
        .stun
        .iter()
        .map(|server| (rand::random::<[u8; 12]>(), *server))
        .collect();
    let stun_base = primary_address()
        .and_then(|ip| sockets.sockets.iter().find(|(local, _)| local.ip() == ip))
        .or_else(|| sockets.sockets.first())
        .map(|(local, socket)| (*local, socket.clone()));
    let mut mapped: Vec<SocketAddr> = Vec::new();
    let mut probe_round = 0u32;
    let mut next_probe = tokio::time::Instant::now();

    let mut channel: Option<ChannelId> = None;
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(64);
    let mut service = Service::new(link.mode, out_tx);
    let mut queue: VecDeque<(bool, Vec<u8>)> = VecDeque::new();
    let mut queued = 0usize;
    // Pongs bypass the HTTP queue, so a busy download never holds them back.
    let mut pongs: VecDeque<(Instant, Vec<u8>)> = VecDeque::new();
    // Pongs handed to SCTP and not yet followed by an outgoing datagram.
    let mut unsent_pongs: Vec<(Instant, Instant)> = Vec::new();
    let mut received_at = Instant::now();
    let mut remote_count = 0usize;

    loop {
        // Drain everything the connection has to say before waiting again.
        let deadline = loop {
            if !rtc.is_alive() {
                return;
            }
            match rtc.poll_output() {
                Ok(Output::Transmit(transmit)) => {
                    let socket = sockets
                        .sockets
                        .iter()
                        .find(|(local, _)| *local == transmit.source)
                        .or_else(|| sockets.sockets.first())
                        .map(|(_, socket)| socket.clone());
                    if let Some(socket) = socket {
                        let _ = socket
                            .send_to(&transmit.contents, transmit.destination)
                            .await;
                    }
                    for (received, written) in unsent_pongs.drain(..) {
                        super::record_ping(received, written, Instant::now());
                    }
                }
                Ok(Output::Timeout(at)) => {
                    // Writing into SCTP only queues data; the datagrams carrying
                    // it come out of the next poll. Without this a pong waited for
                    // the next timer or inbound packet (often 100-200 ms).
                    if write_pending(
                        &mut rtc,
                        channel,
                        &mut pongs,
                        &mut unsent_pongs,
                        &mut queue,
                        &mut queued,
                    ) {
                        continue;
                    }
                    break at;
                }
                Ok(Output::Event(event)) => match event {
                    Event::ChannelOpen(id, label)
                        if label == CHANNEL_LABEL && channel.is_none() =>
                    {
                        if let Some(mut open) = rtc.channel(id) {
                            open.set_buffered_amount_low_threshold(LOW_WATER);
                        }
                        channel = Some(id);
                    }
                    Event::ChannelClose(id) if channel == Some(id) => return,
                    Event::ChannelData(data) if channel == Some(data.id) => {
                        if !data.binary {
                            if let Some(pong) =
                                std::str::from_utf8(&data.data).ok().and_then(frame::pong)
                            {
                                pongs.push_back((received_at, pong.into_bytes()));
                            }
                        } else if let Ok((header, payload)) = frame::decode(&data.data) {
                            service.handle(header, payload);
                        }
                    }
                    _ => {}
                },
                Err(_) => return,
            }
        };
        if queued > MAX_QUEUED_BYTES || pongs.len() > 64 {
            return;
        }
        let wake = tokio::time::Instant::from_std(deadline);
        let probing = !probes.is_empty() && probe_round < 4;
        tokio::select! {
            received = inbound.recv() => {
                let Some((destination, source, packet)) = received else { return };
                let packet = &packet[..];
                if let Some(address) = probes
                    .iter()
                    .find_map(|(id, _)| stun::parse_response(packet, id))
                {
                    if !mapped.contains(&address)
                        && let Some((base, _)) = &stun_base
                    {
                        mapped.push(address);
                        if let Ok(candidate) = Candidate::server_reflexive(address, *base, "udp")
                            && let Some(added) = rtc.add_local_candidate(candidate)
                        {
                            let message = ice_message(added, &mid);
                            let _ = link.signals.send(message).await;
                        }
                    }
                    probes.clear();
                    continue;
                }
                if let Ok(receive) = Receive::new(Protocol::Udp, source, destination, packet) {
                    received_at = Instant::now();
                    let _ = rtc.handle_input(Input::Receive(received_at, receive));
                }
            }
            _ = tokio::time::sleep_until(wake) => {
                let _ = rtc.handle_input(Input::Timeout(Instant::now()));
            }
            _ = tokio::time::sleep_until(next_probe), if probing => {
                if let Some((_, socket)) = &stun_base {
                    for (id, server) in &probes {
                        let _ = socket.send_to(&stun::binding_request(*id), server).await;
                    }
                }
                probe_round += 1;
                next_probe = tokio::time::Instant::now() + Duration::from_millis(400 * u64::from(probe_round));
            }
            frame = out_rx.recv() => {
                if let Some(frame) = frame {
                    queued += frame.len();
                    queue.push_back((true, frame));
                }
            }
            message = link.remote_ice.recv() => {
                let Some(message) = message else { return };
                if remote_count < MAX_REMOTE_CANDIDATES
                    && let Some(candidate) = remote_candidate(&message)
                {
                    remote_count += 1;
                    rtc.add_remote_candidate(candidate);
                }
            }
        }
    }
}

/// Moves pongs, then queued frames, into the SCTP send buffer while there is
/// room. Returns whether anything was written.
fn write_pending(
    rtc: &mut Rtc,
    channel: Option<ChannelId>,
    pongs: &mut VecDeque<(Instant, Vec<u8>)>,
    unsent_pongs: &mut Vec<(Instant, Instant)>,
    queue: &mut VecDeque<(bool, Vec<u8>)>,
    queued: &mut usize,
) -> bool {
    let Some(id) = channel else { return false };
    let Some(mut open) = rtc.channel(id) else {
        return false;
    };
    let mut wrote = false;
    while let Some((received, data)) = pongs.front() {
        match open.write(false, data) {
            Ok(true) => {
                unsent_pongs.push((*received, Instant::now()));
                pongs.pop_front();
                wrote = true;
            }
            _ => return wrote,
        }
    }
    while let Some((binary, data)) = queue.front() {
        match open.write(*binary, data) {
            Ok(true) => {
                *queued = queued.saturating_sub(data.len());
                queue.pop_front();
                wrote = true;
            }
            _ => break,
        }
    }
    wrote
}
