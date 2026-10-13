//! Authenticated, on-demand EFS operations in a crash-isolated native worker.
//! No vendor library is loaded into datad and no operation uses a shell.
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command, sync::Semaphore};

static WORKER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/efs-worker"));
pub const MAX_TRANSFER: usize = 1024 * 1024;
pub const MAX_READ: u32 = 64 * 1024;
pub const MAX_WRITE: usize = 32 * 1024;
const MAX_OUTPUT: usize = 3 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(120);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathQuery {
    pub path: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NvReadQuery {
    pub item: u16,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadQuery {
    pub path: String,
    pub length: Option<u32>,
    pub offset: Option<u32>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteRequest {
    pub path: String,
    pub hex: String,
    pub offset: Option<u32>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteRequest {
    pub path: String,
    #[serde(default)]
    pub directory: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryRequest {
    pub path: String,
    pub mode: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameRequest {
    pub path: String,
    pub destination: String,
}

pub enum Operation {
    NvRead(NvReadQuery),
    List(String),
    Stat(String),
    Read(ReadQuery),
    Write(WriteRequest),
    Delete(DeleteRequest),
    Mkdir(DirectoryRequest),
    Rename(RenameRequest),
    Upload { path: String, bytes: Vec<u8> },
    Download(String),
}
impl Operation {
    pub fn name(&self) -> &'static str {
        match self {
            Self::NvRead(_) => "nvread",
            Self::List(_) => "list",
            Self::Stat(_) => "stat",
            Self::Read(_) => "read",
            Self::Write(_) => "write",
            Self::Delete(_) => "delete",
            Self::Mkdir(_) => "mkdir",
            Self::Rename(_) => "rename",
            Self::Upload { .. } => "upload",
            Self::Download(_) => "download",
        }
    }
    fn path(&self) -> &str {
        match self {
            Self::NvRead(_) => "/",
            Self::List(p) | Self::Stat(p) | Self::Download(p) => p,
            Self::Read(r) => &r.path,
            Self::Write(r) => &r.path,
            Self::Delete(r) => &r.path,
            Self::Mkdir(r) => &r.path,
            Self::Rename(r) => &r.path,
            Self::Upload { path, .. } => path,
        }
    }
    fn validate(&self) -> Result<(), Error> {
        validate_path(self.path())?;
        match self {
            Self::Read(r) => {
                let n = r.length.unwrap_or(4096);
                if n > MAX_READ || r.offset.unwrap_or(0).checked_add(n).is_none() {
                    return Err(Error::input("read range exceeds limits"));
                }
            }
            Self::Write(r) => {
                if r.hex.len() > MAX_WRITE * 2
                    || r.hex.len() % 2 != 0
                    || !r.hex.bytes().all(|b| b.is_ascii_hexdigit())
                    || r.offset
                        .unwrap_or(0)
                        .checked_add((r.hex.len() / 2) as u32)
                        .is_none()
                {
                    return Err(Error::input("hex or write range is invalid"));
                }
            }
            Self::Mkdir(r) => {
                if let Some(mode) = &r.mode
                    && (mode.is_empty()
                        || mode.len() > 4
                        || !mode.bytes().all(|b| (b'0'..=b'7').contains(&b))
                        || u32::from_str_radix(mode, 8).ok().is_none_or(|v| v > 0o777))
                {
                    return Err(Error::input("mode must be octal 0000..0777"));
                }
            }
            Self::Rename(r) => {
                validate_path(&r.destination)?;
                if r.path.len() + r.destination.len() + 6 > 1024 {
                    return Err(Error::input("rename paths exceed packet limit"));
                }
            }
            Self::Upload { bytes, .. } if bytes.len() > MAX_TRANSFER => {
                return Err(Error::input("upload exceeds 1 MiB"));
            }
            _ => {}
        }
        if matches!(
            self,
            Self::Write(_)
                | Self::Delete(_)
                | Self::Mkdir(_)
                | Self::Rename(_)
                | Self::Upload { .. }
        ) {
            validate_mutation_path(self.path())?;
            if let Self::Rename(r) = self {
                validate_mutation_path(&r.destination)?;
            }
        }
        Ok(())
    }
}
fn validate_path(path: &str) -> Result<(), Error> {
    if !path.starts_with('/')
        || path.len() > 1011
        || path.chars().any(char::is_control)
        || path.split('/').any(|v| v == "." || v == "..")
        || path.contains("//")
        || (path.len() > 1 && path.ends_with('/'))
    {
        return Err(Error::input(
            "path must be an absolute EFS path without control or dot components",
        ));
    }
    Ok(())
}
fn validate_mutation_path(path: &str) -> Result<(), Error> {
    if path.split('/').filter(|s| !s.is_empty()).count() < 2 {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "efs_root_readonly",
            "EFS root and its direct children are read-only",
        ));
    }
    Ok(())
}

pub struct Error {
    pub status: StatusCode,
    pub body: Value,
}
impl Error {
    fn new(status: StatusCode, code: &str, message: &str) -> Self {
        Self {
            status,
            body: json!({"ok":false,"error":{"code":code,"message":message}}),
        }
    }
    fn input(message: &str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_parameter", message)
    }
    fn storage() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "efs_storage_failed",
            "private EFS workspace unavailable",
        )
    }
    fn worker() -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            "efs_worker_failed",
            "EFS worker did not complete",
        )
    }
    fn unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "efs_unavailable",
            "EFS worker is not available in this build",
        )
    }
    fn protocol() -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            "efs_worker_protocol",
            "invalid EFS worker response",
        )
    }
}
pub enum Output {
    Json(Value),
    Binary(Vec<u8>),
}

#[derive(Clone)]
pub struct Efs {
    directory: PathBuf,
    slots: Arc<Semaphore>,
}
impl Efs {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            directory: data_dir.join("efs"),
            slots: Arc::new(Semaphore::new(1)),
        }
    }
    pub fn status(&self) -> Value {
        let built = !WORKER.is_empty() || test_worker().is_some();
        json!({"ok":true,"available":built,"backend":"libdiag_worker",
            "reason":if built {"not_probed"} else {"worker_not_built"},
            "device_verified":false,
            "safety":{"version":1,"root_readonly":true,"recursive_delete":false,"no_symlink_mutations":true},
            "operations":["list","stat","read","write","upload","download","delete","mkdir","rename","nvread"],
            "limits":{"read_bytes":MAX_READ,"write_bytes":MAX_WRITE,"transfer_bytes":MAX_TRANSFER,
                "directory_entries":4096,"timeout_seconds":120,"concurrency":1}})
    }
    pub async fn execute(&self, operation: Operation) -> Result<Output, Error> {
        operation.validate()?;
        let permit = self.slots.clone().try_acquire_owned().map_err(|_| {
            Error::new(
                StatusCode::CONFLICT,
                "efs_busy",
                "another EFS operation is running",
            )
        })?;
        let this = self.clone();
        // Retain the permit, child and temporary files even when an HTTP caller
        // disconnects. A second mutation cannot overlap an abandoned first one.
        tokio::spawn(async move {
            let _permit = permit;
            this.run(operation).await
        })
        .await
        .map_err(|_| Error::worker())?
    }
    fn directory(&self) -> Result<(), Error> {
        let parent = self.directory.parent().ok_or_else(Error::storage)?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|_| Error::storage())?;
        let owner = unsafe { libc::geteuid() };
        let m = fs::symlink_metadata(parent).map_err(|_| Error::storage())?;
        if !m.is_dir() || m.uid() != owner || m.mode() & 0o022 != 0 {
            return Err(Error::storage());
        }
        match fs::DirBuilder::new().mode(0o700).create(&self.directory) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(Error::storage()),
        }
        let m = fs::symlink_metadata(&self.directory).map_err(|_| Error::storage())?;
        if !m.is_dir() || m.uid() != owner || m.mode() & 0o077 != 0 {
            return Err(Error::storage());
        }
        Ok(())
    }
    fn worker(&self) -> Result<PathBuf, Error> {
        if let Some(path) = test_worker() {
            return Ok(path);
        }
        if WORKER.is_empty() {
            return Err(Error::unavailable());
        }
        let path = self.directory.join("efs-worker");
        if let Ok(file) = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
        {
            let m = file.metadata().map_err(|_| Error::storage())?;
            if !m.is_file() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
                return Err(Error::storage());
            }
            let mut bytes = Vec::new();
            file.take(WORKER.len() as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| Error::storage())?;
            if bytes == WORKER {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o500))
                    .map_err(|_| Error::storage())?;
                return Ok(path);
            }
        } else if fs::symlink_metadata(&path).is_ok() {
            return Err(Error::storage());
        }
        let temporary = self
            .directory
            .join(format!(".worker-{:016x}", rand::random::<u64>()));
        let mut file = private_file(&temporary)?;
        let result = file
            .write_all(WORKER)
            .and_then(|_| file.set_permissions(fs::Permissions::from_mode(0o500)))
            .and_then(|_| file.sync_all())
            .and_then(|_| fs::rename(&temporary, &path));
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
            return Err(Error::storage());
        }
        Ok(path)
    }
    async fn run(&self, operation: Operation) -> Result<Output, Error> {
        let api_name = operation.name();
        if WORKER.is_empty() && test_worker().is_none() {
            return Err(Error::unavailable());
        }
        self.directory()?;
        let worker = self.worker()?;
        let mut temporaries = TemporaryFiles(Vec::new());
        let download = matches!(operation, Operation::Download(_));
        let mut local = None;
        let mut args = vec!["--json".to_owned(), "--guarded".to_owned()];
        match operation {
            Operation::NvRead(q) => args.extend(["nvread".into(), q.item.to_string()]),
            Operation::List(p) => args.extend(["ls".into(), p]),
            Operation::Stat(p) => args.extend(["stat".into(), p]),
            Operation::Read(r) => args.extend([
                "read".into(),
                r.path,
                r.length.unwrap_or(4096).to_string(),
                r.offset.unwrap_or(0).to_string(),
            ]),
            Operation::Write(r) => {
                let bytes: Vec<u8> = r
                    .hex
                    .as_bytes()
                    .chunks_exact(2)
                    .map(|pair| {
                        let digit = |b: u8| {
                            if b <= b'9' {
                                b - b'0'
                            } else {
                                b.to_ascii_lowercase() - b'a' + 10
                            }
                        };
                        (digit(pair[0]) << 4) | digit(pair[1])
                    })
                    .collect();
                let p = self
                    .directory
                    .join(format!("write-{:016x}", rand::random::<u64>()));
                let mut file = private_file(&p)?;
                temporaries.0.push(p.clone());
                file.write_all(&bytes).map_err(|_| Error::storage())?;
                args.extend([
                    "write-file".into(),
                    r.path,
                    p.to_string_lossy().into_owned(),
                ]);
                if let Some(offset) = r.offset {
                    args.push(offset.to_string());
                }
            }
            Operation::Delete(r) => args.extend([
                if r.directory {
                    "rmdir".into()
                } else {
                    "rm".into()
                },
                r.path,
            ]),
            Operation::Mkdir(r) => args.extend([
                "mkdir".into(),
                r.path,
                r.mode.unwrap_or_else(|| "0755".into()),
            ]),
            Operation::Rename(r) => args.extend(["rename".into(), r.path, r.destination]),
            Operation::Upload { path, bytes } => {
                let p = self
                    .directory
                    .join(format!("upload-{:016x}", rand::random::<u64>()));
                let mut file = private_file(&p)?;
                temporaries.0.push(p.clone());
                file.write_all(&bytes).map_err(|_| Error::storage())?;
                args.extend(["put".into(), p.to_string_lossy().into_owned(), path]);
                local = Some(p);
            }
            Operation::Download(path) => {
                let p = self
                    .directory
                    .join(format!("download-{:016x}", rand::random::<u64>()));
                // Reserve the target with exclusive creation. The worker writes
                // a separate .part file and atomically replaces this empty file.
                drop(private_file(&p)?);
                temporaries.0.push(p.clone());
                args.extend(["get".into(), path, p.to_string_lossy().into_owned()]);
                local = Some(p);
            }
        }
        let expected = args[2].clone();
        let mut process = Command::new(worker);
        process
            .args(&args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .current_dir(&self.directory)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(debug_assertions)]
        for key in [
            "ZWRT_DATAD_EFS_TEST_CONFIG",
            "MOCK_SCENARIO",
            "MOCK_TRACE",
            "MOCK_GUARD_TREE",
        ] {
            if let Some(value) = std::env::var_os(key) {
                process.env(key, value);
            }
        }
        let mut child = process.spawn().map_err(|_| Error::worker())?;
        if download && let (Some(pid), Some(path)) = (child.id(), local.as_ref()) {
            temporaries
                .0
                .push(PathBuf::from(format!("{}.part.{pid}", path.display())));
        }
        let stdout = child.stdout.take().ok_or_else(Error::worker)?;
        let transaction = async {
            let mut bytes = Vec::new();
            stdout
                .take(MAX_OUTPUT as u64 + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| Error::worker())?;
            if bytes.len() > MAX_OUTPUT {
                return Err(Error::protocol());
            }
            let status = child.wait().await.map_err(|_| Error::worker())?;
            let mut reply = parse_reply(&bytes, status.success(), &expected).map_err(|mut e| {
                e.body["command"] = json!(api_name);
                e
            })?;
            reply["command"] = json!(api_name);
            if download {
                let p = local.as_ref().ok_or_else(Error::storage)?;
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(p)
                    .map_err(|_| Error::storage())?;
                if file.metadata().map_err(|_| Error::storage())?.len() > MAX_TRANSFER as u64 {
                    return Err(Error::protocol());
                }
                let mut data = Vec::new();
                file.take(MAX_TRANSFER as u64 + 1)
                    .read_to_end(&mut data)
                    .map_err(|_| Error::storage())?;
                if data.len() > MAX_TRANSFER {
                    return Err(Error::protocol());
                }
                if reply["data"]["bytes"].as_u64() != Some(data.len() as u64) {
                    return Err(Error::protocol());
                }
                return Ok(Output::Binary(data));
            }
            // Device-local spool names are implementation details, not API data.
            if let Some(data) = reply.get_mut("data").and_then(Value::as_object_mut) {
                data.remove("local_path");
            }
            Ok(Output::Json(reply))
        };
        let outcome = tokio::time::timeout(DEADLINE, transaction).await;
        if !matches!(&outcome, Ok(Ok(_))) {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        match outcome {
            Ok(result) => result,
            Err(_) => Err(Error::new(
                StatusCode::GATEWAY_TIMEOUT,
                "efs_timeout",
                "EFS operation timed out; a mutation may be partial",
            )),
        }
    }
}
fn private_file(path: &Path) -> Result<File, Error> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| Error::storage())
}
struct TemporaryFiles(Vec<PathBuf>);
impl Drop for TemporaryFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}
fn test_worker() -> Option<PathBuf> {
    #[cfg(debug_assertions)]
    {
        std::env::var_os("ZWRT_DATAD_EFS_TEST_WORKER").map(PathBuf::from)
    }
    #[cfg(not(debug_assertions))]
    {
        None
    }
}
fn parse_reply(bytes: &[u8], success: bool, expected: &str) -> Result<Value, Error> {
    let reply: Value = serde_json::from_slice(bytes).map_err(|_| Error::protocol())?;
    let ok = reply
        .get("ok")
        .and_then(Value::as_bool)
        .ok_or_else(Error::protocol)?;
    if ok != success || reply.get("command").and_then(Value::as_str) != Some(expected) {
        return Err(Error::protocol());
    }
    if ok {
        if !reply.get("data").is_some_and(Value::is_object) {
            return Err(Error::protocol());
        }
        return Ok(reply);
    }
    let error = reply
        .get("error")
        .and_then(Value::as_object)
        .ok_or_else(Error::protocol)?;
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(Error::protocol)?;
    let status = match code {
        "INVALID_ARGUMENT" => StatusCode::BAD_REQUEST,
        "ROOT_READONLY" | "UNSAFE_PATH" => StatusCode::FORBIDDEN,
        "TIMEOUT" => StatusCode::GATEWAY_TIMEOUT,
        "LIBRARY_ERROR" => StatusCode::SERVICE_UNAVAILABLE,
        "LIMIT_EXCEEDED" => StatusCode::PAYLOAD_TOO_LARGE,
        "EFS_ERROR" => match error.get("efs_errno").and_then(Value::as_u64) {
            Some(2) => StatusCode::NOT_FOUND,
            Some(1 | 13) => StatusCode::FORBIDDEN,
            Some(17 | 39) => StatusCode::CONFLICT,
            Some(28) => StatusCode::INSUFFICIENT_STORAGE,
            _ => StatusCode::BAD_GATEWAY,
        },
        _ => StatusCode::BAD_GATEWAY,
    };
    Err(Error {
        status,
        body: reply,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_paths_hex_and_ranges_before_running_a_worker() {
        for path in ["relative", "/a/../b", "/a/./b", "/a\0b", "/a\nb"] {
            assert!(validate_path(path).is_err());
        }
        assert!(validate_path("/nvefs/a $(id); 中文").is_ok());
        assert!(
            Operation::Write(WriteRequest {
                path: "/file".into(),
                hex: "0x".into(),
                offset: None
            })
            .validate()
            .is_err()
        );
        assert!(
            Operation::Read(ReadQuery {
                path: "/file".into(),
                length: Some(2),
                offset: Some(u32::MAX)
            })
            .validate()
            .is_err()
        );
        assert!(
            Operation::Mkdir(DirectoryRequest {
                path: "/dir".into(),
                mode: Some("1000".into())
            })
            .validate()
            .is_err()
        );
    }
    #[test]
    fn rejects_crashed_wrong_or_mixed_worker_output() {
        for bytes in [
            b"log\n{}".as_slice(),
            b"{}",
            b"{\"ok\":true,\"command\":\"rm\",\"data\":{}}",
        ] {
            assert!(parse_reply(bytes, true, "ls").is_err());
        }
        assert!(parse_reply(b"{\"ok\":true,\"command\":\"ls\",\"data\":{}}", false, "ls").is_err());
    }
    #[test]
    fn preserves_device_errors_and_partial_progress() {
        let e=parse_reply(b"{\"ok\":false,\"command\":\"write\",\"error\":{\"code\":\"EFS_ERROR\",\"efs_errno\":28,\"bytes_transferred\":150}}",false,"write").err().unwrap();
        assert_eq!(e.status, StatusCode::INSUFFICIENT_STORAGE);
        assert_eq!(e.body["error"]["bytes_transferred"], 150);
    }
}
