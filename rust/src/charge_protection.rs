//! The stock long-charge protection on ZTE battery models (TopFlow, U60 Pro).
//!
//! The battery driver counts how long the device has sat on the charger at
//! 100 %. After `single_lcm_period` (960 minutes by default) it switches to
//! customer mode 2 and holds the battery near 80 % by discharging it. The stock
//! UI only shows that state (`bat_mode >= 2`); it has no switch.
//!
//! The driver's sysfs node takes `"2 N"` to set that period to N minutes and
//! `"1 1"` to go back to the built-in default. Neither leaves the hold that is
//! already running: the hold is a charging state machine that only resets when
//! the UI charge policy changes, so a release writes another policy value and
//! then the original one (every policy except 5/ZGD behaves the same).
//! Everything lives in kernel memory and returns to the default at boot, so a
//! disabled protection is applied again on start and checked every 30 s.

use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::io::AsyncWriteExt;

pub const DEFAULT_THRESHOLD_MINUTES: i64 = 960;
/// The driver keeps the period in a 32-bit millisecond counter; this is the
/// largest whole number of minutes that still fits (about 49.7 days).
pub const DISABLED_THRESHOLD_MINUTES: i64 = 71582;
const LONG_CHARGING_MODE: i64 = 2;
const ZGD_POLICY: i64 = 5;

static CONFIG: OnceLock<PathBuf> = OnceLock::new();
static DISABLED: AtomicBool = AtomicBool::new(false);
static NEXT_TICK: AtomicU64 = AtomicU64::new(0);
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Kernel {
    mode: i64,
    threshold_minutes: i64,
    debug: i64,
    policy: i64,
}

fn root() -> PathBuf {
    std::env::var("ZWRT_DATAD_BATTERY_ZTE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| "/sys/class/power_supply/battery_zte".into())
}

/// Loads the saved switch. Called once before the first snapshot.
pub fn init(data_dir: &std::path::Path) {
    let path = CONFIG.get_or_init(|| data_dir.join("charge_protection.conf"));
    let disabled = std::fs::read_to_string(path)
        .ok()
        .is_some_and(|raw| raw.lines().any(|line| line.trim() == "disabled=1"));
    DISABLED.store(disabled, Ordering::Relaxed);
}

pub fn disabled() -> bool {
    DISABLED.load(Ordering::Relaxed)
}

async fn save(disabled: bool) -> Result<(), String> {
    let path = CONFIG
        .get()
        .ok_or_else(|| "charge protection is not initialised".to_owned())?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("conf.tmp");
    tokio::fs::write(&tmp, format!("disabled={}\n", i64::from(disabled)))
        .await
        .map_err(|e| e.to_string())?;
    tokio::fs::rename(&tmp, path)
        .await
        .map_err(|e| e.to_string())
}

fn help_value(help: &str, prefix: &str) -> Option<String> {
    help.lines()
        .find_map(|line| line.trim().strip_prefix(prefix))
        .map(|rest| {
            rest.split_whitespace()
                .next()
                .unwrap_or_default()
                .to_owned()
        })
}

fn parse(mode: &str, help: &str, policy: &str) -> Option<Kernel> {
    Some(Kernel {
        mode: mode.trim().parse().ok()?,
        threshold_minutes: help_value(help, "single_lcm_period is")?.parse().ok()?,
        // printed with %x
        debug: i64::from_str_radix(&help_value(help, "customer_mode_dbg is")?, 16).ok()?,
        policy: policy.trim().parse().ok()?,
    })
}

async fn read() -> Option<Kernel> {
    let root = root();
    let (mode, help, policy) = tokio::join!(
        tokio::fs::read_to_string(root.join("customer_mode")),
        tokio::fs::read_to_string(root.join("customer_mode_help")),
        tokio::fs::read_to_string(root.join("ui_chg_policy_mode")),
    );
    parse(&mode.ok()?, &help.ok()?, &policy.ok()?)
}

async fn write(name: &str, value: &str) -> Result<(), String> {
    let path = root().join(name);
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
        .await
        .map_err(|e| format!("{}: {e}", path.display()))?;
    file.write_all(value.as_bytes())
        .await
        .map_err(|e| format!("{}: {e}", path.display()))?;
    file.flush()
        .await
        .map_err(|e| format!("{}: {e}", path.display()))
}

async fn settle(accept: impl Fn(&Kernel) -> bool) -> Option<Kernel> {
    let mut last = None;
    for attempt in 0..10 {
        if attempt != 0 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        last = read().await;
        if last.as_ref().is_some_and(&accept) {
            break;
        }
    }
    last
}

/// Leaves a running hold: the policy change resets the driver's charging state
/// machine, sets customer mode 1 and reports it to the stock services.
async fn release_hold(kernel: &Kernel) -> Result<Kernel, String> {
    let other = if kernel.policy == 0 { 1 } else { 0 };
    write("ui_chg_policy_mode", &other.to_string()).await?;
    let restored = write("ui_chg_policy_mode", &kernel.policy.to_string()).await;
    let after = settle(|k| k.mode != LONG_CHARGING_MODE && k.policy == kernel.policy).await;
    restored?;
    match after {
        Some(k) if k.policy != kernel.policy => Err(format!(
            "charge policy readback is {}, expected {}",
            k.policy, kernel.policy
        )),
        Some(k) if k.mode == LONG_CHARGING_MODE => {
            Err("long-charge protection is still active after the reset".into())
        }
        Some(k) => Ok(k),
        None => Err("charge protection state is unreadable".into()),
    }
}

async fn set_threshold(minutes: i64) -> Result<Kernel, String> {
    write("customer_mode", &format!("2 {minutes}")).await?;
    match settle(|k| k.threshold_minutes == minutes).await {
        Some(k) if k.threshold_minutes == minutes => Ok(k),
        _ => Err("long-charge threshold readback did not confirm the change".into()),
    }
}

async fn restore_default() -> Result<Kernel, String> {
    write("customer_mode", "1 1").await?;
    match settle(|k| k.debug == 0).await {
        Some(k) if k.debug == 0 => Ok(k),
        _ => Err("long-charge default readback did not confirm the change".into()),
    }
}

/// Keeps a disabled protection disabled: the threshold returns to 960 minutes
/// after every boot, and a hold can still start once the long period expires.
async fn enforce(kernel: Kernel) -> Result<(Kernel, bool), String> {
    // Setting the threshold drops the mode to 1 but leaves a running hold, so
    // decide on the release from the state before the write.
    let holding = holding(&kernel).await;
    let mut kernel = kernel;
    if kernel.debug == 0 || kernel.threshold_minutes != DISABLED_THRESHOLD_MINUTES {
        kernel = set_threshold(DISABLED_THRESHOLD_MINUTES).await?;
    }
    let release = holding && kernel.policy != ZGD_POLICY;
    if release {
        kernel = release_hold(&kernel).await?;
    }
    Ok((kernel, release))
}

/// A hold is running when the driver is in mode 2, or when the stock services
/// still report it after a threshold write moved the mode back to 1 without
/// leaving the hold.
async fn holding(kernel: &Kernel) -> bool {
    kernel.mode == LONG_CHARGING_MODE
        || crate::state::ubus(
            "zwrt_mc.device.manager",
            "get_device_info",
            json!({"deviceInfoList":["bat_mode"]}),
        )
        .await
        .ok()
        .and_then(|value| match value.get("bat_mode") {
            Some(Value::String(mode)) => mode.trim().parse::<i64>().ok(),
            Some(Value::Number(mode)) => mode.as_i64(),
            _ => None,
        })
        .is_some_and(|mode| mode >= LONG_CHARGING_MODE)
}

fn status(kernel: &Kernel, active: bool) -> Value {
    json!({
        "supported":true,
        "active":active,
        "mode":kernel.mode,
        "disabled":disabled(),
        "threshold_minutes":kernel.threshold_minutes,
        "default_threshold_minutes":DEFAULT_THRESHOLD_MINUTES,
    })
}

/// Extra `battery.charge_protection` fields; `None` without the driver node.
pub async fn snapshot() -> Option<Value> {
    let kernel = read().await?;
    Some(json!({
        "supported":true,
        "disabled":disabled(),
        "threshold_minutes":kernel.threshold_minutes,
    }))
}

pub async fn tick() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if !disabled()
        || NEXT_TICK
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                (now >= next).then_some(now + 30)
            })
            .is_err()
    {
        return;
    }
    let _guard = LOCK.lock().await;
    if !disabled() {
        return;
    }
    let Some(kernel) = read().await else {
        return;
    };
    match enforce(kernel).await {
        Ok((_, true)) => {
            eprintln!("charge protection: released a long-charge hold (protection disabled)")
        }
        Ok(_) => {}
        Err(error) => eprintln!("charge protection: {error}"),
    }
}

/// `Err((true, _))` is an invalid request, `Err((false, _))` a failure.
pub async fn execute(action: &str, params: &Value) -> Result<Value, (bool, String)> {
    let _guard = LOCK.lock().await;
    let Some(kernel) = read().await else {
        return if action == "battery.charge_protection.status" {
            Ok(json!({"supported":false,"disabled":disabled()}))
        } else {
            Err((
                false,
                "long-charge protection is not supported on this device".into(),
            ))
        };
    };
    match action {
        "battery.charge_protection.status" => Ok(status(&kernel, holding(&kernel).await)),
        "battery.charge_protection.release" => {
            if !holding(&kernel).await {
                let mut value = status(&kernel, false);
                value["released"] = json!(false);
                return Ok(value);
            }
            if kernel.policy == ZGD_POLICY {
                return Err((
                    false,
                    "the stock ZGD charge policy is active; leave it in the stock UI".into(),
                ));
            }
            let after = release_hold(&kernel).await.map_err(|e| (false, e))?;
            let mut value = status(&after, false);
            value["released"] = json!(true);
            value["verified"] = json!(true);
            Ok(value)
        }
        "battery.charge_protection.set" => {
            let wanted = match params.get("disabled") {
                Some(Value::Bool(value)) => *value,
                Some(Value::Number(value)) if matches!(value.as_i64(), Some(0 | 1)) => {
                    value.as_i64() == Some(1)
                }
                Some(_) => return Err((true, "disabled must be a boolean or 0/1".into())),
                None => return Err((true, "missing parameter: disabled".into())),
            };
            let changed = wanted != disabled();
            save(wanted).await.map_err(|e| (false, e))?;
            DISABLED.store(wanted, Ordering::Relaxed);
            let after = if wanted {
                enforce(kernel).await.map(|(after, _)| after)
            } else if kernel.debug != 0 {
                restore_default().await
            } else {
                Ok(kernel)
            }
            .map_err(|e| (false, e))?;
            let active = !wanted && holding(&after).await;
            let mut value = status(&after, active);
            value["changed"] = json!(changed);
            value["verified"] = json!(true);
            Ok(value)
        }
        _ => Err((true, format!("unsupported action: {action}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELP: &str = "===========current state============\n\ncustomer mode is 2\n\n*****customer mode define info*****\n0: CUSTOMER_UNKNOWN_MODE\n1: CUSTOMER_DEFAULT_MODE\n2: CUSTOMER_SINGLE_LONG_CHARGING_MODE\n\n=====customer mode details parameter=====\n2: CUSTOMER_SINGLE_LONG_CHARGING_MODE parameter\nsingle_lcm_period is 960 minutes\n\n=====customer_mode_dbg details=====\ncustomer_mode_dbg is 0\n";

    #[test]
    fn parses_the_driver_help_text() {
        assert_eq!(
            parse("2\n", HELP, "0\n"),
            Some(Kernel {
                mode: 2,
                threshold_minutes: 960,
                debug: 0,
                policy: 0
            })
        );
        let debug = HELP
            .replace("960 minutes", "71582 minutes")
            .replace("dbg is 0", "dbg is 1");
        assert_eq!(
            parse("1", &debug, "0").map(|k| (k.threshold_minutes, k.debug)),
            Some((71582, 1))
        );
        assert_eq!(parse("1", "no period here", "0"), None);
        assert_eq!(parse("", HELP, "0"), None);
    }

    #[test]
    fn disabled_threshold_fits_the_driver_millisecond_counter() {
        assert!(DISABLED_THRESHOLD_MINUTES * 60_000 <= i64::from(u32::MAX));
        assert!((DISABLED_THRESHOLD_MINUTES + 1) * 60_000 > i64::from(u32::MAX));
    }
}
