//! Bounded unicast SNTP. A validated packet is not cryptographic authentication.
use rand::{RngCore, rngs::OsRng};
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::{
    net::{UdpSocket, lookup_host},
    time::timeout,
};

const OFFSET: i128 = 2_208_988_800;
const LOW: i128 = 1_704_067_200; // 2024-01-01
const HIGH: i128 = 4_102_444_800; // 2100-01-01

#[derive(Clone, Debug)]
pub(super) struct Endpoint {
    pub name: String,
    host: String,
    port: u16,
}

impl Endpoint {
    pub fn parse(input: &str) -> Result<Self, &'static str> {
        if input.is_empty()
            || input.len() > 300
            || input != input.trim()
            || input.chars().any(char::is_control)
            || input.contains(['/', '\\', '@', '?', '#', '%'])
        {
            return Err("invalid_server");
        }
        let bare = input
            .strip_prefix('[')
            .and_then(|v| v.strip_suffix(']'))
            .unwrap_or(input);
        let (host, port) = if let Ok(ip) = bare.parse::<IpAddr>() {
            (ip.to_string(), 123)
        } else if let Ok(addr) = input.parse::<SocketAddr>() {
            (addr.ip().to_string(), addr.port())
        } else if let Some((host, port)) = input.rsplit_once(':') {
            if host.contains(':') {
                return Err("invalid_server");
            }
            (
                host.to_owned(),
                port.parse::<u16>().map_err(|_| "invalid_server")?,
            )
        } else {
            (input.to_owned(), 123)
        };
        if port == 0 || host.is_empty() {
            return Err("invalid_server");
        }
        if host.parse::<IpAddr>().is_err()
            && (host.len() > 253
                || host
                    .strip_suffix('.')
                    .unwrap_or(&host)
                    .split('.')
                    .any(|label| {
                        label.is_empty()
                            || label.len() > 63
                            || label.starts_with('-')
                            || label.ends_with('-')
                            || !label
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    }))
        {
            return Err("invalid_server");
        }
        let name = if port == 123 {
            host.clone()
        } else if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        Ok(Self { name, host, port })
    }
}

fn stamp(bytes: &[u8]) -> Result<i128, &'static str> {
    let raw = u64::from_be_bytes(bytes.try_into().map_err(|_| "invalid_packet")?);
    if raw == 0 {
        return Err("invalid_timestamp");
    }
    let sec = i128::from(raw >> 32);
    let frac = i128::from(raw as u32) * 1_000_000_000 / (1i128 << 32);
    for era in [0, 1] {
        let unix = sec + (i128::from(era) << 32) - OFFSET;
        if (LOW..HIGH).contains(&unix) {
            return Ok(unix * 1_000_000_000 + frac);
        }
    }
    Err("invalid_timestamp")
}

pub(super) fn parse_packet(
    raw: &[u8],
    nonce: &[u8; 8],
    rtt: Duration,
) -> Result<i128, &'static str> {
    if !(48..=512).contains(&raw.len())
        || raw[0] & 7 != 4
        || !matches!((raw[0] >> 3) & 7, 3 | 4)
        || raw[0] >> 6 == 3
    {
        return Err("invalid_packet");
    }
    if raw[1] == 0 {
        return Err("ntp_kod");
    }
    if raw[1] >= 16 || &raw[24..32] != nonce || rtt > Duration::from_secs(3) {
        return Err("invalid_packet");
    }
    let receive = stamp(&raw[32..40])?;
    let transmit = stamp(&raw[40..48])?;
    let reference = stamp(&raw[16..24])?;
    let processing = transmit - receive;
    if reference > transmit || processing < 0 || processing > rtt.as_nanos() as i128 + 50_000_000 {
        return Err("invalid_packet");
    }
    let delay = i32::from_be_bytes(raw[4..8].try_into().unwrap());
    let dispersion = u32::from_be_bytes(raw[8..12].try_into().unwrap());
    if delay < 0 || i64::from(delay) / 2 + i64::from(dispersion) > 10 * 65536 {
        return Err("invalid_packet");
    }
    Ok(transmit + (rtt.as_nanos() as i128 - processing).max(0) / 2)
}

pub(super) async fn query(endpoint: &Endpoint) -> Result<i128, &'static str> {
    timeout(Duration::from_secs(4), async {
        let addresses: Vec<_> = lookup_host((endpoint.host.as_str(), endpoint.port))
            .await
            .map_err(|_| "dns_failed")?
            .take(4)
            .collect();
        if addresses.is_empty() {
            return Err("dns_failed");
        }
        for addr in addresses {
            if addr.ip().is_unspecified() || addr.ip().is_multicast() {
                continue;
            }
            let bind = if addr.is_ipv4() {
                "0.0.0.0:0"
            } else {
                "[::]:0"
            };
            let socket = UdpSocket::bind(bind)
                .await
                .map_err(|_| "ntp_socket_failed")?;
            socket
                .connect(addr)
                .await
                .map_err(|_| "ntp_socket_failed")?;
            let mut request = [0u8; 48];
            request[0] = 0x23;
            let mut nonce = [0u8; 8];
            OsRng.fill_bytes(&mut nonce);
            if nonce == [0; 8] {
                nonce[7] = 1;
            }
            request[40..48].copy_from_slice(&nonce);
            let start = crate::elapsed::now();
            socket.send(&request).await.map_err(|_| "ntp_send_failed")?;
            let mut response = [0u8; 513];
            match timeout(Duration::from_secs(2), socket.recv(&mut response)).await {
                Ok(Ok(length)) => {
                    return parse_packet(
                        &response[..length],
                        &nonce,
                        crate::elapsed::now().saturating_sub(start),
                    );
                }
                _ => continue,
            }
        }
        Err("ntp_timeout")
    })
    .await
    .map_err(|_| "ntp_timeout")?
}

pub(super) async fn first(server: &str) -> Result<(i128, String), &'static str> {
    let mut endpoints = vec![Endpoint::parse(server)?];
    for fallback in ["ntp.aliyun.com", "ntp.tencent.com", "pool.ntp.org"] {
        if endpoints.iter().all(|e| e.name != fallback) {
            endpoints.push(Endpoint::parse(fallback)?);
        }
    }
    let mut queries = tokio::task::JoinSet::new();
    for endpoint in endpoints {
        queries.spawn(async move { query(&endpoint).await.map(|n| (n, endpoint.name)) });
    }
    let mut last = "ntp_timeout";
    while let Some(reply) = queries.join_next().await {
        match reply {
            Ok(Ok(sample)) => {
                queries.abort_all();
                return Ok(sample);
            }
            Ok(Err(error)) => last = error,
            Err(_) => {}
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bytes(unix: u64) -> [u8; 8] {
        ((unix + OFFSET as u64) << 32).to_be_bytes()
    }
    #[test]
    fn server_syntax_is_a_network_endpoint_not_a_url_or_command() {
        for s in [
            "ntp.example.com",
            "ntp.example.com.",
            "ntp.example.com:12345",
            "192.0.2.1:123",
            "2001:db8::1",
            "[2001:db8::1]",
            "[2001:db8::1]:333",
        ] {
            assert!(Endpoint::parse(s).is_ok(), "{s}");
        }
        for s in [
            "https://x",
            "a/b",
            "user@x",
            "x:0",
            "x:65536",
            "$(id)",
            " x",
            "x?token=a",
            "[::1]:0",
            "fe80::1%eth0",
        ] {
            assert!(Endpoint::parse(s).is_err(), "{s}");
        }
    }
    #[test]
    fn packets_require_matching_fresh_unicast_synchronized_time() {
        let nonce = [0x77; 8];
        let mut p = [0u8; 48];
        p[0] = 0x24;
        p[1] = 2;
        p[16..24].copy_from_slice(&bytes(1_800_000_000));
        p[24..32].copy_from_slice(&nonce);
        p[32..40].copy_from_slice(&bytes(1_800_000_000));
        p[40..48].copy_from_slice(&bytes(1_800_000_000));
        assert!(parse_packet(&p, &nonce, Duration::from_millis(20)).is_ok());
        assert!(parse_packet(&p, &[0; 8], Duration::ZERO).is_err());
        p[0] = 0xe4;
        assert!(parse_packet(&p, &nonce, Duration::ZERO).is_err());
        p[0] = 0x24;
        p[1] = 0;
        assert_eq!(parse_packet(&p, &nonce, Duration::ZERO), Err("ntp_kod"));
        p[1] = 16;
        assert!(parse_packet(&p, &nonce, Duration::ZERO).is_err());
        assert!(stamp(&bytes(2_300_000_000)).is_ok()); // era one after 2036
        assert!(stamp(&[0; 8]).is_err());
    }
    #[tokio::test]
    async fn connected_udp_query_matches_the_fresh_nonce_and_bounds_reply() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let responder = tokio::spawn(async move {
            let mut request = [0u8; 48];
            let (n, client) = server.recv_from(&mut request).await.unwrap();
            assert_eq!(n, 48);
            assert_eq!(request[0], 0x23);
            assert_ne!(request[40..48], [0; 8]);
            let mut reply = [0u8; 48];
            reply[0] = 0x24;
            reply[1] = 2;
            reply[16..24].copy_from_slice(&bytes(1_800_000_000));
            reply[24..32].copy_from_slice(&request[40..48]);
            reply[32..40].copy_from_slice(&bytes(1_800_000_000));
            reply[40..48].copy_from_slice(&bytes(1_800_000_000));
            server.send_to(&reply, client).await.unwrap();
        });
        let utc = query(&Endpoint::parse(&address.to_string()).unwrap())
            .await
            .unwrap();
        assert!((1_800_000_000_000_000_000..1_800_000_001_000_000_000).contains(&utc));
        responder.await.unwrap();
    }
}
