use anyhow::{Context, Result, bail};
use std::{ffi::OsStr, process::Stdio, time::Duration};
use tokio::{process::Command, time::timeout};

const MAX_OUTPUT: usize = 1024 * 1024;

pub async fn run<I, S>(program: &str, args: I, deadline: Duration) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {program}"))?;
    let output = timeout(deadline, child.wait_with_output())
        .await
        .with_context(|| format!("{program} timed out"))??;
    if !output.status.success() {
        bail!("{program} exited with {}", output.status);
    }
    if output.stdout.len() > MAX_OUTPUT {
        bail!("{program} output exceeds limit");
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn command_has_timeout_and_no_shell() {
        let out = run("printf", ["%s", "$(id)"], Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(out, b"$(id)");
        assert!(
            run("sleep", ["2"], Duration::from_millis(10))
                .await
                .is_err()
        );
    }
}
