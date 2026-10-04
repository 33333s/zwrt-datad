//! LTE neighbour cells straight from the modem's own scan list.
//!
//! The vendor `zte_nwinfo` service keeps `zte_nwinfo.manual_scan.lteg_nbr_content`
//! up to date every few seconds while the device is camped on LTE (NSA included),
//! as `PCI,EARFCN,B<band>,RSRP,RSRQ;` records that list the serving cell first and
//! then the intra- and inter-frequency neighbours the modem is measuring. Unlike
//! the 5G reports this needs no DIAG capture, so it is cheap enough to read on
//! every state refresh.
use serde_json::{Value, json};
use std::sync::Mutex;

const MAX_CELLS: usize = 32;
const MAX_RAW_BYTES: usize = 4096;

/// The raw list from the latest state refresh. The state collector already
/// loads the whole `zte_nwinfo` package, so it parks the value here instead of
/// the neighbour manager running another `uci` process.
static RAW: Mutex<Option<String>> = Mutex::new(None);

pub fn set_raw(value: Option<String>) {
    if let Ok(mut slot) = RAW.lock() {
        *slot = value;
    }
}

pub fn raw() -> Option<String> {
    RAW.lock().ok().and_then(|slot| slot.clone())
}

struct Reading {
    pci: u32,
    earfcn: i64,
    band: Option<u32>,
    rsrp: i64,
    rsrq: Option<i64>,
}

/// Parses `PCI,EARFCN,B<band>,RSRP,RSRQ;…`. Malformed or implausible records are
/// dropped (never repaired), a repeated PCI/EARFCN keeps the strongest reading.
pub fn parse(content: &str) -> Vec<Value> {
    if content.len() > MAX_RAW_BYTES {
        return Vec::new();
    }
    let mut cells: Vec<Reading> = Vec::new();
    for record in content.split(';') {
        let fields: Vec<&str> = record.split(',').map(str::trim).collect();
        if fields.len() < 4 {
            continue;
        }
        let (Ok(pci), Ok(earfcn)) = (fields[0].parse::<u32>(), fields[1].parse::<i64>()) else {
            continue;
        };
        let Some(rsrp) = fields[3]
            .parse::<i64>()
            .ok()
            .filter(|v| (-140..=-30).contains(v))
        else {
            continue;
        };
        if pci > 503 || !(0..=262_143).contains(&earfcn) {
            continue;
        }
        let band = fields[2]
            .strip_prefix(['B', 'b'])
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|v| (1..=256).contains(v));
        let rsrq = fields
            .get(4)
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|v| (-43..=20).contains(v));
        let reading = Reading {
            pci,
            earfcn,
            band,
            rsrp,
            rsrq,
        };
        match cells
            .iter()
            .position(|c| c.pci == pci && c.earfcn == earfcn)
        {
            Some(i) if cells[i].rsrp >= rsrp => {}
            Some(i) => cells[i] = reading,
            None if cells.len() < MAX_CELLS => cells.push(reading),
            None => {}
        }
    }
    cells.sort_by(|a, b| {
        b.rsrp
            .cmp(&a.rsrp)
            .then(a.earfcn.cmp(&b.earfcn))
            .then(a.pci.cmp(&b.pci))
    });
    cells
        .into_iter()
        .map(|c| {
            json!({
                "rat":"LTE","pci":c.pci,"arfcn":c.earfcn,"band":c.band,
                "rsrp_dbm":c.rsrp,"rsrq_db":c.rsrq,
                "samples":1,"direct_hits":0,"source":"vendor_scan"
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_serving_and_neighbours_strongest_first() {
        let cells = parse("347,2850,B7,-90,-14;339,38852,B40,-95,-16;490,2850,B7,-93,-18;");
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[0]["pci"], 347);
        assert_eq!(cells[0]["arfcn"], 2850);
        assert_eq!(cells[0]["band"], 7);
        assert_eq!(cells[0]["rsrp_dbm"], -90);
        assert_eq!(cells[0]["rsrq_db"], -14);
        assert_eq!(cells[0]["rat"], "LTE");
        assert_eq!(cells[1]["pci"], 490);
        assert_eq!(cells[2]["band"], 40);
    }

    #[test]
    fn earfcn_zero_is_a_valid_value_and_missing_rsrq_is_null() {
        let cells = parse("1,0,B1,-100;");
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0]["arfcn"], 0);
        assert!(cells[0]["rsrq_db"].is_null());
    }

    #[test]
    fn drops_malformed_and_implausible_records_without_repairing_them() {
        assert!(parse("").is_empty());
        assert!(parse("garbage;1,2;;,,,,").is_empty());
        assert!(parse("504,2850,B7,-90,-14;").is_empty(), "PCI above 503");
        assert!(
            parse("1,262144,B7,-90,-14;").is_empty(),
            "EARFCN above 262143"
        );
        assert!(
            parse("1,2850,B7,0,-14;").is_empty(),
            "RSRP 0 is not a measurement"
        );
        assert!(parse("1,2850,B7,-200,-14;").is_empty());
        let cells = parse("1,2850,X7,-90,99;");
        assert!(cells[0]["band"].is_null(), "unknown band stays null");
        assert!(cells[0]["rsrq_db"].is_null(), "implausible RSRQ stays null");
    }

    #[test]
    fn repeated_cells_keep_the_strongest_reading_and_the_list_is_bounded() {
        let cells = parse("5,100,B1,-100,-10;5,100,B1,-90,-9;5,100,B1,-95,-8;");
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0]["rsrp_dbm"], -90);
        let many: String = (0..100).map(|n| format!("{n},100,B1,-90,-9;")).collect();
        assert_eq!(parse(&many).len(), MAX_CELLS);
        assert!(
            parse(&"1,100,B1,-90,-9;".repeat(400)).is_empty(),
            "oversized input is ignored"
        );
    }
}
