use std::{path::Path, process::Stdio, time::Duration};

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
    let mut command = Command::new(launcher.unwrap_or(binary));
    if launcher.is_some() {
        command
            .arg("__media_child")
            .arg(std::process::id().to_string())
            .arg(binary);
    }
    command
        .args(arguments)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let child = command.spawn()?;
    let group = Group(
        child
            .id()
            .and_then(|n| i32::try_from(n).ok())
            .map(Pid::from_raw),
    );
    Ok((child, group))
}

pub struct Group(Option<Pid>);

impl Group {
    pub fn signal(&self, signal: Signal) {
        if let Some(id) = self.0 {
            let _ = killpg(id, signal);
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.signal(Signal::SIGKILL);
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
    let (mut child, group) = launch(binary, args, launcher)
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
            group.signal(Signal::SIGKILL);
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
