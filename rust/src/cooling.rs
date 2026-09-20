use crate::state;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

const DEFAULT_CURVE: [(i64, i64); 5] = [(40, 0), (45, 0), (50, 76), (60, 128), (70, 255)];
static HARD_OVERRIDE: AtomicBool = AtomicBool::new(false);
static STOPPING: AtomicBool = AtomicBool::new(false);
static COOLING_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
#[derive(Clone, Default)]
struct FanPolicyStatus {
    attempted: bool,
    applied: bool,
    error: String,
}
static FAN_POLICY_STATUS: OnceLock<Mutex<FanPolicyStatus>> = OnceLock::new();
#[derive(Clone)]
struct Config {
    fan_always: bool,
    fan_mode: i64,
    fan_speed: i64,
    liquid_always: bool,
    liquid_level: i64,
    temps: [i64; 3],
    hyst: [i64; 3],
    curve: Vec<(i64, i64)>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            fan_always: false,
            fan_mode: 2,
            fan_speed: 50,
            liquid_always: false,
            liquid_level: 1,
            temps: [44, 48, 53],
            hyst: [4, 4, 4],
            curve: DEFAULT_CURVE.to_vec(),
        }
    }
}
fn env(name: &str, default: &str) -> PathBuf {
    std::env::var(name)
        .map(PathBuf::from)
        .unwrap_or_else(|_| default.into())
}
#[allow(clippy::field_reassign_with_default)]
async fn load() -> Config {
    let path = env("ZWRT_DATAD_COOLING_CONFIG", "/data/zwrt-datad/cooling.conf");
    let Ok(raw) = tokio::fs::read_to_string(path).await else {
        return Config::default();
    };
    let mut map = BTreeMap::new();
    for line in raw.lines() {
        if let Some((k, v)) = line.split_once('=')
            && let Ok(v) = v.parse()
        {
            map.insert(k.to_owned(), v);
        }
    }
    let mut c = Config::default();
    c.fan_always = map.get("fan_always_on").copied().unwrap_or(0) != 0;
    c.fan_mode = map.get("fan_mode").copied().unwrap_or_else(|| {
        if map.get("fan_auto").copied().unwrap_or(0) != 0 {
            1
        } else {
            2
        }
    });
    c.fan_speed = map.get("fan_speed_percent").copied().unwrap_or(50);
    c.liquid_always = map
        .get("liquid_always_on")
        .or_else(|| map.get("liquid_enabled"))
        .copied()
        .unwrap_or(0)
        != 0;
    c.liquid_level = map.get("liquid_level").copied().unwrap_or(1).clamp(1, 2);
    for i in 0..3 {
        c.temps[i] = map
            .get(&format!("temperature_{}", i + 1))
            .copied()
            .unwrap_or(c.temps[i]);
        c.hyst[i] = map
            .get(&format!("hysteresis_{}", i + 1))
            .copied()
            .unwrap_or(c.hyst[i]);
    }
    let count = map
        .get("custom_curve_count")
        .copied()
        .unwrap_or(5)
        .clamp(2, 8);
    c.curve = (0..count)
        .map(|i| {
            (
                map.get(&format!("custom_temperature_{}", i + 1))
                    .copied()
                    .unwrap_or(DEFAULT_CURVE.get(i as usize).map(|v| v.0).unwrap_or(70)),
                map.get(&format!("custom_pwm_{}", i + 1))
                    .copied()
                    .unwrap_or(DEFAULT_CURVE.get(i as usize).map(|v| v.1).unwrap_or(255)),
            )
        })
        .collect();
    c
}
async fn save(c: &Config) -> Result<(), String> {
    let path = env("ZWRT_DATAD_COOLING_CONFIG", "/data/zwrt-datad/cooling.conf");
    if let Some(p) = path.parent() {
        tokio::fs::create_dir_all(p)
            .await
            .map_err(|e| e.to_string())?
    }
    let tmp = path.with_extension("conf.tmp");
    let mut out = format!(
        "fan_enabled=1\nfan_always_on={}\nfan_auto={}\nfan_mode={}\nfan_speed_percent={}\nliquid_enabled={}\nliquid_always_on={}\nliquid_level={}\ntemperature_1={}\ntemperature_2={}\ntemperature_3={}\nhysteresis_1={}\nhysteresis_2={}\nhysteresis_3={}\ncustom_curve_count={}\n",
        i64::from(c.fan_always),
        i64::from(c.fan_mode == 1),
        c.fan_mode,
        c.fan_speed,
        i64::from(c.liquid_always),
        i64::from(c.liquid_always),
        c.liquid_level,
        c.temps[0],
        c.temps[1],
        c.temps[2],
        c.hyst[0],
        c.hyst[1],
        c.hyst[2],
        c.curve.len()
    );
    for (i, (t, p)) in c.curve.iter().enumerate() {
        out.push_str(&format!(
            "custom_temperature_{}={}\ncustom_pwm_{}={}\n",
            i + 1,
            t,
            i + 1,
            p
        ));
    }
    tokio::fs::write(&tmp, out)
        .await
        .map_err(|e| e.to_string())?;
    tokio::fs::rename(tmp, path)
        .await
        .map_err(|e| e.to_string())
}
async fn write(path: PathBuf, value: &str) -> Result<(), String> {
    tokio::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
        .await
        .map_err(|e| format!("{}: {e}", path.display()))?
        .write_all(value.as_bytes())
        .await
        .map_err(|e| e.to_string())
}
use tokio::io::AsyncWriteExt;
fn discover_typed(root: &Path, prefix: &str, wanted: &str) -> Option<PathBuf> {
    let mut entries = std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(prefix))
        })
        .collect::<Vec<_>>();
    entries.sort();
    entries.into_iter().find(|path| {
        std::fs::read_to_string(path.join("type"))
            .ok()
            .is_some_and(|value| value.trim() == wanted)
    })
}
pub(crate) fn zone_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("ZWRT_DATAD_COOLING_ZONE_PATH")
        && !path.is_empty()
    {
        return Some(path.into());
    }
    let root = env("ZWRT_DATAD_THERMAL_ROOT", "/sys/class/thermal");
    discover_typed(&root, "thermal_zone", "sys-therm-4")
}
fn fan_cooling_state_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("ZWRT_DATAD_FAN_COOLING_STATE_PATH")
        && !path.is_empty()
    {
        return Some(path.into());
    }
    let root = env("ZWRT_DATAD_THERMAL_ROOT", "/sys/class/thermal");
    discover_typed(&root, "cooling_device", "pwm-fan").map(|path| path.join("cur_state"))
}
fn pwm_path() -> PathBuf {
    env("ZWRT_DATAD_FAN_PWM_PATH", "/sys/class/hwmon/hwmon0/pwm1")
}
fn thermal_enable_path() -> PathBuf {
    env(
        "ZWRT_DATAD_FAN_THERMAL_ENABLE_PATH",
        "/sys/class/hwmon/hwmon0/device/thermal_enable",
    )
}
fn low_speed_path() -> PathBuf {
    std::env::var("ZWRT_DATAD_FAN_LOW_SPEED_MODE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| pwm_path().with_file_name("device").join("low_speed_mode"))
}
fn record_fan_policy(result: &Result<(), String>) {
    let mut status = FAN_POLICY_STATUS
        .get_or_init(|| Mutex::new(FanPolicyStatus::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    status.attempted = true;
    status.applied = result.is_ok();
    status.error = result.as_ref().err().cloned().unwrap_or_default();
}
pub(crate) fn fan_policy_status() -> Value {
    let status = FAN_POLICY_STATUS
        .get_or_init(|| Mutex::new(FanPolicyStatus::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    json!({"attempted":status.attempted,"applied":status.applied,"error":status.error})
}
async fn temperature() -> i64 {
    let Some(zone) = zone_path() else {
        return -1;
    };
    tokio::fs::read_to_string(zone.join("temp"))
        .await
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .map(|v| if v >= 1000 { (v + 500) / 1000 } else { v })
        .unwrap_or(-1)
}
async fn prepare_fan() -> Result<(), String> {
    // Resolve both devices before disabling any thermal controller. In
    // particular cooling_device0 may be a CPU hotplug controller, not a fan.
    let zone = zone_path().ok_or("fan thermal zone not found")?;
    let state = fan_cooling_state_path().ok_or("pwm-fan cooling device not found")?;
    write(thermal_enable_path(), "1").await?;
    write(zone.join("mode"), "disabled").await?;
    write(state, "0").await?;
    let low_speed = low_speed_path();
    if tokio::fs::try_exists(&low_speed)
        .await
        .map_err(|e| e.to_string())?
    {
        // OEM quiet mode otherwise clamps all requests (including 255) to 76.
        write(low_speed, "0").await?;
    }
    Ok(())
}
async fn restore_kernel_fan() -> Result<(), String> {
    let zone = zone_path().ok_or("fan thermal zone not found")?;
    // Attempt both writes even if the driver enable write fails.
    let thermal = write(thermal_enable_path(), "1").await;
    let mode = write(zone.join("mode"), "enabled").await;
    thermal.and(mode)
}
async fn fan_pwm(pwm: i64) -> Result<(), String> {
    prepare_fan().await?;
    let requested = pwm.clamp(0, 255);
    write(pwm_path(), &requested.to_string()).await?;
    let actual = tokio::fs::read_to_string(pwm_path())
        .await
        .map_err(|e| e.to_string())?
        .trim()
        .parse::<i64>()
        .map_err(|e| e.to_string())?;
    if actual != requested {
        return Err(format!(
            "fan PWM readback mismatch: requested {requested}, got {actual}"
        ));
    }
    Ok(())
}
fn curve_pwm(c: &Config, temp: i64) -> Option<i64> {
    if temp >= 80 {
        return Some(255);
    }
    if temp <= 0 {
        return None;
    }
    if temp <= c.curve[0].0 {
        return Some(c.curve[0].1);
    }
    for w in c.curve.windows(2) {
        let ((x0, y0), (x1, y1)) = (w[0], w[1]);
        if temp <= x1 {
            return Some(y0 + ((temp - x0) * (y1 - y0) + (x1 - x0) / 2) / (x1 - x0));
        }
    }
    c.curve.last().map(|v| v.1)
}
async fn apply_fan(c: &Config) -> Result<(), String> {
    let temp = temperature().await;
    if temp >= 80 || (temp <= 0 && HARD_OVERRIDE.load(Ordering::Relaxed)) {
        let result = fan_pwm(255).await;
        if result.is_ok() {
            HARD_OVERRIDE.store(true, Ordering::Relaxed);
        }
        return result;
    }
    if c.fan_always {
        return fan_pwm(128).await;
    }
    if c.fan_mode == 1 {
        let zone = zone_path().ok_or("fan thermal zone not found")?;
        write(thermal_enable_path(), "1").await?;
        for i in 0..3 {
            write(
                zone.join(format!("trip_point_{i}_temp")),
                &(c.temps[i] * 1000).to_string(),
            )
            .await?;
            write(
                zone.join(format!("trip_point_{i}_hyst")),
                &(c.hyst[i] * 1000).to_string(),
            )
            .await?;
        }
        let result = write(zone.join("mode"), "enabled").await;
        if result.is_ok() {
            HARD_OVERRIDE.store(false, Ordering::Relaxed);
        }
        return result;
    }
    let pwm = curve_pwm(c, temp).ok_or("fan temperature unavailable")?;
    let result = fan_pwm(pwm).await;
    if result.is_ok() {
        HARD_OVERRIDE.store(false, Ordering::Relaxed);
    }
    result
}
async fn apply_fan_recorded(c: &Config) -> Result<(), String> {
    let result = match apply_fan(c).await {
        Ok(()) => Ok(()),
        Err(error) => match restore_kernel_fan().await {
            Ok(()) => Err(format!("{error}; kernel thermal control restored")),
            Err(recovery) => Err(format!("{error}; kernel recovery failed: {recovery}")),
        },
    };
    record_fan_policy(&result);
    result
}
async fn apply_liquid(c: &Config) -> Result<(), String> {
    let thermal = env(
        "ZWRT_DATAD_LIQUID_THERMAL_ENABLE_PATH",
        "/sys/class/leds/aw_vibrator/thermal_enable",
    );
    let drive = env(
        "ZWRT_DATAD_LIQUID_DRIVE_PATH",
        "/sys/class/leds/aw_vibrator/atsin0",
    );
    if c.liquid_always {
        write(thermal, "0").await?;
        write(
            drive,
            &format!("1023 {} 200", if c.liquid_level >= 2 { 200 } else { 60 }),
        )
        .await?;
        Ok(())
    } else {
        write(drive, "0 0 0").await?;
        write(thermal, "1").await
    }
}
async fn vendor(path: &str, value: bool) -> Result<(), String> {
    state::uci_write("set", path, Some(if value { "1" } else { "0" })).await?;
    state::uci_write("commit", "zwrt_deviceui", None).await
}
fn invalid(msg: &str) -> (bool, String) {
    (true, msg.into())
}
fn failed(e: String) -> (bool, String) {
    (false, e)
}
pub async fn execute(action: &str, p: &Value) -> Result<Value, (bool, String)> {
    let _guard = COOLING_LOCK.lock().await;
    if STOPPING.load(Ordering::Relaxed) {
        return Err(failed("cooling controller is shutting down".into()));
    }
    let mut c = load().await;
    match action {
        "cooling.fan.set_enabled" => {
            let e = p
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or_else(|| invalid("enabled must be boolean"))?;
            c.fan_always = e;
            c.fan_mode = if e { c.fan_mode } else { 2 };
            apply_fan_recorded(&c).await.map_err(failed)?;
            vendor("zwrt_deviceui.Device.fan_switch_status", e)
                .await
                .map_err(failed)?;
            save(&c).await.map_err(failed)?;
            Ok(json!({"enabled":e,"always_on":e,"mode":if e{"always_on"}else{"custom"}}))
        }
        "cooling.fan.set_mode" => {
            let mode = p
                .get("mode")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("missing parameter: mode"))?;
            match mode {
                "automatic" => {
                    c.fan_always = false;
                    c.fan_mode = 1
                }
                "custom" => {
                    c.fan_always = false;
                    c.fan_mode = 2
                }
                "always_on" => c.fan_always = true,
                _ => return Err(invalid("mode must be automatic, custom, or always_on")),
            }
            apply_fan_recorded(&c).await.map_err(failed)?;
            vendor("zwrt_deviceui.Device.fan_switch_status", c.fan_always)
                .await
                .map_err(failed)?;
            save(&c).await.map_err(failed)?;
            Ok(json!({"enabled":c.fan_always,"always_on":c.fan_always,"mode":mode}))
        }
        "cooling.fan.set_curve" => {
            let points = p
                .get("points")
                .and_then(Value::as_array)
                .ok_or_else(|| invalid("points must be an array"))?;
            if !(2..=8).contains(&points.len()) {
                return Err(invalid("points must contain 2 to 8 objects"));
            }
            let mut curve = Vec::new();
            for point in points {
                let t = point
                    .get("temperature")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| invalid("invalid curve point"))?;
                let pwm = point
                    .get("pwm")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| invalid("invalid curve point"))?;
                if !(20..=80).contains(&t)
                    || !(0..=255).contains(&pwm)
                    || curve.last().is_some_and(|(pt, pp)| t <= *pt || pwm < *pp)
                {
                    return Err(invalid(
                        "curve temperatures must increase and pwm must not decrease",
                    ));
                }
                curve.push((t, pwm));
            }
            c.curve = curve.clone();
            c.fan_mode = 2;
            c.fan_always = false;
            apply_fan_recorded(&c).await.map_err(failed)?;
            vendor("zwrt_deviceui.Device.fan_switch_status", false)
                .await
                .map_err(failed)?;
            save(&c).await.map_err(failed)?;
            Ok(
                json!({"enabled":false,"always_on":false,"mode":"custom","points":curve.into_iter().map(|(temperature,pwm)|json!({"temperature":temperature,"pwm":pwm})).collect::<Vec<_>>()}),
            )
        }
        "cooling.liquid.set_enabled" => {
            let e = p
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or_else(|| invalid("enabled must be boolean"))?;
            c.liquid_always = e;
            apply_liquid(&c).await.map_err(failed)?;
            vendor("zwrt_deviceui.Device.liquid_cooling_switch_status", e)
                .await
                .map_err(failed)?;
            save(&c).await.map_err(failed)?;
            Ok(json!({"enabled":e,"always_on":e}))
        }
        "cooling.liquid.set_mode" => {
            let mode = p
                .get("mode")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("mode must be automatic, low, or high"))?;
            match mode {
                "automatic" => c.liquid_always = false,
                "low" => {
                    c.liquid_always = true;
                    c.liquid_level = 1
                }
                "high" => {
                    c.liquid_always = true;
                    c.liquid_level = 2
                }
                _ => return Err(invalid("mode must be automatic, low, or high")),
            }
            apply_liquid(&c).await.map_err(failed)?;
            vendor(
                "zwrt_deviceui.Device.liquid_cooling_switch_status",
                c.liquid_always,
            )
            .await
            .map_err(failed)?;
            save(&c).await.map_err(failed)?;
            Ok(
                json!({"enabled":c.liquid_always,"always_on":c.liquid_always,"mode":mode,"level":if c.liquid_always{c.liquid_level}else{0},"amplitude":if c.liquid_always{if c.liquid_level==2{200}else{60}}else{0},"speed_percent":if c.liquid_always{if c.liquid_level==2{100}else{30}}else{0}}),
            )
        }
        _ => Err(invalid("unsupported cooling action")),
    }
}

pub async fn tick() {
    let _guard = COOLING_LOCK.lock().await;
    if STOPPING.load(Ordering::Relaxed) {
        return;
    }
    let path = env("ZWRT_DATAD_COOLING_CONFIG", "/data/zwrt-datad/cooling.conf");
    if tokio::fs::metadata(path).await.is_err() {
        return;
    }
    let config = load().await;
    if config.liquid_always {
        let _ = apply_liquid(&config).await;
    }
    let _ = apply_fan_recorded(&config).await;
}

pub async fn shutdown() {
    let _guard = COOLING_LOCK.lock().await;
    if STOPPING.swap(true, Ordering::Relaxed) {
        return;
    }
    let config = env("ZWRT_DATAD_COOLING_CONFIG", "/data/zwrt-datad/cooling.conf");
    if tokio::fs::try_exists(config).await.unwrap_or(false)
        && let Err(error) = restore_kernel_fan().await
    {
        eprintln!("cooling shutdown: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_fan_zone_and_pwm_cooling_device_by_type() {
        let root = std::env::temp_dir().join(format!(
            "datad-cooling-discovery-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let zone0 = root.join("thermal_zone0");
        let zone32 = root.join("thermal_zone32");
        let cooling0 = root.join("cooling_device0");
        let cooling7 = root.join("cooling_device7");
        for path in [&zone0, &zone32, &cooling0, &cooling7] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(zone0.join("type"), "sdr0_pa\n").unwrap();
        std::fs::write(zone32.join("type"), "sys-therm-4\n").unwrap();
        std::fs::write(cooling0.join("type"), "cpu-hotplug1\n").unwrap();
        std::fs::write(cooling7.join("type"), "pwm-fan\n").unwrap();
        assert_eq!(
            discover_typed(&root, "thermal_zone", "sys-therm-4"),
            Some(zone32)
        );
        assert_eq!(
            discover_typed(&root, "cooling_device", "pwm-fan"),
            Some(cooling7)
        );
        assert_eq!(discover_typed(&root, "thermal_zone", "missing-type"), None);
        assert_eq!(
            discover_typed(&root, "cooling_device", "missing-type"),
            None
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn curve_preserves_interpolation_and_overheat_guard() {
        let c = Config {
            curve: vec![(40, 0), (47, 41), (58, 74), (65, 120), (73, 128), (80, 255)],
            ..Config::default()
        };
        assert_eq!(curve_pwm(&c, -273), None);
        assert_eq!(curve_pwm(&c, 0), None);
        assert_eq!(curve_pwm(&c, 39), Some(0));
        assert_eq!(curve_pwm(&c, 64), Some(113));
        assert_eq!(curve_pwm(&c, 65), Some(120));
        assert_eq!(curve_pwm(&c, 80), Some(255));
    }
}
