//! Read-only USB negotiated link information from the Linux sysfs ABI.
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

const MAX_ATTRIBUTE: u64 = 128;
const MAX_ENTRIES: usize = 256;
const MAX_CONTROLLERS: usize = 16;
const MAX_DEVICES: usize = 64;

fn text(path: &Path) -> Option<String> {
    let mut value = String::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_ATTRIBUTE + 1)
        .read_to_string(&mut value)
        .ok()?;
    if value.len() as u64 > MAX_ATTRIBUTE {
        return None;
    }
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn speed_name(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "low-speed" => Some("low-speed"),
        "full-speed" => Some("full-speed"),
        "high-speed" => Some("high-speed"),
        "super-speed" => Some("super-speed"),
        "super-speed-plus" => Some("super-speed-plus"),
        _ => None,
    }
}

fn named_mbps(name: Option<&str>) -> Option<f64> {
    match name {
        Some("low-speed") => Some(1.5),
        Some("full-speed") => Some(12.0),
        Some("high-speed") => Some(480.0),
        Some("super-speed") => Some(5000.0),
        // The generic UDC enum cannot distinguish SSP lane/rate variants.
        // Never invent 10 or 20 Gbit/s from this label alone.
        _ => None,
    }
}

fn numeric_mbps(value: &str) -> Option<f64> {
    if !value.bytes().all(|v| v.is_ascii_digit() || v == b'.') {
        return None;
    }
    value
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0)
}

fn device_name(name: &str) -> bool {
    let Some((bus, ports)) = name.split_once('-') else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    digits(bus) && ports.split('.').all(digits)
}

fn entries(root: &Path) -> (bool, Vec<(String, PathBuf)>, bool) {
    let Ok(entries) = fs::read_dir(root) else {
        return (false, Vec::new(), false);
    };
    let mut paths = Vec::new();
    let mut truncated = false;
    for (index, entry) in entries.enumerate() {
        if index >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        if let Ok(entry) = entry
            && let Some(name) = entry.file_name().to_str()
        {
            paths.push((name.to_owned(), entry.path()));
        }
    }
    paths.sort_by(|a, b| a.0.cmp(&b.0));
    (true, paths, truncated)
}

fn controller(name: &str, path: &Path) -> Value {
    let state = text(&path.join("state")).map(|s| {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join("-")
            .to_ascii_lowercase()
    });
    let reported = text(&path.join("current_speed"));
    let reported = reported.as_deref().and_then(speed_name);
    let connected = match state.as_deref() {
        Some("not-attached") => Some(false),
        Some(
            "attached" | "powered" | "reconnecting" | "unauthenticated" | "default" | "addressed"
            | "configured" | "suspended",
        ) => Some(true),
        _ => reported.map(|_| true),
    };
    // A disconnected UDC may briefly retain its previous speed. Do not show
    // that stale value as a live negotiated rate.
    let speed = if connected == Some(false) {
        None
    } else {
        reported
    };
    let maximum = text(&path.join("maximum_speed"));
    json!({"name":name,"state":state,"connected":connected,"speed":speed,
        "speed_mbps":named_mbps(speed),"maximum_speed":maximum.as_deref().and_then(speed_name)})
}

fn host_device(name: &str, path: &Path) -> Value {
    let mbps = text(&path.join("speed")).as_deref().and_then(numeric_mbps);
    let speed = match mbps {
        Some(1.5) => Some("low-speed"),
        Some(12.0) => Some("full-speed"),
        Some(480.0) => Some("high-speed"),
        Some(5000.0) => Some("super-speed"),
        Some(10000.0 | 20000.0) => Some("super-speed-plus"),
        _ => None,
    };
    let lanes = |attribute| {
        text(&path.join(attribute))
            .and_then(|v| v.parse::<u8>().ok())
            .filter(|v| *v > 0)
    };
    json!({"name":name,"speed":speed,"speed_mbps":mbps,
        "rx_lanes":lanes("rx_lanes"),"tx_lanes":lanes("tx_lanes")})
}

fn collect(udc_root: &Path, host_root: &Path) -> Value {
    let (gadget_available, udcs, mut truncated) = entries(udc_root);
    let mut controllers = Vec::new();
    for (name, path) in udcs {
        if !path.is_dir() {
            continue;
        }
        if controllers.len() >= MAX_CONTROLLERS {
            truncated = true;
            break;
        }
        controllers.push(controller(&name, &path));
    }
    let (host_available, hosts, host_truncated) = entries(host_root);
    truncated |= host_truncated;
    let mut devices = Vec::new();
    for (name, path) in hosts {
        // usbN root hubs describe controllers, not a negotiated upstream
        // cable. Interfaces (N-P:I.A) are not separate USB links either.
        if !device_name(&name) || !path.is_dir() {
            continue;
        }
        if devices.len() >= MAX_DEVICES {
            truncated = true;
            break;
        }
        devices.push(host_device(&name, &path));
    }
    json!({"source":"sysfs","gadget_available":gadget_available,"host_available":host_available,
        "controllers":controllers,"devices":devices,"truncated":truncated})
}

pub fn snapshot() -> Value {
    let udc = std::env::var_os("ZWRT_DATAD_USB_UDC_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/sys/class/udc".into());
    let host = std::env::var_os("ZWRT_DATAD_USB_HOST_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/sys/bus/usb/devices".into());
    collect(&udc, &host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_semantics_and_root_hub_filtering() {
        assert_eq!(named_mbps(speed_name("HIGH-SPEED")), Some(480.0));
        assert_eq!(named_mbps(speed_name("super-speed-plus")), None);
        assert_eq!(numeric_mbps("1.5"), Some(1.5));
        for invalid in ["NaN", "inf", "-1", "0", "480 Mbps", "1e6", ""] {
            assert!(numeric_mbps(invalid).is_none());
        }
        for name in ["1-1", "2-3.4", "10-2.3.4"] {
            assert!(device_name(name));
        }
        for name in ["usb1", "1-1:1.0", "1-", "1-2.", "../1-1"] {
            assert!(!device_name(name));
        }
    }

    #[test]
    fn absent_roots_are_unknown_not_fabricated_speeds() {
        let value = collect(
            Path::new("/no-such-datad-usb-udc"),
            Path::new("/no-such-datad-usb-host"),
        );
        assert_eq!(value["gadget_available"], false);
        assert_eq!(value["host_available"], false);
        assert_eq!(value["controllers"], json!([]));
        assert_eq!(value["devices"], json!([]));
    }
}
