use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::Zeroizing;

#[derive(Deserialize)]
pub struct Login {
    pub username: String,
    pub password: String,
}

impl Login {
    pub fn into_credentials(self) -> (String, Zeroizing<String>) {
        (self.username, Zeroizing::new(self.password))
    }
}

#[derive(Serialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Serialize)]
pub struct Refreshed {
    pub access_token: String,
}

#[derive(Serialize)]
pub struct Discovery<'a> {
    pub app: &'static str,
    pub auths: &'a [&'static str],
    pub version: Version<'a>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub node: Option<Node>,
    pub capabilities: Capabilities,
}

#[derive(Serialize)]
pub struct Version<'a> {
    pub number: &'a str,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub build: Option<Build>,
}

#[derive(Serialize)]
pub struct Build {
    pub repository_commit: &'static str,
    pub repository_branch: &'static str,
    pub build_date: &'static str,
    pub arch: &'static str,
    pub compiler: &'static str,
}

#[derive(Serialize)]
pub struct Node {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub uptime_seconds: i64,
    pub resources: Resources,
}

#[derive(Serialize, Default)]
pub struct Resources {
    pub is_throttling: bool,
    pub ncpu: usize,
    pub cpu_used: u64,
    pub cpu_limit: u64,
    pub cpu_core: u64,
    pub memory_used_bytes: u64,
    pub memory_limit_bytes: u64,
    pub memory_total_bytes: u64,
    pub memory_core_bytes: u64,
    pub gpu: [(); 0],
}

#[derive(Serialize)]
pub struct Capabilities {
    pub api_version: u8,
    pub configuration: bool,
    pub configuration_write: bool,
    pub process_engine: bool,
    pub local_login: bool,
    pub media: bool,
    pub clustering: bool,
}

#[derive(Serialize)]
pub struct Configuration<'a> {
    pub created_at: &'a str,
    pub updated_at: &'a str,
    pub loaded_at: &'a str,
    pub config: Value,
    pub overrides: Vec<&'a str>,
}
