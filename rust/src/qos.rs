use serde::Serialize;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Default, Debug, PartialEq, Serialize)]
pub struct Values {
    pub qci: i64,
    pub ambr_dl: String,
    pub ambr_ul: String,
}

#[derive(Clone, Default, Debug)]
struct Candidate {
    current: bool,
    mcc: Option<i64>,
    mnc: Option<i64>,
    rank: i32,
    seq: usize,
    qci: Option<i64>,
    dl: Option<f64>,
    ul: Option<f64>,
}

pub fn read_for_plmn(mcc: i64, mnc: i64) -> Values {
    let current =
        std::env::var("ZWRT_DATAD_KEY_LOG").unwrap_or_else(|_| "/data/logfs/key.log".into());
    let rotated = std::env::var("ZWRT_DATAD_KEY_LOG_ROTATED")
        .unwrap_or_else(|_| "/data/logfs/key.log.0".into());
    parse_logs(
        &[(rotated.as_str(), false), (current.as_str(), true)],
        mcc,
        mnc,
    )
}

fn read_tail(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let start = size.saturating_sub(MAX_LOG_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::with_capacity((size - start) as usize);
    file.take(MAX_LOG_BYTES).read_to_end(&mut bytes).ok()?;
    if start > 0 {
        if let Some(pos) = bytes.iter().position(|b| *b == b'\n') {
            bytes.drain(..=pos);
        }
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn parse_logs(paths: &[(&str, bool)], mcc: i64, mnc: i64) -> Values {
    let mut candidates = Vec::<Candidate>::new();
    let mut fallback = Candidate::default();
    let mut seq = 0usize;
    for (path, current) in paths {
        let Some(text) = read_tail(Path::new(path)) else {
            continue;
        };
        let mut context: Option<usize> = None;
        let mut context_left: u8 = 0;
        let mut pending_qci = None;
        let mut pending_left: u8 = 0;
        for line in text.lines().filter(|line| line.contains("[DATA]")) {
            seq += 1;
            let lower = line.to_ascii_lowercase();
            let data_context = !["dnn=ims", "dnn=sos", "dnn=emergency", "access_point=ims"]
                .iter()
                .any(|v| lower.contains(v))
                && (lower.contains("dnn=") || lower.contains("access_point="));
            let mut line_candidate = None;
            if data_context {
                let cmcc = digits_after_ci(&lower, "mcc");
                let cmnc = digits_after_ci(&lower, "mnc");
                let has_plmn = cmcc.is_some() && cmnc.is_some();
                let rank = if has_plmn {
                    30
                } else if lower.contains("access_point=") {
                    20
                } else {
                    10
                };
                let idx = candidates
                    .iter()
                    .position(|c| c.mcc == cmcc && c.mnc == cmnc)
                    .unwrap_or_else(|| {
                        candidates.push(Candidate {
                            current: *current,
                            mcc: cmcc,
                            mnc: cmnc,
                            rank,
                            seq,
                            ..Default::default()
                        });
                        candidates.len() - 1
                    });
                let c = &mut candidates[idx];
                c.current |= *current;
                c.rank = c.rank.max(rank);
                c.seq = seq;
                if let Some(qci) = pending_qci.take() {
                    c.qci = Some(qci);
                }
                context = Some(idx);
                context_left = 4;
                pending_left = 0;
                line_candidate = Some(idx);
            }
            if lower.contains("qci") {
                if let Some(qci) = integer_after_ci(&lower, "qci") {
                    if let Some(idx) = context.filter(|_| context_left > 0) {
                        candidates[idx].qci = Some(qci);
                    } else if lower.contains("default bearer qci") {
                        pending_qci = Some(qci);
                        pending_left = 4;
                        fallback.qci.get_or_insert(qci);
                    } else {
                        fallback.qci.get_or_insert(qci);
                    }
                }
            }
            if let Some(idx) = line_candidate {
                if lower.contains("session_ambr") {
                    candidates[idx].dl =
                        session_ambr(&lower, "session_ambr_dl=", "session_ambr_dl_unit=");
                    candidates[idx].ul =
                        session_ambr(&lower, "session_ambr_ul=", "session_ambr_ul_unit=");
                } else if lower.contains("apn_ambr") {
                    candidates[idx].dl = apn_ambr(&lower, "apn_ambr_dl");
                    candidates[idx].ul = apn_ambr(&lower, "apn_ambr_ul");
                }
            }
            context_left = context_left.saturating_sub(1);
            if context_left == 0 {
                context = None;
            }
            pending_left = pending_left.saturating_sub(1);
            if pending_left == 0 {
                pending_qci = None;
            }
        }
    }
    let score = |c: &Candidate| {
        let base = if c.current { 1000 } else { 0 };
        base + if c.mcc == Some(mcc) && c.mnc == Some(mnc) {
            300 + c.rank
        } else if c.mcc.is_none() {
            100 + c.rank
        } else {
            10 + c.rank
        }
    };
    candidates.sort_by_key(|c| (score(c), c.seq));
    let mut selected = candidates.last().cloned().unwrap_or_default();
    for c in candidates.iter().rev() {
        if selected.qci.is_none() {
            selected.qci = c.qci;
        }
        if selected.dl.is_none() {
            selected.dl = c.dl;
        }
        if selected.ul.is_none() {
            selected.ul = c.ul;
        }
    }
    Values {
        qci: selected.qci.or(fallback.qci).unwrap_or(0),
        ambr_dl: selected.dl.map(format_mbps).unwrap_or_default(),
        ambr_ul: selected.ul.map(format_mbps).unwrap_or_default(),
    }
}

fn digits_after_ci(s: &str, tag: &str) -> Option<i64> {
    let p = s.find(tag)? + tag.len();
    let digits: String = s[p..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .take(6)
        .collect();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}
fn integer_after_ci(s: &str, tag: &str) -> Option<i64> {
    let p = s.find(tag)? + tag.len();
    let tail = s[p..].trim_start_matches(|c: char| !c.is_ascii_digit());
    let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}
fn number_after(s: &str, key: &str) -> Option<f64> {
    let p = s.find(key)? + key.len();
    let token: String = s[p..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    token.parse().ok()
}
fn apn_ambr(s: &str, key: &str) -> Option<f64> {
    number_after(s, &format!("{key}_ext2="))
        .or_else(|| number_after(s, &format!("{key}_ext=")))
        .or_else(|| number_after(s, &format!("{key}=")).map(|v| v / 1000.0))
}
fn session_ambr(s: &str, value_key: &str, unit_key: &str) -> Option<f64> {
    let value = number_after(s, value_key)?;
    let p = s.find(unit_key)? + unit_key.len();
    let unit = &s[p..];
    let open = unit.find('(')? + 1;
    let scale_token: String = unit[open..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut scale: f64 = scale_token.parse().ok()?;
    let suffix = unit[open + scale_token.len()..].to_ascii_lowercase();
    if suffix.contains("gbps") {
        scale *= 1000.0;
    } else if suffix.contains("mbps") {
    } else if suffix.contains("kbps") {
        scale /= 1000.0;
    } else if suffix.contains("bps") {
        scale /= 1_000_000.0;
    } else {
        return None;
    }
    Some(value * scale)
}
fn format_mbps(v: f64) -> String {
    format!("{v:.3}")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_helpers() {
        assert_eq!(integer_after_ci("default bearer qci = 9", "qci"), Some(9));
        assert_eq!(apn_ambr("apn_ambr_dl=64000", "apn_ambr_dl"), Some(64.0));
        assert_eq!(
            session_ambr(
                "session_ambr_dl=2 session_ambr_dl_unit=(100Mbps)",
                "session_ambr_dl=",
                "session_ambr_dl_unit="
            ),
            Some(200.0)
        );
    }
}
