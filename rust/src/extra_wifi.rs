use crate::{command, state};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        LazyLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex;

#[derive(Default)]
struct Reconcile {
    signature: String,
    attempts: u8,
}
static NEXT_TICK: AtomicU64 = AtomicU64::new(0);
static RECONCILE: LazyLock<Mutex<[Reconcile; 2]>> =
    LazyLock::new(|| Mutex::new([Reconcile::default(), Reconcile::default()]));

#[derive(Clone)]
pub struct Config {
    pub section: String,
    pub band: String,
    pub ssid: String,
    pub encryption: String,
    pub key: String,
    pub enabled: bool,
    pub hidden: bool,
    pub isolate: bool,
}

#[derive(Clone, Debug)]
struct Live {
    name: String,
    ssid: String,
    frequency: i64,
    index: i64,
}

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.into())
}
fn runtime_dir() -> PathBuf {
    PathBuf::from(env("ZWRT_DATAD_WIFI_RUNTIME_DIR", "/data/zwrt-datad/wifi"))
}
fn path(section: &str, suffix: &str) -> PathBuf {
    runtime_dir().join(format!("{section}.{suffix}"))
}
fn ifname(section: &str) -> &'static str {
    if section == "datad_ssid_1" {
        "wlan4"
    } else {
        "wlan5"
    }
}
fn net_path(name: &str) -> PathBuf {
    PathBuf::from(env("ZWRT_DATAD_NET_CLASS_DIR", "/sys/class/net")).join(name)
}
fn boot_id() -> Result<String, String> {
    fs::read_to_string(env(
        "ZWRT_DATAD_BOOT_ID_PATH",
        "/proc/sys/kernel/random/boot_id",
    ))
    .map(|v| v.trim().to_owned())
    .map_err(|e| e.to_string())
}
fn marker_matches(section: &str) -> bool {
    let Ok(marker) = fs::read_to_string(path(section, "interface")) else {
        return false;
    };
    let mut values = marker.split_whitespace();
    let Some(saved_boot) = values.next() else {
        return false;
    };
    let Some(saved_index) = values.next().and_then(|v| v.parse::<i64>().ok()) else {
        return false;
    };
    values.next().is_none()
        && boot_id().is_ok_and(|v| v == saved_boot)
        && fs::read_to_string(net_path(ifname(section)).join("ifindex"))
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            == Some(saved_index)
}
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(path)
        .map_err(|e| e.to_string())?;
    file.write_all(bytes).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())
}
fn process_owned(section: &str) -> Option<i32> {
    let pid = fs::read_to_string(path(section, "pid"))
        .ok()?
        .trim()
        .parse::<i32>()
        .ok()?;
    if pid <= 1 {
        return None;
    }
    let proc_root = env("ZWRT_DATAD_PROC_ROOT", "/proc");
    let mut cmdline = Vec::new();
    fs::File::open(Path::new(&proc_root).join(pid.to_string()).join("cmdline"))
        .ok()?
        .take(4096)
        .read_to_end(&mut cmdline)
        .ok()?;
    let text = String::from_utf8_lossy(&cmdline);
    let conf = path(section, "conf");
    (text.contains("hostapd") && text.contains(conf.to_string_lossy().as_ref())).then_some(pid)
}
fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

pub async fn stop(section: &str) -> Result<(), String> {
    if let Some(pid) = process_owned(section) {
        unsafe { libc::kill(pid, libc::SIGTERM) };
        for _ in 0..20 {
            if !alive(pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if alive(pid) {
            return Err("owned hostapd did not stop".into());
        }
    }
    if marker_matches(section) {
        command::run(
            &env("ZWRT_DATAD_IW_BIN", "/usr/sbin/iw"),
            ["dev", ifname(section), "del"],
            Duration::from_secs(5),
        )
        .await
        .map_err(|e| e.to_string())?;
    }
    for suffix in ["interface", "pid", "conf"] {
        match fs::remove_file(path(section, suffix)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

fn parse_live(raw: &str) -> Vec<Live> {
    let mut result = Vec::new();
    let mut current: Option<Live> = None;
    for line in raw.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix("Interface ") {
            if let Some(item) = current.take() {
                result.push(item);
            }
            current = Some(Live {
                name: name.into(),
                ssid: String::new(),
                frequency: 0,
                index: 0,
            });
        } else if let Some(item) = current.as_mut() {
            if let Some(ssid) = line.strip_prefix("ssid ") {
                item.ssid = ssid.into();
            } else if let Some(index) = line.strip_prefix("ifindex ") {
                item.index = index.parse().unwrap_or_default();
            } else if let Some(channel) = line.strip_prefix("channel ")
                && let Some(freq) = channel
                    .split_once('(')
                    .and_then(|(_, rest)| rest.split_whitespace().next())
            {
                item.frequency = freq.parse().unwrap_or_default();
            }
        }
    }
    if let Some(item) = current {
        result.push(item);
    }
    result
}

async fn ready(name: &str, extra: bool) -> bool {
    let control = if extra {
        runtime_dir().to_string_lossy().into_owned()
    } else {
        env(
            "ZWRT_DATAD_VENDOR_HOSTAPD_CTRL_DIR",
            "/data/vendor/wifi/hostapd",
        )
    };
    command::run(
        &env("ZWRT_DATAD_HOSTAPD_CLI_BIN", "/usr/sbin/hostapd_cli"),
        ["-p", &control, "-i", name, "status"],
        Duration::from_secs(5),
    )
    .await
    .is_ok_and(|out| String::from_utf8_lossy(&out).contains("state=ENABLED"))
}

async fn base_for(config: &Config) -> Option<Live> {
    let raw = command::run(
        &env("ZWRT_DATAD_IW_BIN", "/usr/sbin/iw"),
        ["dev"],
        Duration::from_secs(5),
    )
    .await
    .ok()?;
    let live = parse_live(&String::from_utf8_lossy(&raw));
    for section in if config.band == "2g" {
        ["main_2g", "guest_2g"]
    } else {
        ["main_5g", "guest_5g"]
    } {
        let ssid = state::uci_read(&format!("wireless.{section}.ssid")).await;
        if let Some(item) = live.iter().find(|item| {
            item.ssid == ssid
                && if config.band == "2g" {
                    item.frequency > 0 && item.frequency < 4900
                } else {
                    item.frequency >= 4900
                }
        }) && ready(&item.name, false).await
        {
            return Some(item.clone());
        }
    }
    None
}

fn render_config(config: &Config, base: &Live) -> Result<(), String> {
    let root = PathBuf::from(env("ZWRT_DATAD_VENDOR_WIFI_DIR", "/data/vendor/wifi"));
    let source = fs::read_to_string(root.join(format!("hostapd-{}.conf", base.name)))
        .map_err(|e| e.to_string())?;
    fs::create_dir_all(runtime_dir()).map_err(|e| e.to_string())?;
    fs::set_permissions(runtime_dir(), fs::Permissions::from_mode(0o700))
        .map_err(|e| e.to_string())?;
    let mut output = String::new();
    for line in source.lines() {
        let key = line.split_once('=').map(|(key, _)| key).unwrap_or("");
        if matches!(
            key,
            "interface"
                | "ssid"
                | "ssid2"
                | "bssid"
                | "ctrl_interface"
                | "bridge"
                | "wpa"
                | "ieee80211w"
                | "ap_isolate"
                | "ignore_broadcast_ssid"
        ) || key.starts_with("wpa_")
            || key.starts_with("sae_")
            || key.starts_with("wps_")
        {
            continue;
        }
        output.push_str(line);
        output.push('\n');
    }
    output.push_str(&format!(
        "\ninterface={}\nctrl_interface={}\nbridge=br-lan\nssid={}\nignore_broadcast_ssid={}\nap_isolate={}\nwps_state=0\n",
        ifname(&config.section), runtime_dir().display(), config.ssid, i32::from(config.hidden), i32::from(config.isolate)
    ));
    if config.encryption == "none" {
        output.push_str("wpa=0\nieee80211w=0\n");
    } else {
        let (management, ieee80211w) = match config.encryption.as_str() {
            "sae" => ("SAE", 2),
            "sae-mixed" => ("WPA-PSK SAE", 1),
            _ => ("WPA-PSK", 0),
        };
        output.push_str(&format!(
            "wpa=2\nwpa_passphrase={}\nrsn_pairwise=CCMP\nwpa_key_mgmt={}\nieee80211w={}\n",
            config.key, management, ieee80211w
        ));
        if matches!(config.encryption.as_str(), "sae" | "sae-mixed") {
            output.push_str("sae_pwe=2\n");
        }
    }
    write_private(&path(&config.section, "conf"), output.as_bytes())
}

async fn start(config: &Config, base: &Live) -> Result<(), String> {
    if process_owned(&config.section).is_some() || marker_matches(&config.section) {
        stop(&config.section).await?;
    }
    if net_path(ifname(&config.section)).exists() {
        return Err("reserved interface is owned by another service".into());
    }
    render_config(config, base)?;
    command::run(
        &env("ZWRT_DATAD_IW_BIN", "/usr/sbin/iw"),
        [
            "dev",
            &base.name,
            "interface",
            "add",
            ifname(&config.section),
            "type",
            "__ap",
        ],
        Duration::from_secs(5),
    )
    .await
    .map_err(|e| e.to_string())?;
    let index = fs::read_to_string(net_path(ifname(&config.section)).join("ifindex"))
        .map_err(|e| e.to_string())?;
    write_private(
        &path(&config.section, "interface"),
        format!("{} {}\n", boot_id()?, index.trim()).as_bytes(),
    )?;
    write_private(&path(&config.section, "log"), b"")?;
    let pidfile = path(&config.section, "pid").to_string_lossy().into_owned();
    let logfile = path(&config.section, "log").to_string_lossy().into_owned();
    let conffile = path(&config.section, "conf").to_string_lossy().into_owned();
    let result = command::run(
        &env("ZWRT_DATAD_HOSTAPD_BIN", "/usr/sbin/hostapd"),
        ["-B", "-P", &pidfile, "-f", &logfile, &conffile],
        Duration::from_secs(5),
    )
    .await;
    if let Err(error) = result {
        let _ = command::run(
            &env("ZWRT_DATAD_IW_BIN", "/usr/sbin/iw"),
            ["dev", ifname(&config.section), "del"],
            Duration::from_secs(5),
        )
        .await;
        return Err(error.to_string());
    }
    Ok(())
}

pub async fn apply(config: &Config) -> Result<bool, String> {
    if !config.enabled {
        stop(&config.section).await?;
        return Ok(false);
    }
    let radio = if config.band == "2g" {
        "wifi0"
    } else {
        "wifi1"
    };
    if state::uci_read(&format!("wireless.{radio}.disabled")).await == "1" {
        stop(&config.section).await?;
        return Ok(false);
    }
    let Some(base) = base_for(config).await else {
        return Ok(false);
    };
    if process_owned(&config.section).is_some()
        && marker_matches(&config.section)
        && ready(ifname(&config.section), true).await
    {
        return Ok(true);
    }
    start(config, &base).await?;
    Ok(true)
}

pub async fn reset_attempts(section: &str) {
    let slot = usize::from(section == "datad_ssid_2");
    RECONCILE.lock().await[slot] = Reconcile::default();
}

async fn read(section: &str) -> Option<Config> {
    if state::uci_read(&format!("datad_wifi.{section}"))
        .await
        .is_empty()
    {
        return None;
    }
    let value = |option: &'static str| async move {
        state::uci_read(&format!("datad_wifi.{section}.{option}")).await
    };
    Some(Config {
        section: section.into(),
        band: value("band").await,
        ssid: value("ssid").await,
        encryption: value("encryption").await,
        key: value("key").await,
        enabled: value("disabled").await != "1",
        hidden: value("hidden").await == "1",
        isolate: value("isolate").await == "1",
    })
}

pub async fn tick() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if NEXT_TICK
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
            (now >= next).then_some(now + 5)
        })
        .is_err()
    {
        return;
    }
    for (slot, section) in ["datad_ssid_1", "datad_ssid_2"].into_iter().enumerate() {
        if let Some(config) = read(section).await {
            let signature = format!(
                "{}\0{}\0{}\0{}\0{}\0{}\0{}",
                config.band,
                config.ssid,
                config.encryption,
                config.key,
                config.enabled,
                config.hidden,
                config.isolate
            );
            {
                let mut states = RECONCILE.lock().await;
                if states[slot].signature != signature {
                    states[slot] = Reconcile {
                        signature,
                        attempts: 0,
                    };
                }
                if states[slot].attempts >= 3 {
                    continue;
                }
            }
            match apply(&config).await {
                Ok(false) => {}
                Ok(true) | Err(_) => {
                    RECONCILE.lock().await[slot].attempts += 1;
                }
            }
        } else {
            let _ = stop(section).await;
            RECONCILE.lock().await[slot] = Reconcile::default();
        }
    }
}
