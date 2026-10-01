//! Authenticated, session-scoped typed filesystem operations. File content is
//! never sampled, retained in telemetry, or passed to a command shell.
mod archive;
mod fsops;
#[cfg(test)]
mod tests;
pub(super) mod transfer;

use serde_json::Value;
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
#[derive(Clone)]
pub(super) struct Guard {
    cancel: Arc<AtomicBool>,
    deadline: Instant,
}
impl Guard {
    pub(super) fn check(&self) -> Result<()> {
        if self.cancel.load(Ordering::Acquire) || Instant::now() >= self.deadline {
            return Err("files_cancelled".into());
        }
        Ok(())
    }
}

pub(crate) const MAX_FILE_BYTES: u64 = 500 * 1024 * 1024;
pub(crate) const CHUNK_BYTES: usize = 128 * 1024;
#[derive(Debug)]
pub(crate) struct FileError {
    pub code: &'static str,
}
impl From<&'static str> for FileError {
    fn from(code: &'static str) -> Self {
        Self { code }
    }
}
pub(crate) type Result<T> = std::result::Result<T, FileError>;
pub(crate) struct Session {
    guard: Guard,
    uploads: HashMap<String, transfer::Upload>,
    downloads: HashMap<String, transfer::Download>,
}
impl Session {
    pub(crate) fn new(_data_dir: &Path) -> Result<Self> {
        Ok(Self {
            guard: Guard {
                cancel: Arc::new(AtomicBool::new(false)),
                deadline: Instant::now() + Duration::from_secs(3600),
            },
            uploads: HashMap::new(),
            downloads: HashMap::new(),
        })
    }
    pub(crate) fn cancellation_token(&self) -> Arc<AtomicBool> {
        self.guard.cancel.clone()
    }
    pub(crate) fn set_deadline(&mut self, deadline: Instant) {
        self.guard.deadline = deadline;
    }
    pub(crate) async fn execute(
        &mut self,
        action: &str,
        params: Value,
        confirmed: bool,
    ) -> Result<Value> {
        self.guard.check()?;
        let write = matches!(
            action,
            "files.mkdir"
                | "files.touch"
                | "files.rename"
                | "files.chmod"
                | "files.remove"
                | "files.compress"
                | "files.extract"
                | "files.upload.begin"
                | "files.upload.chunk"
                | "files.upload.commit"
                | "files.upload.abort"
        );
        if write && !confirmed {
            return Err("confirmation_required".into());
        }
        match action {
            "files.status" => fsops::status(params),
            "files.list" => fsops::list(params),
            "files.stat" => fsops::stat(params),
            "files.disk" => fsops::disk(params),
            "files.mkdir" => fsops::mkdir(params),
            "files.touch" => fsops::touch(params),
            "files.rename" => fsops::rename(params),
            "files.chmod" => fsops::chmod(params),
            "files.remove" => fsops::remove(params, &self.guard),
            "files.compress" => archive::compress(params, &self.guard),
            "files.extract" => archive::extract(params, &self.guard),
            "files.upload.begin" => self.upload_begin(params),
            "files.upload.chunk" => self.upload_chunk(params),
            "files.upload.commit" => self.upload_commit(params),
            "files.upload.abort" => self.upload_abort(params),
            "files.download.begin" => self.download_begin(params),
            "files.download.chunk" => self.download_chunk(params),
            "files.download.end" => self.download_end(params),
            _ => Err("unsupported_action".into()),
        }
    }
}
