use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// Resolves an external CLI tool: sibling of the avxs binary first, then PATH.
pub fn external_bin(name: &str) -> OsString {
    let file_name = with_exe_suffix(name);

    if let Some(sibling) = sibling_of_exe(&file_name)
        && is_executable(&sibling)
    {
        return sibling.into_os_string();
    }

    OsString::from(name)
}

/// A neighbour that exists but is not executable must not shadow a working copy on PATH.
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn with_exe_suffix(name: &str) -> String {
    let suffix = std::env::consts::EXE_SUFFIX;
    if suffix.is_empty() || name.ends_with(suffix) {
        name.to_string()
    } else {
        format!("{name}{suffix}")
    }
}

fn sibling_of_exe(file_name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    Some(dir.join(file_name))
}

/// Kills the command if it overruns; nothing else would notice a hung tool.
pub async fn output_with_timeout(
    cmd: &mut Command,
    secs: u64,
    what: &str,
) -> Result<std::process::Output> {
    cmd.kill_on_drop(true);
    match tokio::time::timeout(std::time::Duration::from_secs(secs), cmd.output()).await {
        Ok(res) => res.with_context(|| format!("run {what}")),
        // Transient: a share that stopped answering or a GPU mid-reset comes back.
        Err(_) => Err(anyhow::Error::new(crate::job::Transient)
            .context(format!("{what} did not finish within {secs}s - killed"))),
    }
}

/// The blocking counterpart for the chunk workers; drains stderr on a thread.
pub fn blocking_output_with_timeout(
    child: &mut std::process::Child,
    secs: u64,
    what: &str,
) -> Result<(std::process::ExitStatus, String)> {
    use std::io::Read;

    let mut err = child.stderr.take().context("stderr must be piped")?;
    let drain = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        match child.try_wait().with_context(|| format!("wait for {what}"))? {
            Some(status) => return Ok((status, drain.join().unwrap_or_default())),
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow::Error::new(crate::job::Transient)
                    .context(format!("{what} did not finish within {secs}s - killed")));
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    }
}

/// `args` has to request JSON. A whole-file query needs [`ffprobe_json_with_timeout`].
pub async fn ffprobe_json<T: DeserializeOwned>(args: &[&str], input: &Path) -> Result<T> {
    ffprobe_json_with_timeout(args, input, 120).await
}

pub async fn ffprobe_json_with_timeout<T: DeserializeOwned>(
    args: &[&str],
    input: &Path,
    secs: u64,
) -> Result<T> {
    let mut cmd = Command::new(external_bin("ffprobe"));
    cmd.args(args).arg(input);
    let out = output_with_timeout(&mut cmd, secs, "ffprobe").await?;
    if !out.status.success() {
        bail!("ffprobe failed:\n{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    serde_json::from_slice(&out.stdout).context("parse ffprobe json")
}
