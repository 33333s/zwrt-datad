//! A minimal STUN binding client (RFC 5389), plus the RFC 5780 attributes the
//! NAT type probe needs. The mesh probe runs on the ICE socket itself, so the
//! mapping it finds is the one the peer will actually reach.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

const MAGIC: [u8; 4] = [0x21, 0x12, 0xa4, 0x42];
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const BINDING_ERROR: u16 = 0x0111;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;
const MAPPED_ADDRESS: u16 = 0x0001;
const CHANGE_REQUEST: u16 = 0x0003;
/// RFC 3489 name of OTHER-ADDRESS; older servers still send it.
const CHANGED_ADDRESS: u16 = 0x0005;
const OTHER_ADDRESS: u16 = 0x802c;
/// CHANGE-REQUEST flags (RFC 5780 section 7.2).
pub const CHANGE_IP: u8 = 0x04;
pub const CHANGE_PORT: u8 = 0x02;

/// A binding response for one transaction.
#[derive(Debug, PartialEq)]
pub enum Response {
    /// Success: the reflexive address and, from an RFC 5780 server, the
    /// alternate address it can answer from.
    Mapped {
        mapped: SocketAddr,
        other: Option<SocketAddr>,
    },
    /// The server rejected the request (for example an unknown CHANGE-REQUEST).
    Error,
}

/// `stun:host[:port]` as NMS sends it (port defaults to 3478). Returns host and port.
pub fn parse_server(value: &str) -> Option<(String, u16)> {
    let rest = value.strip_prefix("stun:")?;
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) => (host, port.parse::<u16>().ok().filter(|p| *p != 0)?),
        None => (rest, 3478),
    };
    (!host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'))
    .then(|| (host.to_owned(), port))
}

#[cfg_attr(not(feature = "mesh"), allow(dead_code))]
pub fn binding_request(transaction: [u8; 12]) -> Vec<u8> {
    request(transaction, 0)
}

/// A binding request; `change` holds CHANGE_IP/CHANGE_PORT flags, or 0 for none.
pub fn request(transaction: [u8; 12], change: u8) -> Vec<u8> {
    let length: u16 = if change == 0 { 0 } else { 8 };
    let mut packet = Vec::with_capacity(20 + usize::from(length));
    packet.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
    packet.extend_from_slice(&length.to_be_bytes());
    packet.extend_from_slice(&MAGIC);
    packet.extend_from_slice(&transaction);
    if change != 0 {
        packet.extend_from_slice(&CHANGE_REQUEST.to_be_bytes());
        packet.extend_from_slice(&4u16.to_be_bytes());
        packet.extend_from_slice(&[0, 0, 0, change]);
    }
    packet
}

/// The mapped address of a binding success response for `transaction`.
#[cfg_attr(not(feature = "mesh"), allow(dead_code))]
pub fn parse_response(packet: &[u8], transaction: &[u8; 12]) -> Option<SocketAddr> {
    match parse(packet, transaction)? {
        Response::Mapped { mapped, .. } => Some(mapped),
        Response::Error => None,
    }
}

/// A binding success or error response for `transaction`; anything else
/// (another transaction, a damaged packet, a success without an address) is None.
pub fn parse(packet: &[u8], transaction: &[u8; 12]) -> Option<Response> {
    if packet.len() < 20 || packet[4..8] != MAGIC || packet[8..20] != transaction[..] {
        return None;
    }
    match u16::from_be_bytes([packet[0], packet[1]]) {
        BINDING_SUCCESS => {}
        BINDING_ERROR => return Some(Response::Error),
        _ => return None,
    }
    let length = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    let body = packet.get(20..20 + length)?;
    let mut offset = 0;
    let (mut xor, mut plain, mut other, mut changed) = (None, None, None, None);
    while offset + 4 <= body.len() {
        let kind = u16::from_be_bytes([body[offset], body[offset + 1]]);
        let size = usize::from(u16::from_be_bytes([body[offset + 2], body[offset + 3]]));
        let value = body.get(offset + 4..offset + 4 + size)?;
        match kind {
            XOR_MAPPED_ADDRESS => xor = decode_address(value, true, transaction),
            MAPPED_ADDRESS => plain = decode_address(value, false, transaction),
            OTHER_ADDRESS => other = decode_address(value, false, transaction),
            CHANGED_ADDRESS => changed = decode_address(value, false, transaction),
            _ => {}
        }
        offset += 4 + size.div_ceil(4) * 4;
    }
    Some(Response::Mapped {
        mapped: xor.or(plain)?,
        other: other.or(changed),
    })
}

fn decode_address(value: &[u8], xor: bool, transaction: &[u8; 12]) -> Option<SocketAddr> {
    if value.len() < 4 {
        return None;
    }
    let mut port = u16::from_be_bytes([value[2], value[3]]);
    if xor {
        port ^= u16::from_be_bytes([MAGIC[0], MAGIC[1]]);
    }
    let ip = match value[1] {
        1 if value.len() >= 8 => {
            let mut octets = [value[4], value[5], value[6], value[7]];
            if xor {
                for (octet, mask) in octets.iter_mut().zip(MAGIC) {
                    *octet ^= mask;
                }
            }
            IpAddr::V4(Ipv4Addr::from(octets))
        }
        2 if value.len() >= 20 => {
            let mut octets: [u8; 16] = value[4..20].try_into().ok()?;
            if xor {
                let mask: Vec<u8> = MAGIC.iter().chain(transaction.iter()).copied().collect();
                for (octet, mask) in octets.iter_mut().zip(mask) {
                    *octet ^= mask;
                }
            }
            IpAddr::V6(Ipv6Addr::from(octets))
        }
        _ => return None,
    };
    (port != 0).then_some(SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(transaction: [u8; 12], attribute: u16, value: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&attribute.to_be_bytes());
        body.extend_from_slice(&(value.len() as u16).to_be_bytes());
        body.extend_from_slice(value);
        while body.len() % 4 != 0 {
            body.push(0);
        }
        let mut packet = Vec::new();
        packet.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        packet.extend_from_slice(&(body.len() as u16).to_be_bytes());
        packet.extend_from_slice(&MAGIC);
        packet.extend_from_slice(&transaction);
        packet.extend(body);
        packet
    }

    #[test]
    fn server_names_follow_the_nms_pattern() {
        assert_eq!(
            parse_server("stun:services.ericsfj.com:3478"),
            Some(("services.ericsfj.com".into(), 3478))
        );
        assert_eq!(
            parse_server("stun:stun.cloudflare.com"),
            Some(("stun.cloudflare.com".into(), 3478))
        );
        for bad in [
            "stuns:a.b:1",
            "a.b:3478",
            "stun:",
            "stun::3478",
            "stun:a b:3478",
            "stun:a.b:0",
            "stun:a.b:70000",
            "stun:a/b:1",
            "stun:a.b:x",
        ] {
            assert!(parse_server(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn request_is_a_well_formed_binding_request() {
        let packet = binding_request([7; 12]);
        assert_eq!(packet.len(), 20);
        assert_eq!(&packet[..2], &[0, 1]);
        assert_eq!(&packet[2..4], &[0, 0]);
        assert_eq!(&packet[4..8], &MAGIC);
        assert_eq!(&packet[8..], &[7; 12]);
    }

    #[test]
    fn xor_mapped_addresses_decode_for_both_families() {
        let transaction = [9; 12];
        // 203.0.113.9:54321 XORed with the magic cookie.
        let port = 54321u16 ^ 0x2112;
        let mut v4 = vec![0, 1, (port >> 8) as u8, port as u8];
        v4.extend([203 ^ 0x21, 0x12, 113 ^ 0xa4, 9 ^ 0x42]);
        let got = parse_response(
            &response(transaction, XOR_MAPPED_ADDRESS, &v4),
            &transaction,
        );
        assert_eq!(got, Some("203.0.113.9:54321".parse().unwrap()));
        let ip: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let mask: Vec<u8> = MAGIC.iter().chain(transaction.iter()).copied().collect();
        let mut v6 = vec![0, 2, (port >> 8) as u8, port as u8];
        v6.extend(ip.octets().iter().zip(mask).map(|(a, m)| a ^ m));
        let got = parse_response(
            &response(transaction, XOR_MAPPED_ADDRESS, &v6),
            &transaction,
        );
        assert_eq!(got, Some(SocketAddr::new(IpAddr::V6(ip), 54321)));
        // The legacy MAPPED-ADDRESS attribute is accepted as a fallback.
        let plain = [0, 1, 0x30, 0x39, 198, 51, 100, 7];
        let got = parse_response(&response(transaction, MAPPED_ADDRESS, &plain), &transaction);
        assert_eq!(got, Some("198.51.100.7:12345".parse().unwrap()));
    }

    #[test]
    fn change_requests_and_rfc5780_addresses() {
        let packet = request([3; 12], CHANGE_IP | CHANGE_PORT);
        assert_eq!(packet.len(), 28);
        assert_eq!(&packet[2..4], &[0, 8]);
        assert_eq!(&packet[20..], &[0, 3, 0, 4, 0, 0, 0, 6]);
        let transaction = [4; 12];
        let mut body = Vec::new();
        for (kind, value) in [
            (MAPPED_ADDRESS, [0, 1, 0x30, 0x39, 198, 51, 100, 7]),
            (OTHER_ADDRESS, [0, 1, 0x0d, 0x97, 203, 0, 113, 2]),
        ] {
            body.extend_from_slice(&kind.to_be_bytes());
            body.extend_from_slice(&8u16.to_be_bytes());
            body.extend_from_slice(&value);
        }
        let mut packet = Vec::new();
        packet.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        packet.extend_from_slice(&(body.len() as u16).to_be_bytes());
        packet.extend_from_slice(&MAGIC);
        packet.extend_from_slice(&transaction);
        packet.extend(body);
        assert_eq!(
            parse(&packet, &transaction),
            Some(Response::Mapped {
                mapped: "198.51.100.7:12345".parse().unwrap(),
                other: Some("203.0.113.2:3479".parse().unwrap()),
            })
        );
        // RFC 3489 servers name the same attribute CHANGED-ADDRESS.
        packet[20 + 12..20 + 14].copy_from_slice(&CHANGED_ADDRESS.to_be_bytes());
        assert!(matches!(
            parse(&packet, &transaction),
            Some(Response::Mapped { other: Some(_), .. })
        ));
        let mut error = response(transaction, 0x0009, &[0, 0, 4, 20]);
        error[1] = 0x11;
        assert_eq!(parse(&error, &transaction), Some(Response::Error));
        assert_eq!(parse(&error, &[5; 12]), None);
    }

    #[test]
    fn foreign_or_damaged_responses_are_ignored() {
        let transaction = [9; 12];
        let plain = [0, 1, 0x30, 0x39, 198, 51, 100, 7];
        let good = response(transaction, MAPPED_ADDRESS, &plain);
        assert!(
            parse_response(&good, &[1; 12]).is_none(),
            "other transaction"
        );
        assert!(parse_response(&good[..19], &transaction).is_none());
        let mut wrong_type = good.clone();
        wrong_type[1] = 0x11;
        assert!(parse_response(&wrong_type, &transaction).is_none());
        let mut wrong_magic = good.clone();
        wrong_magic[4] = 0;
        assert!(parse_response(&wrong_magic, &transaction).is_none());
        let mut lying_length = good.clone();
        lying_length[3] = 200;
        assert!(parse_response(&lying_length, &transaction).is_none());
        assert!(
            parse_response(
                &response(transaction, MAPPED_ADDRESS, &[0, 1, 0, 0, 1, 2, 3, 4]),
                &transaction
            )
            .is_none(),
            "port 0"
        );
        assert!(
            parse_response(&response(transaction, 0x8022, b"software"), &transaction).is_none()
        );
    }
}
