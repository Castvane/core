use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::engine::{
    self, Config, Endpoint, Error, Execution, Limits, Metadata, Progress, Run, Runtime, Usage,
};

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Configuration {
    id: String,
    owner: String,
    domain: String,
    zone: String,
    binary: String,
    #[serde(rename = "type")]
    kind: String,
    reference: String,
    input: Option<Vec<Io>>,
    output: Option<Vec<Io>>,
    options: Option<Vec<String>>,
    reconnect: bool,
    reconnect_delay_seconds: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    reconnect_max_retries: Option<u32>,
    autostart: bool,
    stale_timeout_seconds: u64,
    runtime_duration_seconds: u64,
    scheduler: String,
    log_patterns: Option<Vec<String>>,
    limits: Limits,
    #[serde(skip_serializing_if = "Metadata::is_empty")]
    metadata: Metadata,
}

#[derive(Deserialize, Serialize)]
#[serde(default)]
struct Io {
    id: String,
    address: String,
    options: Option<Vec<String>>,
    cleanup: Option<Vec<engine::Cleanup>>,
}

impl Default for Io {
    fn default() -> Self {
        Self {
            id: short_id(),
            address: String::new(),
            options: None,
            cleanup: None,
        }
    }
}

impl Configuration {
    pub fn from_config(config: &Config) -> Self {
        let endpoint = |io: &Endpoint| Io {
            id: io.id.clone(),
            address: io.address.clone(),
            options: Some(io.options.clone()),
            cleanup: Some(io.cleanup.clone()),
        };
        Self {
            id: config.id.clone(),
            owner: config.owner.clone(),
            domain: config.domain.clone(),
            zone: config.zone.clone(),
            binary: config.binary.clone(),
            kind: "ffmpeg".into(),
            reference: config.reference.clone(),
            input: Some(config.inputs.iter().map(endpoint).collect()),
            output: Some(config.outputs.iter().map(endpoint).collect()),
            options: Some(config.options.clone()),
            reconnect: config.reconnect,
            reconnect_delay_seconds: config.delay,
            reconnect_max_retries: config.retries,
            autostart: config.autostart,
            stale_timeout_seconds: config.stale,
            runtime_duration_seconds: config.duration,
            scheduler: config.schedule.clone(),
            log_patterns: Some(config.patterns.clone()),
            limits: config.limits.clone(),
            metadata: Metadata::new(),
        }
    }

    pub fn from_patch(
        previous: Option<&Config>,
        patch: Value,
        owner: &str,
        autostart: bool,
    ) -> Result<Self, Error> {
        let baseline = previous.map(Self::from_config).unwrap_or_else(|| Self {
            id: short_id(),
            owner: owner.to_owned(),
            kind: "ffmpeg".into(),
            autostart,
            ..Self::default()
        });
        let mut document = serde_json::to_value(baseline)?;
        patch_into(&mut document, patch)?;
        let mut candidate: Self = serde_json::from_value(document)
            .map_err(|_| Error::Invalid("invalid JSON process configuration"))?;
        if candidate.kind != "ffmpeg" {
            return Err(Error::Invalid(
                "unsupported process type, supported process types are: ffmpeg",
            ));
        }
        if candidate.owner.is_empty() {
            candidate.owner = owner.to_owned();
        }
        for io in candidate
            .input
            .iter_mut()
            .flatten()
            .chain(candidate.output.iter_mut().flatten())
        {
            if io.id.is_empty() {
                io.id = short_id();
            }
        }
        Ok(candidate)
    }

    pub fn into_config(self) -> (Config, Metadata) {
        let endpoint = |io: Io| Endpoint {
            id: io.id,
            address: io.address,
            options: io.options.unwrap_or_default(),
            cleanup: io.cleanup.unwrap_or_default(),
        };
        (
            Config {
                id: self.id,
                owner: self.owner,
                scoped: false,
                domain: self.domain,
                zone: if self.zone.is_empty() {
                    "$default".into()
                } else {
                    self.zone
                },
                binary: self.binary,
                reference: self.reference,
                inputs: self
                    .input
                    .unwrap_or_default()
                    .into_iter()
                    .map(endpoint)
                    .collect(),
                outputs: self
                    .output
                    .unwrap_or_default()
                    .into_iter()
                    .map(endpoint)
                    .collect(),
                options: self.options.unwrap_or_default(),
                reconnect: self.reconnect,
                delay: self.reconnect_delay_seconds,
                retries: self.reconnect_max_retries,
                autostart: self.autostart,
                stale: self.stale_timeout_seconds,
                duration: self.runtime_duration_seconds,
                schedule: self.scheduler,
                patterns: self.log_patterns.unwrap_or_default(),
                limits: self.limits,
            },
            self.metadata,
        )
    }
}

fn patch_into(base: &mut Value, revision: Value) -> Result<(), Error> {
    let Value::Object(fields) = revision else {
        return Err(Error::Invalid("invalid JSON process configuration"));
    };
    let target = base.as_object_mut().ok_or(Error::Internal)?;
    for (field, value) in fields {
        if field == "limits" && !value.is_null() {
            let limits = target.get_mut(&field).ok_or(Error::Internal)?;
            patch_into(limits, value)?;
        } else if !value.is_null()
            || matches!(
                field.as_str(),
                "input" | "output" | "options" | "log_patterns" | "reconnect_max_retries"
            )
        {
            target.insert(field, value);
        }
    }
    Ok(())
}

fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

pub fn state(config: &Config, desired: bool, runtime: &Runtime) -> Value {
    let runtime_seconds = runtime
        .launched
        .map_or(0, |start| start.elapsed().as_secs());
    let reconnect_seconds = runtime.reconnect.map_or(-1, |instant| {
        i64::try_from(
            instant
                .saturating_duration_since(std::time::Instant::now())
                .as_secs(),
        )
        .unwrap_or(i64::MAX)
    });
    let (exit_code, exit_reason) = runtime.last_exit.clone().unwrap_or_default();
    json!({
        "order": if desired { "start" } else { "stop" },
        "exec": runtime.execution.wire(),
        "runtime_seconds": runtime_seconds, "reconnect_seconds": reconnect_seconds,
        "last_logline": runtime.active.log.back().map_or(exit_reason.as_str(), |line| line[1].as_str()),
        "progress": progress(&runtime.progress, &runtime.inputs, &runtime.outputs),
        "memory_bytes": runtime.usage.memory, "cpu_usage": runtime.usage.cpu,
        "limit_mode": if runtime.usage.enforced { "hard" } else if runtime.usage.soft && runtime.usage.samples > 0 { "soft" } else { "none" },
        "resources": resources(&runtime.usage,&config.limits),
        "command": runtime.command, "pid": runtime.pid.map_or(-1, i64::from),
        "restart_count": runtime.restart_count, "exit_code": exit_code, "exit_reason": exit_reason,
    })
}

fn progress(
    progress: &Progress,
    inputs: &[(String, String)],
    outputs: &[(String, String)],
) -> Value {
    fn io(items: &[(String, String)]) -> Vec<Value> {
        items.iter().enumerate().map(|(index,(id,address))| json!({
            "id":id,"address":address,"index":index,"stream":0,"format":"","type":"","codec":"","coder":"",
            "profile":0,"level":0,"frame":0,"keyframe":0,"framerate":{"min":0,"max":0,"avg":0},
            "fps":0,"packet":0,"pps":0,"size_kb":0,"bitrate_kbit":0,"extradata_size_bytes":0,
            "iomap":[],"avstream":null,"tee":[]
        })).collect()
    }
    json!({
        "started":progress.started, "inputs":io(inputs),"outputs":io(outputs),"mapping":{"graphs":[],"mapping":[]},
        "frame":progress.frame,"packet":0,"fps":progress.fps,"q":progress.quality,
        "size_kb":progress.bytes / 1024,"time":progress.seconds,"bitrate_kbit":progress.bitrate,
        "speed":progress.speed,"drop":progress.drop,"dup":progress.dup
    })
}

fn resources(usage: &Usage, limits: &Limits) -> Value {
    let zero = json!({"cur":0,"avg":0,"max":0,"limit":0});
    json!({
        "cpu_usage":{"ncpu":std::thread::available_parallelism().map_or(1,usize::from),
            "cur":usage.cpu,"avg":usage.cpu_average,"max":usage.cpu_peak,"limit":limits.cpu_usage,"throttling":false},
        "memory_bytes":{"cur":usage.memory,"avg":usage.memory_average,"max":usage.memory_peak,"limit":limits.memory_mbytes * 1024 * 1024},
        "gpu":{"index":0,"memory_bytes":zero,"usage":zero,"encoder":zero,"decoder":zero},
        "cpu_memory_hard_limit_applied":usage.enforced,"gpu_limits_enforced":false
    })
}

pub fn report(active: &Run, history: &[Run], limits: &Limits) -> Value {
    let mut response = entry(active);
    response["history"] = Value::Array(
        history
            .iter()
            .map(|run| {
                let mut result = entry(run);
                result["exited_at"] = json!(run.exited);
                result["exit_state"] = json!(run.state);
                result["exit_reason"] = json!(run.reason);
                result["exit_code"] = json!(run.code);
                result["restart_count"] = json!(run.restart_count);
                if let Some(data) = &run.progress {
                    result["progress"] = progress(data, &[], &[]);
                }
                if let Some(data) = &run.usage {
                    result["resources"] = resources(data, limits);
                }
                result
            })
            .collect(),
    );
    response
}

fn entry(run: &Run) -> Value {
    let mut result = json!({"created_at":run.created});
    if !run.log.is_empty() {
        result["log"] = json!(run.log);
    }
    if !run.prelude.is_empty() {
        result["prelude"] = json!(run.prelude);
    }
    if !run.matches.is_empty() {
        result["matches"] = json!(run.matches);
    }
    if !run.lines.is_empty() {
        result["lines"] = json!(run.lines);
    }
    result
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub struct ImportReport {
    history: Vec<ReportEntry>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ReportEntry {
    created_at: i64,
    exited_at: i64,
    exit_state: String,
    exit_reason: String,
    exit_code: Option<i32>,
    restart_count: u32,
    log: Vec<[String; 2]>,
    prelude: Vec<String>,
    matches: Vec<String>,
    lines: std::collections::BTreeMap<String, u64>,
    progress: Option<WireProgress>,
    resources: Option<WireResources>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct WireProgress {
    started: bool,
    frame: u64,
    fps: f64,
    q: f64,
    size_kb: u64,
    time: f64,
    bitrate_kbit: f64,
    speed: f64,
    drop: u64,
    dup: u64,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct WireResources {
    cpu_usage: Measurement,
    memory_bytes: Measurement,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Measurement {
    cur: f64,
    avg: f64,
    max: f64,
}

impl ImportReport {
    pub fn history(self) -> Result<Vec<Run>, Error> {
        use engine::sanitize;
        self.history
            .into_iter()
            .take(32)
            .map(|item| {
                if item.created_at > item.exited_at
                    || !["finished", "killed", "failed", ""].contains(&item.exit_state.as_str())
                {
                    return Err(Error::Invalid("invalid report history entry"));
                }
                Ok(Run {
                    created: item.created_at,
                    exited: item.exited_at,
                    state: item.exit_state,
                    reason: sanitize(&item.exit_reason),
                    code: item.exit_code,
                    restart_count: item.restart_count,
                    log: item
                        .log
                        .into_iter()
                        .take(500)
                        .map(|[stamp, line]| {
                            [
                                stamp.parse::<i64>().unwrap_or(0).to_string(),
                                sanitize(&line),
                            ]
                        })
                        .collect(),
                    prelude: item
                        .prelude
                        .into_iter()
                        .take(32)
                        .map(|line| sanitize(&line))
                        .collect(),
                    matches: item
                        .matches
                        .into_iter()
                        .take(32)
                        .map(|line| sanitize(&line))
                        .collect(),
                    lines: item
                        .lines
                        .into_iter()
                        .take(32)
                        .map(|(line, count)| (sanitize(&line), count))
                        .collect(),
                    progress: item.progress.map(|value| Progress {
                        started: value.started,
                        frame: value.frame,
                        fps: value.fps,
                        quality: value.q,
                        bytes: value.size_kb.saturating_mul(1024),
                        seconds: value.time,
                        bitrate: value.bitrate_kbit,
                        speed: value.speed,
                        drop: value.drop,
                        dup: value.dup,
                    }),
                    usage: item.resources.map(|value| Usage {
                        cpu: value.cpu_usage.cur,
                        cpu_average: value.cpu_usage.avg,
                        cpu_peak: value.cpu_usage.max,
                        memory: value.memory_bytes.cur as u64,
                        memory_average: value.memory_bytes.avg,
                        memory_peak: value.memory_bytes.max as u64,
                        ..Usage::default()
                    }),
                })
            })
            .collect()
    }
}

pub fn probe(data: engine::ResultSet) -> Value {
    let mut streams = Vec::new();
    for source in data.streams {
        let stream = source.stream;
        let fps = [stream.avg_frame_rate, stream.r_frame_rate]
            .iter()
            .find_map(|value| {
                let (a, b) = value.split_once('/')?;
                let fps = a.parse::<f64>().ok()? / b.parse::<f64>().ok()?;
                (fps.is_finite() && fps > 0.0).then_some(fps)
            })
            .unwrap_or(0.0);
        streams.push(json!({
            "url":source.address,"format":source.format,"index":source.input,"stream":source.index,
            "language":if stream.tags.language.is_empty() { "und" } else { &stream.tags.language },
            "type":stream.codec_type,"codec":stream.codec_name,"coder":"",
            "bitrate_kbps":stream.bit_rate.parse::<f64>().unwrap_or(0.0) / 1000.0,
            "duration_sec":stream.duration.parse::<f64>().unwrap_or(0.0),"fps":fps,
            "pix_fmt":stream.pix_fmt,"width":stream.width,"height":stream.height,
            "sampling_hz":stream.sample_rate.parse::<u64>().unwrap_or(0),"layout":stream.channel_layout,
            "channels":stream.channels,
        }));
    }
    json!({"streams":streams,"log":data.log})
}

pub fn is_current(runtime: &Runtime) -> bool {
    matches!(
        runtime.execution,
        Execution::Running | Execution::Starting | Execution::Finishing
    )
}
