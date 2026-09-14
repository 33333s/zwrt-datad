use serde::Serialize;
use std::{
    collections::{HashMap, VecDeque},
    fs::OpenOptions,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

const RECORDS: usize = 4096;
const FRAME_MAX: usize = 65536;
const ANCHOR_FRAMES: u64 = 256;
const ANCHOR_MS: i64 = 5000;
const CELL_SIGNALS: usize = 64;
const INPUT_MAX: u64 = 32 * 1024 * 1024;
const TTL_MS: i64 = 60000;
const MAX_CELLS: usize = 128;

#[derive(Clone)]
struct Direct {
    seq: u64,
    capture: u64,
    at: i64,
    pci: u32,
    arfcn: u32,
    band: Option<u32>,
}
#[derive(Clone)]
struct Sample {
    seq: u64,
    capture: u64,
    at: i64,
    rat: Rat,
    pci: u32,
    arfcn: Option<u32>,
    band: Option<u32>,
    rsrp: Option<f64>,
    require_anchor: bool,
}
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, PartialOrd, Ord)]
enum Rat {
    #[serde(rename = "NR")]
    Nr,
    #[serde(rename = "LTE")]
    Lte,
}
#[derive(Serialize, Clone)]
struct Cell {
    rat: Rat,
    pci: u32,
    arfcn: Option<u32>,
    band: Option<u32>,
    rsrp_dbm: Option<f64>,
    samples: u32,
    direct_hits: u32,
}
#[derive(Serialize)]
struct Output {
    source: &'static str,
    frames: u64,
    malformed: u64,
    discarded: u64,
    ambiguous: u64,
    partial: bool,
    cells: Vec<Cell>,
}
#[derive(Default)]
struct Parser {
    directs: VecDeque<Direct>,
    samples: VecDeque<Sample>,
    frames: u64,
    malformed: u64,
    discarded: u64,
    ambiguous: u64,
    seq: u64,
    malformed_at: i64,
    discarded_at: i64,
    now: i64,
    capture: Option<u64>,
    frame: Vec<u8>,
    escaped: bool,
    dropping: bool,
}

impl Parser {
    fn push_direct(&mut self, value: Direct) {
        if self.directs.len() == RECORDS {
            if self.now - self.directs[0].at <= TTL_MS {
                self.discarded += 1;
                self.discarded_at = self.now;
            }
            self.directs.pop_front();
        }
        self.directs.push_back(value);
    }
    fn push_sample(&mut self, value: Sample) {
        if self.samples.len() == RECORDS {
            if self.now - self.samples[0].at <= TTL_MS {
                self.discarded += 1;
                self.discarded_at = self.now;
            }
            self.samples.pop_front();
        }
        self.samples.push_back(value);
    }
    fn end_file(&mut self, capture: u64) {
        if self.capture != Some(capture) {
            return;
        }
        if !self.frame.is_empty() || self.escaped || self.dropping {
            self.malformed += 1;
            self.malformed_at = self.now;
        }
        self.frame.clear();
        self.escaped = false;
        self.dropping = false;
        self.capture = None;
    }
    fn feed(&mut self, input: &[u8], capture: u64, now: i64) {
        if let Some(old) = self.capture
            && old != capture
        {
            self.end_file(old);
        }
        self.capture = Some(capture);
        self.now = now;
        for &raw in input {
            if raw == 0x7e {
                if self.dropping
                    || self.escaped
                    || (!self.frame.is_empty() && (self.frame.len() < 3 || !valid_fcs(&self.frame)))
                {
                    self.malformed += 1;
                    self.malformed_at = now;
                } else if !self.frame.is_empty() {
                    self.frames += 1;
                    self.seq += 1;
                    let payload = self.frame[..self.frame.len() - 2].to_vec();
                    self.process(&payload, self.seq, capture);
                }
                self.frame.clear();
                self.escaped = false;
                self.dropping = false;
                continue;
            }
            if self.dropping {
                continue;
            }
            let byte = if self.escaped {
                self.escaped = false;
                raw ^ 0x20
            } else if raw == 0x7d {
                self.escaped = true;
                continue;
            } else {
                raw
            };
            if self.frame.len() == FRAME_MAX {
                self.dropping = true;
            } else {
                self.frame.push(byte);
            }
        }
    }
    fn process(&mut self, frame: &[u8], seq: u64, capture: u64) {
        if frame.len() < 16 || frame[0] != 0x9d {
            return;
        }
        let Some(n) = frame[4].checked_sub(0x13).map(usize::from) else {
            return;
        };
        if n > 236 || frame.len() < 16 + n * 4 {
            return;
        }
        let args: Vec<u32> = (0..n).map(|i| le32(&frame[16 + i * 4..])).collect();
        let hash = le32(&frame[12..]);
        if let Some((pci, arfcn, band)) = direct(hash, &args) {
            self.push_direct(Direct {
                seq,
                capture,
                at: self.now,
                pci,
                arfcn,
                band,
            });
        }
        let mut add = |rat, pci, arfcn, band, rsrp, require_anchor| {
            self.push_sample(Sample {
                seq,
                capture,
                at: self.now,
                rat,
                pci,
                arfcn,
                band,
                rsrp,
                require_anchor,
            });
        };
        let paired = |sizes: &[u32]| {
            sizes.contains(&hash)
                && args.len()
                    >= if matches!(hash, 0xda01af24 | 0xda06ba1c | 0xda054a0c) {
                        11
                    } else {
                        12
                    }
                && valid_pci(args[3])
                && valid_pci(args[5])
                && plausible_q7(args[4])
                && plausible_q7(args[6])
        };
        if paired(&[3657540452, 3657523352, 4182273084]) {
            add(Rat::Nr, args[3], None, None, Some(q7(args[4])), false);
        } else if paired(&[0xda0539fc, 0xda054a0c]) {
            add(Rat::Nr, args[3], None, None, Some(q7(args[4])), true);
        } else if paired(&[0xda019f14, 0xda01af24]) {
            add(
                Rat::Nr,
                args[3],
                None,
                None,
                Some(q7(args[4])),
                hash == 0xda01af24,
            );
        } else if paired(&[0xda06aa0c, 0xda06ba1c]) {
            add(
                Rat::Nr,
                args[3],
                None,
                None,
                Some(q7(args[4])),
                hash == 0xda06ba1c,
            );
        } else if [3657934788, 3657937792, 3657920232].contains(&hash)
            && args.len() == 12
            && args[4] != (-19968i32) as u32
            && args[6] != (-19968i32) as u32
            && valid_pci(args[3])
            && valid_pci(args[5])
            && plausible_q7(args[4])
            && plausible_q7(args[6])
        {
            add(Rat::Nr, args[3], None, None, Some(q7(args[4])), true);
        } else if hash == 3657646332
            && args.len() == 7
            && valid_arfcn(args[1])
            && valid_pci(args[2])
            && args[3] != (-19968i32) as u32
            && plausible_q7(args[3])
        {
            add(
                Rat::Nr,
                args[2],
                Some(args[1]),
                None,
                Some(q7(args[3])),
                false,
            );
        } else if hash == 3640546464 && args.len() >= 4 && args[1] <= 503 && args[0] <= 262143 {
            add(
                Rat::Lte,
                args[1],
                Some(args[0]),
                None,
                lte_rsrp(args[2]),
                false,
            );
        } else if hash == 3644228716 && args.len() >= 2 && args[1] <= 503 && args[0] <= 262143 {
            add(Rat::Lte, args[1], Some(args[0]), None, None, false);
        }
    }
    fn result(&mut self, now: i64, failed: bool) -> Output {
        #[derive(Default)]
        struct Group {
            values: VecDeque<f64>,
            samples: u32,
            hits: u32,
            band: Option<u32>,
            conflict: bool,
        }
        let recent = |at| now >= at && now - at <= TTL_MS;
        let mut groups: HashMap<(Rat, u32, Option<u32>), Group> = HashMap::new();
        let mut group_overflow = false;
        for sample in &self.samples {
            if !recent(sample.at) {
                continue;
            }
            let mut arfcn = sample.arfcn;
            if sample.rat == Rat::Nr && arfcn.is_none() {
                let mut found = None;
                let mut comparisons = 0;
                let mut ambiguous = false;
                for d in self
                    .directs
                    .iter()
                    .filter(|d| d.pci == sample.pci && d.capture == sample.capture)
                {
                    if d.seq.abs_diff(sample.seq) > ANCHOR_FRAMES
                        || (d.at - sample.at).abs() > ANCHOR_MS
                    {
                        continue;
                    }
                    comparisons += 1;
                    if comparisons > 512 {
                        ambiguous = true;
                        break;
                    }
                    if found.is_some() && found != Some(d.arfcn) {
                        ambiguous = true;
                    }
                    found = Some(d.arfcn);
                }
                if ambiguous {
                    self.ambiguous += 1;
                    arfcn = None;
                } else {
                    arfcn = found;
                }
                if sample.require_anchor && (found.is_none() || ambiguous) {
                    continue;
                }
            }
            if !groups.contains_key(&(sample.rat, sample.pci, arfcn)) && groups.len() == MAX_CELLS {
                group_overflow = true;
                continue;
            }
            let g = groups.entry((sample.rat, sample.pci, arfcn)).or_default();
            g.samples += 1;
            if let Some(v) = sample.rsrp {
                if g.values.len() == CELL_SIGNALS {
                    g.values.pop_front();
                }
                g.values.push_back(v);
            }
            if let Some(b) = sample.band {
                if g.band.is_some() && g.band != Some(b) {
                    g.conflict = true;
                }
                g.band = Some(b);
            }
        }
        for d in &self.directs {
            if !recent(d.at) {
                continue;
            }
            let key = (Rat::Nr, d.pci, Some(d.arfcn));
            if !groups.contains_key(&key) && groups.len() == MAX_CELLS {
                group_overflow = true;
                continue;
            }
            let g = groups.entry(key).or_default();
            g.hits += 1;
            if let Some(b) = d.band {
                if g.band.is_some() && g.band != Some(b) {
                    g.conflict = true;
                }
                g.band = Some(b);
            }
        }
        let mut cells = Vec::new();
        for ((rat, pci, arfcn), mut g) in groups {
            let rsrp = if g.values.is_empty() {
                None
            } else {
                let mut v: Vec<_> = g.values.drain(..).collect();
                v.sort_by(f64::total_cmp);
                let m = v.len() / 2;
                Some(if v.len() % 2 == 1 {
                    v[m]
                } else {
                    round2((v[m - 1] + v[m]) / 2.0)
                })
            };
            cells.push(Cell {
                rat,
                pci,
                arfcn,
                band: if g.conflict { None } else { g.band },
                rsrp_dbm: rsrp,
                samples: g.samples,
                direct_hits: g.hits,
            });
        }
        cells.sort_by(|a, b| {
            b.rsrp_dbm
                .is_some()
                .cmp(&a.rsrp_dbm.is_some())
                .then_with(|| match (a.rsrp_dbm, b.rsrp_dbm) {
                    (Some(x), Some(y)) => y.total_cmp(&x),
                    _ => std::cmp::Ordering::Equal,
                })
                .then(a.rat.cmp(&b.rat))
                .then(a.arfcn.cmp(&b.arfcn))
                .then(a.pci.cmp(&b.pci))
        });
        Output {
            source: "qtrace",
            frames: self.frames,
            malformed: self.malformed,
            discarded: self.discarded,
            ambiguous: self.ambiguous,
            partial: failed
                || group_overflow
                || (self.malformed > 0 && recent(self.malformed_at))
                || (self.discarded > 0 && recent(self.discarded_at)),
            cells,
        }
    }
}

fn direct(hash: u32, a: &[u32]) -> Option<(u32, u32, Option<u32>)> {
    let (pci, arfcn, band) = match hash {
        3640397572 if a.len() >= 4 => (a[2], a[1], Some(a[0])),
        3657515396 if a.len() >= 6 => (a[2], a[4], None),
        3657212968 | 4181705066 if a.len() >= 7 => (a[2], a[4], None),
        3657202244 | 4181706276 if a.len() >= 6 => (a[1], a[3], None),
        4188869441 if a.len() >= 5 => (a[1], a[0], None),
        3166370540 if a.len() >= 3 => (a[1], a[0], None),
        0xd8facf74 | 0xd8f773e8 | 0xd8fc5cc0 | 0xd8fb8ad0 | 0xd8f82d98 | 0xd8f84514
            if a.len() >= 4 =>
        {
            (a[2], a[1], Some(a[0]))
        }
        0xd8f72994 | 0xd8f7e394 if a.len() >= 4 => (a[0], a[1], None),
        0xd9877e9c if a.len() >= 5 => (a[0], a[1], None),
        3640387444 | 3640166840 | 3640172852 if a.len() == 4 => (a[2], a[1], Some(a[0])),
        _ => return None,
    };
    (valid_pci(pci) && valid_arfcn(arfcn) && band.is_none_or(|b| b > 0 && b <= 1024))
        .then_some((pci, arfcn, band))
}
fn le32(p: &[u8]) -> u32 {
    u32::from_le_bytes(p[..4].try_into().unwrap())
}
fn valid_pci(n: u32) -> bool {
    n <= 1007
}
fn valid_arfcn(n: u32) -> bool {
    n > 0 && n <= 3279165
}
fn round2(n: f64) -> f64 {
    (n * 100.0 + 0.5).floor() / 100.0
}
fn q7(n: u32) -> f64 {
    round2((n as i32) as f64 / 128.0)
}
fn plausible_q7(n: u32) -> bool {
    let d = (n as i32) as f64 / 128.0;
    (-160.0..=-20.0).contains(&d)
}
fn lte_rsrp(raw: u32) -> Option<f64> {
    let n = raw as i32;
    if (-140..=-30).contains(&n) {
        Some(n as f64)
    } else if (-1400..=-300).contains(&n) {
        Some(round2(n as f64 / 10.0))
    } else {
        None
    }
}
fn valid_fcs(data: &[u8]) -> bool {
    let mut crc = 0xffffu16;
    for &b in data {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { 0x8408 } else { 0 };
        }
    }
    crc == 0xf0b8
}

pub fn parse_cli(paths: &[String]) -> i32 {
    if paths.is_empty() || paths.len() > 32 {
        eprintln!("usage: zwrt-datad --neighbor-parse FILE.qmdl [FILE.qmdl ...]");
        return 64;
    }
    let mut parser = Parser::default();
    let mut total = 0u64;
    let mut failed = false;
    for (i, name) in paths.iter().enumerate() {
        let path = Path::new(name);
        let meta = match path.symlink_metadata() {
            Ok(v) => v,
            Err(_) => {
                eprintln!("cannot read bounded regular input: {name}");
                failed = true;
                continue;
            }
        };
        if !meta.file_type().is_file()
            || meta.len() > INPUT_MAX
            || total.saturating_add(meta.len()) > INPUT_MAX
        {
            eprintln!("cannot read bounded regular input: {name}");
            failed = true;
            continue;
        }
        total += meta.len();
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        {
            Ok(v) => v,
            Err(_) => {
                failed = true;
                continue;
            }
        };
        let mut remain = meta.len();
        let mut buf = [0u8; 65536];
        while remain > 0 {
            let want = usize::try_from(remain.min(buf.len() as u64)).unwrap();
            match file.read(&mut buf[..want]) {
                Ok(0) | Err(_) => {
                    failed = true;
                    break;
                }
                Ok(n) => {
                    parser.feed(&buf[..n], i as u64 + 1, 1);
                    remain -= n as u64
                }
            }
        }
        parser.end_file(i as u64 + 1);
    }
    let out = parser.result(1, failed);
    println!("{}", serde_json::to_string(&out).unwrap());
    if failed { 66 } else { 0 }
}
