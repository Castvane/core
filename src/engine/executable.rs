use std::{path::Path, process::Stdio, time::Duration};

#[cfg(target_os = "linux")]
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
};

use super::Error;

pub fn launch(
    binary: &Path,
    arguments: &[String],
    launcher: Option<&Path>,
) -> std::io::Result<(Child, Group)> {
    let mut command = command(binary, launcher);
    command
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    spawn(&mut command)
}

pub(crate) fn command(binary: &Path, launcher: Option<&Path>) -> Command {
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = Command::new(launcher.unwrap_or(binary));
        if launcher.is_some() {
            command
                .arg("__media_child")
                .arg(std::process::id().to_string())
                .arg(binary);
        }
        command
    };
    #[cfg(windows)]
    let mut command = {
        let _ = launcher;
        Command::new(binary)
    };
    command
        .env_clear()
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    #[cfg(target_os = "linux")]
    command
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .process_group(0);
    #[cfg(windows)]
    for name in ["PATH", "SystemRoot", "WINDIR", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

#[cfg(target_os = "linux")]
pub(crate) fn spawn(command: &mut Command) -> std::io::Result<(Child, Group)> {
    let child = command.spawn()?;
    let group = Group(
        child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .map(Pid::from_raw),
    );
    Ok((child, group))
}

#[cfg(windows)]
pub(crate) fn spawn(command: &mut Command) -> std::io::Result<(Child, Group)> {
    let mut limits = win32job::ExtendedLimitInfo::new();
    limits.limit_kill_on_job_close();
    let job = win32job::Job::create_with_limit_info(&mut limits).map_err(std::io::Error::other)?;
    let mut child = command.spawn()?;
    let handle = child
        .raw_handle()
        .ok_or_else(|| std::io::Error::other("media process handle unavailable"))?;
    if let Err(error) = job.assign_process(handle as isize)
        && child.try_wait()?.is_none()
    {
        return Err(std::io::Error::other(error));
    }
    Ok((child, Group(Some(job))))
}

#[cfg(target_os = "linux")]
pub struct Group(Option<Pid>);
#[cfg(windows)]
pub struct Group(Option<win32job::Job>);

impl Group {
    pub fn stop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(id) = self.0 {
            let _ = killpg(id, Signal::SIGTERM);
        }
        #[cfg(windows)]
        self.kill();
    }

    pub fn kill(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(id) = self.0.take() {
            let _ = killpg(id, Signal::SIGKILL);
        }
        #[cfg(windows)]
        drop(self.0.take());
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
    }
}

pub struct Capture {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub async fn capture(
    binary: &Path,
    args: &[String],
    timeout: Duration,
    launcher: Option<&Path>,
) -> Result<Capture, Error> {
    let (mut child, mut group) = launch(binary, args, launcher)
        .map_err(|_| Error::Unavailable("configured media executable could not start"))?;
    let stdout = child.stdout.take().ok_or(Error::Internal)?;
    let stderr = child.stderr.take().ok_or(Error::Internal)?;
    let result = tokio::time::timeout(timeout, async {
        tokio::join!(
            child.wait(),
            read(stdout, 2 * 1024 * 1024),
            read(stderr, 64 * 1024)
        )
    })
    .await;
    match result {
        Ok((status, stdout, stderr)) => {
            let status = status.map_err(|_| Error::Internal)?;
            Ok(Capture {
                success: status.success(),
                stdout: stdout?,
                stderr: stderr?,
            })
        }
        Err(_) => {
            group.kill();
            let _ = child.wait().await;
            Err(Error::Unavailable("media command timed out"))
        }
    }
}

async fn read(mut stream: impl AsyncRead + Unpin, limit: usize) -> Result<String, Error> {
    let mut data = Vec::new();
    let mut buffer = [0; 8192];
    let mut overflow = false;
    loop {
        let consumed = stream
            .read(&mut buffer)
            .await
            .map_err(|_| Error::Internal)?;
        if consumed == 0 {
            break;
        }
        if data.len() + consumed <= limit {
            data.extend_from_slice(&buffer[..consumed]);
        } else {
            overflow = true;
        }
    }
    if overflow {
        return Err(Error::Unavailable("media command produced too much output"));
    }
    Ok(String::from_utf8_lossy(&data).into_owned())
}
