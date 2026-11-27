use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use regex::Regex;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt};

use super::{Progress, Runtime, command::Redactor, model::StreamProgress, unix_now};

const LOG_LINE_BYTES: usize = 2048;
const JSON_LINE_BYTES: usize = 32 * 1024;
const STREAMS_PER_DIRECTION: usize = 32;

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
    let mut progress = Progress::default();
    lines(stdout, LOG_LINE_BYTES, |line| {
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
                // An FFmpeg build emitting both forms keeps the richer stderr record.
                if state.progress.inputs.is_empty() && state.progress.outputs.is_empty() {
                    publish(&mut state, &heartbeat, progress.clone());
                }
            }
            _ => {}
        }
    })
    .await;
}

#[derive(Deserialize)]
struct JsonProgress<'a> {
    #[serde(default)]
    inputs: Vec<StreamProgress>,
    #[serde(default)]
    outputs: Vec<StreamProgress>,
    frame: u64,
    #[serde(default)]
    packet: u64,
    #[serde(default)]
    q: f64,
    #[serde(default)]
    size_bytes: u64,
    time: &'a str,
    #[serde(default)]
    speed: f64,
    #[serde(default)]
    dup: u64,
    #[serde(default)]
    drop: u64,
}

fn json_clock(value: &str) -> Option<f64> {
    let (negative, value) = value
        .strip_prefix('-')
        .map_or((false, value), |v| (true, v));
    let (hours, rest) = value.split_once('h')?;
    let (minutes, seconds) = rest.split_once('m')?;
    let hours = hours.parse::<u32>().ok()?;
    let minutes = minutes.parse::<u32>().ok()?;
    let seconds = seconds.strip_suffix('s')?.parse::<f64>().ok()?;
    if minutes >= 60 || !(0.0..60.0).contains(&seconds) {
        return None;
    }
    let seconds = f64::from(hours) * 3600.0 + f64::from(minutes) * 60.0 + seconds;
    Some(if negative { -seconds } else { seconds })
}

fn extended_progress(payload: &str) -> Option<Progress> {
    let mut report: JsonProgress<'_> = serde_json::from_str(payload).ok()?;
    report.inputs.truncate(STREAMS_PER_DIRECTION);
    report.outputs.truncate(STREAMS_PER_DIRECTION);
    let seconds = json_clock(report.time).unwrap_or_default();
    let bitrate = (report.size_bytes as f64 * 0.008) / seconds;
    let bitrate = if seconds > 0.0 && bitrate.is_finite() {
        bitrate
    } else {
        0.0
    };
    Some(Progress {
        frame: report.frame,
        packet: report.packet,
        fps: report
            .outputs
            .first()
            .map_or(0.0, |stream| stream.framerate.avg),
        quality: report.q,
        bytes: report.size_bytes,
        seconds,
        bitrate,
        speed: report.speed,
        drop: report.drop,
        dup: report.dup,
        inputs: report.inputs,
        outputs: report.outputs,
        ..Progress::default()
    })
}

fn publish(state: &mut Runtime, heartbeat: &Mutex<Heartbeat>, mut progress: Progress) {
    let advanced = progress.frame > state.progress.frame
        || progress.seconds > state.progress.seconds
        || progress.packet > state.progress.packet;
    if advanced {
        lock(heartbeat).0 = Some(Instant::now());
    }
    progress.started = state.progress.started || advanced;
    state.progress = progress;
}

pub async fn collect_logs(
    stderr: impl AsyncRead + Unpin,
    state: Arc<Mutex<Runtime>>,
    heartbeat: Arc<Mutex<Heartbeat>>,
    redactor: Arc<Redactor>,
    patterns: Vec<Regex>,
    limit: usize,
) {
    lines(stderr, JSON_LINE_BYTES, |line| {
        if let Some(payload) = line.strip_prefix("ffmpeg.progress:") {
            if let Some(progress) = extended_progress(payload) {
                publish(&mut lock(&state), &heartbeat, progress);
            }
            return;
        }
        // These records can contain raw URLs and filter arguments. Consume
        // numeric progress only and keep the wire records out of reports.
        if limit == 0
            || line.len() > LOG_LINE_BYTES
            || [
                "ffmpeg.inputs:",
                "ffmpeg.outputs:",
                "ffmpeg.mapping:",
                "hls.streammap:",
            ]
            .iter()
            .any(|prefix| line.starts_with(prefix))
        {
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

async fn lines(
    mut reader: impl AsyncRead + Unpin,
    max_bytes: usize,
    mut consume: impl FnMut(&str),
) {
    let mut buffer = [0; 8192];
    let mut line = Vec::with_capacity(LOG_LINE_BYTES);
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
            } else if line.len() < max_bytes {
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
