use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use regex::Regex;
use tokio::io::{AsyncRead, AsyncReadExt};

use super::{Runtime, command::Redactor, unix_now};

pub fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Default)]
pub struct Heartbeat(pub Option<Instant>);

pub async fn collect(
    stdout: impl AsyncRead + Unpin,
    state: Arc<Mutex<Runtime>>,
    heartbeat: Arc<Mutex<Heartbeat>>,
) {
    let mut progress = super::Progress::default();
    lines(stdout, |line| {
        let Some((key, value)) = line.split_once('=') else {
            return;
        };
        let value = value.trim();
        let number = |suffix: &str| {
            value
                .strip_suffix(suffix)
                .unwrap_or(value)
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite())
                .unwrap_or(0.0)
        };
        match key {
            "frame" => progress.frame = value.parse().unwrap_or(0),
            "fps" => progress.fps = number(""),
            "stream_0_0_q" => progress.quality = number(""),
            "total_size" => progress.bytes = value.parse().unwrap_or(0),
            "out_time_us" => progress.seconds = number("") / 1_000_000.0,
            "bitrate" => progress.bitrate = number("kbits/s"),
            "speed" => progress.speed = number("x"),
            "drop_frames" => progress.drop = value.parse().unwrap_or(0),
            "dup_frames" => progress.dup = value.parse().unwrap_or(0),
            "progress" => {
                let mut state = lock(&state);
                if progress.frame > state.progress.frame
                    || progress.seconds > state.progress.seconds
                {
                    progress.started = true;
                    lock(&heartbeat).0 = Some(Instant::now());
                }
                state.progress = progress.clone();
            }
            _ => {}
        }
    })
    .await;
}

pub async fn collect_logs(
    stderr: impl AsyncRead + Unpin,
    state: Arc<Mutex<Runtime>>,
    redactor: Arc<Redactor>,
    patterns: Vec<Regex>,
    limit: usize,
) {
    lines(stderr, |line| {
        if limit == 0 || line.is_empty() {
            return;
        }
        let line = redactor.clean(line);
        let mut state = lock(&state);
        while state.active.log.len() >= limit || state.log_bytes + line.len() > 128 * 1024 {
            let Some(removed) = state.active.log.pop_front() else {
                break;
            };
            state.log_bytes = state.log_bytes.saturating_sub(removed[1].len());
        }
        state.log_bytes += line.len();
        let active = &mut state.active;
        for regex in &patterns {
            if regex.is_match(&line) {
                let count = active.lines.entry(regex.as_str().to_owned()).or_default();
                *count = count.saturating_add(1);
                if active.matches.len() < 32 && !active.matches.contains(&line) {
                    active.matches.push(line.clone());
                }
            }
        }
        active.log.push_back([unix_now().to_string(), line]);
    })
    .await;
}

async fn lines(mut reader: impl AsyncRead + Unpin, mut consume: impl FnMut(&str)) {
    let mut buffer = [0; 8192];
    let mut line = Vec::with_capacity(2048);
    let mut truncated = false;
    while let Ok(read) = reader.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        for &byte in &buffer[..read] {
            if matches!(byte, b'\n' | b'\r') {
                if !truncated && !line.is_empty() {
                    consume(&String::from_utf8_lossy(&line));
                }
                line.clear();
                truncated = false;
            } else if line.len() < 2048 {
                line.push(byte);
            } else {
                truncated = true;
            }
        }
    }
    if !truncated && !line.is_empty() {
        consume(&String::from_utf8_lossy(&line));
    }
}
