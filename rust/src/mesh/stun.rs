//! A minimal STUN binding client (RFC 5389): enough to learn this socket's
//! server-reflexive address. The probe runs on the ICE socket itself, so the
//! mapping it finds is the one the peer will actually reach.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

const MAGIC: [u8; 4] = [0x21, 0x12, 0xa4, 0x42];
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;
const MAPPED_ADDRESS: u16 = 0x0001;

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

pub fn binding_request(transaction: [u8; 12]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(20);
    packet.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
    packet.extend_from_slice(&0u16.to_be_bytes());
    packet.extend_from_slice(&MAGIC);
    packet.extend_from_slice(&transaction);
    packet
}

/// The mapped address of a binding success response for `transaction`.
pub fn parse_response(packet: &[u8], transaction: &[u8; 12]) -> Option<SocketAddr> {
    if packet.len() < 20
        || u16::from_be_bytes([packet[0], packet[1]]) != BINDING_SUCCESS
        || packet[4..8] != MAGIC
        || packet[8..20] != transaction[..]
    {
        return None;
    }
    let length = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    let body = packet.get(20..20 + length)?;
    let mut offset = 0;
    let mut plain = None;
    while offset + 4 <= body.len() {
        let kind = u16::from_be_bytes([body[offset], body[offset + 1]]);
        let size = usize::from(u16::from_be_bytes([body[offset + 2], body[offset + 3]]));
        let value = body.get(offset + 4..offset + 4 + size)?;
        match kind {
            XOR_MAPPED_ADDRESS => return decode_address(value, true, transaction),
            MAPPED_ADDRESS => plain = decode_address(value, false, transaction),
            _ => {}
        }
        offset += 4 + size.div_ceil(4) * 4;
    }
    plain
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
