//! Readings from TopFlow's two external 4G modems (V3E1/V3E2) over ADB: the
//! board temperature and the QCI/AMBR the modem logs. A shell into a modem
//! takes 110-180 ms and both values change slowly, so a background task
//! samples them every SAMPLE_INTERVAL and `/state` reads the cache instead of
//! holding every snapshot back for four ADB round trips.
use crate::{command, qos};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const SERIALS: [&str; 2] = ["V3E1T12345", "V3E2T12345"];
const SAMPLE_INTERVAL: Duration = Duration::from_secs(30);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_ADB: &str = "/usr/bin/adb";
const TEMP_PATH: &str = "/sys/devices/virtual/power/zte_power/adc2_temp";

#[derive(Clone, Default)]
pub struct Sample {
    pub temp: Option<i64>,
    pub qos: qos::Values,
    pub sampled_at: i64,
}

static CACHE: OnceLock<Mutex<HashMap<&'static str, Sample>>> = OnceLock::new();
static STARTED: AtomicBool = AtomicBool::new(false);

fn cache() -> &'static Mutex<HashMap<&'static str, Sample>> {
    CACHE.get_or_init(Default::default)
}

/// The adb client: ZWRT_DATAD_ADB_BIN, else the firmware's own when present.
pub fn adb() -> Option<String> {
    std::env::var("ZWRT_DATAD_ADB_BIN")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| Path::new(DEFAULT_ADB).exists().then(|| DEFAULT_ADB.into()))
}

/// The latest sample for a modem, None until the first one is in. The first
/// call starts the sampler.
pub fn latest(serial: &str) -> Option<Sample> {
    if let Some(adb) = adb()
        && !STARTED.swap(true, Ordering::AcqRel)
    {
        tokio::spawn(sampler(adb));
    }
    cache().lock().ok()?.get(serial).cloned()
}

async fn sampler(adb: String) {
    loop {
        for serial in SERIALS {
            let sample = read(&adb, serial).await;
            if let Ok(mut cache) = cache().lock() {
                cache.insert(serial, sample);
            }
        }
        tokio::time::sleep(SAMPLE_INTERVAL).await;
    }
}

async fn read(adb: &str, serial: &str) -> Sample {
    let mut sample = Sample::default();
    if let Ok(raw) = command::run(
        adb,
        [
            "-s",
            serial,
            "shell",
            "grep QCI= /logfs/key.log | tail -n 64",
        ],
        COMMAND_TIMEOUT,
    )
    .await
        && let Some(parsed) = qos::parse_external(&String::from_utf8_lossy(&raw))
    {
        sample.qos = parsed;
    }
    if let Ok(raw) = command::run(
        adb,
        ["-s", serial, "shell", "cat", TEMP_PATH],
        COMMAND_TIMEOUT,
    )
    .await
    {
        sample.temp = parse_temp(&String::from_utf8_lossy(&raw));
    }
    if sample.temp.is_some() || sample.qos != qos::Values::default() {
        sample.sampled_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
    }
    sample
}

/// adc2_temp holds whole degrees Celsius; anything outside a plausible board
/// range is a read error, not a temperature.
fn parse_temp(raw: &str) -> Option<i64> {
    raw.trim()
        .parse::<i64>()
        .ok()
        .filter(|v| (-40..=125).contains(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temperature_is_whole_degrees_in_a_board_range() {
        assert_eq!(parse_temp("48\n"), Some(48));
        assert_eq!(parse_temp("-5"), Some(-5));
        assert_eq!(parse_temp("error: device not found"), None);
        assert_eq!(parse_temp("480"), None);
    }
}
