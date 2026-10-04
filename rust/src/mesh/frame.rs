//! Data-channel framing from docs/P2P.md: binary frames are
//! `[u32 big-endian header length][JSON header][payload]`, the whole frame at
//! most 256 KiB and the header at most 32 KiB. Text frames carry ping/pong.
use serde_json::Value;

pub const MAX_FRAME: usize = 256 * 1024;
pub const MAX_HEADER: usize = 32 * 1024;

/// Splits a request frame. Anything outside the limits or not a JSON object is
/// refused rather than repaired.
pub fn decode(data: &[u8]) -> Result<(Value, &[u8]), &'static str> {
    if data.len() < 4 || data.len() > MAX_FRAME {
        return Err("bad_frame");
    }
    let header_len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if header_len == 0 || header_len > MAX_HEADER || data.len() < 4 + header_len {
        return Err("bad_frame");
    }
    let header: Value =
        serde_json::from_slice(&data[4..4 + header_len]).map_err(|_| "bad_header")?;
    if !header.is_object() {
        return Err("bad_header");
    }
    Ok((header, &data[4 + header_len..]))
}

/// Builds a frame. `None` when it would exceed the size limits.
pub fn encode(header: &Value, payload: &[u8]) -> Option<Vec<u8>> {
    let header = serde_json::to_vec(header).ok()?;
    if header.len() > MAX_HEADER || 4 + header.len() + payload.len() > MAX_FRAME {
        return None;
    }
    let mut out = Vec::with_capacity(4 + header.len() + payload.len());
    out.extend_from_slice(&(header.len() as u32).to_be_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(payload);
    Some(out)
}

/// The reply to a text ping: `{"t":"pong","n":<same>}`. Anything else is ignored.
pub fn pong(text: &str) -> Option<String> {
    let value: Value = serde_json::from_str(text).ok()?;
    if value.get("t")?.as_str()? != "ping" {
        return None;
    }
    let n = value.get("n")?;
    n.is_number()
        .then(|| serde_json::json!({"t":"pong","n":n}).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frames_round_trip_and_keep_the_payload_untouched() {
        let frame = encode(&json!({"t":"http","id":7}), b"\x00\xffbody").unwrap();
        let (header, payload) = decode(&frame).unwrap();
        assert_eq!(header["id"], 7);
        assert_eq!(payload, b"\x00\xffbody");
        let empty = encode(&json!({"t":"x"}), b"").unwrap();
        let (header, payload) = decode(&empty).unwrap();
        assert_eq!(header["t"], "x");
        assert!(payload.is_empty());
    }

    #[test]
    fn malformed_and_oversized_frames_are_refused() {
        assert!(decode(b"").is_err());
        assert!(decode(&[0, 0, 0]).is_err());
        assert!(decode(&[0, 0, 0, 0, b'{', b'}']).is_err(), "empty header");
        assert!(
            decode(&[0, 0, 0, 9, b'{', b'}']).is_err(),
            "header longer than frame"
        );
        let not_object = [&3u32.to_be_bytes()[..], b"[1]"].concat();
        assert!(decode(&not_object).is_err());
        let not_json = [&3u32.to_be_bytes()[..], b"abc"].concat();
        assert!(decode(&not_json).is_err());
        let huge_header = [
            &((MAX_HEADER + 1) as u32).to_be_bytes()[..],
            &vec![b' '; MAX_HEADER + 1],
        ]
        .concat();
        assert!(decode(&huge_header).is_err());
        let big = vec![0u8; MAX_FRAME];
        assert!(decode(&big).is_err());
        assert!(encode(&json!({"t":"x"}), &big).is_none());
        assert!(encode(&json!({"t":"x"}), &vec![0u8; MAX_FRAME - 40]).is_some());
    }

    #[test]
    fn only_numbered_pings_get_a_pong() {
        assert_eq!(
            pong(r#"{"t":"ping","n":42}"#).unwrap(),
            r#"{"n":42,"t":"pong"}"#
        );
        assert!(pong(r#"{"t":"ping"}"#).is_none());
        assert!(pong(r#"{"t":"ping","n":"x"}"#).is_none());
        assert!(pong(r#"{"t":"pong","n":1}"#).is_none());
        assert!(pong("nope").is_none());
    }
}
