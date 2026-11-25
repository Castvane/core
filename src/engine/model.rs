use std::time::Instant;

use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
pub struct Config {
    pub id: String,
    pub owner: String,
    pub domain: String,
    pub zone: String,
    pub binary: String,
    pub reference: String,
    pub inputs: Vec<Endpoint>,
    pub outputs: Vec<Endpoint>,
    pub options: Vec<String>,
    pub reconnect: bool,
    pub delay: u64,
    pub retries: Option<u32>,
    pub autostart: bool,
    pub stale: u64,
    pub duration: u64,
    pub schedule: String,
    pub patterns: Vec<String>,
    pub limits: Limits,
}

#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
pub struct Endpoint {
    pub id: String,
    pub address: String,
    pub options: Vec<String>,
    pub cleanup: Vec<Cleanup>,
}

#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct Cleanup {
    pub pattern: String,
    pub max_files: u64,
    pub max_file_age_seconds: u64,
    pub purge_on_delete: bool,
}

#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct Limits {
    pub cpu_usage: f64,
    pub memory_mbytes: u64,
    pub gpu_usage: f64,
    pub gpu_encoder: f64,
    pub gpu_decoder: f64,
    pub gpu_memory_mbytes: u64,
    pub waitfor_seconds: u64,
    pub log_event_rate: f64,
}

#[derive(Clone)]
pub struct Job {
    pub uid: String,
    pub config: Config,
    pub desired: bool,
    pub created: i64,
    pub updated: i64,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Execution {
    #[default]
    Finished,
    Starting,
    Running,
    Finishing,
    Failed,
    Killed,
}

impl Execution {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Finished => "finished",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Finishing => "finishing",
            Self::Failed => "failed",
            Self::Killed => "killed",
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Progress {
    pub started: bool,
    pub frame: u64,
    pub fps: f64,
    pub quality: f64,
    pub bytes: u64,
    pub seconds: f64,
    pub bitrate: f64,
    pub speed: f64,
    pub drop: u64,
    pub dup: u64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub cpu: f64,
    pub cpu_peak: f64,
    pub cpu_average: f64,
    pub memory: u64,
    pub memory_peak: u64,
    pub memory_average: f64,
    pub samples: u64,
    pub enforced: bool,
    pub soft: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Run {
    pub created: i64,
    pub exited: i64,
    pub state: String,
    pub reason: String,
    pub code: Option<i32>,
    pub restart_count: u32,
    pub log: std::collections::VecDeque<[String; 2]>,
    pub prelude: Vec<String>,
    pub matches: Vec<String>,
    pub lines: std::collections::BTreeMap<String, u64>,
    pub progress: Option<Progress>,
    pub usage: Option<Usage>,
}

impl Default for Run {
    fn default() -> Self {
        Self {
            created: -62_135_596_800,
            exited: 0,
            state: String::new(),
            reason: String::new(),
            code: None,
            restart_count: 0,
            log: Default::default(),
            prelude: Vec::new(),
            matches: Vec::new(),
            lines: Default::default(),
            progress: None,
            usage: None,
        }
    }
}

#[derive(Clone, Default)]
pub struct Runtime {
    pub execution: Execution,
    pub pid: Option<u32>,
    pub launched: Option<Instant>,
    pub reconnect: Option<Instant>,
    pub progress: Progress,
    pub usage: Usage,
    pub active: Run,
    pub log_bytes: usize,
    pub command: Vec<String>,
    pub restart_count: u32,
    pub inputs: Vec<(String, String)>,
    pub outputs: Vec<(String, String)>,
    pub last_exit: Option<(Option<i32>, String)>,
}
