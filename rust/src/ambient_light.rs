//! Ambient light sensor of the MU5252 template (TopFlow: stk3329b, input
//! device "lightsensor-level"). `state.rs` and `control.rs` only reach it on
//! that template.
//!
//! The driver adds `enable` under the input device's `driver/` directory and,
//! while it is 1, reports the reading as ABS_MISC about ten times a second.
//! Its `lux` attribute is not refreshed by that polling, so readings come from
//! the input events. A reader thread runs while the sensor is enabled and keeps
//! the latest value for `/state.ambient_light`; clients turn the sensor on only
//! while they need it (devui: screen on with auto brightness).
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicI64, Ordering},
    time::Duration,
};

const SENSOR_NAME: &str = "lightsensor-level";
const EV_ABS: u16 = 3;
const ABS_MISC: u16 = 0x28;
/// `struct input_event`: a timeval, then u16 type, u16 code, i32 value.
const EVENT_SIZE: usize = std::mem::size_of::<libc::timeval>() + 8;
const POLL: Duration = Duration::from_millis(200);

static READING: AtomicBool = AtomicBool::new(false);
static LUX: AtomicI64 = AtomicI64::new(-1);

fn class_dir() -> PathBuf {
    std::env::var_os("ZWRT_DATAD_INPUT_CLASS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/sys/class/input".into())
}
fn dev_dir() -> PathBuf {
    std::env::var_os("ZWRT_DATAD_INPUT_DEV_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/dev/input".into())
}

/// The `eventN` node of the sensor, if this device has one.
fn find_event() -> Option<String> {
    let mut names: Vec<String> = fs::read_dir(class_dir())
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with("event"))
        .collect();
    names.sort();
    names.into_iter().find(|n| {
        fs::read_to_string(class_dir().join(n).join("device/name"))
            .map(|s| s.trim() == SENSOR_NAME)
            .unwrap_or(false)
    })
}
fn enable_path(event: &str) -> PathBuf {
    class_dir().join(event).join("device/driver/enable")
}
fn sensor_enabled(event: &str) -> bool {
    fs::read_to_string(enable_path(event))
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

/// Latest ABS_MISC value out of a buffer of whole input events.
fn last_reading(buf: &[u8]) -> Option<i64> {
    (0..buf.len() / EVENT_SIZE).rev().find_map(|i| {
        let at = (i + 1) * EVENT_SIZE - 8;
        let kind = u16::from_ne_bytes([buf[at], buf[at + 1]]);
        let code = u16::from_ne_bytes([buf[at + 2], buf[at + 3]]);
        let value = i32::from_ne_bytes([buf[at + 4], buf[at + 5], buf[at + 6], buf[at + 7]]);
        (kind == EV_ABS && code == ABS_MISC && value >= 0).then_some(i64::from(value))
    })
}

/// Read the sensor's events until it is turned off. One reader at a time.
fn start_reader(event: String) {
    if READING.swap(true, Ordering::AcqRel) {
        return;
    }
    LUX.store(-1, Ordering::Release);
    std::thread::spawn(move || {
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(dev_dir().join(&event));
        if let Ok(mut file) = file {
            let mut buf = vec![0u8; EVENT_SIZE * 64];
            while sensor_enabled(&event) {
                match file.read(&mut buf) {
                    Ok(n) if n > 0 => {
                        if let Some(v) = last_reading(&buf[..n - n % EVENT_SIZE]) {
                            LUX.store(v, Ordering::Release);
                        }
                        continue;
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => break,
                }
                std::thread::sleep(POLL);
            }
        }
        LUX.store(-1, Ordering::Release);
        READING.store(false, Ordering::Release);
    });
}

/// `/state.ambient_light`, or None on devices without the sensor.
pub fn state() -> Option<Value> {
    let event = find_event()?;
    let enabled = sensor_enabled(&event);
    if enabled {
        start_reader(event); // enabled before this datad started, or by the OEM UI
    }
    let lux = LUX.load(Ordering::Acquire);
    Some(json!({
        "available": true,
        "enabled": enabled,
        "lux": (enabled && lux >= 0).then_some(lux),
    }))
}

/// `ambient_light.set {enabled}`: turn the sensor on or off.
pub async fn set(enabled: bool) -> Result<Value, String> {
    let event = find_event().ok_or("ambient light sensor not present on this device")?;
    let path = enable_path(&event);
    tokio::task::spawn_blocking(move || fs::write(&path, if enabled { "1" } else { "0" }))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("cannot switch the light sensor: {e}"))?;
    if enabled {
        start_reader(event);
    }
    Ok(state().unwrap_or_else(|| json!({"available": false})))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: u16, code: u16, value: i32) -> Vec<u8> {
        let mut ev = vec![0u8; EVENT_SIZE - 8];
        ev.extend_from_slice(&kind.to_ne_bytes());
        ev.extend_from_slice(&code.to_ne_bytes());
        ev.extend_from_slice(&value.to_ne_bytes());
        ev
    }

    #[test]
    fn takes_the_last_abs_misc_reading() {
        let mut buf = event(EV_ABS, ABS_MISC, 153);
        buf.extend(event(0, 0, 0)); // EV_SYN
        buf.extend(event(EV_ABS, ABS_MISC, 160));
        buf.extend(event(EV_ABS, 0x00, 999)); // another axis
        assert_eq!(last_reading(&buf), Some(160));
        assert_eq!(last_reading(&event(0, 0, 0)), None);
        assert_eq!(last_reading(&event(EV_ABS, ABS_MISC, -1)), None);
    }
}
