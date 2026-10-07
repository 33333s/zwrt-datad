use crate::{
    auth::{self, Sessions},
    cloud::{Cloud, RemoteFeatures, Update as CloudUpdate},
    model::{DatadVersion, Snapshot, UbusCall},
    neighbor_manager::Manager as NeighborManager,
    ota::{self, Config as OtaConfig, Ota},
    reboot_schedule::{self, Schedule},
    sms_forward::{self, Forwarder, Update as SmsForwardUpdate},
    speedtest::SpeedTest,
    state,
    task_schedule::{TaskInput, TaskSchedule},
    traffic_history::{History, Usage},
    webshell::WebShell,
};
use anyhow::Result;
use axum::{
    Json, Router,
    extract::rejection::JsonRejection,
    extract::{ConnectInfo, Request, WebSocketUpgrade},
    extract::{Query, State},
    http::{HeaderMap, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    convert::Infallible,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Mutex, RwLock, Semaphore, watch},
};
use tokio_stream::{StreamExt, wrappers::WatchStream};
use tower_http::limit::RequestBodyLimitLayer;

#[derive(Clone)]
pub struct App {
    pub(crate) inner: Arc<Inner>,
}
pub(crate) struct Inner {
    snapshot: RwLock<Snapshot>,
    tx: watch::Sender<Snapshot>,
    interval_ms: AtomicU64,
    pub(crate) _data_dir: PathBuf,
    identity: crate::identity::Identity,
    token: Option<String>,
    sessions: Mutex<Sessions>,
    device_session: Mutex<Option<DeviceSession>>,
    sse_slots: Arc<Semaphore>,
    cloud: RwLock<Cloud>,
    pub(crate) ota: Mutex<Ota>,
    ota_view: watch::Receiver<Value>,
    neighbor: Mutex<NeighborManager>,
    webshell: WebShell,
    activity: Mutex<crate::activity::Journal>,
    recovery: Arc<Mutex<crate::network_recovery::Recovery>>,
    history: Mutex<History>,
    history_tx: watch::Sender<Vec<Usage>>,
    schedule: Arc<Mutex<Schedule>>,
    tasks: Arc<Mutex<TaskSchedule>>,
    speedtest: Arc<Mutex<SpeedTest>>,
    sms_forward: Arc<Mutex<Forwarder>>,
    time_control: crate::time_control::Manager,
    hosts: Mutex<crate::hosts::Manager>,
}

struct DeviceSession {
    _token: String,
    _password_hash: String,
}

impl App {
    pub(crate) async fn panel_hosts_status(&self) -> Value {
        self.inner.hosts.lock().await.status()
    }
    pub(crate) async fn panel_hosts_action(
        &self,
        action: &str,
        params: Value,
    ) -> Result<String, &'static str> {
        self.inner.hosts.lock().await.action(action, params).await
    }
    /// Switches only the automatic datad update setting; the signed update
    /// path, mirrors and install tasks stay as configured.
    pub(crate) async fn panel_ota_set(&self, enabled: bool) -> Result<bool, String> {
        let mut manager = tokio::time::timeout(Duration::from_secs(5), self.inner.ota.lock())
            .await
            .map_err(|_| "update_in_progress".to_string())?;
        if manager.busy {
            return Err("update_in_progress".into());
        }
        manager.set_auto_update(enabled)
    }

    async fn record(&self, category: &str, action: &str, result: &str, reason: &str, task: &str) {
        self.inner
            .activity
            .lock()
            .await
            .push(category, action, result, reason, task);
    }
    async fn record_delivery(&self, action: &str, result: &Result<(), String>) {
        self.record(
            "notification",
            action,
            if result.is_ok() { "success" } else { "failed" },
            if result.is_ok() {
                ""
            } else {
                sms_forward::delivery_result_code(result)
            },
            "",
        )
        .await;
    }
    fn spawn_recovery(&self) {
        let app = self.clone();
        tokio::spawn(async move {
            loop {
                let interval = app.inner.recovery.lock().await.config().interval_seconds;
                tokio::time::sleep(Duration::from_secs(interval)).await;
                let Some(_clock_lease) = crate::time_control::clock_sensitive_operation() else {
                    continue;
                };
                let generation = {
                    let mut r = app.inner.recovery.lock().await;
                    if !r.may_probe(crate::activity::now()) {
                        continue;
                    }
                    r.generation
                };
                let snapshot = app.inner.snapshot.read().await.clone();
                if !crate::network_recovery::eligible(&snapshot, crate::activity::now()) {
                    app.inner.recovery.lock().await.hold();
                    continue;
                }
                let online = crate::network_recovery::probe().await;
                if !crate::network_recovery::eligible(
                    &*app.inner.snapshot.read().await,
                    crate::activity::now(),
                ) {
                    app.inner.recovery.lock().await.hold();
                    continue;
                }
                let (plan, recovered) = {
                    let mut r = app.inner.recovery.lock().await;
                    if r.generation != generation {
                        continue;
                    }
                    let recovered = online
                        && matches!(
                            r.status,
                            "offline" | "cooldown" | "redialing" | "limit_reached"
                        );
                    (r.observe(online, crate::activity::now()), recovered)
                };
                if recovered {
                    app.record("recovery", "online", "success", "", "").await;
                }
                let Some(plan) = plan else {
                    continue;
                };
                let action = if plan.reboot { "reboot" } else { "redial" };
                app.record("recovery", action, "requested", "", "").await;
                let mut success = false;
                if plan.reboot {
                    success = matches!(
                        tokio::time::timeout(
                            Duration::from_secs(20),
                            crate::control::execute_recovery("device.reboot", plan)
                        )
                        .await,
                        Ok(crate::control::Outcome::Ok(_))
                    );
                } else {
                    // Connect even when disconnect fails: the bearer may already be down.
                    let _ = tokio::time::timeout(
                        Duration::from_secs(20),
                        crate::control::execute_recovery("cellular.disconnect", plan),
                    )
                    .await;
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    if app.inner.recovery.lock().await.allows(plan) {
                        success = matches!(
                            tokio::time::timeout(
                                Duration::from_secs(30),
                                crate::control::execute_recovery("cellular.connect", plan)
                            )
                            .await,
                            Ok(crate::control::Outcome::Ok(_))
                        );
                    }
                }
                // Accepted control is not proof that internet connectivity is restored.
                app.record(
                    "recovery",
                    action,
                    if success { "requested" } else { "failed" },
                    if success {
                        "awaiting_probe"
                    } else {
                        "execution_failed"
                    },
                    "",
                )
                .await;
            }
        });
    }
    pub(crate) fn cloud_panel_state(&self) -> watch::Receiver<Snapshot> {
        self.inner.tx.subscribe()
    }

    pub(crate) fn cloud_panel_history(&self) -> watch::Receiver<Vec<Usage>> {
        self.inner.history_tx.subscribe()
    }

    pub(crate) fn cloud_panel_schedule(&self) -> Arc<Mutex<Schedule>> {
        self.inner.schedule.clone()
    }

    pub(crate) fn cloud_panel_speedtest(&self) -> Arc<Mutex<SpeedTest>> {
        self.inner.speedtest.clone()
    }

    pub(crate) async fn panel_task_status(&self) -> Value {
        self.inner.tasks.lock().await.status()
    }

    pub(crate) async fn panel_task_upsert(&self, input: TaskInput) -> Result<Value, String> {
        if input.action == "device.reboot" {
            let plan = self.inner.schedule.lock().await.status();
            if reboot_schedule::oem_conflict().await
                || plan["enabled"] == true && plan["time"] == input.time
            {
                return Err("reboot_schedule_conflict".into());
            }
        }
        self.inner.tasks.lock().await.upsert(input)
    }

    pub(crate) async fn panel_task_remove(&self, id: &str) -> Result<Value, String> {
        self.inner.tasks.lock().await.remove(id)
    }

    pub(crate) async fn panel_sms_forward_status(&self) -> Value {
        let battery = self
            .inner
            .snapshot
            .read()
            .await
            .fields
            .get("battery")
            .cloned();
        let mut status = self.inner.sms_forward.lock().await.status();
        if let Some(fields) = status.as_object_mut() {
            fields.insert(
                "power_supported".into(),
                json!(sms_forward::power_state(battery.as_ref()).is_some()),
            );
        }
        status
    }

    pub(crate) async fn panel_sms_forward_update(
        &self,
        input: SmsForwardUpdate,
    ) -> Result<Value, String> {
        let _clock_lease = crate::time_control::clock_sensitive_operation()
            .ok_or_else(|| "clock_change_in_progress".to_owned())?;
        let battery = self
            .inner
            .snapshot
            .read()
            .await
            .fields
            .get("battery")
            .cloned();
        let mut manager = self.inner.sms_forward.lock().await;
        let baseline = if manager.requires_baseline(&input) {
            Some(sms_forward::fresh_baseline().await?)
        } else {
            None
        };
        let mut status = manager.update(input, baseline.as_ref(), battery.as_ref())?;
        if let Some(fields) = status.as_object_mut() {
            fields.insert(
                "power_supported".into(),
                json!(sms_forward::power_state(battery.as_ref()).is_some()),
            );
        }
        Ok(status)
    }

    pub(crate) async fn panel_sms_forward_test(&self) -> Result<Value, String> {
        let _clock_lease = crate::time_control::clock_sensitive_operation()
            .ok_or_else(|| "clock_change_in_progress".to_owned())?;
        let device_snapshot = self.inner.snapshot.read().await.clone();
        let battery = device_snapshot.fields.get("battery").cloned();
        let time_origin = self.inner.cloud.read().await.panel_config()["platform_url"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let mut manager = self.inner.sms_forward.lock().await;
        let mut details = manager.delivery();
        details.attach_device_info(&device_snapshot);
        let test_message = sms_forward::Message::test();
        if let Err(error) = manager.reserve_sms(&test_message) {
            manager.mark_delivery(&Err(error.clone()));
            self.record_delivery("test", &Err(error.clone())).await;
            return Err(error);
        }
        let result = sms_forward::deliver(&details, &time_origin, &test_message).await;
        manager.mark_delivery(&result);
        self.record_delivery("test", &result).await;
        let mut status = manager.status();
        if let Some(fields) = status.as_object_mut() {
            fields.insert(
                "power_supported".into(),
                json!(sms_forward::power_state(battery.as_ref()).is_some()),
            );
        }
        result.map(|()| status)
    }

    pub(crate) async fn panel_neighbor_set(&self, enabled: bool) -> Result<Value, String> {
        let mut neighbor = self.inner.neighbor.lock().await;
        if enabled && neighbor.status()["collector_supported"] != true {
            return Err("neighbor_dependency_missing".into());
        }
        neighbor.set_enabled(enabled).await
    }

    async fn scheduled_device_info_forward(&self) -> Result<(), String> {
        let device_snapshot = self.inner.snapshot.read().await.clone();
        let time_origin = self.inner.cloud.read().await.panel_config()["platform_url"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let mut manager = self.inner.sms_forward.lock().await;
        if !manager.device_info_enabled() {
            return Err("forward_disabled".into());
        }
        let mut details = manager.delivery();
        details.force_device_info(&device_snapshot);
        let message = sms_forward::Message::device_info();
        if let Err(error) = manager.reserve_sms(&message) {
            manager.mark_delivery(&Err(error.clone()));
            self.record_delivery("device_info", &Err(error.clone()))
                .await;
            return Err(error);
        }
        let result = sms_forward::deliver(&details, &time_origin, &message).await;
        manager.mark_delivery(&result);
        self.record_delivery("device_info", &result).await;
        result
    }

    pub(crate) async fn cloud_panel_config(&self) -> Value {
        let mut view = self.inner.cloud.read().await.panel_config();
        if let Some(fields) = view.as_object_mut() {
            fields.insert(
                "webshell_available".into(),
                json!(self.cloud_webshell_available()),
            );
        }
        view
    }

    pub(crate) async fn cloud_panel_save_features(
        &self,
        input: RemoteFeatures,
    ) -> Result<Value, String> {
        let (mut view, changed) = self.inner.cloud.write().await.save_remote_features(input)?;
        if let Some(fields) = view.as_object_mut() {
            fields.insert(
                "webshell_available".into(),
                json!(self.cloud_webshell_available()),
            );
        }
        if changed {
            let app = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(750)).await;
                app.inner.cloud.read().await.activate_saved_features();
            });
        }
        Ok(view)
    }

    pub(crate) fn cloud_webshell_available(&self) -> bool {
        self.inner.webshell.enabled()
    }

    pub(crate) fn cloud_webshell_slot(
        &self,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, &'static str> {
        if !self.cloud_webshell_available() {
            return Err("webshell_disabled");
        }
        self.inner
            .webshell
            .try_acquire()
            .ok_or("webshell_session_limit")
    }

    pub(crate) fn cloud_webshell(&self) -> WebShell {
        self.inner.webshell.clone()
    }
    pub async fn new(
        data_dir: PathBuf,
        interval: Duration,
        token: Option<String>,
        neighbor_enabled: bool,
        webshell_enabled: bool,
    ) -> Result<Self> {
        let time_control = crate::time_control::Manager::new(&data_dir);
        let hosts = crate::hosts::Manager::new(&data_dir);
        crate::cooling::tick().await;
        crate::extra_wifi::tick().await;
        let mut initial = state::collect(interval.as_millis() as u64).await;
        initial.fields.insert("time".into(), time_control.status());
        let mut neighbor = NeighborManager::new(neighbor_enabled);
        neighbor.observe_lte(initial.fields.get("net").unwrap_or(&Value::Null));
        neighbor
            .tick(initial.fields.get("net").unwrap_or(&Value::Null))
            .await;
        initial.fields.insert("neighbor".into(), neighbor.status());
        let mut schedule = Schedule::load(&data_dir);
        schedule.set_environment(
            reboot_schedule::local_clock(),
            reboot_schedule::oem_conflict().await,
        );
        initial
            .fields
            .insert("reboot_schedule".into(), schedule.status());
        let speedtest = SpeedTest::new();
        initial
            .fields
            .insert("speedtest".into(), speedtest.status());
        let activity = crate::activity::Journal::load(&data_dir);
        let recovery = Arc::new(Mutex::new(crate::network_recovery::Recovery::load(
            &data_dir,
        )));
        crate::network_recovery::install(&recovery);
        let tasks = TaskSchedule::load(&data_dir);
        let sms_forward = Forwarder::load(&data_dir);
        let mut history = History::load(&data_dir);
        history.record(&initial);
        let (history_tx, _) = watch::channel(history.days());
        let (tx, _) = watch::channel(initial.clone());
        let ota = Ota::load(&data_dir).map_err(anyhow::Error::msg)?;
        let ota_view = ota.subscribe();
        let app = Self {
            inner: Arc::new(Inner {
                snapshot: RwLock::new(initial),
                tx,
                interval_ms: AtomicU64::new(interval.as_millis() as u64),
                cloud: RwLock::new(Cloud::load(&data_dir)),
                ota: Mutex::new(ota),
                ota_view,
                neighbor: Mutex::new(neighbor),
                identity: crate::identity::Identity::new(&data_dir),
                _data_dir: data_dir,
                token,
                sessions: Mutex::new(Sessions::default()),
                device_session: Mutex::new(None),
                sse_slots: Arc::new(Semaphore::new(16)),
                webshell: WebShell::new(webshell_enabled),
                activity: Mutex::new(activity),
                recovery,
                history: Mutex::new(history),
                history_tx,
                schedule: Arc::new(Mutex::new(schedule)),
                tasks: Arc::new(Mutex::new(tasks)),
                speedtest: Arc::new(Mutex::new(speedtest)),
                sms_forward: Arc::new(Mutex::new(sms_forward)),
                time_control,
                hosts: Mutex::new(hosts),
            }),
        };
        app.inner
            .cloud
            .read()
            .await
            .start(app.inner.tx.subscribe(), app.clone());
        app.spawn_recovery();
        app.spawn_sampler();
        app.spawn_ota();
        Ok(app)
    }
    pub async fn snapshot(&self) -> Snapshot {
        self.inner.snapshot.read().await.clone()
    }
    pub fn spawn_time_control(&self) {
        self.inner.time_control.start();
    }
    pub(crate) fn panel_time_action(&self, action: &str, params: Value) -> Result<Value, String> {
        self.inner
            .time_control
            .submit(action, params)
            .map_err(str::to_owned)
    }
    fn spawn_sampler(&self) {
        let app = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(
                    app.inner.interval_ms.load(Ordering::Relaxed),
                ))
                .await;
                app.refresh_snapshot().await;
            }
        });
    }
    pub fn spawn_reboot_schedule(&self) {
        let app = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let mut ticks = tokio::time::interval(Duration::from_secs(10));
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticks.tick().await;
                let Some(_clock_lease) = crate::time_control::clock_sensitive_operation() else {
                    continue;
                };
                let clock = reboot_schedule::local_clock();
                let conflict = reboot_schedule::oem_conflict().await;
                let due = app.inner.schedule.lock().await.observe(clock, conflict);
                if due {
                    app.record("task", "device.reboot", "requested", "", "定时重启")
                        .await;
                    if !matches!(
                        crate::control::execute("device.reboot", &json!({})).await,
                        crate::control::Outcome::Ok(_)
                    ) {
                        app.inner.schedule.lock().await.reboot_failed();
                        app.record(
                            "task",
                            "device.reboot",
                            "failed",
                            "execution_failed",
                            "定时重启",
                        )
                        .await;
                    }
                }
            }
        });
    }

    pub fn spawn_task_schedule(&self) {
        let app = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let mut ticks = tokio::time::interval(Duration::from_secs(10));
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticks.tick().await;
                let Some(_clock_lease) = crate::time_control::clock_sensitive_operation() else {
                    continue;
                };
                let Some(clock) = reboot_schedule::local_clock() else {
                    continue;
                };
                // Drain this minute's tasks sequentially without a ten-second gap
                // between each one. Otherwise a full 16-task minute loses jobs.
                for _ in 0..crate::task_schedule::MAX_TASKS {
                    let Some(task) = app.inner.tasks.lock().await.observe(Some(&clock)) else {
                        break;
                    };
                    if task.action == "device.reboot" {
                        let plan = app.inner.schedule.lock().await.status();
                        if reboot_schedule::oem_conflict().await
                            || plan["enabled"] == true && plan["time"] == task.time
                        {
                            app.inner
                                .tasks
                                .lock()
                                .await
                                .finish(&task.id, &clock.date, false);
                            app.record(
                                "task",
                                &task.action,
                                "skipped",
                                "reboot_schedule_conflict",
                                &task.id,
                            )
                            .await;
                            continue;
                        }
                    }
                    app.record("task", &task.action, "requested", "", &task.id)
                        .await;
                    let result: Result<(), &str> = if task.action == "sms.forward.device_info" {
                        match tokio::time::timeout(
                            Duration::from_secs(50),
                            app.scheduled_device_info_forward(),
                        )
                        .await
                        {
                            Ok(Ok(())) => Ok(()),
                            Ok(Err(e)) if e == "forward_disabled" => Err("forward_disabled"),
                            Ok(Err(e)) => Err(sms_forward::delivery_result_code(&Err(e))),
                            Err(_) => Err("execution_timeout"),
                        }
                    } else if task.action == "sms.send_scheduled" {
                        if let Some(params) =
                            crate::task_schedule::scheduled_sms_params(&task, &clock)
                        {
                            match app.inner.sms_forward.lock().await.reserve_scheduled_sms() {
                                Err(_) => Err("rate_limited"),
                                Ok(()) => match tokio::time::timeout(
                                    Duration::from_secs(50),
                                    crate::sms::send(&params),
                                )
                                .await
                                {
                                    Ok(Ok(_)) => Ok(()),
                                    Ok(Err(_)) => Err("execution_failed"),
                                    Err(_) => Err("execution_timeout"),
                                },
                            }
                        } else {
                            Err("invalid_parameters")
                        }
                    } else {
                        match tokio::time::timeout(
                            Duration::from_secs(20),
                            crate::control::execute(&task.action, &task.params),
                        )
                        .await
                        {
                            Ok(crate::control::Outcome::Ok(v)) if v["verified"] != false => Ok(()),
                            Ok(crate::control::Outcome::Invalid(_)) => Err("invalid_parameters"),
                            Ok(crate::control::Outcome::NotHandled) => Err("unsupported_action"),
                            Err(_) => Err("execution_timeout"),
                            _ => Err("execution_failed"),
                        }
                    };
                    let success = result.is_ok();
                    app.inner
                        .tasks
                        .lock()
                        .await
                        .finish(&task.id, &clock.date, success);
                    app.record(
                        "task",
                        &task.action,
                        if success { "success" } else { "failed" },
                        result.err().unwrap_or_default(),
                        &task.id,
                    )
                    .await;
                }
            }
        });
    }

    pub fn spawn_sms_forward(&self) {
        let app = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let mut ticks = tokio::time::interval(Duration::from_secs(15));
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticks.tick().await;
                let device_snapshot = app.inner.snapshot.read().await.clone();
                let battery = device_snapshot.fields.get("battery").cloned();
                let time_origin = app.inner.cloud.read().await.panel_config()["platform_url"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                {
                    let Some(_clock_lease) = crate::time_control::clock_sensitive_operation()
                    else {
                        continue;
                    };
                    let mut manager = app.inner.sms_forward.lock().await;
                    match manager.observe_power(battery.as_ref()) {
                        Ok(Some(message)) => {
                            let mut details = manager.delivery();
                            details.attach_device_info(&device_snapshot);
                            let result = match manager.reserve_sms(&message) {
                                Ok(()) => {
                                    sms_forward::deliver(&details, &time_origin, &message).await
                                }
                                Err(error) => Err(error),
                            };
                            manager.mark_power_delivery(&result);
                            app.record_delivery("power", &result).await;
                        }
                        Ok(None) => {}
                        Err(error) => {
                            manager.mark_power_delivery(&Err(error.clone()));
                            app.record_delivery("power", &Err(error)).await;
                        }
                    }
                }
                if app.panel_sms_forward_status().await["enabled"] != true {
                    continue;
                }
                let snapshot = match sms_forward::baseline().await {
                    Ok(snapshot) => snapshot,
                    Err(_) => {
                        app.inner.sms_forward.lock().await.mark_source_unavailable();
                        app.record_delivery("sms", &Err("source_unavailable".into()))
                            .await;
                        continue;
                    }
                };
                for _ in 0..4 {
                    let Some(_clock_lease) = crate::time_control::clock_sensitive_operation()
                    else {
                        break;
                    };
                    let device_snapshot = app.inner.snapshot.read().await.clone();
                    let time_origin = app.inner.cloud.read().await.panel_config()["platform_url"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    let mut manager = app.inner.sms_forward.lock().await;
                    let message = match manager.next(&snapshot) {
                        Ok(Some(message)) => message,
                        Ok(None) => break,
                        Err(error) => {
                            if error == "forward_storage_failed" {
                                manager.mark_storage_failed();
                            } else {
                                manager.mark_source_unavailable();
                            }
                            break;
                        }
                    };
                    let mut details = manager.delivery();
                    details.attach_device_info(&device_snapshot);
                    if let Err(error) = manager.reserve_sms(&message) {
                        manager.mark_delivery(&Err(error.clone()));
                        app.record_delivery("sms", &Err(error)).await;
                        break;
                    }
                    let result = sms_forward::deliver(&details, &time_origin, &message).await;
                    manager.mark_delivery(&result);
                    app.record_delivery("sms", &result).await;
                }
            }
        });
    }
    fn spawn_ota(&self) {
        if std::env::var("ZWRT_DATAD_OTA_DISABLE_AUTO").as_deref() == Ok("1") {
            return;
        }
        let app = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(90)).await;
            loop {
                let snapshot = serde_json::to_value(app.snapshot().await).unwrap_or(Value::Null);
                let mut manager = app.inner.ota.lock().await;
                manager.reconcile_install_result();
                if manager.should_auto_check() && manager.begin_update().is_ok() {
                    manager.mark_auto_check();
                    let _ = manager.check().await;
                    manager.finish_update();
                }
                if let Some(candidate) = manager.auto_candidate()
                    && manager.begin_update().is_ok()
                {
                    if let Err(error) = manager.install(&candidate, &snapshot, false).await
                        && !error.starts_with("等待安装条件:")
                    {
                        manager.fail(Some(&candidate), error);
                    }
                    manager.finish_update();
                }
                drop(manager);
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
    }
    async fn refresh_snapshot(&self) {
        crate::cooling::tick().await;
        crate::extra_wifi::tick().await;
        let round = std::time::Instant::now();
        let mut next = crate::ubus_socket::collecting(state::collect(
            self.inner.interval_ms.load(Ordering::Relaxed),
        ))
        .await;
        crate::ubus_socket::record_round(round.elapsed());
        if std::env::var("ZWRT_DATAD_UBUS_STATS").as_deref() == Ok("1") {
            next.fields
                .insert("ubus_stats".into(), crate::ubus_socket::stats());
        }
        next.fields
            .insert("time".into(), self.inner.time_control.status());
        next.fields.insert(
            "reboot_schedule".into(),
            self.inner.schedule.lock().await.status(),
        );
        let mut speedtest = self.inner.speedtest.lock().await;
        speedtest.refresh_availability();
        next.fields.insert("speedtest".into(), speedtest.status());
        drop(speedtest);
        let mut history = self.inner.history.lock().await;
        if history.record(&next) {
            self.inner.history_tx.send_replace(history.days());
        }
        drop(history);
        let mut neighbor = self.inner.neighbor.lock().await;
        neighbor.observe_lte(next.fields.get("net").unwrap_or(&Value::Null));
        neighbor
            .tick(next.fields.get("net").unwrap_or(&Value::Null))
            .await;
        next.fields.insert("neighbor".into(), neighbor.status());
        drop(neighbor);
        let mut old = self.inner.snapshot.write().await;
        let mut comparable = next.clone();
        comparable.ts = old.ts;
        if comparable != *old {
            *old = next.clone();
            let _ = self.inner.tx.send(next);
        } else {
            old.ts = next.ts;
        }
    }
    pub async fn serve(
        self,
        addr: SocketAddr,
        require_auth: bool,
        open_auth_routes: bool,
    ) -> Result<()> {
        let mut router = Router::new()
            .route("/", get(index))
            .route("/healthz", get(health))
            .route("/version", get(version))
            .route("/state", get(snapshot))
            .route("/usb/status", get(usb_status))
            .route("/identity/public-key", get(identity_public_key))
            .route(
                "/identity/init",
                post(identity_init).layer(RequestBodyLimitLayer::new(2048)),
            )
            .route(
                "/identity/sign",
                post(identity_sign).layer(RequestBodyLimitLayer::new(2048)),
            )
            .route("/events", get(events))
            .route("/capabilities", get(capabilities))
            .route("/ubus", get(ubus_list))
            .route("/ubus/list", get(ubus_list))
            .route("/ubus/call", post(ubus_call))
            .route("/control", post(control))
            .route("/webshell/status", get(webshell_status))
            .route("/webshell", get(webshell_upgrade));
        let app_routes = Router::new()
            .route("/ota/app", get(ota_app_status))
            .route(
                "/ota/app/config",
                post(ota_app_config).layer(RequestBodyLimitLayer::new(8192)),
            )
            .route(
                "/ota/app/check",
                post(ota_app_check).layer(RequestBodyLimitLayer::new(256)),
            )
            .route(
                "/ota/app/install",
                post(ota_app_install).layer(RequestBodyLimitLayer::new(512)),
            )
            .route("/activity", get(activity_get))
            .route(
                "/network/recovery/check",
                post(recovery_check).layer(RequestBodyLimitLayer::new(256)),
            )
            .route(
                "/activity/clear",
                post(activity_clear).layer(RequestBodyLimitLayer::new(256)),
            )
            .route(
                "/network/recovery",
                get(recovery_get)
                    .post(recovery_put)
                    .layer(RequestBodyLimitLayer::new(2048)),
            )
            .route(
                "/network/recovery/resume",
                post(recovery_resume).layer(RequestBodyLimitLayer::new(256)),
            )
            .route("/traffic/history", get(traffic_history_get))
            .route(
                "/lan",
                get(lan_get)
                    .post(lan_put)
                    .layer(RequestBodyLimitLayer::new(2048)),
            )
            .route(
                "/lan/mtu",
                post(lan_mtu_put).layer(RequestBodyLimitLayer::new(256)),
            )
            .route(
                "/tasks",
                get(tasks_get)
                    .post(tasks_put)
                    .layer(RequestBodyLimitLayer::new(4096)),
            )
            .route(
                "/tasks/remove",
                post(tasks_remove).layer(RequestBodyLimitLayer::new(1024)),
            )
            .route(
                "/sms/forward/config",
                get(sms_forward_config_get)
                    .post(sms_forward_config_post)
                    .layer(RequestBodyLimitLayer::new(32 * 1024)),
            )
            .route("/sms/forward/status", get(sms_forward_status_get))
            .route(
                "/sms/forward/test",
                post(sms_forward_test_post).layer(RequestBodyLimitLayer::new(1024)),
            )
            .route(
                "/cloud/app/config",
                get(cloud_app_config_get)
                    .post(cloud_app_config_post)
                    .layer(RequestBodyLimitLayer::new(64 * 1024)),
            )
            .route("/cloud/app/status", get(cloud_app_status))
            .route(
                "/cloud/app/quick-connect",
                post(cloud_app_quick_connect).layer(RequestBodyLimitLayer::new(4096)),
            )
            .route_layer(middleware::from_fn_with_state(
                self.clone(),
                app_management_guard,
            ));
        router = router.merge(app_routes);
        if !require_auth {
            router = router
                .route(
                    "/cloud/config",
                    get(cloud_config_get).post(cloud_config_post),
                )
                .route(
                    "/cloud/quick-connect",
                    post(cloud_quick_connect).layer(RequestBodyLimitLayer::new(4096)),
                )
                .route("/cloud/status", get(cloud_status));
            router = router
                .route("/ota/config", get(ota_config_get).post(ota_config_post))
                .route("/ota/status", get(ota_status))
                .route("/ota/check", post(ota_check))
                .route("/ota/update", post(ota_update));
        }
        if open_auth_routes {
            router = router
                .route("/auth/login", post(auth_login))
                .route("/auth/exchange", post(auth_exchange));
        }
        if require_auth {
            router = router.layer(middleware::from_fn_with_state(self.clone(), authenticate));
        }
        if open_auth_routes {
            router = router.layer(middleware::from_fn(lan_source_filter));
        }
        let router = router
            .layer(RequestBodyLimitLayer::new(1024 * 1024))
            .with_state(self.clone());
        let listener = TcpListener::bind(addr).await.map_err(|error| {
            anyhow::anyhow!(
                "cannot listen on {addr}: {error} (is another zwrt-datad already running?)"
            )
        })?;
        let result = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown())
        .await;
        self.inner.neighbor.lock().await.shutdown().await;
        result?;
        Ok(())
    }
}

fn lan_source_allowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ipv4_lan_source_allowed(ip),
        IpAddr::V6(ip) => {
            ip.to_ipv4_mapped().is_some_and(ipv4_lan_source_allowed)
                || ip.is_loopback()
                || (ip.segments()[0] & 0xfe00) == 0xfc00
                || (ip.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

fn ipv4_lan_source_allowed(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || (octets[0] == 100 && (octets[1] & 0xc0) == 64)
}

async fn lan_source_filter(request: Request, next: Next) -> Response {
    let allowed = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|ConnectInfo(peer)| lan_source_allowed(peer.ip()));
    if allowed {
        next.run(request).await
    } else {
        webshell_error(
            StatusCode::FORBIDDEN,
            "source_not_allowed",
            "LAN listener only accepts private network sources",
        )
    }
}

const WEBSHELL_PROTOCOL: &str = "datad-webshell-v1";
const WEBSHELL_AUTH_PROTOCOL_PREFIX: &str = "datad-auth.";

fn webshell_protocol_token(headers: &HeaderMap) -> Option<&str> {
    let protocols = headers.get("sec-websocket-protocol")?.to_str().ok()?;
    let mut supports_webshell = false;
    let mut token = None;
    for protocol in protocols.split(',').map(str::trim) {
        if protocol == WEBSHELL_PROTOCOL {
            supports_webshell = true;
        } else if let Some(value) = protocol.strip_prefix(WEBSHELL_AUTH_PROTOCOL_PREFIX)
            && !value.is_empty()
        {
            token = Some(value);
        }
    }
    supports_webshell.then_some(token).flatten()
}

async fn webshell_auth(app: &App, headers: &HeaderMap, allow_protocol: bool) -> bool {
    let bearer = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let legacy = headers
        .get("x-auth-token")
        .and_then(|value| value.to_str().ok());
    let protocol = allow_protocol
        .then(|| webshell_protocol_token(headers))
        .flatten();
    let Some(presented) = bearer.or(legacy).or(protocol) else {
        return false;
    };
    static_token_valid(app.inner.token.as_deref(), Some(presented))
        || app.inner.sessions.lock().await.validate(presented)
}

fn webshell_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"ok":false,"error":{"code":code,"message":message}})),
    )
        .into_response()
}

fn identity_result(result: Result<Value, crate::identity::Error>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => webshell_error(
            StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            error.code,
            error.message,
        ),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityInit {}

async fn identity_public_key(State(app): State<App>, headers: HeaderMap) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return webshell_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "authentication required",
        );
    }
    identity_result(app.inner.identity.public_key().await)
}
async fn identity_init(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<IdentityInit>, JsonRejection>,
) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return webshell_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "authentication required",
        );
    }
    if body.is_err() {
        return webshell_error(
            StatusCode::BAD_REQUEST,
            "invalid_identity_request",
            "expected an empty JSON object",
        );
    }
    identity_result(app.inner.identity.initialize().await)
}
async fn identity_sign(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<crate::identity::SignRequest>, JsonRejection>,
) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return webshell_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "authentication required",
        );
    }
    match body {
        Ok(Json(request)) => identity_result(app.inner.identity.sign(request).await),
        Err(_) => webshell_error(
            StatusCode::BAD_REQUEST,
            "invalid_identity_request",
            "expected purpose and challenge JSON fields",
        ),
    }
}

async fn webshell_status(State(app): State<App>, headers: HeaderMap) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return webshell_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "authentication required",
        );
    }
    if !app.inner.webshell.enabled() {
        return webshell_error(StatusCode::NOT_FOUND, "disabled", "WebShell is disabled");
    }
    Json(app.inner.webshell.status()).into_response()
}

async fn webshell_upgrade(
    State(app): State<App>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !webshell_auth(&app, &headers, true).await {
        return webshell_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "authentication required",
        );
    }
    if !app.inner.webshell.enabled() {
        return webshell_error(StatusCode::NOT_FOUND, "disabled", "WebShell is disabled");
    }
    if !valid_websocket_key(&headers) {
        return webshell_error(
            StatusCode::BAD_REQUEST,
            "invalid_websocket_key",
            "Sec-WebSocket-Key must be canonical base64 for 16 bytes",
        );
    }
    let Some(permit) = app.inner.webshell.try_acquire() else {
        return webshell_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "session_limit",
            "too many WebShell sessions",
        );
    };
    let shell = app.inner.webshell.clone();
    ws.protocols([WEBSHELL_PROTOCOL])
        .read_buffer_size(crate::webshell::MAX_MESSAGE_SIZE)
        .write_buffer_size(8 * 1024)
        .max_write_buffer_size(64 * 1024)
        .max_message_size(crate::webshell::MAX_MESSAGE_SIZE)
        .max_frame_size(crate::webshell::MAX_MESSAGE_SIZE)
        .accept_unmasked_frames(false)
        .on_upgrade(move |socket| shell.serve(socket, permit))
        .into_response()
}

fn valid_websocket_key(headers: &HeaderMap) -> bool {
    let Some(value) = headers
        .get("sec-websocket-key")
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    BASE64
        .decode(value)
        .ok()
        .is_some_and(|decoded| decoded.len() == 16 && BASE64.encode(decoded) == value)
}

async fn authenticate(State(app): State<App>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if matches!(
        path,
        "/" | "/healthz" | "/auth/login" | "/auth/exchange" | "/webshell" | "/webshell/status"
    ) {
        return next.run(request).await;
    }
    let headers = request.headers();
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let legacy = headers.get("x-auth-token").and_then(|v| v.to_str().ok());
    let query = request.uri().query().and_then(|q| {
        q.split('&')
            .find_map(|part| part.strip_prefix("access_token="))
    });
    let presented = [bearer, legacy, query].into_iter().flatten().next();
    let static_valid = static_token_valid(app.inner.token.as_deref(), presented);
    let session_valid = if static_valid {
        false
    } else if let Some(value) = presented {
        app.inner.sessions.lock().await.validate(value)
    } else {
        false
    };
    if static_valid || session_valid {
        next.run(request).await
    } else {
        (StatusCode::UNAUTHORIZED, Json(json!({"ok":false,"error":{"code":"unauthorized","message":"authentication required"}}))).into_response()
    }
}

fn static_token_valid(configured: Option<&str>, presented: Option<&str>) -> bool {
    configured.is_some_and(|wanted| {
        presented.is_some_and(|value| constant_time_eq(value.as_bytes(), wanted.as_bytes()))
    })
}

async fn auth_login(State(app): State<App>, headers: HeaderMap) -> Response {
    let Some((username, password)) = basic_credentials(&headers) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok":false,"error":"missing_credentials"})),
        )
            .into_response();
    };
    if !auth::verify_password(&username, &password).await {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"ok":false,"error":"invalid_credentials"})),
        )
            .into_response();
    }
    match app.inner.sessions.lock().await.issue() {
        Ok((token, expires_at)) => {
            (StatusCode::OK, Json(auth::token_reply(token, expires_at))).into_response()
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok":false,"error":"token_issue_failed"})),
        )
            .into_response(),
    }
}

async fn auth_exchange(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let token = headers
        .get("x-web-token")
        .or_else(|| headers.get("x-zte-webtoken"))
        .and_then(|value| value.to_str().ok());
    let Some(token) = token.filter(|value| !value.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok":false,"error":"missing_webtoken"})),
        )
            .into_response();
    };
    let mode = headers
        .get("x-z-mode")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or_default();
    let tag = headers
        .get("x-z-tag")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("zwrt-datad");
    if !auth::verify_webtoken(token, mode, &peer.ip().to_string(), tag).await {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"ok":false,"error":"invalid_webtoken"})),
        )
            .into_response();
    }
    match app.inner.sessions.lock().await.issue() {
        Ok((token, expires_at)) => {
            (StatusCode::OK, Json(auth::token_reply(token, expires_at))).into_response()
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok":false,"error":"token_issue_failed"})),
        )
            .into_response(),
    }
}

fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let encoded = headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Basic ")?;
    if encoded.len() > 1024 {
        return None;
    }
    let decoded = decode_base64(encoded)?;
    if decoded.len() > 512 || decoded.contains(&0) {
        return None;
    }
    let separator = decoded.iter().position(|b| *b == b':')?;
    if separator == 0 {
        return None;
    }
    let username = String::from_utf8(decoded[..separator].to_vec()).ok()?;
    let password = String::from_utf8(decoded[separator + 1..].to_vec()).ok()?;
    if username.len() >= 257 || password.len() >= 257 {
        return None;
    }
    Some((username, password))
}

fn decode_base64(value: &str) -> Option<Vec<u8>> {
    let mut acc = 0u32;
    let mut bits = 0u8;
    let mut out = Vec::new();
    for byte in value.bytes().filter(|b| !b.is_ascii_whitespace()) {
        if byte == b'=' {
            break;
        }
        let digit = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(digit);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
            acc &= if bits == 0 { 0 } else { (1 << bits) - 1 };
        }
    }
    Some(out)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

async fn index() -> &'static str {
    "zwrt-datad Rust rewrite\n"
}
async fn health() -> &'static str {
    "ok\n"
}
async fn version() -> Json<DatadVersion> {
    Json(Default::default())
}
async fn snapshot(State(app): State<App>) -> Json<Snapshot> {
    Json(app.snapshot().await)
}
async fn usb_status(State(app): State<App>) -> Json<Value> {
    Json(
        app.snapshot()
            .await
            .fields
            .get("usb")
            .cloned()
            .unwrap_or(Value::Null),
    )
}
fn capability_controls() -> Vec<&'static str> {
    let mut controls = vec![
        "device.login_info",
        "device.login",
        "device.logout",
        "device.session_status",
        "device.change_password",
        "wifi.status",
        "wifi.dual_band_status",
        "wifi.txpower.status",
        "wifi.advanced.status",
        "sleep.status",
        "usb.status",
        "power.direct_supply.status",
        "apn.list",
        "client.access",
        "neighbor.status",
        "neighbor.set",
        "state.refresh",
        "state.set_interval",
        "qos.reload",
        "time.status",
        "hosts.status",
        "datad.ota.set",
    ];
    controls.extend_from_slice(crate::control::ACTIONS);
    controls
}

async fn capabilities() -> Json<Value> {
    let controls = capability_controls();
    Json(json!({
        "schema_version":1,
        "protocol":1,
        "events":["state"],
        "discovery":["ubus.list","ubus.list_verbose"],
        "passthrough":["ubus.call"],
        "transport":["http","sse"],
        "control":controls,
        "controls":controls,
        "rewrite":"rust"
    }))
}

async fn events(State(app): State<App>) -> Response {
    let Ok(permit) = app.inner.sse_slots.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok":false,"error":{"code":"sse_client_limit","message":"too many SSE clients"}})),
        )
            .into_response();
    };
    let stream = WatchStream::new(app.inner.tx.subscribe()).map(move |v| {
        let _keep_permit_alive = &permit;
        Ok::<_, Infallible>(Event::default().event("state").json_data(v).unwrap())
    });
    Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::new())
        .into_response()
}
#[derive(Deserialize)]
struct ListQuery {
    verbose: Option<u8>,
}
async fn ubus_list(Query(q): Query<ListQuery>) -> Response {
    result(state::ubus_list(q.verbose == Some(1)).await)
}
async fn ubus_call(Json(req): Json<UbusCall>) -> Response {
    let service = req.service.clone();
    let method = req.method.clone();
    match state::ubus(&req.service, &req.method, req.args).await {
        Ok(value) => (
            StatusCode::OK,
            Json(json!({"ok":true,"service":service,"method":method,"result":value})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"ok":false,"error":{"code":"device_call_failed","message":e}})),
        )
            .into_response(),
    }
}
async fn control(
    State(app): State<App>,
    method: Method,
    payload: Result<Json<Value>, JsonRejection>,
) -> Response {
    let _ = method;
    let Json(body) = match payload {
        Ok(body) => body,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok":false,"error":{"code":"invalid_request","message":"request body must be valid JSON"}})),
            )
                .into_response();
        }
    };
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if action.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok":false,"error":{"code":"invalid_request","message":"missing action"}})),
        )
            .into_response();
    }
    if body.get("params").is_some_and(|params| !params.is_object()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok":false,"action":action,"error":{"code":"invalid_request","message":"params must be a JSON object"}})),
        )
            .into_response();
    }
    if action == "time.status" {
        if body
            .get("params")
            .is_some_and(|v| !v.as_object().is_some_and(Map::is_empty))
        {
            return invalid_parameter(action, "time.status accepts no parameters");
        }
        return control_ok(action, app.inner.time_control.status());
    }
    if action == "hosts.status" {
        if body
            .get("params")
            .is_some_and(|v| !v.as_object().is_some_and(Map::is_empty))
        {
            return invalid_parameter(action, "hosts.status accepts no parameters");
        }
        return control_ok(action, app.panel_hosts_status().await);
    }
    if matches!(action, "hosts.save" | "hosts.restore") {
        if body.get("confirmed").and_then(Value::as_bool) != Some(true) {
            return invalid_parameter(action, "hosts writes require confirmed=true");
        }
        return match app
            .panel_hosts_action(action, body.get("params").cloned().unwrap_or(json!({})))
            .await
        {
            Ok(revision) => control_ok(action, json!({"revision":revision})),
            Err(code) => (
                StatusCode::CONFLICT,
                Json(json!({"ok":false,"action":action,"error":{"code":code}})),
            )
                .into_response(),
        };
    }
    if matches!(action, "time.config.set" | "time.sync") {
        if body.get("confirmed").and_then(Value::as_bool) != Some(true) {
            return invalid_parameter(action, "time actions require confirmed=true");
        }
        return match app.panel_time_action(action, body.get("params").cloned().unwrap_or(json!({})))
        {
            Ok(value) => control_ok(action, value),
            Err(error)
                if matches!(
                    error.as_str(),
                    "config_storage_failed" | "clock_permission_denied" | "operation_busy"
                ) =>
            {
                control_failed(action, error)
            }
            Err(error) => invalid_parameter(action, &error),
        };
    }
    if action == "neighbor.status" {
        return (StatusCode::OK,Json(json!({"ok":true,"action":action,"result":app.inner.neighbor.lock().await.status()}))).into_response();
    }
    if action == "neighbor.set" {
        let enabled = body
            .get("params")
            .and_then(|v| v.get("enabled"))
            .and_then(Value::as_bool);
        let Some(enabled) = enabled else {
            return (StatusCode::BAD_REQUEST,Json(json!({"ok":false,"action":action,"error":{"code":"invalid_parameter","message":"enabled must be boolean"}}))).into_response();
        };
        return match app.inner.neighbor.lock().await.set_enabled(enabled).await {Ok(value)=>(StatusCode::OK,Json(json!({"ok":true,"action":action,"result":value}))).into_response(),Err(error)=>(StatusCode::BAD_GATEWAY,Json(json!({"ok":false,"action":action,"error":{"code":"device_call_failed","message":error}}))).into_response()};
    }
    if action == "schedule.reboot.set" {
        let params = body.get("params").and_then(Value::as_object);
        if params.is_none_or(|params| params.len() != 2) {
            return invalid_parameter(action, "enabled and time are the only accepted fields");
        }
        let enabled = params
            .and_then(|params| params.get("enabled"))
            .and_then(Value::as_bool);
        let time = params
            .and_then(|params| params.get("time"))
            .and_then(Value::as_str);
        let (Some(enabled), Some(time)) = (enabled, time) else {
            return invalid_parameter(action, "enabled must be boolean and time must be HH:MM");
        };
        let conflict = reboot_schedule::oem_conflict().await;
        let result = app.inner.schedule.lock().await.set(enabled, time, conflict);
        return match result {
            Ok(value) => {
                let refresh = app.clone();
                tokio::spawn(async move { refresh.refresh_snapshot().await });
                control_ok(action, value)
            }
            Err(error) => invalid_parameter(action, &error),
        };
    }
    if action == "schedule.task.put" {
        let params = body.get("params").cloned().unwrap_or(Value::Null);
        let input: TaskInput = match serde_json::from_value(params) {
            Ok(input) => input,
            Err(_) => return invalid_parameter(action, "invalid scheduled task"),
        };
        return match app.panel_task_upsert(input).await {
            Ok(value) => control_ok(action, value),
            Err(error) if error == "task_storage_failed" => control_failed(action, error),
            Err(error) => invalid_parameter(action, &error),
        };
    }
    if action == "schedule.task.remove" {
        let params = body.get("params").and_then(Value::as_object);
        let id = params
            .filter(|params| params.len() == 1)
            .and_then(|params| params.get("id"))
            .and_then(Value::as_str);
        let Some(id) = id else {
            return invalid_parameter(action, "id is required");
        };
        return match app.panel_task_remove(id).await {
            Ok(value) => control_ok(action, value),
            Err(error) if error == "task_storage_failed" => control_failed(action, error),
            Err(error) => invalid_parameter(action, &error),
        };
    }
    if action == "speedtest.start" {
        let params = body.get("params").unwrap_or(&Value::Null);
        return match SpeedTest::start(app.inner.speedtest.clone(), params).await {
            Ok(value) => {
                let refresh = app.clone();
                tokio::spawn(async move { refresh.refresh_snapshot().await });
                control_ok(action, value)
            }
            Err(error) => invalid_parameter(action, error),
        };
    }
    if action == "speedtest.stop" {
        if !body
            .get("params")
            .and_then(Value::as_object)
            .is_some_and(Map::is_empty)
        {
            return invalid_parameter(action, "stop accepts no params");
        }
        let value = SpeedTest::stop(app.inner.speedtest.clone()).await;
        let refresh = app.clone();
        tokio::spawn(async move { refresh.refresh_snapshot().await });
        return control_ok(action, value);
    }
    if action == "cloud.remote_features.set" {
        let params = body.get("params").cloned().unwrap_or(Value::Null);
        let input: RemoteFeatures = match serde_json::from_value(params) {
            Ok(input) => input,
            Err(_) => return invalid_parameter(action, "invalid remote features"),
        };
        return match app.cloud_panel_save_features(input).await {
            Ok(value) => control_ok(action, value),
            Err(error) if error == "保存配置失败" => control_failed(action, error),
            Err(error) => invalid_parameter(action, &error),
        };
    }
    if action == "sms.forward.set" {
        let params = body.get("params").cloned().unwrap_or(Value::Null);
        let input: SmsForwardUpdate = match serde_json::from_value(params) {
            Ok(input) => input,
            Err(_) => return invalid_parameter(action, "invalid SMS forwarding config"),
        };
        return match app.panel_sms_forward_update(input).await {
            Ok(value) => control_ok(action, value),
            Err(error) if error == "forward_storage_failed" => control_failed(action, error),
            Err(error) => invalid_parameter(action, &error),
        };
    }
    if action == "sms.forward.test" {
        if !body
            .get("params")
            .and_then(Value::as_object)
            .is_some_and(Map::is_empty)
        {
            return invalid_parameter(action, "test accepts no params");
        }
        return match app.panel_sms_forward_test().await {
            Ok(value) => control_ok(action, value),
            Err(error) => control_failed(action, error),
        };
    }
    if action == "datad.ota.set" {
        let enabled = body
            .get("params")
            .and_then(Value::as_object)
            .filter(|params| params.len() == 1)
            .and_then(|params| params.get("enabled"))
            .and_then(Value::as_bool);
        let Some(enabled) = enabled else {
            return invalid_parameter(action, "enabled must be the only field and a boolean");
        };
        return match app.panel_ota_set(enabled).await {
            Ok(enabled) => control_ok(action, json!({"auto_update_enabled":enabled})),
            Err(error) => control_failed(action, error),
        };
    }
    if action == "device.login_info" {
        return readonly_ubus(action, "zwrt_web", "web_login_info", json!({})).await;
    }
    if action == "device.login" {
        let password_hash = body
            .get("params")
            .and_then(|value| value.get("password_hash"))
            .and_then(Value::as_str)
            .filter(|value| {
                value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            });
        let Some(password_hash) = password_hash else {
            return invalid_parameter(
                action,
                "password_hash must be a 64 character SHA-256 hex value",
            );
        };
        let password_hash = password_hash.to_ascii_uppercase();
        return match state::ubus("zwrt_web", "web_login", json!({"password":password_hash})).await {
            Ok(value)
                if value.get("result").and_then(Value::as_i64) == Some(0)
                    && value
                        .get("ubus_rpc_session")
                        .and_then(Value::as_str)
                        .is_some_and(|token| !token.is_empty()) =>
            {
                let token = value["ubus_rpc_session"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                *app.inner.device_session.lock().await = Some(DeviceSession {
                    _token: token,
                    _password_hash: password_hash,
                });
                control_ok(action, value)
            }
            Ok(_) => control_failed(action, "device login rejected".into()),
            Err(error) => control_failed(action, error),
        };
    }
    if action == "device.logout" {
        *app.inner.device_session.lock().await = None;
        return control_ok(action, json!({"logged_in":false}));
    }
    if action == "device.session_status" {
        return control_ok(
            action,
            json!({"logged_in":app.inner.device_session.lock().await.is_some()}),
        );
    }
    if action == "device.change_password" {
        let valid_hash = |name: &str| {
            body.get("params")
                .and_then(|v| v.get(name))
                .and_then(Value::as_str)
                .filter(|v| v.len() == 64 && v.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .map(|v| v.to_ascii_uppercase())
        };
        let (Some(old_hash), Some(new_hash)) = (valid_hash("old_hash"), valid_hash("new_hash"))
        else {
            return invalid_parameter(action, "old_hash and new_hash must be SHA-256 hex values");
        };
        return match state::ubus(
            "zwrt_web",
            "web_change_password",
            json!({"password_old":old_hash,"password_new":new_hash}),
        )
        .await
        {
            Ok(value) => {
                *app.inner.device_session.lock().await = None;
                control_ok(action, value)
            }
            Err(error) => control_failed(action, error),
        };
    }
    if action == "wifi.dual_band_status" {
        return match crate::wifi::dual_band_status().await {
            Ok(value) => control_ok(action, value),
            Err(error) => control_failed(action, error),
        };
    }
    if action == "wifi.status" {
        let mut result = serde_json::Map::new();
        let mut sections = vec!["main_2g", "main_5g"];
        if crate::wifi::band_present("6g").await {
            sections.push("main_6g");
        }
        for section in sections {
            let mut item = serde_json::Map::new();
            for field in [
                "ssid",
                "key",
                "encryption",
                "disabled",
                "hidden",
                "isolate",
                "pmf",
                "maxassoc",
            ] {
                item.insert(
                    field.into(),
                    json!(state::uci_read(&format!("wireless.{section}.{field}")).await),
                );
            }
            result.insert(section.into(), Value::Object(item));
        }
        return control_ok(action, Value::Object(result));
    }
    if action == "wifi.txpower.status" {
        let model = state::uci_read("zwrt_common_info.common_config.model_name").await;
        let hardware = state::uci_read("zwrt_common_info.common_config.hardware_version").await;
        if model != "MU5252" && !hardware.starts_with("MU5252_") {
            return (StatusCode::BAD_REQUEST,Json(json!({"ok":false,"action":action,"error":{"code":"invalid_parameter","message":"wifi power control is only supported on MU5252"}}))).into_response();
        }
        let mut result = Map::new();
        for (band, section, factory_limit) in [("2g", "wifi0", 19), ("5g", "wifi1", 18)] {
            let mut values = Vec::new();
            for option in ["disabled", "txpowerpercent", "txpower", "max_power"] {
                let raw = state::uci_read(&format!("wireless.{section}.{option}")).await;
                let Ok(value) = raw.parse::<i64>() else {
                    return control_failed(
                        action,
                        format!("failed to read {band} wifi power configuration"),
                    );
                };
                values.push(value);
            }
            result.insert(band.into(), json!({"enabled":values[0]==0,"percent":values[1],"txpower_dbm":values[2],"limit_dbm":values[3],"factory_limit_dbm":factory_limit}));
        }
        return control_ok(action, Value::Object(result));
    }
    if action == "wifi.advanced.status" {
        return match crate::wifi::advanced_status().await {
            Ok(value) => control_ok(action, value),
            Err(error) => control_failed(action, error),
        };
    }
    if action == "wireless.config" {
        let mutating = body.get("params").is_some_and(|params| {
            params.get("country").is_some() || params.get("channel").is_some()
        });
        if !mutating {
            return match crate::wifi::wireless_config_status().await {
                Ok(value) => control_ok(action, value),
                Err(error) => control_failed(action, error),
            };
        }
    }
    if action == "sleep.status" {
        return control_ok(
            action,
            json!({
                "idle_seconds":state::uci_read("zwrt_sleep.ztmp_time.SysIdTime").await,
                "enabled":state::uci_read("zwrt_sleep.ztmp_switch.sleepSwitch").await,
                "wakeup":state::uci_read("zwrt_sleep.ztmp_switch.wakeupSwitch").await,
                "status":state::uci_read("zwrt_sleep.ztmp_status.sleepStatus").await,
            }),
        );
    }
    if action == "usb.status" {
        let typec = state::ubus("zwrt_bsp.typec", "list", json!({})).await;
        let usb = state::ubus("zwrt_bsp.usb", "list", json!({})).await;
        return match (typec, usb) {
            (Ok(typec), Ok(usb)) => control_ok(
                action,
                json!({"typec":typec,"usb":usb,"link":app.snapshot().await.fields.get("usb")}),
            ),
            (Err(error), _) | (_, Err(error)) => control_failed(action, error),
        };
    }
    if action == "power.direct_supply.status" {
        return match state::ubus("zwrt_bsp.charger", "list", json!({})).await {
            Ok(value) => {
                let result = match value
                    .get("direct_power_supply_mode")
                    .and_then(Value::as_str)
                {
                    Some("enable") => json!({"supported":true,"enabled":true,"mode":"enable"}),
                    Some("disable") => json!({"supported":true,"enabled":false,"mode":"disable"}),
                    Some(_) => json!({"supported":true,"enabled":Value::Null,"mode":Value::Null}),
                    None => json!({"supported":false,"enabled":Value::Null,"mode":Value::Null}),
                };
                control_ok(action, result)
            }
            Err(error) => control_failed(action, error),
        };
    }
    if action == "apn.list" {
        let values = tokio::join!(
            state::ubus("zwrt_apn_object", "get_apn_mode", json!({})),
            state::ubus("zwrt_apn_object", "getAutoApnList", json!({})),
            state::ubus("zwrt_apn_object", "getManuApnList", json!({})),
            state::ubus("zwrt_apn_object", "get_enabled_manu_apn_id", json!({}))
        );
        return match values {
            (Ok(mode), Ok(automatic), Ok(manual), Ok(enabled)) => control_ok(
                action,
                json!({"mode":mode,"automatic":automatic,"manual":manual,"enabled":enabled}),
            ),
            (Err(error), _, _, _)
            | (_, Err(error), _, _)
            | (_, _, Err(error), _)
            | (_, _, _, Err(error)) => control_failed(action, error),
        };
    }
    if action == "client.access" {
        let values = tokio::join!(
            state::ubus(
                "uci",
                "get",
                json!({"config":"wireless","section":"main_2g"})
            ),
            state::ubus(
                "zwrt_router.api",
                "router_lan_access_list",
                json!({"start_id":1,"end_id":64})
            ),
            state::ubus(
                "zwrt_router.api",
                "router_wireless_access_list",
                json!({"start_id":1,"end_id":64})
            )
        );
        return match values {
            (Ok(policy), Ok(lan), Ok(wifi)) => {
                control_ok(action, json!({"policy":policy,"lan":lan,"wifi":wifi}))
            }
            (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                control_failed(action, error)
            }
        };
    }
    match crate::control::execute(action, body.get("params").unwrap_or(&json!({}))).await {
        crate::control::Outcome::Ok(value) => {
            let refresh = app.clone();
            tokio::spawn(async move { refresh.refresh_snapshot().await });
            return control_ok(action, value);
        }
        crate::control::Outcome::Invalid(error) => return invalid_parameter(action, &error),
        crate::control::Outcome::Failed(error) => return control_failed(action, error),
        crate::control::Outcome::Coded(code) => return coded_failure(action, code),
        crate::control::Outcome::NotHandled => {}
    }
    if action == "state.refresh" {
        let refresh = app.clone();
        tokio::spawn(async move { refresh.refresh_snapshot().await });
        return control_ok(action, json!({"queued":true}));
    }
    if action == "state.set_interval" {
        let milliseconds = body
            .get("params")
            .and_then(|value| value.get("milliseconds"))
            .and_then(Value::as_u64);
        let Some(milliseconds) = milliseconds.filter(|value| (500..=5000).contains(value)) else {
            return (StatusCode::BAD_REQUEST,Json(json!({"ok":false,"action":action,"error":{"code":"invalid_parameter","message":"milliseconds must be between 500 and 5000"}}))).into_response();
        };
        app.inner.interval_ms.store(milliseconds, Ordering::Relaxed);
        let refresh = app.clone();
        tokio::spawn(async move { refresh.refresh_snapshot().await });
        return control_ok(action, json!({"sample_interval_ms":milliseconds}));
    }
    if action == "qos.reload" {
        crate::qos::invalidate();
        let refresh = app.clone();
        tokio::spawn(async move { refresh.refresh_snapshot().await });
        return control_ok(action, json!({"queued":true}));
    }
    (
        StatusCode::NOT_FOUND,
        Json(json!({"ok":false,"action":action,"error":{"code":"unknown_action","message":"unsupported control action"}})),
    )
        .into_response()
}

fn control_ok(action: &str, value: Value) -> Response {
    (
        StatusCode::OK,
        Json(json!({"ok":true,"action":action,"result":value})),
    )
        .into_response()
}

fn control_failed(action: &str, error: String) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({"ok":false,"action":action,"error":{"code":"device_call_failed","message":error}})),
    )
        .into_response()
}

fn coded_failure(action: &str, code: &'static str) -> Response {
    let status = match code {
        "invalid_credentials" => StatusCode::UNAUTHORIZED,
        "device_session_rate_limited" => StatusCode::TOO_MANY_REQUESTS,
        "device_session_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
        "device_session_expired" => StatusCode::CONFLICT,
        _ => StatusCode::BAD_GATEWAY,
    };
    (
        status,
        Json(json!({"ok":false,"action":action,"error":{"code":code,"message":code}})),
    )
        .into_response()
}

fn invalid_parameter(action: &str, message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"ok":false,"action":action,"error":{"code":"invalid_parameter","message":message}})),
    )
        .into_response()
}

async fn readonly_ubus(action: &str, service: &str, method: &str, args: Value) -> Response {
    match state::ubus(service, method, args).await {
        Ok(value) => control_ok(action, value),
        Err(error) => control_failed(action, error),
    }
}
async fn app_management_guard(State(app): State<App>, request: Request, next: Next) -> Response {
    let mut response = if webshell_auth(&app, request.headers(), false).await {
        next.run(request).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    };
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

async fn cloud_config_get(State(app): State<App>) -> Json<Value> {
    Json(app.inner.cloud.read().await.public_config())
}

async fn app_task_status(app: &App) -> Value {
    let mut status = app.panel_task_status().await;
    let controls = capability_controls();
    let slots = app
        .inner
        .snapshot
        .read()
        .await
        .fields
        .get("sim")
        .and_then(|sim| sim.get("dual_sim"))
        .and_then(Value::as_i64)
        .unwrap_or(1);
    status["actions"] = json!(
        crate::task_schedule::ACTIONS
            .iter()
            .copied()
            .filter(|action| {
                match *action {
                    "sms.send_scheduled" => controls.contains(&"sms.send_raw"),
                    "sms.forward.device_info" => controls.contains(&"sms.forward.set"),
                    "sim.set_slot" => slots == 2 && controls.contains(action),
                    _ => controls.contains(action),
                }
            })
            .collect::<Vec<_>>()
    );
    status["max_tasks"] = json!(crate::task_schedule::MAX_TASKS);
    status["network_modes"] = json!([
        "WL_AND_5G",
        "LTE_AND_5G",
        "Only_5G",
        "WCDMA_AND_LTE",
        "Only_LTE",
        "Only_WCDMA"
    ]);
    status
}

fn task_app_reply(status: StatusCode, value: Value) -> Response {
    (
        status,
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(value),
    )
        .into_response()
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ActivityQuery {
    before: Option<u64>,
    category: Option<String>,
    #[serde(default)]
    failed: bool,
}
async fn activity_get(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<ActivityQuery>,
) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if q.category
        .as_deref()
        .is_some_and(|c| !["notification", "task", "recovery"].contains(&c))
    {
        return task_app_reply(
            StatusCode::BAD_REQUEST,
            json!({"error":"invalid_activity_filter"}),
        );
    }
    task_app_reply(
        StatusCode::OK,
        app.inner
            .activity
            .lock()
            .await
            .view(q.before, q.category.as_deref(), q.failed),
    )
}
async fn activity_clear(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !body.as_object().is_some_and(|v| v.is_empty()) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match app.inner.activity.lock().await.clear() {
        Ok(()) => task_app_reply(StatusCode::OK, json!({"ok":true})),
        Err(e) => task_app_reply(StatusCode::INTERNAL_SERVER_ERROR, json!({"error":e})),
    }
}
async fn recovery_get(State(app): State<App>, headers: HeaderMap) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    task_app_reply(StatusCode::OK, app.inner.recovery.lock().await.view())
}
async fn recovery_check(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !body.as_object().is_some_and(|v| v.is_empty()) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if !app
        .inner
        .recovery
        .lock()
        .await
        .reserve_check(crate::activity::now())
    {
        return task_app_reply(
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error":"probe_rate_limited"}),
        );
    }
    // Manual check never feeds recovery counters or executes recovery actions.
    let online = crate::network_recovery::probe().await;
    task_app_reply(
        StatusCode::OK,
        json!({"online":online,"checked_at":crate::activity::now()}),
    )
}
async fn recovery_put(
    State(app): State<App>,
    headers: HeaderMap,
    Json(config): Json<crate::network_recovery::Config>,
) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let enabled = config.enabled;
    let mut r = app.inner.recovery.lock().await;
    match r.update(config, crate::activity::now()) {
        Ok(()) => {
            let view = r.view();
            drop(r);
            app.record(
                "recovery",
                if enabled { "enable" } else { "disable" },
                "success",
                "",
                "",
            )
            .await;
            task_app_reply(StatusCode::OK, view)
        }
        Err(e) => task_app_reply(
            if e == "storage_failed" {
                StatusCode::INTERNAL_SERVER_ERROR
            } else {
                StatusCode::BAD_REQUEST
            },
            json!({"error":e}),
        ),
    }
}
async fn recovery_resume(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !body.as_object().is_some_and(|v| v.is_empty()) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let mut r = app.inner.recovery.lock().await;
    match r.resume(crate::activity::now()) {
        Ok(()) => {
            let view = r.view();
            drop(r);
            app.record("recovery", "resume", "success", "", "").await;
            task_app_reply(StatusCode::OK, view)
        }
        Err(e) => task_app_reply(StatusCode::INTERNAL_SERVER_ERROR, json!({"error":e})),
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    start: Option<String>,
    end: Option<String>,
}
async fn traffic_history_get(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<HistoryQuery>,
) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if q.start
        .iter()
        .chain(q.end.iter())
        .any(|s| !crate::traffic_history::valid_date(s))
        || matches!((&q.start, &q.end), (Some(a), Some(b)) if a > b)
    {
        return task_app_reply(
            StatusCode::BAD_REQUEST,
            json!({"error":"invalid_date_range"}),
        );
    }
    let history = app.inner.history.lock().await;
    let days = history.days();
    let oldest = days.first().map(|d| d.date.clone());
    let selected: Vec<_> = days
        .into_iter()
        .filter(|d| {
            q.start.as_ref().is_none_or(|s| &d.date >= s)
                && q.end.as_ref().is_none_or(|s| &d.date <= s)
        })
        .collect();
    task_app_reply(
        StatusCode::OK,
        json!({"days":selected,"device_date":crate::traffic_history::local_date(),
        "oldest_date":oldest,"sample_interval_seconds":300,"max_days":400}),
    )
}
async fn lan_view(app: &App) -> Value {
    crate::lan::settings(&*app.inner.snapshot.read().await, false)
}
async fn lan_get(State(app): State<App>, headers: HeaderMap) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    task_app_reply(StatusCode::OK, lan_view(&app).await)
}
async fn lan_apply(app: &App, action: &str, params: Value) -> Response {
    match crate::control::execute(action, &params).await {
        crate::control::Outcome::Ok(_) => {
            app.refresh_snapshot().await;
            let current = lan_view(app).await;
            task_app_reply(
                StatusCode::OK,
                json!({"verified":crate::lan::matches(&params,&current),"settings":current,"changed":true}),
            )
        }
        crate::control::Outcome::Invalid(_) => task_app_reply(
            StatusCode::BAD_REQUEST,
            json!({"error":"invalid_lan_settings"}),
        ),
        _ => task_app_reply(StatusCode::BAD_GATEWAY, json!({"error":"lan_apply_failed"})),
    }
}
async fn lan_put(
    State(app): State<App>,
    headers: HeaderMap,
    Json(input): Json<crate::lan::Update>,
) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let current = lan_view(&app).await;
    let params = match input.params(&current) {
        Ok(p) => p,
        Err(e) => return task_app_reply(StatusCode::BAD_REQUEST, json!({"error":e})),
    };
    if crate::lan::matches(&params, &current) {
        return task_app_reply(
            StatusCode::OK,
            json!({"verified":true,"settings":current,"changed":false}),
        );
    }
    lan_apply(&app, "lan.set", params).await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MtuUpdate {
    mtu: u64,
}
async fn lan_mtu_put(
    State(app): State<App>,
    headers: HeaderMap,
    Json(input): Json<MtuUpdate>,
) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !(576..=1500).contains(&input.mtu) {
        return task_app_reply(StatusCode::BAD_REQUEST, json!({"error":"invalid_mtu"}));
    }
    let current = lan_view(&app).await;
    if current["mtu"] == input.mtu {
        return task_app_reply(
            StatusCode::OK,
            json!({"verified":true,"settings":current,"changed":false}),
        );
    }
    lan_apply(&app, "lan.set_mtu", json!({"mtu":input.mtu.to_string()})).await
}

async fn tasks_get(State(app): State<App>, headers: HeaderMap) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    task_app_reply(StatusCode::OK, app_task_status(&app).await)
}

async fn tasks_put(
    State(app): State<App>,
    headers: HeaderMap,
    Json(input): Json<TaskInput>,
) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let status = app_task_status(&app).await;
    if !status["actions"]
        .as_array()
        .is_some_and(|actions| actions.iter().any(|a| a == &input.action))
        || input.action == "network.set_mode"
            && !status["network_modes"]
                .as_array()
                .is_some_and(|modes| modes.iter().any(|mode| mode == &input.params["mode"]))
    {
        return task_app_reply(
            StatusCode::BAD_REQUEST,
            json!({"error":"unsupported_task_action"}),
        );
    }
    match app.panel_task_upsert(input).await {
        Ok(_) => task_app_reply(StatusCode::OK, app_task_status(&app).await),
        Err(error) => task_app_reply(
            if error == "task_storage_failed" {
                StatusCode::INTERNAL_SERVER_ERROR
            } else {
                StatusCode::BAD_REQUEST
            },
            json!({"error":error}),
        ),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveTask {
    id: String,
}

async fn tasks_remove(
    State(app): State<App>,
    headers: HeaderMap,
    Json(input): Json<RemoveTask>,
) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match app.panel_task_remove(&input.id).await {
        Ok(_) => task_app_reply(StatusCode::OK, app_task_status(&app).await),
        Err(error) => task_app_reply(
            if error == "task_storage_failed" {
                StatusCode::INTERNAL_SERVER_ERROR
            } else {
                StatusCode::BAD_REQUEST
            },
            json!({"error":error}),
        ),
    }
}
// These routes require a token on BOTH listeners; the existing loopback-only
// /cloud/* management API and NMS remote control permissions remain unchanged.
async fn sms_forward_config_get(State(app): State<App>, headers: HeaderMap) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let config = app.inner.sms_forward.lock().await.app_config();
    let status = app.panel_sms_forward_status().await;
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(json!({"config":config,"status":status})),
    )
        .into_response()
}

async fn sms_forward_status_get(State(app): State<App>, headers: HeaderMap) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(app.panel_sms_forward_status().await).into_response()
}

async fn sms_forward_config_post(
    State(app): State<App>,
    headers: HeaderMap,
    Json(input): Json<SmsForwardUpdate>,
) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match app.panel_sms_forward_update(input).await {
        Ok(status) => Json(status).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response(),
    }
}

async fn sms_forward_test_post(
    State(app): State<App>,
    headers: HeaderMap,
    Json(input): Json<Value>,
) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !input.as_object().is_some_and(Map::is_empty) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid_test_params"})),
        )
            .into_response();
    }
    match app.panel_sms_forward_test().await {
        Ok(status) => Json(status).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response(),
    }
}

async fn cloud_app_config_get(State(app): State<App>, headers: HeaderMap) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(app.inner.cloud.read().await.public_config()).into_response()
}
async fn cloud_app_status(State(app): State<App>, headers: HeaderMap) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(app.inner.cloud.read().await.status()).into_response()
}
async fn cloud_app_config_post(
    State(app): State<App>,
    headers: HeaderMap,
    Json(input): Json<crate::cloud::AppUpdate>,
) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match app.inner.cloud.write().await.app_update(input) {
        Ok(value) => Json(value).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response(),
    }
}
async fn cloud_app_quick_connect(
    State(app): State<App>,
    headers: HeaderMap,
    Json(input): Json<crate::cloud::QuickConnect>,
) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let snapshot = app.snapshot().await;
    match app
        .inner
        .cloud
        .write()
        .await
        .app_quick_connect(input, &snapshot)
    {
        Ok(value) => Json(value).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response(),
    }
}
async fn cloud_status(State(app): State<App>) -> Json<Value> {
    Json(app.inner.cloud.read().await.status())
}
async fn cloud_config_post(State(app): State<App>, Json(update): Json<CloudUpdate>) -> Response {
    match app.inner.cloud.write().await.update(update) {
        Ok(value) => (StatusCode::OK, Json(value)).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response(),
    }
}
async fn cloud_quick_connect(
    State(app): State<App>,
    Json(input): Json<crate::cloud::QuickConnect>,
) -> Response {
    let snapshot = app.snapshot().await;
    match app
        .inner
        .cloud
        .write()
        .await
        .quick_connect(input, &snapshot)
    {
        Ok(value) => (StatusCode::OK, Json(value)).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response(),
    }
}
// App routes require a header credential even on the loopback listener.
// Status uses a published snapshot so downloads never block progress polling.
async fn ota_app_status(State(app): State<App>, headers: HeaderMap) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Ok(mut manager) = app.inner.ota.try_lock() {
        manager.reconcile_install_result();
    }
    task_app_reply(StatusCode::OK, app.inner.ota_view.borrow().clone())
}
async fn ota_app_config(
    State(app): State<App>,
    headers: HeaderMap,
    Json(config): Json<OtaConfig>,
) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(mut manager) = app.inner.ota.try_lock() else {
        return ota_app_busy();
    };
    match manager.update_config(config) {
        Ok(_) => task_app_reply(StatusCode::OK, app.inner.ota_view.borrow().clone()),
        Err(error) => task_app_reply(StatusCode::BAD_REQUEST, json!({"error":error})),
    }
}
fn ota_app_busy() -> Response {
    task_app_reply(StatusCode::CONFLICT, json!({"error":"更新任务正在运行"}))
}
async fn ota_app_check(
    State(app): State<App>,
    headers: HeaderMap,
    Json(_): Json<serde_json::Map<String, Value>>,
) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    {
        let Ok(mut manager) = app.inner.ota.try_lock() else {
            return ota_app_busy();
        };
        manager.reconcile_install_result();
        if manager.begin_update().is_err() {
            return ota_app_busy();
        }
    }
    tokio::spawn(async move {
        let mut manager = app.inner.ota.lock().await;
        let _ = manager.check().await;
        manager.finish_update();
    });
    task_app_reply(StatusCode::ACCEPTED, json!({"started":true}))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OtaApproval {
    candidate_id: String,
}
async fn ota_app_install(
    State(app): State<App>,
    headers: HeaderMap,
    Json(input): Json<OtaApproval>,
) -> Response {
    if !webshell_auth(&app, &headers, false).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let candidate = {
        let Ok(mut manager) = app.inner.ota.try_lock() else {
            return ota_app_busy();
        };
        let candidate = match manager.approved_candidate(&input.candidate_id) {
            Ok(c) => c,
            Err(error) => return task_app_reply(StatusCode::CONFLICT, json!({"error":error})),
        };
        if manager.begin_update().is_err() {
            return ota_app_busy();
        }
        candidate
    };
    tokio::spawn(async move {
        let snapshot = serde_json::to_value(app.snapshot().await).unwrap_or(Value::Null);
        let mut manager = app.inner.ota.lock().await;
        if let Err(error) = manager.install(&candidate, &snapshot, true).await {
            manager.fail(Some(&candidate), error);
        }
        manager.finish_update();
    });
    task_app_reply(StatusCode::ACCEPTED, json!({"started":true}))
}

async fn ota_config_get(State(app): State<App>) -> Json<Value> {
    Json(app.inner.ota.lock().await.config_json())
}
async fn ota_status(State(app): State<App>) -> Json<Value> {
    Json(app.inner.ota.lock().await.status_json())
}
async fn ota_config_post(
    State(app): State<App>,
    payload: Result<Json<OtaConfig>, JsonRejection>,
) -> Response {
    let Json(config) = match payload {
        Ok(value) => value,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"success":false,"error":"配置格式无效"})),
            )
                .into_response();
        }
    };
    match app.inner.ota.lock().await.update_config(config) {
        Ok(value) => (StatusCode::OK, Json(value)).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"success":false,"error":error})),
        )
            .into_response(),
    }
}
async fn ota_check(State(app): State<App>) -> Response {
    let mut manager = app.inner.ota.lock().await;
    match manager.check().await {
        Ok(candidate) => (
            StatusCode::OK,
            Json(json!({"success":true,"has_update":ota::has_update(&candidate),"manifest":candidate.manifest,"source":ota::source_name(&candidate.base_url)})),
        )
            .into_response(),
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"success":false,"error":error})),
        )
            .into_response(),
    }
}
async fn ota_update(State(app): State<App>) -> Response {
    {
        let mut manager = app.inner.ota.lock().await;
        if let Err(error) = manager.begin_update() {
            return (
                StatusCode::CONFLICT,
                Json(json!({"success":false,"error":error})),
            )
                .into_response();
        }
    }
    let task = app.clone();
    tokio::spawn(async move {
        let snapshot = serde_json::to_value(task.snapshot().await).unwrap_or(Value::Null);
        let mut manager = task.inner.ota.lock().await;
        match manager.check().await {
            Ok(candidate) if !ota::has_update(&candidate) => {
                manager.fail(Some(&candidate), "当前已是最新版本".into());
            }
            Ok(candidate) => match manager.install(&candidate, &snapshot, true).await {
                Ok(()) => {}
                Err(error) => {
                    manager.fail(Some(&candidate), error);
                }
            },
            Err(error) => manager.fail(None, error),
        }
        manager.finish_update();
    });
    (
        StatusCode::ACCEPTED,
        Json(json!({"success":true,"status":"started"})),
    )
        .into_response()
}
fn result(value: Result<Value, String>) -> Response {
    match value {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"ok":false,"error":e}))).into_response(),
    }
}
async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
    crate::cooling::shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    #[test]
    fn version_shape() {
        let v = serde_json::to_value(DatadVersion::default()).unwrap();
        assert_eq!(v["name"], "zwrt-datad");
    }

    #[test]
    fn capability_controls_match_complete_legacy_count() {
        let controls = capability_controls();
        assert_eq!(controls.len(), 88 + 6 + 2);
        for action in [
            "time.status",
            "time.config.set",
            "time.sync",
            "hosts.status",
            "hosts.save",
            "hosts.restore",
            "device.session.login",
            "datad.ota.set",
        ] {
            assert!(controls.contains(&action));
        }
        assert_eq!(
            controls.iter().copied().collect::<HashSet<_>>().len(),
            88 + 6 + 2
        );
    }

    #[test]
    fn missing_static_token_never_disables_lan_authentication() {
        assert!(!static_token_valid(None, None));
        assert!(!static_token_valid(None, Some("anything")));
        assert!(static_token_valid(Some("secret"), Some("secret")));
        assert!(!static_token_valid(Some("secret"), Some("wrong")));
    }

    #[test]
    fn lan_listener_rejects_public_sources() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "100.64.0.1",
            "100.127.255.254",
            "169.254.1.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:192.168.0.1",
        ] {
            assert!(lan_source_allowed(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "8.8.8.8",
            "100.63.255.255",
            "100.128.0.1",
            "172.15.255.255",
            "172.32.0.1",
            "2001:4860:4860::8888",
        ] {
            assert!(!lan_source_allowed(ip.parse().unwrap()), "{ip}");
        }
    }
}
