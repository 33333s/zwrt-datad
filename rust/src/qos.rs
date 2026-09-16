use serde::Serialize;
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Stamp {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
}

#[derive(Default, Debug)]
struct LogCache {
    path: PathBuf,
    stamp: Option<Stamp>,
    offset: u64,
    head_guard: Vec<u8>,
    tail_guard: Vec<u8>,
    partial: Vec<u8>,
    data_lines: String,
}

fn update_head_guard(guard: &mut Vec<u8>, bytes: &[u8]) {
    const GUARD_BYTES: usize = 64;
    let remaining = GUARD_BYTES.saturating_sub(guard.len());
    guard.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
}

fn update_tail_guard(guard: &mut Vec<u8>, bytes: &[u8]) {
    const GUARD_BYTES: usize = 64;
    if bytes.len() >= GUARD_BYTES {
        guard.clear();
        guard.extend_from_slice(&bytes[bytes.len() - GUARD_BYTES..]);
        return;
    }
    if guard.len() + bytes.len() > GUARD_BYTES {
        guard.drain(..guard.len() + bytes.len() - GUARD_BYTES);
    }
    guard.extend_from_slice(bytes);
}

#[derive(Default, Debug)]
struct Cache {
    current: LogCache,
    rotated: LogCache,
    initialized: bool,
}

static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

pub fn read_for_plmn(mcc: i64, mnc: i64) -> Values {
    let current = std::env::var("ZWRT_DATAD_QOS_LOG")
        .or_else(|_| std::env::var("ZWRT_DATAD_KEY_LOG"))
        .unwrap_or_else(|_| "/data/logfs/key.log".into());
    let rotated = std::env::var("ZWRT_DATAD_QOS_LOG_ROTATED")
        .or_else(|_| std::env::var("ZWRT_DATAD_KEY_LOG_ROTATED"))
        .unwrap_or_else(|_| "/data/logfs/key.log.0".into());
    let cache = CACHE.get_or_init(|| Mutex::new(Cache::default()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.refresh(Path::new(&rotated), Path::new(&current));
    parse_texts(
        &[
            (cache.rotated.data_lines.as_str(), false),
            (cache.current.data_lines.as_str(), true),
        ],
        mcc,
        mnc,
    )
}

pub fn invalidate() {
    if let Some(cache) = CACHE.get() {
        *cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Cache::default();
    }
}

pub fn parse_external(text: &str) -> Option<Values> {
    let mut selected = None;
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if !line.contains("[DATA]") || !lower.contains("cid1") {
            continue;
        }
        let qci = integer_after_ci(&lower, "qci=")?;
        let dl = integer_after_ci(&lower, "dl_ambr=")?;
        let ul = integer_after_ci(&lower, "ul_ambr=")?;
        if !(1..=255).contains(&qci) || dl < 0 || ul < 0 {
            continue;
        }
        selected = Some(Values {
            qci,
            ambr_dl: format_mbps(dl as f64 / 1000.0),
            ambr_ul: format_mbps(ul as f64 / 1000.0),
        });
    }
    selected
}

fn stamp(path: &Path) -> Option<Stamp> {
    let metadata = path.metadata().ok()?;
    Some(Stamp {
        dev: metadata.dev(),
        ino: metadata.ino(),
        len: metadata.len(),
        mtime: metadata.mtime(),
        mtime_nsec: metadata.mtime_nsec(),
    })
}

fn push_complete_lines(cache: &mut LogCache, bytes: &[u8]) {
    let mut combined = std::mem::take(&mut cache.partial);
    combined.extend_from_slice(bytes);
    let complete = combined
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|position| position + 1)
        .unwrap_or(0);
    for line in combined[..complete].split_inclusive(|byte| *byte == b'\n') {
        if line
            .windows(b"[DATA]".len())
            .any(|window| window == b"[DATA]")
        {
            cache.data_lines.push_str(&String::from_utf8_lossy(line));
        }
    }
    cache.partial.extend_from_slice(&combined[complete..]);
}

impl LogCache {
    fn load(path: &Path) -> Self {
        let mut cache = Self {
            path: path.to_path_buf(),
            ..Default::default()
        };
        let Ok(file) = File::open(path) else {
            return cache;
        };
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        loop {
            line.clear();
            let Ok(read) = reader.read_until(b'\n', &mut line) else {
                break;
            };
            if read == 0 {
                break;
            }
            update_head_guard(&mut cache.head_guard, &line);
            update_tail_guard(&mut cache.tail_guard, &line);
            if line.last() == Some(&b'\n') {
                if line
                    .windows(b"[DATA]".len())
                    .any(|window| window == b"[DATA]")
                {
                    cache.data_lines.push_str(&String::from_utf8_lossy(&line));
                }
            } else {
                cache.partial.extend_from_slice(&line);
            }
        }
        cache.stamp = stamp(path);
        cache.offset = cache.stamp.map(|value| value.len).unwrap_or(0);
        cache
    }

    fn append(&mut self, next: Stamp) -> bool {
        if self.path.as_os_str().is_empty()
            || self
                .stamp
                .is_some_and(|previous| previous.dev != next.dev || previous.ino != next.ino)
            || next.len < self.offset
        {
            return false;
        }
        let Ok(mut file) = File::open(&self.path) else {
            return false;
        };
        if !self.head_guard.is_empty() {
            let mut actual = vec![0; self.head_guard.len()];
            if file.read_exact(&mut actual).is_err() || actual != self.head_guard {
                return false;
            }
        }
        if !self.tail_guard.is_empty() {
            if file
                .seek(SeekFrom::Start(
                    self.offset.saturating_sub(self.tail_guard.len() as u64),
                ))
                .is_err()
            {
                return false;
            }
            let mut actual = vec![0; self.tail_guard.len()];
            if file.read_exact(&mut actual).is_err() || actual != self.tail_guard {
                return false;
            }
        }
        if next.len == self.offset {
            self.stamp = Some(next);
            return true;
        }
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return false;
        }
        let mut bytes = Vec::with_capacity((next.len - self.offset) as usize);
        if file.read_to_end(&mut bytes).is_err() {
            return false;
        }
        push_complete_lines(self, &bytes);
        update_head_guard(&mut self.head_guard, &bytes);
        update_tail_guard(&mut self.tail_guard, &bytes);
        self.offset += bytes.len() as u64;
        self.stamp = stamp(&self.path).or(Some(next));
        true
    }
}

impl Cache {
    fn reload(&mut self, rotated: &Path, current: &Path) {
        self.rotated = LogCache::load(rotated);
        self.current = LogCache::load(current);
        self.initialized = true;
    }

    fn refresh(&mut self, rotated: &Path, current: &Path) {
        if !self.initialized || self.rotated.path != rotated || self.current.path != current {
            self.reload(rotated, current);
            return;
        }
        let rotated_stamp = stamp(rotated);
        let current_stamp = stamp(current);
        let rotated_changed = self.rotated.stamp != rotated_stamp;
        let current_replaced = match (self.current.stamp, current_stamp) {
            (Some(previous), Some(next)) => {
                previous.dev != next.dev
                    || previous.ino != next.ino
                    || next.len < self.current.offset
            }
            (None, None) => false,
            _ => true,
        };
        if rotated_changed || current_replaced {
            self.reload(rotated, current);
            return;
        }
        if let Some(next) = current_stamp
            && !self.current.append(next)
        {
            self.reload(rotated, current);
        }
    }
}

fn parse_texts(texts: &[(&str, bool)], mcc: i64, mnc: i64) -> Values {
    let mut candidates = Vec::<Candidate>::new();
    let mut fallback = Candidate::default();
    let mut seq = 0usize;
    for (text, current) in texts {
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
            if lower.contains("qci")
                && let Some(qci) = integer_after_ci(&lower, "qci")
            {
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
    use std::{fs, io::Write};

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "zwrt-datad-qos-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

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
        assert_eq!(
            parse_external("[DATA] cid1, QCI=[9], DL_AMBR=[150000]kbps, UL_AMBR=[75000]kbps"),
            Some(Values {
                qci: 9,
                ambr_dl: "150.000".into(),
                ambr_ul: "75.000".into()
            })
        );
    }

    #[test]
    fn cache_reads_complete_logs_and_tracks_append_and_rotation() {
        let dir = temp_dir("rotation");
        let current = dir.join("key.log");
        let rotated = dir.join("key.log.0");
        let padding = "not data\n".repeat(300_000);
        fs::write(
            &rotated,
            format!(
                "[DATA] cid1 LTE default bearer qci = 8\n[DATA] eps_bearer_id=5 access_point=CMHK.MNC012.MCC454.GPRS apn_ambr_dl_ext2=20008.641Mbps apn_ambr_ul_ext2=20008.641Mbps\n{padding}"
            ),
        )
        .unwrap();
        fs::write(&current, "ordinary log line\n").unwrap();

        let mut cache = Cache::default();
        cache.refresh(&rotated, &current);
        let first = parse_texts(
            &[
                (&cache.rotated.data_lines, false),
                (&cache.current.data_lines, true),
            ],
            454,
            12,
        );
        assert_eq!(first.qci, 8);
        assert_eq!(first.ambr_dl, "20008.641");

        let mut file = fs::OpenOptions::new().append(true).open(&current).unwrap();
        file.write_all(b"[DATA] cid1 LTE default bearer qci = 9\n")
            .unwrap();
        file.write_all(b"[DATA] eps_bearer_id=6 access_point=CMHK.MNC012.MCC454.GPRS apn_ambr_dl_ext=64.000Mbps apn_ambr_ul_ext=32.000Mbps\n").unwrap();
        file.flush().unwrap();
        cache.refresh(&rotated, &current);
        let appended = parse_texts(
            &[
                (&cache.rotated.data_lines, false),
                (&cache.current.data_lines, true),
            ],
            454,
            12,
        );
        assert_eq!(appended.qci, 9);
        assert_eq!(appended.ambr_dl, "64.000");

        fs::rename(&current, &rotated).unwrap();
        fs::write(
            &current,
            "[DATA] cid1 LTE default bearer qci = 7\n[DATA] eps_bearer_id=7 access_point=CMHK.MNC012.MCC454.GPRS apn_ambr_dl_ext=32.000Mbps apn_ambr_ul_ext=16.000Mbps\n",
        )
        .unwrap();
        cache.refresh(&rotated, &current);
        let after_rotation = parse_texts(
            &[
                (&cache.rotated.data_lines, false),
                (&cache.current.data_lines, true),
            ],
            454,
            12,
        );
        assert_eq!(after_rotation.qci, 7);
        assert_eq!(after_rotation.ambr_dl, "32.000");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cache_handles_partial_lines_and_truncation() {
        let dir = temp_dir("partial");
        let current = dir.join("key.log");
        let rotated = dir.join("key.log.0");
        fs::write(&rotated, "").unwrap();
        fs::write(&current, "[DATA] cid1 LTE default bearer qci = ").unwrap();
        let mut cache = Cache::default();
        cache.refresh(&rotated, &current);
        assert!(cache.current.data_lines.is_empty());

        let mut file = fs::OpenOptions::new().append(true).open(&current).unwrap();
        file.write_all(b"9\n[DATA] dnn=internet session_ambr_dl=2 session_ambr_dl_unit=1(100Mbps) session_ambr_ul=1 session_ambr_ul_unit=1(100Mbps)\n").unwrap();
        file.flush().unwrap();
        cache.refresh(&rotated, &current);
        let appended = parse_texts(&[(&cache.current.data_lines, true)], 0, 0);
        assert_eq!(appended.qci, 9);
        assert_eq!(appended.ambr_dl, "200.000");

        fs::write(
            &current,
            "[DATA] cid1 LTE default bearer qci = 6\n[DATA] dnn=internet session_ambr_dl=1 session_ambr_dl_unit=1(100Mbps) session_ambr_ul=1 session_ambr_ul_unit=1(100Mbps)\n",
        )
        .unwrap();
        cache.refresh(&rotated, &current);
        let truncated = parse_texts(&[(&cache.current.data_lines, true)], 0, 0);
        assert_eq!(truncated.qci, 6);
        fs::remove_dir_all(dir).unwrap();
    }
}
