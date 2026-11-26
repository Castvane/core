use std::time::Duration;

use serde::Deserialize;

use super::{Error, Options, command::CommandLine, executable::capture};

pub struct ResultSet {
    pub streams: Vec<Stream>,
    pub log: Vec<String>,
}

pub struct Stream {
    pub address: String,
    pub format: String,
    pub input: usize,
    pub index: u64,
    pub stream: Track,
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub struct Track {
    pub index: u64,
    pub codec_name: String,
    pub codec_type: String,
    pub bit_rate: String,
    pub duration: String,
    pub avg_frame_rate: String,
    pub r_frame_rate: String,
    pub pix_fmt: String,
    pub width: u64,
    pub height: u64,
    pub sample_rate: String,
    pub channel_layout: String,
    pub channels: u64,
    pub tags: Tags,
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub struct Tags {
    pub language: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Container {
    format_name: String,
    duration: String,
    bit_rate: String,
}

#[derive(Deserialize)]
struct Probe {
    #[serde(default)]
    streams: Vec<Track>,
    #[serde(default)]
    format: Container,
}

pub async fn inspect(options: &Options, command: CommandLine) -> Result<ResultSet, Error> {
    let mut result = ResultSet {
        streams: Vec::new(),
        log: Vec::new(),
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(options.timeout);
    for (index, input) in command.inputs.iter().enumerate() {
        let mut args = vec![
            "-v".into(),
            "error".into(),
            "-show_format".into(),
            "-show_streams".into(),
            "-of".into(),
            "json".into(),
        ];
        args.extend(probe_options(&input.options));
        args.extend(["-i".into(), input.address.clone()]);
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(Error::Unavailable("FFprobe timed out"));
        }
        let report = capture(
            &options.ffprobe,
            &args,
            remaining,
            options.launcher.as_deref(),
        )
        .await?;
        result.log.extend(
            report
                .stderr
                .lines()
                .take(32)
                .map(|line| command.redactor.clean(line)),
        );
        if !report.success {
            if result.log.is_empty() {
                result
                    .log
                    .push("FFprobe could not inspect the input".into());
            }
            continue;
        }
        let probe: Probe = serde_json::from_str(&report.stdout)
            .map_err(|_| Error::Unavailable("invalid FFprobe response"))?;
        for mut track in probe.streams.into_iter().take(64) {
            if track.duration.is_empty() {
                track.duration.clone_from(&probe.format.duration);
            }
            if track.bit_rate.is_empty() {
                track.bit_rate.clone_from(&probe.format.bit_rate);
            }
            let address = if input.address.contains("://") || input.address.contains('?') {
                command.redactor.clean(&input.address)
            } else {
                input.address.clone()
            };
            result.streams.push(Stream {
                address,
                format: probe.format.format_name.clone(),
                input: index,
                index: track.index,
                stream: track,
            });
        }
    }
    result.log.truncate(64);
    Ok(result)
}

fn probe_options(options: &[String]) -> Vec<String> {
    // FFprobe shares input AVOptions. FFmpeg-only timing, seek and streaming switches are omitted.
    let mut output = Vec::new();
    let mut args = options.iter();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "-f" | "-analyzeduration" | "-probesize" | "-rw_timeout" | "-timeout"
            | "-rtsp_transport" | "-video_size" | "-pixel_format" | "-framerate"
            | "-sample_rate" | "-channels" | "-user_agent" | "-headers" => {
                if let Some(value) = args.next() {
                    output.extend([flag.clone(), value.clone()]);
                }
            }
            _ => {}
        }
    }
    output
}
