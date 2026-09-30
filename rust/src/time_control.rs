//! Device-owned time calibration. Disabled by default; no vendor store or UI dependencies.
mod ntp;
mod platform;
use platform::Platform;
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex as AsyncMutex, watch};

struct ClockDomain {
    leases: AtomicIsize,
    waiting: AtomicBool,
    generation: AtomicU64,
}
impl ClockDomain {
    fn new() -> Self {
        Self {
            leases: AtomicIsize::new(0),
            waiting: AtomicBool::new(false),
            generation: AtomicU64::new(0),
        }
    }
    fn read(self: &Arc<Self>) -> Option<ClockSensitiveOperation> {
        loop {
            if self.waiting.load(Ordering::SeqCst) {
                return None;
            }
            let current = self.leases.load(Ordering::SeqCst);
            if current < 0 || current == isize::MAX {
                return None;
            }
            if self
                .leases
                .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Some(ClockSensitiveOperation {
                    domain: self.clone(),
                });
            }
        }
    }
}
fn global_domain() -> &'static Arc<ClockDomain> {
    static GLOBAL: std::sync::OnceLock<Arc<ClockDomain>> = std::sync::OnceLock::new();
    GLOBAL.get_or_init(|| Arc::new(ClockDomain::new()))
}
#[cfg(test)]
thread_local! { static TEST_STATE: std::cell::Cell<Option<(bool,u64)>> = const { std::cell::Cell::new(None) }; }
#[cfg(test)]
pub(crate) fn with_clock_state<R>(changing: bool, generation: u64, run: impl FnOnce() -> R) -> R {
    struct Reset(Option<(bool, u64)>);
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_STATE.with(|s| s.set(self.0));
        }
    }
    let _reset = Reset(TEST_STATE.with(|s| s.replace(Some((changing, generation)))));
    run()
}
pub(crate) fn clock_change_in_progress() -> bool {
    #[cfg(test)]
    if let Some((changing, _)) = TEST_STATE.with(|s| s.get()) {
        return changing;
    }
    global_domain().leases.load(Ordering::SeqCst) < 0
}
pub(crate) fn clock_generation() -> u64 {
    #[cfg(test)]
    if let Some((_, generation)) = TEST_STATE.with(|s| s.get()) {
        return generation;
    }
    global_domain().generation.load(Ordering::SeqCst)
}
pub(crate) struct ClockSensitiveOperation {
    domain: Arc<ClockDomain>,
}
pub(crate) fn clock_sensitive_operation() -> Option<ClockSensitiveOperation> {
    #[cfg(test)]
    if TEST_STATE.with(|s| s.get()).is_some_and(|s| s.0) {
        return None;
    }
    global_domain().read()
}
impl Drop for ClockSensitiveOperation {
    fn drop(&mut self) {
        self.domain.leases.fetch_sub(1, Ordering::SeqCst);
    }
}
struct Waiting(Arc<ClockDomain>);
impl Drop for Waiting {
    fn drop(&mut self) {
        self.0.waiting.store(false, Ordering::SeqCst);
    }
}
struct Transition {
    domain: Arc<ClockDomain>,
    _waiting: Waiting,
}
impl Transition {
    async fn begin(domain: Arc<ClockDomain>) -> Result<Self, &'static str> {
        if domain.waiting.swap(true, Ordering::SeqCst) {
            return Err("clock_operation_busy");
        }
        let waiting = Waiting(domain.clone());
        let deadline = crate::elapsed::now() + Duration::from_secs(45);
        loop {
            if domain
                .leases
                .compare_exchange(0, -1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Ok(Self {
                    domain,
                    _waiting: waiting,
                });
            }
            if crate::elapsed::now() >= deadline {
                return Err("clock_operation_busy");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
impl Drop for Transition {
    fn drop(&mut self) {
        self.domain.generation.fetch_add(1, Ordering::SeqCst);
        self.domain.leases.store(0, Ordering::SeqCst);
    }
}
fn new_tag() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn valid_tag(tag: &str) -> bool {
    tag.len() == 32 && tag.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    schema: u8,
    server: String,
    calibration_enabled: bool,
    boot_sync_enabled: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            schema: 1,
            server: "ntp.aliyun.com".into(),
            calibration_enabled: false,
            boot_sync_enabled: true,
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigRequest {
    server: Option<String>,
    calibration_enabled: Option<bool>,
    boot_sync_enabled: Option<bool>,
    operation_tag: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncRequest {
    server: Option<String>,
    operation_tag: Option<String>,
}
#[derive(Clone, Serialize)]
struct Operation {
    tag: String,
    kind: &'static str,
    phase: &'static str,
    configuration_saved: bool,
    verified: Option<bool>,
    error_code: Option<&'static str>,
    before_epoch: Option<i64>,
    after_epoch: Option<i64>,
    offset_ms: Option<i64>,
    server: Option<String>,
}
struct Work {
    tag: String,
    seq: u64,
    server: String,
    must_sync: bool,
    boot: bool,
    restore: bool,
    only_drifted: bool,
}
struct Receipt {
    operation: Operation,
    fingerprint: [u8; 32],
}
#[derive(Clone, Serialize)]
struct LastSync {
    operation_tag: String,
    server: String,
    at_epoch: Option<i64>,
    offset_ms: Option<i64>,
    applied: bool,
    verified: bool,
    error_code: Option<&'static str>,
}
struct Anchor {
    utc_ns: i128,
    elapsed: Duration,
    boot: Option<String>,
}
struct State {
    config: Config,
    seq: u64,
    revision: u64,
    phase: &'static str,
    verified: Option<bool>,
    error: Option<&'static str>,
    anchor: Option<Anchor>,
    last_sync: Option<LastSync>,
    operations: VecDeque<Receipt>,
    timezone_verified: Option<bool>,
    oem_disabled: Option<bool>,
    rtc_verified: Option<bool>,
    guard_corrections: u64,
}
struct Inner {
    state: Mutex<State>,
    work: AsyncMutex<()>,
    cancel: watch::Sender<u64>,
    platform: Platform,
    domain: Arc<ClockDomain>,
    path: PathBuf,
    storage_ok: bool,
}
#[derive(Clone)]
pub(crate) struct Manager {
    inner: Arc<Inner>,
}

fn load(path: &Path) -> Result<Config, &'static str> {
    let meta = match fs::symlink_metadata(path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(_) => return Err("config_read_failed"),
    };
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.len() > 4096
        || meta.permissions().mode() & 0o077 != 0
    {
        return Err("config_invalid");
    };
    let mut value: Config =
        serde_json::from_slice(&fs::read(path).map_err(|_| "config_read_failed")?)
            .map_err(|_| "config_invalid")?;
    if value.schema != 1 {
        return Err("config_invalid");
    };
    value.server = ntp::Endpoint::parse(&value.server)?.name;
    Ok(value)
}
fn epoch(ns: Option<i128>) -> Option<i64> {
    ns.and_then(|n| (n / 1_000_000_000).try_into().ok())
}
fn formatted(seconds: Option<i64>, offset: i64) -> Option<String> {
    let seconds = seconds?.checked_add(offset)?;
    let value = seconds as libc::time_t;
    if value as i128 != seconds as i128 {
        return None;
    };
    let mut parts = std::mem::MaybeUninit::<libc::tm>::uninit();
    if unsafe { libc::gmtime_r(&value, parts.as_mut_ptr()) }.is_null() {
        return None;
    };
    let p = unsafe { parts.assume_init() };
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        p.tm_year + 1900,
        p.tm_mon + 1,
        p.tm_mday,
        p.tm_hour,
        p.tm_min,
        p.tm_sec
    ))
}

impl Manager {
    pub fn new(data_dir: &Path) -> Self {
        let dir = data_dir.join("time-control");
        let storage_ok = (|| {
            if !dir.exists() {
                fs::create_dir(&dir).map_err(|_| ())?;
                fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).map_err(|_| ())?
            };
            let m = fs::symlink_metadata(&dir).map_err(|_| ())?;
            if !m.is_dir()
                || m.uid() != unsafe { libc::geteuid() }
                || m.permissions().mode() & 0o077 != 0
            {
                return Err(());
            };
            Ok(())
        })()
        .is_ok();
        let path = dir.join("config.json");
        let loaded = load(&path);
        let error = loaded.as_ref().err().copied();
        let config = loaded.unwrap_or_default();
        let (cancel, _) = watch::channel(0);
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    config,
                    seq: 0,
                    revision: 0,
                    phase: if error.is_some() { "failed" } else { "idle" },
                    verified: None,
                    error,
                    anchor: None,
                    last_sync: None,
                    operations: VecDeque::new(),
                    timezone_verified: None,
                    oem_disabled: None,
                    rtc_verified: None,
                    guard_corrections: 0,
                }),
                work: AsyncMutex::new(()),
                cancel,
                platform: Platform::new(&dir),
                domain: global_domain().clone(),
                path,
                storage_ok,
            }),
        }
    }
    pub fn status(&self) -> Value {
        let s = self.inner.state.lock().unwrap();
        let e = epoch(self.inner.platform.clock.wall());
        let journal = self.inner.platform.load_journal();
        let owned = journal.as_ref().ok().and_then(|j| j.as_ref());
        let guard = s.config.calibration_enabled
            && s.anchor
                .as_ref()
                .is_some_and(|a| a.boot == crate::elapsed::boot_id())
            && s.timezone_verified == Some(true)
            && s.phase != "conflict";
        json!({"supported":true,"write_supported":self.inner.platform.write_supported()&&self.inner.storage_ok,
            "epoch":e,"utc":formatted(e,0),"beijing":formatted(e,28800),"server":s.config.server,
            "calibration_enabled":s.config.calibration_enabled,"boot_sync_enabled":s.config.boot_sync_enabled,
            "phase":s.phase,"guard_active":guard,"verified":s.verified,"error_code":s.error,
            "revision":s.revision,"clock_generation":self.inner.domain.generation.load(Ordering::SeqCst),"authenticated":false,
            "last_sync":s.last_sync,"last_operation":s.operations.back().map(|r|&r.operation),
            "recent_operations":s.operations.iter().map(|r|&r.operation).collect::<Vec<_>>(),
            "oem":{"supported":self.inner.platform.oem_supported(),"owned":owned.is_some_and(|j|j.oem_owned()),"disabled":s.oem_disabled,"restore_pending":self.inner.platform.journal_present()&&!s.config.calibration_enabled&&matches!(s.phase,"restoring"|"failed"|"conflict"),"error_code":if journal.is_err(){journal.as_ref().err().copied()}else{None}},
            "timezone":{"managed":owned.is_some(),"verified":s.timezone_verified,"restore_pending":self.inner.platform.journal_present()&&!s.config.calibration_enabled&&matches!(s.phase,"restoring"|"failed"|"conflict")},
            "rtc":{"supported":self.inner.platform.rtc_supported(),"verified":s.rtc_verified},"guard_corrections":s.guard_corrections})
    }
    fn remember(
        s: &mut State,
        tag: String,
        kind: &'static str,
        saved: bool,
        fingerprint: [u8; 32],
    ) -> Result<bool, &'static str> {
        if let Some(old) = s.operations.iter().find(|r| r.operation.tag == tag) {
            return if old.fingerprint == fingerprint {
                Ok(false)
            } else {
                Err("operation_tag_conflict")
            };
        }
        if s.operations.len() >= 16 {
            let Some(index) = s
                .operations
                .iter()
                .position(|r| matches!(r.operation.phase, "succeeded" | "failed"))
            else {
                return Err("operation_busy");
            };
            s.operations.remove(index);
        }
        s.operations.push_back(Receipt {
            fingerprint,
            operation: Operation {
                tag,
                kind,
                phase: "queued",
                configuration_saved: saved,
                verified: None,
                error_code: None,
                before_epoch: None,
                after_epoch: None,
                offset_ms: None,
                server: None,
            },
        });
        Ok(true)
    }
    pub fn submit(&self, action: &str, params: Value) -> Result<Value, &'static str> {
        if params
            .as_object()
            .is_none_or(|fields| fields.values().any(Value::is_null))
        {
            return Err("invalid_parameter");
        };
        if !self.inner.storage_ok {
            return Err("config_storage_failed");
        };
        let fingerprint: [u8; 32] = Sha256::digest(
            serde_json::to_vec(&json!({"action":action,"params":params}))
                .map_err(|_| "invalid_parameter")?,
        )
        .into();
        let mut s = self.inner.state.lock().unwrap();
        let (tag, server, configuration, disable) = match action {
            "time.config.set" => {
                let req: ConfigRequest =
                    serde_json::from_value(params).map_err(|_| "invalid_parameter")?;
                if req.server.is_none()
                    && req.calibration_enabled.is_none()
                    && req.boot_sync_enabled.is_none()
                {
                    return Err("invalid_parameter");
                };
                let mut config = s.config.clone();
                if let Some(server) = req.server {
                    config.server = ntp::Endpoint::parse(&server)?.name
                };
                if let Some(v) = req.calibration_enabled {
                    config.calibration_enabled = v
                };
                if let Some(v) = req.boot_sync_enabled {
                    config.boot_sync_enabled = v
                };
                (
                    req.operation_tag.unwrap_or_else(new_tag),
                    config.server.clone(),
                    Some(config),
                    req.calibration_enabled == Some(false),
                )
            }
            "time.sync" => {
                let req: SyncRequest =
                    serde_json::from_value(params).map_err(|_| "invalid_parameter")?;
                let server = if let Some(server) = req.server.filter(|v| !v.is_empty()) {
                    ntp::Endpoint::parse(&server)?.name
                } else {
                    s.config.server.clone()
                };
                (
                    req.operation_tag.unwrap_or_else(new_tag),
                    server,
                    None,
                    false,
                )
            }
            _ => return Err("unsupported_action"),
        };
        if !valid_tag(&tag) {
            return Err("invalid_parameter");
        };
        if let Some(old) = s.operations.iter().find(|r| r.operation.tag == tag) {
            return if old.fingerprint == fingerprint {
                Ok(
                    json!({"accepted":true,"operation_tag":tag,"configuration_saved":old.operation.configuration_saved,"verified":old.operation.verified}),
                )
            } else {
                Err("operation_tag_conflict")
            };
        }
        if s.operations
            .iter()
            .any(|r| matches!(r.operation.phase, "queued" | "running"))
            && !disable
        {
            return Err("operation_busy");
        };
        if (configuration
            .as_ref()
            .is_some_and(|c| c.calibration_enabled)
            || action == "time.sync")
            && !self.inner.platform.write_supported()
        {
            return Err("clock_permission_denied");
        };
        if s.operations.len() >= 16
            && !s
                .operations
                .iter()
                .any(|r| matches!(r.operation.phase, "succeeded" | "failed"))
        {
            return Err("operation_busy");
        };
        let previous = s.config.clone();
        let saved = configuration.is_some();
        if let Some(config) = configuration {
            platform::atomic(
                &self.inner.path,
                &serde_json::to_vec(&config).map_err(|_| "config_storage_failed")?,
                0o600,
            )
            .map_err(|_| "config_storage_failed")?;
            s.config = config;
            s.seq = s.seq.wrapping_add(1);
            self.inner.cancel.send_replace(s.seq);
        }
        let seq = s.seq;
        let must_sync = action == "time.sync"
            || s.config.calibration_enabled
                && (!previous.calibration_enabled
                    || previous.server != server
                    || s.anchor.is_none()
                    || s.phase != "synchronized");
        Self::remember(
            &mut s,
            tag.clone(),
            if saved { "config" } else { "sync" },
            saved,
            fingerprint,
        )?;
        s.phase = "queued";
        s.verified = None;
        s.error = None;
        s.revision = s.revision.wrapping_add(1);
        if disable {
            s.anchor = None
        };
        drop(s);
        let worker = self.clone();
        let task_tag = tag.clone();
        tokio::spawn(async move {
            worker
                .run(Work {
                    tag: task_tag,
                    seq,
                    server,
                    must_sync,
                    boot: false,
                    restore: disable,
                    only_drifted: false,
                })
                .await;
        });
        Ok(
            json!({"accepted":true,"operation_tag":tag,"configuration_saved":saved,"verified":Value::Null}),
        )
    }
    fn update(&self, tag: &str, phase: &'static str, error: Option<&'static str>, done: bool) {
        let mut s = self.inner.state.lock().unwrap();
        if let Some(r) = s.operations.iter_mut().find(|r| r.operation.tag == tag) {
            r.operation.phase = if done {
                if error.is_none() {
                    "succeeded"
                } else {
                    "failed"
                }
            } else {
                "running"
            };
            r.operation.verified = if done { Some(error.is_none()) } else { None };
            r.operation.error_code = error;
        }
        if s.operations.back().is_some_and(|r| r.operation.tag == tag) {
            s.phase = phase;
            s.error = error;
            s.verified = if done { Some(error.is_none()) } else { None };
        }
        s.revision = s.revision.wrapping_add(1);
    }
    async fn sample(
        &self,
        seq: u64,
        server: &str,
        boot: bool,
    ) -> Result<(i128, String), &'static str> {
        #[cfg(test)]
        if let Some(result) = self.inner.platform.clock.fixture_sample() {
            return result.map(|n| (n, server.into()));
        }
        let mut changed = self.inner.cancel.subscribe();
        let deadline = crate::elapsed::now() + Duration::from_secs(if boot { 90 } else { 8 });
        loop {
            if self.inner.state.lock().unwrap().seq != seq {
                return Err("operation_cancelled");
            };
            let remaining = deadline.saturating_sub(crate::elapsed::now());
            if remaining.is_zero() {
                return Err("ntp_timeout");
            };
            let query =
                tokio::time::timeout(remaining.min(Duration::from_secs(5)), ntp::first(server));
            let answer = tokio::select! {v=query=>v.map_err(|_|"ntp_timeout")?,_=changed.changed()=>return Err("operation_cancelled")};
            if crate::elapsed::now() >= deadline {
                return Err("ntp_timeout");
            };
            if answer.is_ok() || !boot {
                return answer;
            };
            let wait = deadline
                .saturating_sub(crate::elapsed::now())
                .min(Duration::from_secs(3));
            tokio::select! {_=tokio::time::sleep(wait)=>{},_=changed.changed()=>return Err("operation_cancelled")}
        }
    }
    async fn run(&self, work: Work) {
        let Work {
            tag,
            seq,
            server,
            must_sync,
            boot,
            restore,
            only_drifted,
        } = work;
        let _serial = self.inner.work.lock().await;
        if self.inner.state.lock().unwrap().seq != seq {
            self.update(&tag, "failed", Some("operation_cancelled"), true);
            return;
        };
        self.update(&tag, "applying", None, false);
        let calibration = self.inner.state.lock().unwrap().config.calibration_enabled;
        if !calibration && !must_sync {
            let result = if restore && self.inner.platform.journal_present() {
                self.update(&tag, "restoring", None, false);
                match Transition::begin(self.inner.domain.clone()).await {
                    Ok(_gate) => match self.inner.platform.load_journal() {
                        Ok(Some(mut journal)) => {
                            let result = tokio::time::timeout(
                                Duration::from_secs(30),
                                self.inner.platform.restore(&mut journal),
                            )
                            .await
                            .unwrap_or(Err("restore_timeout"));
                            self.inner.state.lock().unwrap().oem_disabled = if result.is_ok() {
                                journal.service_disabled_before()
                            } else {
                                None
                            };
                            result
                        }
                        Ok(None) => Ok(()),
                        Err(e) => Err(e),
                    },
                    Err(e) => Err(e),
                }
            } else {
                Ok(())
            };
            self.update(
                &tag,
                if result == Err("restore_conflict") {
                    "conflict"
                } else if result.is_err() {
                    "failed"
                } else {
                    "idle"
                },
                result.err(),
                true,
            );
            return;
        }
        if !must_sync {
            let ownership = self.inner.platform.ownership_valid().await;
            let clock_ok = {
                let s = self.inner.state.lock().unwrap();
                s.anchor.as_ref().is_some_and(|a| {
                    a.boot == crate::elapsed::boot_id()
                        && self.inner.platform.clock.wall().is_some_and(|n| {
                            (n - (a.utc_ns
                                + crate::elapsed::now().saturating_sub(a.elapsed).as_nanos()
                                    as i128))
                                .abs()
                                <= 5_000_000_000
                        })
                })
            };
            let error = ownership.err().or(if clock_ok {
                None
            } else {
                Some("clock_readback_failed")
            });
            self.update(
                &tag,
                if error == Some("restore_conflict") {
                    "conflict"
                } else if error.is_some() {
                    "failed"
                } else {
                    "synchronized"
                },
                error,
                true,
            );
            return;
        };
        let (utc, used) = match self.sample(seq, &server, boot).await {
            Ok(v) => v,
            Err(e) => {
                self.update(&tag, "failed", Some(e), true);
                return;
            }
        };
        let received = crate::elapsed::now();
        let before = self.inner.platform.clock.wall();
        if only_drifted && before.is_some_and(|n| (n - utc).abs() <= 5_000_000_000) {
            if let Err(error) = self.inner.platform.ownership_valid().await {
                self.inner.state.lock().unwrap().oem_disabled = None;
                self.update(
                    &tag,
                    if error == "restore_conflict" {
                        "conflict"
                    } else {
                        "failed"
                    },
                    Some(error),
                    true,
                );
                return;
            }
            let mut s = self.inner.state.lock().unwrap();
            s.anchor = Some(Anchor {
                utc_ns: utc,
                elapsed: received,
                boot: crate::elapsed::boot_id(),
            });
            s.last_sync = Some(LastSync {
                operation_tag: tag.clone(),
                server: used,
                at_epoch: epoch(before),
                offset_ms: before.and_then(|n| ((n - utc) / 1_000_000).try_into().ok()),
                applied: false,
                verified: true,
                error_code: None,
            });
            drop(s);
            self.update(&tag, "synchronized", None, true);
            return;
        }
        let _gate = match Transition::begin(self.inner.domain.clone()).await {
            Ok(v) => v,
            Err(e) => {
                self.update(&tag, "failed", Some(e), true);
                return;
            }
        };
        let mut journal = match self.inner.platform.prepare(calibration).await {
            Ok(v) => v,
            Err(e) => {
                self.update(
                    &tag,
                    if e == "restore_conflict" {
                        "conflict"
                    } else {
                        "failed"
                    },
                    Some(e),
                    true,
                );
                return;
            }
        };
        let applied = tokio::time::timeout(Duration::from_secs(25), async {
            if self.inner.state.lock().unwrap().seq != seq {
                return Err("operation_cancelled");
            };
            if calibration {
                self.inner.platform.take_over(&mut journal).await?;
                self.inner.state.lock().unwrap().oem_disabled = if journal.oem_owned() {
                    Some(true)
                } else {
                    None
                };
            };
            self.inner.platform.timezone(&mut journal).await?;
            if self.inner.state.lock().unwrap().seq != seq {
                return Err("operation_cancelled");
            };
            let target = utc + crate::elapsed::now().saturating_sub(received).as_nanos() as i128;
            self.inner.platform.clock.set(target)?;
            let actual = self
                .inner
                .platform
                .clock
                .wall()
                .ok_or("clock_readback_failed")?;
            if (actual - target).abs() > 2_000_000_000 {
                return Err("clock_readback_failed");
            };
            self.inner.platform.seal(&mut journal)?;
            let rtc = self.inner.platform.rtc().await;
            let mut s = self.inner.state.lock().unwrap();
            s.anchor = if calibration {
                Some(Anchor {
                    utc_ns: actual,
                    elapsed: crate::elapsed::now(),
                    boot: crate::elapsed::boot_id(),
                })
            } else {
                None
            };
            s.timezone_verified = Some(true);
            s.rtc_verified = rtc;
            Ok(())
        })
        .await
        .unwrap_or(Err("apply_timeout"));
        if let Err(error) = applied {
            self.update(&tag, "restoring", Some(error), false);
            let restored = tokio::time::timeout(
                Duration::from_secs(30),
                self.inner.platform.restore(&mut journal),
            )
            .await
            .unwrap_or(Err("restore_timeout"));
            let error = restored.err().unwrap_or(error);
            let mut s = self.inner.state.lock().unwrap();
            s.anchor = None;
            s.timezone_verified = Some(false);
            s.oem_disabled = if restored.is_ok() {
                journal.service_disabled_before()
            } else {
                None
            };
            drop(s);
            self.update(
                &tag,
                if error == "restore_conflict" {
                    "conflict"
                } else {
                    "failed"
                },
                Some(error),
                true,
            );
            return;
        }
        let after = self.inner.platform.clock.wall();
        let offset = before.and_then(|n| ((n - utc) / 1_000_000).try_into().ok());
        let mut s = self.inner.state.lock().unwrap();
        s.last_sync = Some(LastSync {
            operation_tag: tag.clone(),
            server: used.clone(),
            at_epoch: epoch(after),
            offset_ms: offset,
            applied: true,
            verified: true,
            error_code: None,
        });
        if let Some(r) = s.operations.iter_mut().find(|r| r.operation.tag == tag) {
            r.operation.before_epoch = epoch(before);
            r.operation.after_epoch = epoch(after);
            r.operation.offset_ms = offset;
            r.operation.server = Some(used);
        }
        if s.config.server != server {
            let mut config = s.config.clone();
            config.server = server;
            match platform::atomic(
                &self.inner.path,
                &serde_json::to_vec(&config).unwrap_or_default(),
                0o600,
            ) {
                Ok(()) => {
                    s.config = config;
                    if let Some(r) = s.operations.iter_mut().find(|r| r.operation.tag == tag) {
                        r.operation.configuration_saved = true
                    }
                }
                Err(_) => {
                    drop(s);
                    self.update(&tag, "failed", Some("config_storage_failed"), true);
                    return;
                }
            }
        }
        drop(s);
        self.update(&tag, "synchronized", None, true);
    }
    pub fn start(&self) {
        let boot = self.clone();
        tokio::spawn(async move {
            let (enabled, sync, seq, server) = {
                let s = boot.inner.state.lock().unwrap();
                (
                    s.config.calibration_enabled,
                    s.config.boot_sync_enabled,
                    s.seq,
                    s.config.server.clone(),
                )
            };
            if (!enabled && boot.inner.platform.journal_present()) || (enabled && sync) {
                let tag = new_tag();
                {
                    let mut s = boot.inner.state.lock().unwrap();
                    let _ = Self::remember(&mut s, tag.clone(), "config", true, [0; 32]);
                }
                boot.run(Work {
                    tag,
                    seq,
                    server: server.clone(),
                    must_sync: enabled,
                    boot: true,
                    restore: !enabled,
                    only_drifted: false,
                })
                .await;
                if enabled && boot.inner.state.lock().unwrap().phase == "synchronized" {
                    let started = crate::elapsed::now();
                    for seconds in [60, 180] {
                        tokio::time::sleep(
                            (started + Duration::from_secs(seconds))
                                .saturating_sub(crate::elapsed::now()),
                        )
                        .await;
                        let valid = {
                            let s = boot.inner.state.lock().unwrap();
                            s.seq == seq
                                && s.config.calibration_enabled
                                && s.config.boot_sync_enabled
                        };
                        if !valid {
                            return;
                        };
                        let tag = new_tag();
                        {
                            let mut s = boot.inner.state.lock().unwrap();
                            if s.operations
                                .iter()
                                .any(|r| matches!(r.operation.phase, "queued" | "running"))
                            {
                                continue;
                            };
                            let _ = Self::remember(&mut s, tag.clone(), "sync", false, [0; 32]);
                        }
                        boot.run(Work {
                            tag,
                            seq,
                            server: server.clone(),
                            must_sync: true,
                            boot: false,
                            restore: false,
                            only_drifted: true,
                        })
                        .await;
                    }
                }
            }
        });
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                let Some(inner) = weak.upgrade() else { return };
                Manager { inner }.guard().await;
            }
        });
    }
    async fn guard(&self) {
        let Ok(_serial) = self.inner.work.try_lock() else {
            return;
        };
        let expected = {
            let s = self.inner.state.lock().unwrap();
            if !s.config.calibration_enabled
                || s.phase == "conflict"
                || s.timezone_verified != Some(true)
            {
                return;
            };
            let Some(a) = &s.anchor else { return };
            if a.boot != crate::elapsed::boot_id() {
                return;
            };
            a.utc_ns + crate::elapsed::now().saturating_sub(a.elapsed).as_nanos() as i128
        };
        let Some(actual) = self.inner.platform.clock.wall() else {
            return;
        };
        // Correct the specifically observed positive OEM UTC+8 step. Do not
        // undo arbitrary changes made by other time owners or a person.
        let ahead = actual - expected;
        if !(28_680_000_000_000..=28_920_000_000_000).contains(&ahead) {
            return;
        };
        if let Err(error) = self.inner.platform.ownership_valid().await {
            let mut s = self.inner.state.lock().unwrap();
            s.phase = "conflict";
            s.oem_disabled = None;
            s.error = Some(error);
            s.verified = Some(false);
            s.revision = s.revision.wrapping_add(1);
            return;
        };
        let Ok(_gate) = Transition::begin(self.inner.domain.clone()).await else {
            return;
        };
        let expected = {
            let s = self.inner.state.lock().unwrap();
            let Some(a) = &s.anchor else { return };
            a.utc_ns + crate::elapsed::now().saturating_sub(a.elapsed).as_nanos() as i128
        };
        let result = self.inner.platform.clock.set(expected).and_then(|_| {
            if self
                .inner
                .platform
                .clock
                .wall()
                .is_some_and(|n| (n - expected).abs() < 2_000_000_000)
            {
                Ok(())
            } else {
                Err("clock_readback_failed")
            }
        });
        let mut s = self.inner.state.lock().unwrap();
        s.revision = s.revision.wrapping_add(1);
        if let Err(e) = result {
            s.phase = "failed";
            s.error = Some(e);
            s.verified = Some(false)
        } else {
            s.guard_corrections += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform::Clock;
    use std::sync::atomic::{AtomicBool, AtomicI64};
    struct FakeClock {
        base: AtomicI64,
        set_fail: AtomicBool,
    }
    impl Clock for FakeClock {
        fn wall(&self) -> Option<i128> {
            Some(i128::from(self.base.load(Ordering::SeqCst)) * 1_000_000_000)
        }
        fn writable(&self) -> bool {
            true
        }
        fn set(&self, n: i128) -> Result<(), &'static str> {
            if self.set_fail.load(Ordering::SeqCst) {
                return Err("clock_apply_failed");
            };
            self.base
                .store((n / 1_000_000_000) as i64, Ordering::SeqCst);
            Ok(())
        }
        fn fixture_sample(&self) -> Option<Result<i128, &'static str>> {
            Some(Ok(1_800_000_000_000_000_000))
        }
    }
    struct Fixture {
        root: PathBuf,
        manager: Manager,
        clock: Arc<FakeClock>,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("datad-time-{}", new_tag()));
            for path in ["etc/init.d", "tmp", "data/time-control"] {
                fs::create_dir_all(root.join(path)).unwrap();
            }
            fs::set_permissions(
                root.join("data/time-control"),
                fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            fs::write(root.join("etc/localtime"), b"original-localtime").unwrap();
            fs::write(root.join("tmp/TZ"), b"UTC0\n").unwrap();
            let script = root.join("etc/init.d/zte_topsw_ntp");
            let text = format!(
                "#!/bin/sh\nbase='{}'\ncase \"$1\" in\nenabled) test -f \"$base/enabled\";;\nstatus) test -f \"$base/running\";;\nenable) touch \"$base/enabled\";;\ndisable) rm -f \"$base/enabled\";;\nstart) touch \"$base/running\";;\nstop) rm -f \"$base/running\";;\n*) exit 2;;\nesac\n",
                root.display()
            );
            fs::write(&script, text).unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(root.join("enabled"), b"").unwrap();
            fs::write(root.join("running"), b"").unwrap();
            let clock = Arc::new(FakeClock {
                base: AtomicI64::new(1_800_028_800),
                set_fail: AtomicBool::new(false),
            });
            let mut manager = Manager::new(&root.join("data"));
            let inner = Arc::get_mut(&mut manager.inner).unwrap();
            inner.platform =
                Platform::fake(&root.join("data/time-control"), root.clone(), clock.clone());
            inner.domain = Arc::new(ClockDomain::new());
            Self {
                root,
                manager,
                clock,
            }
        }
        async fn wait(&self, tag: &str) -> Value {
            for _ in 0..100 {
                let status = self.manager.status();
                if let Some(op) = status["recent_operations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|v| {
                        v["tag"] == tag
                            && matches!(v["phase"].as_str(), Some("succeeded" | "failed"))
                    })
                {
                    return op.clone();
                };
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("operation did not terminate: {}", self.manager.status());
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    #[tokio::test]
    async fn default_off_has_no_clock_oem_or_timezone_side_effects() {
        let f = Fixture::new();
        f.manager.start();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(f.manager.status()["calibration_enabled"], false);
        assert!(f.root.join("enabled").exists());
        assert!(f.root.join("running").exists());
        assert_eq!(
            fs::read(f.root.join("etc/localtime")).unwrap(),
            b"original-localtime"
        );
        assert_eq!(f.clock.base.load(Ordering::SeqCst), 1_800_028_800);
        assert_eq!(f.manager.status()["last_operation"], Value::Null);
    }
    #[tokio::test]
    async fn manual_sync_off_does_not_disable_oem_and_correlates_verified_receipt() {
        let f = Fixture::new();
        let tag = "A".repeat(32);
        let ack = f
            .manager
            .submit(
                "time.sync",
                json!({"operation_tag":tag,"server":"127.0.0.1:12345"}),
            )
            .unwrap();
        assert_eq!(ack["verified"], Value::Null);
        let op = f.wait(&tag).await;
        assert_eq!(op["phase"], "succeeded");
        assert_eq!(op["verified"], true);
        assert_eq!(op["tag"], tag);
        assert!(f.root.join("enabled").exists());
        assert!(f.root.join("running").exists());
        assert_eq!(f.manager.status()["guard_active"], false);
        assert_eq!(f.manager.status()["last_sync"]["operation_tag"], tag);
        let generation = f.manager.status()["clock_generation"].clone();
        f.manager
            .submit(
                "time.sync",
                json!({"operation_tag":tag,"server":"127.0.0.1:12345"}),
            )
            .unwrap();
        assert_eq!(f.manager.status()["clock_generation"], generation);
        assert_eq!(
            f.manager.submit(
                "time.sync",
                json!({"operation_tag":tag,"server":"different.test"})
            ),
            Err("operation_tag_conflict")
        );
    }
    #[tokio::test]
    async fn calibration_guard_and_disable_restore_the_owned_baseline() {
        let f = Fixture::new();
        let tag = "b".repeat(32);
        f.manager
            .submit(
                "time.config.set",
                json!({"operation_tag":tag,"calibration_enabled":true}),
            )
            .unwrap();
        assert_eq!(f.wait(&tag).await["verified"], true);
        assert!(!f.root.join("enabled").exists());
        assert!(!f.root.join("running").exists());
        assert_eq!(f.manager.status()["guard_active"], true);
        f.clock.base.fetch_add(28800, Ordering::SeqCst);
        f.manager.guard().await;
        assert_eq!(f.manager.status()["guard_corrections"], 1);
        let tag = "c".repeat(32);
        f.manager
            .submit(
                "time.config.set",
                json!({"operation_tag":tag,"calibration_enabled":false}),
            )
            .unwrap();
        assert_eq!(f.wait(&tag).await["verified"], true);
        assert!(f.root.join("enabled").exists());
        assert!(f.root.join("running").exists());
        assert_eq!(
            fs::read(f.root.join("etc/localtime")).unwrap(),
            b"original-localtime"
        );
        assert_eq!(fs::read(f.root.join("tmp/TZ")).unwrap(), b"UTC0\n");
        assert!(!f.root.join("etc/localtime.datad").exists());
    }
    #[tokio::test]
    async fn manual_sync_then_calibration_takes_over_oem_and_restore_conflicts_are_preflighted() {
        let f = Fixture::new();
        let tag = "1".repeat(32);
        f.manager
            .submit("time.sync", json!({"operation_tag":tag}))
            .unwrap();
        assert_eq!(f.wait(&tag).await["verified"], true);
        let tag = "2".repeat(32);
        f.manager
            .submit(
                "time.config.set",
                json!({"operation_tag":tag,"calibration_enabled":true}),
            )
            .unwrap();
        assert_eq!(f.wait(&tag).await["verified"], true);
        assert!(!f.root.join("enabled").exists());
        fs::write(f.root.join("enabled"), b"").unwrap();
        let tz = fs::read(f.root.join("tmp/TZ")).unwrap();
        let link = fs::read_link(f.root.join("etc/localtime")).unwrap();
        let tag = "3".repeat(32);
        f.manager
            .submit(
                "time.config.set",
                json!({"operation_tag":tag,"calibration_enabled":false}),
            )
            .unwrap();
        assert_eq!(f.wait(&tag).await["error_code"], "restore_conflict");
        assert_eq!(fs::read(f.root.join("tmp/TZ")).unwrap(), tz);
        assert_eq!(fs::read_link(f.root.join("etc/localtime")).unwrap(), link);
    }
    #[tokio::test]
    async fn receipt_capacity_rejection_does_not_save_or_cancel_anything() {
        let f = Fixture::new();
        for n in 0..16 {
            f.manager
                .submit(
                    "time.config.set",
                    json!({"operation_tag":format!("{n:032x}"),"calibration_enabled":false}),
                )
                .unwrap();
        }
        let before = f.manager.status();
        let config = fs::read(&f.manager.inner.path).unwrap();
        let seq = f.manager.inner.state.lock().unwrap().seq;
        assert_eq!(f.manager.submit("time.config.set",json!({"operation_tag":"f".repeat(32),"calibration_enabled":false,"server":"different.test"})),Err("operation_busy"));
        assert_eq!(f.manager.status()["revision"], before["revision"]);
        assert_eq!(f.manager.inner.state.lock().unwrap().seq, seq);
        assert_eq!(fs::read(&f.manager.inner.path).unwrap(), config);
        f.wait(&format!("{:032x}", 15)).await;
    }
    #[tokio::test]
    async fn guard_recomputes_after_wait_and_configuration_does_not_erase_owner_conflict() {
        let f = Fixture::new();
        let tag = "4".repeat(32);
        f.manager
            .submit(
                "time.config.set",
                json!({"operation_tag":tag,"calibration_enabled":true}),
            )
            .unwrap();
        assert_eq!(f.wait(&tag).await["verified"], true);
        f.clock.base.fetch_add(28800, Ordering::SeqCst);
        let read = f.manager.inner.domain.read().unwrap();
        let manager = f.manager.clone();
        let guard = tokio::spawn(async move {
            manager.guard().await;
        });
        for _ in 0..100 {
            if f.manager.inner.domain.waiting.load(Ordering::SeqCst) {
                break;
            };
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(f.manager.inner.domain.waiting.load(Ordering::SeqCst));
        f.manager
            .inner
            .state
            .lock()
            .unwrap()
            .anchor
            .as_mut()
            .unwrap()
            .utc_ns += 30_000_000_000;
        drop(read);
        guard.await.unwrap();
        assert!(f.clock.base.load(Ordering::SeqCst) >= 1_800_000_030);
        fs::write(f.root.join("tmp/TZ"), b"external\n").unwrap();
        f.clock.base.fetch_add(28800, Ordering::SeqCst);
        f.manager.guard().await;
        assert_eq!(f.manager.status()["phase"], "conflict");
        let tag = "5".repeat(32);
        f.manager
            .submit(
                "time.config.set",
                json!({"operation_tag":tag,"boot_sync_enabled":false}),
            )
            .unwrap();
        let op = f.wait(&tag).await;
        assert_eq!(op["verified"], false);
        assert_eq!(op["error_code"], "restore_conflict");
        assert_eq!(f.manager.status()["phase"], "conflict");
    }
    #[tokio::test]
    async fn drift_check_does_not_bypass_service_ownership_when_clock_is_correct() {
        let f = Fixture::new();
        let tag = "6".repeat(32);
        f.manager
            .submit(
                "time.config.set",
                json!({"operation_tag":tag,"calibration_enabled":true}),
            )
            .unwrap();
        assert_eq!(f.wait(&tag).await["verified"], true);
        fs::write(f.root.join("enabled"), b"").unwrap();
        let tag = "7".repeat(32);
        let seq = f.manager.inner.state.lock().unwrap().seq;
        {
            let mut s = f.manager.inner.state.lock().unwrap();
            Manager::remember(&mut s, tag.clone(), "sync", false, [0; 32]).unwrap();
        }
        f.manager
            .run(Work {
                tag: tag.clone(),
                seq,
                server: "ntp.example.test".into(),
                must_sync: true,
                boot: false,
                restore: false,
                only_drifted: true,
            })
            .await;
        assert_eq!(f.wait(&tag).await["error_code"], "restore_conflict");
        assert_eq!(f.manager.status()["phase"], "conflict");
        assert_eq!(f.manager.status()["oem"]["disabled"], Value::Null);
    }
    #[tokio::test]
    async fn failed_apply_rolls_back_and_external_timezone_owner_is_not_overwritten() {
        let f = Fixture::new();
        f.clock.set_fail.store(true, Ordering::SeqCst);
        let tag = "d".repeat(32);
        f.manager
            .submit(
                "time.config.set",
                json!({"operation_tag":tag,"calibration_enabled":true}),
            )
            .unwrap();
        let receipt = f.wait(&tag).await;
        assert_eq!(receipt["phase"], "failed");
        assert_eq!(receipt["verified"], false);
        assert_eq!(receipt["configuration_saved"], true);
        assert!(f.root.join("running").exists());
        assert_eq!(
            fs::read(f.root.join("etc/localtime")).unwrap(),
            b"original-localtime"
        );
        f.clock.set_fail.store(false, Ordering::SeqCst);
        let tag = "e".repeat(32);
        f.manager
            .submit("time.sync", json!({"operation_tag":tag}))
            .unwrap();
        assert_eq!(f.wait(&tag).await["verified"], true);
        fs::write(f.root.join("tmp/TZ"), b"external-owner\n").unwrap();
        let tag = "f".repeat(32);
        f.manager
            .submit(
                "time.config.set",
                json!({"operation_tag":tag,"calibration_enabled":false}),
            )
            .unwrap();
        let receipt = f.wait(&tag).await;
        assert_eq!(receipt["error_code"], "restore_conflict");
        assert_eq!(f.manager.status()["phase"], "conflict");
        assert_eq!(
            fs::read(f.root.join("tmp/TZ")).unwrap(),
            b"external-owner\n"
        );
    }
    #[tokio::test]
    async fn waiting_writer_drains_existing_readers_and_cancelled_wait_releases_reservation() {
        let domain = Arc::new(ClockDomain::new());
        let read = domain.read().unwrap();
        let d = domain.clone();
        let writer = tokio::spawn(async move { Transition::begin(d).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(domain.read().is_none());
        assert_eq!(domain.leases.load(Ordering::SeqCst), 1);
        drop(read);
        let held = writer.await.unwrap();
        assert_eq!(domain.leases.load(Ordering::SeqCst), -1);
        drop(held);
        assert_eq!(domain.generation.load(Ordering::SeqCst), 1);
        let read = domain.read().unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(5), Transition::begin(domain.clone()))
                .await
                .is_err()
        );
        assert!(!domain.waiting.load(Ordering::SeqCst));
        drop(read);
    }
    #[test]
    fn test_pause_is_thread_local_and_parameter_sets_are_closed() {
        with_clock_state(true, 42, || {
            assert!(clock_change_in_progress());
            assert_eq!(clock_generation(), 42);
            assert!(clock_sensitive_operation().is_none());
            assert!(!std::thread::spawn(clock_change_in_progress).join().unwrap());
        });
        let f = Fixture::new();
        for params in [
            json!({"server":"x", "extra":true}),
            json!({}),
            json!({"calibration_enabled":"yes"}),
            json!({"server":"https://x"}),
            json!({"server":"x", "operation_tag":"bad"}),
        ] {
            assert!(f.manager.submit("time.config.set", params).is_err());
        }
    }
}
