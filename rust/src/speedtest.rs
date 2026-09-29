//! Bounded device-origin download test for the NMS-hosted UFI panel.
//! The browser and NMS only receive small status frames, never the test body.
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::Command as StdCommand,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    process::Command,
    sync::Mutex,
    task::{JoinHandle, JoinSet},
};

const ENDPOINT: &str = "https://speed.cloudflare.com/__down";
const MIN_BYTES: u64 = 1 << 20;
const MAX_BYTES: u64 = 50 << 20;
const MAX_THREADS: u8 = 5;
const MAX_RUNS: u8 = 3;
const RUN_TIMEOUT: Duration = Duration::from_secs(18);

#[derive(Clone, Copy)]
struct Request {
    bytes: u64,
    threads: u8,
    runs: u8,
}

impl Request {
    fn parse(params: &Value) -> Result<Self, &'static str> {
        let map = params.as_object().ok_or("invalid_parameters")?;
        if map.len() != 3 {
            return Err("invalid_parameters");
        }
        let bytes = map
            .get("bytes")
            .and_then(Value::as_u64)
            .ok_or("invalid_bytes")?;
        let threads = map
            .get("threads")
            .and_then(Value::as_u64)
            .ok_or("invalid_threads")?;
        let runs = map
            .get("runs")
            .and_then(Value::as_u64)
            .ok_or("invalid_runs")?;
        if !(MIN_BYTES..=MAX_BYTES).contains(&bytes) {
            return Err("invalid_bytes");
        }
        if !(1..=u64::from(MAX_THREADS)).contains(&threads) {
            return Err("invalid_threads");
        }
        if runs != 1 && runs != u64::from(MAX_RUNS) {
            return Err("invalid_runs");
        }
        Ok(Self {
            bytes,
            threads: threads as u8,
            runs: runs as u8,
        })
    }
}

#[derive(Clone, Serialize)]
struct Status {
    supported: bool,
    state: &'static str,
    provider: &'static str,
    route_interface: Option<String>,
    requested_bytes: u64,
    received_bytes: u64,
    threads: u8,
    runs: u8,
    completed_runs: u8,
    failed_requests: u8,
    elapsed_ms: u64,
    average_mbps: Option<f64>,
}

impl Status {
    fn idle(supported: bool, route_interface: Option<String>) -> Self {
        Self {
            supported,
            state: if supported { "idle" } else { "unavailable" },
            provider: "cloudflare",
            route_interface,
            requested_bytes: 0,
            received_bytes: 0,
            threads: 0,
            runs: 0,
            completed_runs: 0,
            failed_requests: 0,
            elapsed_ms: 0,
            average_mbps: None,
        }
    }
}

pub struct SpeedTest {
    curl: PathBuf,
    status: Status,
    generation: u64,
    last_route_check: Instant,
    started: Option<Instant>,
    task: Option<JoinHandle<()>>,
}

fn safe_interface(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 32
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
        && value != "lo"
}

fn default_route_interface() -> Option<String> {
    let output = StdCommand::new("/sbin/ip")
        .args(["-4", "route", "get", "1.1.1.1"])
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.len() > 4096 {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    parse_route_interface(&raw)
}

fn parse_route_interface(raw: &str) -> Option<String> {
    let mut words = raw.split_whitespace();
    while let Some(word) = words.next() {
        if word == "dev" {
            return words
                .next()
                .filter(|name| safe_interface(name))
                .map(str::to_owned);
        }
    }
    None
}

impl SpeedTest {
    pub fn new() -> Self {
        let curl = std::env::var_os("ZWRT_DATAD_SPEEDTEST_CURL_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/usr/bin/curl"));
        let route_interface = default_route_interface();
        let supported = curl.is_file() && route_interface.is_some();
        Self {
            curl,
            status: Status::idle(supported, route_interface),
            generation: 0,
            last_route_check: Instant::now(),
            started: None,
            task: None,
        }
    }

    pub fn status(&self) -> Value {
        json!(self.status)
    }

    pub fn refresh_availability(&mut self) {
        if !matches!(self.status.state, "idle" | "unavailable")
            || self.last_route_check.elapsed() < Duration::from_secs(30)
        {
            return;
        }
        self.last_route_check = Instant::now();
        let interface = default_route_interface();
        self.status.supported = self.curl.is_file() && interface.is_some();
        self.status.route_interface = interface;
        self.status.state = if self.status.supported {
            "idle"
        } else {
            "unavailable"
        };
    }

    pub async fn start(shared: Arc<Mutex<Self>>, params: &Value) -> Result<Value, &'static str> {
        let route_interface = default_route_interface().ok_or("route_unavailable")?;
        Self::start_on_interface(shared, params, route_interface).await
    }

    async fn start_on_interface(
        shared: Arc<Mutex<Self>>,
        params: &Value,
        route_interface: String,
    ) -> Result<Value, &'static str> {
        let request = Request::parse(params)?;
        if !safe_interface(&route_interface) {
            return Err("route_unavailable");
        }
        let mut guard = shared.lock().await;
        if guard.status.state == "running" {
            return Err("already_running");
        }
        if !guard.curl.is_file() {
            return Err("curl_unavailable");
        }
        guard.generation = guard.generation.wrapping_add(1);
        let generation = guard.generation;
        guard.started = Some(Instant::now());
        guard.status = Status {
            supported: true,
            state: "running",
            provider: "cloudflare",
            route_interface: Some(route_interface.clone()),
            requested_bytes: request.bytes,
            received_bytes: 0,
            threads: request.threads,
            runs: request.runs,
            completed_runs: 0,
            failed_requests: 0,
            elapsed_ms: 0,
            average_mbps: None,
        };
        let curl = guard.curl.clone();
        let manager = shared.clone();
        guard.task = Some(tokio::spawn(async move {
            run(manager, generation, request, curl, route_interface).await;
        }));
        Ok(guard.status())
    }

    pub async fn stop(shared: Arc<Mutex<Self>>) -> Value {
        let task = {
            let mut guard = shared.lock().await;
            if guard.status.state != "running" {
                return guard.status();
            }
            guard.generation = guard.generation.wrapping_add(1);
            guard.status.state = "cancelled";
            guard.status.elapsed_ms = guard
                .started
                .take()
                .map_or(0, |start| start.elapsed().as_millis() as u64);
            guard.task.take()
        };
        if let Some(task) = task {
            task.abort();
            let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        }
        shared.lock().await.status()
    }
}

async fn fetch(curl: PathBuf, interface: String, bytes: u64) -> (u64, bool) {
    let mut command = Command::new(curl);
    command
        .kill_on_drop(true)
        .args([
            "-q",
            "--ipv4",
            "--silent",
            "--show-error",
            "--noproxy",
            "*",
            "--proxy",
            "",
            "--proto",
            "=https",
            "--connect-timeout",
            "5",
            "--max-time",
            "15",
            "--interface",
            &interface,
            "--max-filesize",
            &bytes.to_string(),
            "--output",
            "/dev/null",
            "--write-out",
            "%{http_code} %{size_download}",
        ])
        .arg(format!("{ENDPOINT}?bytes={bytes}"));
    let Ok(Ok(output)) = tokio::time::timeout(RUN_TIMEOUT, command.output()).await else {
        return (0, false);
    };
    if output.stdout.len() > 64 {
        return (0, false);
    }
    let Ok(raw) = String::from_utf8(output.stdout) else {
        return (0, false);
    };
    let mut parts = raw.split_whitespace();
    let (Some("200"), Some(size), None) = (parts.next(), parts.next(), parts.next()) else {
        return (0, false);
    };
    let Ok(received) = size.parse::<u64>() else {
        return (0, false);
    };
    if received > bytes {
        return (0, false);
    }
    (received, output.status.success() && received == bytes)
}

async fn run(
    manager: Arc<Mutex<SpeedTest>>,
    generation: u64,
    request: Request,
    curl: PathBuf,
    interface: String,
) {
    let started = Instant::now();
    let total_parts = u64::from(request.threads) * u64::from(request.runs);
    let mut received = 0_u64;
    let mut failed = 0_u8;
    for run_index in 0..request.runs {
        let mut workers = JoinSet::new();
        for thread_index in 0..request.threads {
            let part_index =
                u64::from(run_index) * u64::from(request.threads) + u64::from(thread_index);
            let bytes =
                request.bytes / total_parts + u64::from(part_index < request.bytes % total_parts);
            workers.spawn(fetch(curl.clone(), interface.clone(), bytes));
        }
        let mut joined = 0_u8;
        let result = tokio::time::timeout(RUN_TIMEOUT, async {
            while let Some(result) = workers.join_next().await {
                joined += 1;
                match result {
                    Ok((bytes, true)) => received = received.saturating_add(bytes),
                    Ok((bytes, false)) => {
                        received = received.saturating_add(bytes);
                        failed += 1;
                    }
                    Err(_) => failed += 1,
                }
            }
        })
        .await;
        if result.is_err() {
            workers.abort_all();
            failed = failed.saturating_add(request.threads.saturating_sub(joined));
        }
        let mut guard = manager.lock().await;
        if guard.generation != generation {
            return;
        }
        guard.status.received_bytes = received.min(request.bytes);
        guard.status.failed_requests = failed;
        guard.status.completed_runs = run_index + 1;
        guard.status.elapsed_ms = started.elapsed().as_millis() as u64;
    }
    let mut guard = manager.lock().await;
    if guard.generation != generation {
        return;
    }
    let seconds = started.elapsed().as_secs_f64().max(0.001);
    guard.status.average_mbps =
        (received > 0).then_some((received as f64 * 8.0 / seconds / 1_000_000.0).min(100_000.0));
    guard.status.state = if received == 0 {
        "error"
    } else if failed > 0 {
        "partial"
    } else {
        "completed"
    };
    guard.status.elapsed_ms = started.elapsed().as_millis() as u64;
    guard.started = None;
    guard.task = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_parameters_and_interfaces() {
        assert!(Request::parse(&json!({"bytes":MIN_BYTES,"threads":1,"runs":1})).is_ok());
        for params in [
            json!({"bytes":MAX_BYTES+1,"threads":1,"runs":1}),
            json!({"bytes":MIN_BYTES,"threads":6,"runs":1}),
            json!({"bytes":MIN_BYTES,"threads":1,"runs":2}),
            json!({"bytes":MIN_BYTES,"threads":1,"runs":1,"url":"http://127.0.0.1"}),
        ] {
            assert!(Request::parse(&params).is_err());
        }
        assert!(safe_interface("rmnet_data0"));
        assert!(!safe_interface("lo"));
        assert!(!safe_interface("--proxy"));
        assert!(!safe_interface("rmnet;reboot"));
        assert_eq!(
            parse_route_interface("1.1.1.1 via 10.0.0.1 dev rmnet_data0 src 10.0.0.2"),
            Some("rmnet_data0".into())
        );
        assert!(parse_route_interface("1.1.1.1 dev --proxy").is_none());
    }

    #[tokio::test]
    async fn bounded_runs_use_only_the_fixed_endpoint_and_can_be_cancelled() {
        use std::{fs, os::unix::fs::PermissionsExt};
        let dir = std::env::temp_dir().join(format!(
            "datad-speedtest-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&dir).unwrap();
        let script = dir.join("curl-mock.sh");
        let log = dir.join("requests.log");
        fs::write(&script,format!("#!/bin/sh\ntest \"$1\" = \"-q\" || exit 1\ncase \" $* \" in *\" --interface rmnet_data0 \"*) ;; *) exit 1;; esac\nfor arg in \"$@\"; do case \"$arg\" in https://speed.cloudflare.com/__down?bytes=*) size=${{arg##*=}};; esac; done\nprintf '%s\\n' \"$size\" >> '{}'\nprintf '200 %s' \"$size\"\n",log.display())).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let manager = Arc::new(Mutex::new(SpeedTest {
            curl: script.clone(),
            status: Status::idle(true, Some("rmnet_data0".into())),
            generation: 0,
            last_route_check: Instant::now(),
            started: None,
            task: None,
        }));
        SpeedTest::start_on_interface(
            manager.clone(),
            &json!({"bytes":MIN_BYTES,"threads":2,"runs":3}),
            "rmnet_data0".into(),
        )
        .await
        .unwrap();
        for _ in 0..100 {
            if manager.lock().await.status.state != "running" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let status = manager.lock().await.status();
        assert_eq!(status["state"], "completed");
        assert_eq!(status["received_bytes"], MIN_BYTES);
        assert_eq!(status["completed_runs"], 3);
        let amounts: Vec<u64> = fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|line| line.parse().unwrap())
            .collect();
        assert_eq!(amounts.len(), 6);
        assert_eq!(amounts.iter().sum::<u64>(), MIN_BYTES);
        fs::write(&script, "#!/bin/sh\nexec sleep 3\n").unwrap();
        SpeedTest::start_on_interface(
            manager.clone(),
            &json!({"bytes":MIN_BYTES,"threads":1,"runs":1}),
            "rmnet_data0".into(),
        )
        .await
        .unwrap();
        let stopped = SpeedTest::stop(manager).await;
        assert_eq!(stopped["state"], "cancelled");
        fs::remove_dir_all(dir).unwrap();
    }
}
