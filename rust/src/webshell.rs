use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::PermissionsExt,
    },
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::unix::AsyncFd,
    process::{Child, Command},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc},
};

pub const MAX_MESSAGE_SIZE: usize = 16 * 1024;
const MAX_SESSIONS: usize = 4;
const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const READ_SIZE: usize = 8 * 1024;

#[derive(Clone)]
pub struct WebShell {
    enabled: bool,
    slots: Arc<Semaphore>,
    active: Arc<AtomicUsize>,
}

impl WebShell {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            slots: Arc::new(Semaphore::new(MAX_SESSIONS)),
            active: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn status(&self) -> Value {
        json!({
            "enabled": self.enabled,
            "active_sessions": self.active.load(Ordering::Relaxed),
            "max_sessions": MAX_SESSIONS,
            "protocol": "websocket-binary-v1",
        })
    }

    pub fn try_acquire(&self) -> Option<OwnedSemaphorePermit> {
        self.slots.clone().try_acquire_owned().ok()
    }

    pub async fn serve(self, socket: WebSocket, permit: OwnedSemaphorePermit) {
        let _permit = permit;
        let _active = ActiveGuard::new(self.active.clone());
        if let Ok(mut process) = PtyProcess::spawn() {
            run_session(socket, &mut process).await;
            process.terminate().await;
        }
    }
}

struct ActiveGuard(Arc<AtomicUsize>);

impl ActiveGuard {
    fn new(active: Arc<AtomicUsize>) -> Self {
        active.fetch_add(1, Ordering::Relaxed);
        Self(active)
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

struct PtyProcess {
    master: AsyncFd<File>,
    child: Child,
    pid: i32,
}

impl PtyProcess {
    fn spawn() -> io::Result<Self> {
        let shell = select_shell().ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let mut master_fd = -1;
        let mut slave_fd = -1;
        if unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let master = unsafe { File::from_raw_fd(master_fd) };
        let slave = unsafe { File::from_raw_fd(slave_fd) };
        set_fd_flag(
            master.as_raw_fd(),
            libc::F_GETFL,
            libc::F_SETFL,
            libc::O_NONBLOCK,
        )?;
        set_fd_flag(
            master.as_raw_fd(),
            libc::F_GETFD,
            libc::F_SETFD,
            libc::FD_CLOEXEC,
        )?;

        let mut command = Command::new(shell);
        command
            .arg("-i")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave))
            .env_clear()
            .env("HOME", "/root")
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin:/system/bin")
            .env("TERM", "xterm-256color")
            .env("LANG", "C.UTF-8")
            .kill_on_drop(true);
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        let pid = i32::try_from(child.id().unwrap_or_default()).unwrap_or_default();
        Ok(Self {
            master: AsyncFd::new(master)?,
            child,
            pid,
        })
    }

    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let result = unsafe {
            libc::ioctl(
                self.master.get_ref().as_raw_fd(),
                libc::TIOCSWINSZ as _,
                &size,
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    async fn terminate(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            if self.pid > 0 {
                unsafe {
                    libc::kill(-self.pid, libc::SIGHUP);
                    libc::kill(-self.pid, libc::SIGKILL);
                }
            }
            if tokio::time::timeout(Duration::from_secs(1), self.child.wait())
                .await
                .is_err()
            {
                let _ = self.child.start_kill();
                let _ = tokio::time::timeout(Duration::from_millis(100), self.child.wait()).await;
            }
        }
    }
}

fn set_fd_flag(fd: i32, get: i32, set: i32, flag: i32) -> io::Result<()> {
    let current = unsafe { libc::fcntl(fd, get) };
    if current < 0 || unsafe { libc::fcntl(fd, set, current | flag) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn select_shell() -> Option<&'static str> {
    ["/system/bin/sh", "/bin/ash", "/bin/sh"]
        .into_iter()
        .find(|path| {
            OpenOptions::new()
                .read(true)
                .open(path)
                .ok()
                .is_some_and(|file| {
                    file.metadata().ok().is_some_and(|metadata| {
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    })
                })
        })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resize {
    #[serde(rename = "type")]
    kind: String,
    cols: u16,
    rows: u16,
}

fn parse_resize(text: &str) -> Option<(u16, u16)> {
    let resize: Resize = serde_json::from_str(text).ok()?;
    (resize.kind == "resize"
        && (20..=500).contains(&resize.cols)
        && (5..=300).contains(&resize.rows))
    .then_some((resize.cols, resize.rows))
}

async fn run_session(socket: WebSocket, process: &mut PtyProcess) {
    let (mut sender, mut receiver) = socket.split();
    if sender
        .send(Message::Text(
            r#"{"type":"ready","cols":80,"rows":24}"#.into(),
        ))
        .await
        .is_err()
    {
        return;
    }
    let _ = process.resize(80, 24);
    let (output_tx, mut output_rx) = mpsc::channel::<Message>(1);
    let mut writer = tokio::spawn(async move {
        while let Some(message) = output_rx.recv().await {
            if sender.send(message).await.is_err() {
                break;
            }
        }
    });
    let mut buffer = [0u8; READ_SIZE];
    let idle = tokio::time::sleep(IDLE_TIMEOUT);
    tokio::pin!(idle);
    loop {
        tokio::select! {
            _ = &mut idle => break,
            _ = &mut writer => break,
            status = process.child.wait() => {
                let _ = status;
                break;
            }
            incoming = receiver.next() => {
                match incoming {
                    Some(Ok(Message::Binary(data))) if !data.is_empty() => {
                        if write_master(&process.master, &data).await.is_err() { break; }
                        idle.as_mut().reset(tokio::time::Instant::now() + IDLE_TIMEOUT);
                    }
                    Some(Ok(Message::Binary(_))) => {}
                    Some(Ok(Message::Text(text))) => {
                        let Some((cols, rows)) = parse_resize(&text) else { break; };
                        if process.resize(cols, rows).is_err() { break; }
                        idle.as_mut().reset(tokio::time::Instant::now() + IDLE_TIMEOUT);
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                }
            }
            ready = process.master.readable() => {
                let Ok(mut guard) = ready else { break; };
                match guard.try_io(|inner| {
                    let read = unsafe {
                        libc::read(inner.get_ref().as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len())
                    };
                    if read < 0 { Err(io::Error::last_os_error()) } else { Ok(read as usize) }
                }) {
                    Ok(Ok(0)) | Ok(Err(_)) => break,
                    Ok(Ok(count)) => {
                        if !matches!(
                            tokio::time::timeout(
                                Duration::from_millis(250),
                                output_tx.send(Message::Binary(buffer[..count].to_vec().into())),
                            )
                            .await,
                            Ok(Ok(()))
                        ) {
                            break;
                        }
                        idle.as_mut().reset(tokio::time::Instant::now() + IDLE_TIMEOUT);
                    }
                    Err(_) => continue,
                }
            }
        }
    }
    drop(output_tx);
    writer.abort();
}

async fn write_master(master: &AsyncFd<File>, data: &[u8]) -> io::Result<()> {
    let mut offset = 0;
    while offset < data.len() {
        let mut guard = master.writable().await?;
        match guard.try_io(|inner| {
            let written = unsafe {
                libc::write(
                    inner.get_ref().as_raw_fd(),
                    data[offset..].as_ptr().cast(),
                    data.len() - offset,
                )
            };
            if written < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(written as usize)
            }
        }) {
            Ok(Ok(0)) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(Ok(count)) => offset += count,
            Ok(Err(error)) => return Err(error),
            Err(_) => continue,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_is_strict_and_bounded() {
        assert_eq!(
            parse_resize(r#"{"type":"resize","cols":80,"rows":24}"#),
            Some((80, 24))
        );
        assert_eq!(
            parse_resize(r#"{"type":"resize","cols":19,"rows":24}"#),
            None
        );
        assert_eq!(
            parse_resize(r#"{"type":"resize","cols":80,"rows":301}"#),
            None
        );
        assert_eq!(
            parse_resize(r#"{"type":"resize","cols":80,"rows":24,"extra":1}"#),
            None
        );
        assert_eq!(
            parse_resize(r#"{"type":"command","cols":80,"rows":24}"#),
            None
        );
    }

    #[test]
    fn shell_selection_never_uses_environment() {
        if let Some(shell) = select_shell() {
            assert!(["/system/bin/sh", "/bin/ash", "/bin/sh"].contains(&shell));
            assert!(std::path::Path::new(shell).is_absolute());
        }
    }
}
