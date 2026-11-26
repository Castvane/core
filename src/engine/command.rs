use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};

use regex::Regex;
use serde_json::Value;

use super::{Config, Endpoint, Error, Job, scheduler::Schedule};

type Key = (String, String);

#[derive(Clone)]
pub struct Options {
    pub ffmpeg: PathBuf,
    media: crate::media::Endpoints,
    pub ffprobe: PathBuf,
    pub launcher: Option<PathBuf>,
    pub max_children: usize,
    pub timeout: u64,
    pub stop_timeout: u64,
    pub retry_limit: u32,
    pub log_lines: usize,
    pub history: u32,
    pub disk: String,
    pub cgroup: Option<PathBuf>,
    input: Policy,
    output: Policy,
    values: BTreeMap<String, String>,
}

#[derive(Clone, Default)]
struct Policy {
    allow: Vec<Regex>,
    block: Vec<Regex>,
}

impl Policy {
    fn read(value: &Value) -> Result<Self, Error> {
        fn patterns(value: &Value) -> Result<Vec<Regex>, Error> {
            value
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(|pattern| {
                    Regex::new(pattern)
                        .map_err(|_| Error::Invalid("invalid FFmpeg access expression"))
                })
                .collect()
        }
        Ok(Self {
            allow: patterns(&value["allow"])?,
            block: patterns(&value["block"])?,
        })
    }

    fn allows(&self, address: &str) -> bool {
        !self.block.iter().any(|regex| regex.is_match(address))
            && (self.allow.is_empty() || self.allow.iter().any(|regex| regex.is_match(address)))
    }

    fn active(&self) -> bool {
        !self.allow.is_empty() || !self.block.is_empty()
    }
}

impl Options {
    pub fn from_document(document: &Value, overrides: &super::Overrides) -> Result<Self, Error> {
        let ff = &document["ffmpeg"];
        let ffmpeg = PathBuf::from(ff["binary"].as_str().unwrap_or("ffmpeg"));
        if ffmpeg.as_os_str().is_empty() {
            return Err(Error::Invalid("FFmpeg binary is empty"));
        }
        let probe = ff["probe_binary"]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| ffmpeg.with_file_name("ffprobe"));
        let ceiling = ff["max_processes"]
            .as_u64()
            .ok_or(Error::Invalid("invalid FFmpeg process limit"))?;
        let timeout = ff["probe_timeout_seconds"].as_u64().unwrap_or(8);
        let stop_timeout = ff["stop_timeout_seconds"].as_u64().unwrap_or(3);
        let retry_limit = ff["max_retries"].as_u64().unwrap_or(10);
        let log_lines = ff["log"]["max_lines"]
            .as_u64()
            .ok_or(Error::Invalid("invalid log line limit"))?;
        let history = ff["log"]["max_history"]
            .as_u64()
            .ok_or(Error::Invalid("invalid log history limit"))?;
        if ceiling > 128
            || !(1..=12).contains(&timeout)
            || !(1..=10).contains(&stop_timeout)
            || retry_limit > 100
            || log_lines > 500
            || history > 32
        {
            return Err(Error::Invalid("FFmpeg limits outside supported bounds"));
        }
        Ok(Self {
            ffmpeg,
            media: Default::default(),
            ffprobe: probe,
            launcher: overrides.launcher.clone(),
            max_children: if ceiling == 0 { 32 } else { ceiling as usize },
            timeout,
            stop_timeout,
            retry_limit: retry_limit as u32,
            log_lines: log_lines as usize,
            history: history as u32,
            disk: document["storage"]["disk"]["dir"]
                .as_str()
                .unwrap_or("./data")
                .to_owned(),
            cgroup: overrides.cgroup.clone(),
            values: overrides.values.clone(),
            input: Policy::read(&ff["access"]["input"])?,
            output: Policy::read(&ff["access"]["output"])?,
        })
    }

    pub fn with_media(mut self, media: crate::media::Endpoints) -> Self {
        self.media = media;
        self
    }

    pub fn for_probe(&mut self) {
        self.output = Policy::default();
    }

    pub fn validate(&self, config: &Config) -> Result<(), Error> {
        if !valid_id(&config.id)
            || (!config.domain.is_empty() && !valid_id(&config.domain))
            || !matches!(config.zone.as_str(), "" | "$default")
        {
            return Err(Error::Invalid("invalid process ID, domain or zone"));
        }
        if !config.binary.is_empty()
            && config.binary != "ffmpeg"
            && Path::new(&config.binary) != self.ffmpeg
        {
            return Err(Error::Invalid(
                "process binary must match the configured FFmpeg executable",
            ));
        }
        if config.inputs.is_empty()
            || config.outputs.is_empty()
            || config.inputs.len() > 32
            || config.outputs.len() > 32
        {
            return Err(Error::Invalid(
                "at least one input and one output need to be defined, with at most 32 each",
            ));
        }
        for endpoints in [&config.inputs, &config.outputs] {
            let mut ids = HashSet::new();
            for endpoint in endpoints {
                if !valid_id(&endpoint.id)
                    || !ids.insert(&endpoint.id)
                    || endpoint.address.trim().is_empty()
                {
                    return Err(Error::Invalid(
                        "invalid or duplicate endpoint ID or empty address",
                    ));
                }
                if endpoint.cleanup.len() > 16
                    || endpoint
                        .cleanup
                        .iter()
                        .any(|item| item.pattern.is_empty() || item.pattern.len() > 1024)
                {
                    return Err(Error::Invalid("invalid output cleanup pattern"));
                }
            }
        }
        if config.delay > 86_400
            || config.duration > 31_536_000
            || config.stale > 86_400
            || config.retries.is_some_and(|n| n > 100)
            || config.schedule.len() > 256
            || config.patterns.len() > 32
            || config
                .patterns
                .iter()
                .any(|p| p.len() > 256 || Regex::new(p).is_err())
        {
            return Err(Error::Invalid(
                "invalid recovery, scheduling or log settings",
            ));
        }
        Schedule::parse(&config.schedule).map_err(Error::Invalid)?;
        let limits = &config.limits;
        if !(0.0..=12_800.0).contains(&limits.cpu_usage)
            || limits.memory_mbytes > 1_048_576
            || limits.gpu_memory_mbytes > 1_048_576
            || limits.waitfor_seconds > 3600
            || ![limits.gpu_usage, limits.gpu_encoder, limits.gpu_decoder]
                .into_iter()
                .all(|n| (0.0..=100.0).contains(&n))
            || !limits.log_event_rate.is_finite()
            || limits.log_event_rate < 0.0
        {
            return Err(Error::Invalid("invalid process resource limits"));
        }
        Ok(())
    }

    pub fn build(
        &self,
        config: &Config,
        jobs: &BTreeMap<Key, std::sync::Arc<Job>>,
        live: &BTreeMap<String, BTreeMap<String, String>>,
    ) -> Result<CommandLine, Error> {
        self.validate(config)?;
        validate_references(config, jobs)?;
        let mut original = Vec::new();
        let mut actual = Vec::new();
        let mut secrets = Vec::new();
        let mut inputs = Vec::new();
        let mut outputs = Vec::new();
        let strict = self.input.active() || self.output.active();
        let now = jiff::Timestamp::now();
        let mut context = Expansion {
            options: self,
            config,
            now,
            secrets: &mut secrets,
        };
        let global = context.arguments(&config.options, None, strict)?;
        original.extend(global.iter().cloned());
        actual.extend(global);
        for input in &config.inputs {
            let opts = context.arguments(&input.options, Some((input, true)), strict)?;
            let address = if let Some(reference) = Reference::parse(&input.address)? {
                let target = jobs
                    .get(&(reference.domain.to_owned(), reference.id.to_owned()))
                    .ok_or(Error::Invalid("unknown referenced process"))?;
                let endpoint = target
                    .config
                    .outputs
                    .iter()
                    .find(|output| output.id == reference.output)
                    .ok_or(Error::Invalid("unknown referenced output"))?;
                let fallback = Expansion {
                    options: self,
                    config: &target.config,
                    now,
                    secrets: context.secrets,
                }
                .string(&endpoint.address, Some((endpoint, true)))?;
                let resolved = live
                    .get(&target.uid)
                    .and_then(|outputs| outputs.get(&endpoint.id))
                    .unwrap_or(&fallback);
                destinations(resolved)?
                    .into_iter()
                    .find(|address| {
                        reference.source.is_empty()
                            || address.split(':').next() == Some(reference.source)
                    })
                    .ok_or(Error::Invalid("unknown referenced source"))?
                    .to_owned()
            } else {
                context.string(&input.address, Some((input, true)))?
            };
            evaluate_address(&self.input, &address, true)?;
            original.extend(opts.iter().cloned());
            original.extend(["-i".into(), address.clone()]);
            actual.extend(opts.iter().cloned());
            actual.extend(["-i".into(), address.clone()]);
            inputs.push(Resolved {
                id: input.id.clone(),
                address,
                options: opts,
            });
        }
        for output in &config.outputs {
            let mut opts = context.arguments(&output.options, Some((output, false)), strict)?;
            let address = context.string(&output.address, Some((output, false)))?;
            if let Ok(url) = url::Url::parse(&address)
                && self
                    .media
                    .memory_auth
                    .as_ref()
                    .is_some_and(|(user, _)| url.username() == user)
                && cleanup_location(self, &address).is_some_and(|(name, _)| name != "disk")
            {
                if !opts.iter().any(|value| value == "-auth_type") {
                    opts.extend(["-auth_type".into(), "basic".into()]);
                }
                if !opts.iter().any(|value| value == "-send_expect_100") {
                    opts.extend(["-send_expect_100".into(), "1".into()]);
                }
            }
            for target in destinations(&address)? {
                evaluate_address(&self.output, target, false)?;
            }
            if address == "-" && !opts.windows(2).any(|args| args == ["-f", "null"]) {
                return Err(Error::Invalid("standard output is reserved for progress"));
            }
            original.extend(opts.iter().cloned());
            original.push(address.clone());
            actual.extend(opts.iter().cloned());
            actual.push(if address == "-" {
                "/dev/null".into()
            } else {
                address.clone()
            });
            outputs.push(Resolved {
                id: output.id.clone(),
                address,
                options: opts,
            });
        }
        let mut args = vec![
            "-nostdin".into(),
            "-hide_banner".into(),
            "-nostats".into(),
            "-progress".into(),
            "pipe:1".into(),
            "-stats_period".into(),
            "0.5".into(),
        ];
        args.extend(actual);
        let mut redactor = Redactor { secrets };
        for endpoint in inputs.iter().chain(&outputs) {
            redactor.sensitive_address(&endpoint.address);
        }
        for argument in &original {
            redactor.sensitive_address(argument);
        }
        redactor
            .secrets
            .sort_unstable_by_key(|secret| std::cmp::Reverse(secret.len()));
        redactor.secrets.dedup();
        let display = original.iter().map(|arg| redactor.clean(arg)).collect();
        Ok(CommandLine {
            args,
            display,
            inputs,
            outputs,
            redactor,
        })
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value != "."
        && value != ".."
        && !value
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | '#' | '?' | '{' | '}'))
}

fn evaluate_address(policy: &Policy, address: &str, input: bool) -> Result<(), Error> {
    if address.is_empty()
        || address.len() > 16_384
        || address.starts_with('-') && address != "-"
        || address.contains(['\0', '\n', '\r'])
        || ["pipe:", "fd:", "concat:", "concatf:", "subfile:", "crypto:"]
            .iter()
            .any(|prefix| address.starts_with(prefix))
        || (input && address == "-")
    {
        return Err(Error::Invalid("invalid or reserved FFmpeg address"));
    }
    if !policy.allows(address) {
        return Err(Error::Forbidden("FFmpeg address denied by access policy"));
    }
    Ok(())
}

fn destinations(address: &str) -> Result<Vec<&str>, Error> {
    address
        .split('|')
        .map(|part| {
            if let Some(options) = part.strip_prefix('[') {
                let (_, target) = options
                    .split_once(']')
                    .ok_or(Error::Invalid("invalid tee destination"))?;
                Ok(target)
            } else {
                Ok(part)
            }
        })
        .collect()
}

struct Reference<'a> {
    id: &'a str,
    domain: &'a str,
    output: &'a str,
    source: &'a str,
}

impl<'a> Reference<'a> {
    fn parse(address: &'a str) -> Result<Option<Self>, Error> {
        let Some(value) = address.strip_prefix('#') else {
            return Ok(None);
        };
        let index = [":output=", ":domain=", ":source="]
            .iter()
            .filter_map(|marker| value.find(marker))
            .min()
            .ok_or(Error::Invalid("invalid process reference"))?;
        let mut result = Self {
            id: &value[..index],
            domain: "",
            output: "",
            source: "",
        };
        let parts = Regex::new(r":(output|domain|source)=").map_err(|_| Error::Internal)?;
        let markers: Vec<_> = parts.find_iter(value).collect();
        for (index, marker) in markers.iter().enumerate() {
            let end = markers.get(index + 1).map_or(value.len(), |m| m.start());
            let field = &value[marker.end()..end];
            match marker.as_str() {
                ":domain=" => result.domain = field,
                ":output=" => result.output = field,
                _ => result.source = field,
            }
        }
        if result.id.is_empty() || result.output.is_empty() {
            return Err(Error::Invalid("invalid process reference"));
        }
        Ok(Some(result))
    }
}

pub fn validate_references(
    config: &Config,
    jobs: &BTreeMap<Key, std::sync::Arc<Job>>,
) -> Result<(), Error> {
    fn visit(
        config: &Config,
        jobs: &BTreeMap<Key, std::sync::Arc<Job>>,
        stack: &mut HashSet<Key>,
        visited: &mut HashSet<Key>,
    ) -> Result<(), Error> {
        let key = (config.domain.clone(), config.id.clone());
        if stack.contains(&key) {
            return Err(Error::Invalid("circular process reference"));
        }
        if !visited.insert(key.clone()) {
            return Ok(());
        }
        stack.insert(key.clone());
        for input in &config.inputs {
            if let Some(reference) = Reference::parse(&input.address)? {
                let target = (reference.domain.to_owned(), reference.id.to_owned());
                if stack.contains(&target) {
                    return Err(Error::Invalid("circular process reference"));
                }
                let job = jobs
                    .get(&target)
                    .ok_or(Error::Invalid("unknown referenced process"))?;
                if !job
                    .config
                    .outputs
                    .iter()
                    .any(|endpoint| endpoint.id == reference.output)
                {
                    return Err(Error::Invalid("unknown referenced output"));
                }
                visit(&job.config, jobs, stack, visited)?;
            }
        }
        stack.remove(&key);
        Ok(())
    }
    visit(config, jobs, &mut HashSet::new(), &mut HashSet::new())
}

pub struct CommandLine {
    pub args: Vec<String>,
    pub display: Vec<String>,
    pub inputs: Vec<Resolved>,
    pub outputs: Vec<Resolved>,
    pub redactor: Redactor,
}

pub struct Resolved {
    pub id: String,
    pub address: String,
    pub options: Vec<String>,
}

#[derive(Default)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    fn sensitive_address(&mut self, value: &str) {
        if let Some((_, rest)) = value.split_once("://") {
            if let Some((credentials, _)) = rest
                .split(['/', '?', '#'])
                .next()
                .and_then(|authority| authority.rsplit_once('@'))
            {
                self.secrets.push(credentials.to_owned());
                if let Some((_, password)) = credentials.split_once(':')
                    && !password.is_empty()
                {
                    self.secrets.push(password.to_owned());
                }
            }
            if let Some((_, query)) = rest.split_once('?') {
                self.secrets.push(query.to_owned());
                for pair in query.split('&') {
                    if let Some((_, value)) = pair.split_once('=')
                        && value.len() >= 4
                    {
                        self.secrets.push(value.to_owned());
                    }
                }
            }
        }
        if value.contains("://") || value.contains('?') {
            self.secrets.push(value.to_owned());
        }
    }

    pub fn clean(&self, value: &str) -> String {
        let mut value = value.to_owned();
        for secret in &self.secrets {
            if !secret.is_empty() {
                value = value.replace(secret, "[redacted]");
            }
        }
        sanitize(&value)
    }
}

pub fn sanitize(value: &str) -> String {
    // FFmpeg can report redirects and dynamically generated URLs which did not occur in its argv.
    static URL: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)[a-z][a-z0-9+.-]*://[^\s"'<>,]+"#).expect("static URL expression")
    });
    static HEADER: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)(?:authorization|proxy-authorization|cookie|set-cookie):[^\r\n]*")
            .expect("static header expression")
    });
    let value = URL.replace_all(value, "[url]");
    HEADER
        .replace_all(&value, "[credential]")
        .chars()
        .filter(|c| !c.is_control())
        .take(1024)
        .collect()
}

struct Expansion<'a> {
    options: &'a Options,
    config: &'a Config,
    now: jiff::Timestamp,
    secrets: &'a mut Vec<String>,
}

impl Expansion<'_> {
    fn string(
        &mut self,
        value: &str,
        endpoint: Option<(&Endpoint, bool)>,
    ) -> Result<String, Error> {
        let mut result = String::new();
        let mut rest = value;
        let mut rtmp = false;
        while let Some((prefix, tail)) = rest.split_once('{') {
            result.push_str(prefix);
            let Some((name, remaining)) = tail.split_once('}') else {
                return Err(Error::Invalid("unclosed placeholder"));
            };
            rest = remaining;
            match name {
                "processid" => result.push_str(&self.config.id),
                "domain" => result.push_str(&self.config.domain),
                "reference" => result.push_str(&self.config.reference),
                "inputid" | "outputid" => {
                    let (io, _) =
                        endpoint.ok_or(Error::Invalid("endpoint placeholder outside endpoint"))?;
                    result.push_str(&io.id);
                }
                "diskfs" | "fs:disk" => {
                    result.push_str(&self.options.disk);
                    if !self.config.domain.is_empty() {
                        result.push('/');
                        result.push_str(&self.config.domain);
                    }
                }
                "date" => result.push_str(&self.format_date("%Y-%m-%d_%H-%M-%S")?),
                "memfs" | "fs:mem" => {
                    let base = self
                        .options
                        .media
                        .memory
                        .as_ref()
                        .ok_or(Error::Invalid("memory mount unavailable"))?;
                    result.push_str(&self.storage_address(base)?);
                }
                name if name == "rtmp" || name.starts_with("rtmp,name=") => {
                    let base = self
                        .options
                        .media
                        .rtmp
                        .as_deref()
                        .ok_or(Error::Invalid("RTMP listener unavailable"))?;
                    result.push_str(base.trim_end_matches('/'));
                    if !self.config.domain.is_empty() {
                        result.push('/');
                        result.push_str(&self.config.domain);
                    }
                    if let Some(name) = name.strip_prefix("rtmp,name=") {
                        super::super::media::storage::Key::parse(name)
                            .map_err(|_| Error::Invalid("invalid RTMP stream name"))?;
                        result.push('/');
                        result.push_str(name.trim_start_matches('/'));
                    }
                    rtmp = true;
                }
                name if name == "srt" || name.starts_with("srt,name=") => {
                    let (io, input) =
                        endpoint.ok_or(Error::Invalid("SRT placeholder requires an endpoint"))?;
                    let url = if input {
                        &self.options.media.srt_read
                    } else {
                        &self.options.media.srt_write
                    };
                    let key = name
                        .strip_prefix("srt,name=")
                        .or_else(|| rest.strip_prefix('/'))
                        .filter(|path| !path.is_empty())
                        .unwrap_or(io.id.as_str());
                    if key.contains([':', ',', '?', '#', '&', '=', '%']) {
                        return Err(Error::Invalid("invalid SRT stream ID"));
                    }
                    let url = url
                        .as_deref()
                        .ok_or(Error::Invalid("SRT listener unavailable"))?;
                    let key = if self.config.domain.is_empty() {
                        key.to_owned()
                    } else {
                        format!("{}/{key}", self.config.domain)
                    };
                    result.push_str(&url.replace("COREMEDIAKEY", &key));
                    if rest.starts_with('/') {
                        rest = "";
                    }
                }
                value if value.starts_with("fs:") => {
                    let base = self
                        .options
                        .media
                        .remote_storage
                        .get(&value[3..])
                        .ok_or(Error::Invalid("unknown object storage mount"))?;
                    result.push_str(&self.storage_address(base)?);
                }
                "hwdevice" => {
                    return Err(Error::Invalid(
                        "placeholder requires an unavailable Core integration",
                    ));
                }
                name if name.starts_with("date,format=") => {
                    result.push_str(&self.format_date(&name["date,format=".len()..])?);
                }
                name if name.starts_with("env:") || name.starts_with("secret:") => {
                    let (kind, key) = name
                        .split_once(':')
                        .ok_or(Error::Invalid("invalid placeholder"))?;
                    let prefix = if kind == "env" {
                        "CORE_PROCESS_VALUE_"
                    } else {
                        "CORE_PROCESS_SECRET_"
                    };
                    let value = self
                        .options
                        .values
                        .get(&format!("{prefix}{key}"))
                        .ok_or(Error::Invalid("unconfigured FFmpeg placeholder"))?;
                    if kind == "secret" {
                        self.secrets.push(value.clone());
                    }
                    result.push_str(value);
                }
                _ => return Err(Error::Invalid("unrecognized FFmpeg placeholder")),
            }
            if result.len() > 16_384 {
                return Err(Error::Invalid("expanded FFmpeg value too large"));
            }
        }
        result.push_str(rest);
        if rtmp && !self.options.media.rtmp_token.is_empty() {
            self.secrets.push(self.options.media.rtmp_token.clone());
            let mut url =
                url::Url::parse(&result).map_err(|_| Error::Invalid("invalid RTMP address"))?;
            url.query_pairs_mut()
                .append_pair("token", &self.options.media.rtmp_token);
            result = url.into();
        }
        Ok(result)
    }

    fn storage_address(&mut self, base: &str) -> Result<String, Error> {
        let mut url =
            url::Url::parse(base).map_err(|_| Error::Invalid("storage mount unavailable"))?;
        if let Some((user, password)) = &self.options.media.memory_auth {
            url.set_username(user)
                .map_err(|_| Error::Invalid("invalid storage user"))?;
            url.set_password(Some(password))
                .map_err(|_| Error::Invalid("invalid storage credential"))?;
            self.secrets.push(password.clone());
        }
        Ok(url.as_str().trim_end_matches('/').to_owned())
    }

    fn format_date(&self, format: &str) -> Result<String, Error> {
        jiff::fmt::strtime::format(format, &super::scheduler::utc(self.now))
            .map_err(|_| Error::Invalid("invalid date format"))
    }

    fn arguments(
        &mut self,
        options: &[String],
        endpoint: Option<(&Endpoint, bool)>,
        strict: bool,
    ) -> Result<Vec<String>, Error> {
        let mut next_value = None;
        let mut result = Vec::with_capacity(options.len());
        for arg in options {
            let value = self.string(arg, endpoint)?;
            if value.len() > 16_384 || value.contains('\0') {
                return Err(Error::Invalid("invalid option size"));
            }
            if let Some(previous) = next_value {
                if value.starts_with('-')
                    && value != "-"
                    && value.parse::<f64>().is_err()
                    && !value[1..].split(':').next().is_some_and(|part| {
                        !part.is_empty() && part.bytes().all(|c| c.is_ascii_digit())
                    })
                {
                    return Err(Error::Invalid("unexpected flag in FFmpeg option value"));
                }
                if matches!(
                    previous,
                    "headers"
                        | "password"
                        | "passphrase"
                        | "encryption_key"
                        | "decryption_key"
                        | "cookies"
                        | "cookie"
                        | "key"
                        | "http_proxy"
                        | "authorization"
                ) {
                    self.secrets.push(value.clone());
                    self.secrets.extend(
                        value
                            .lines()
                            .filter_map(|line| line.split_once(':'))
                            .map(|(_, secret)| secret.trim())
                            .filter(|secret| secret.len() > 3)
                            .map(str::to_owned),
                    );
                }
                result.push(value);
                next_value = None;
                continue;
            }
            let flag = value
                .strip_prefix('-')
                .filter(|flag| !flag.is_empty())
                .ok_or(Error::Invalid("unexpected positional FFmpeg argument"))?;
            if !flag.as_bytes()[0].is_ascii_alphanumeric()
                || flag.contains(['=', '/', '\\'])
                || matches!(
                    flag,
                    "i" | "progress"
                        | "stdin"
                        | "nostdin"
                        | "report"
                        | "filter_script"
                        | "filter_complex_script"
                        | "passlogfile"
                        | "attach"
                        | "dump_attachment"
                        | "vstats"
                        | "pass"
                        | "vstats_file"
                        | "sdp_file"
                        | "print_graphs_file"
                )
                || flag.starts_with("protocol_")
                || flag.starts_with("stats_")
                || (strict && !policy_option(flag))
            {
                return Err(Error::Invalid(
                    "reserved FFmpeg option or option unavailable with access policies",
                ));
            }
            let takes_value = !matches!(
                flag,
                "y" | "n"
                    | "an"
                    | "vn"
                    | "sn"
                    | "dn"
                    | "re"
                    | "benchmark"
                    | "shortest"
                    | "copyts"
                    | "start_at_zero"
                    | "bitexact"
                    | "accurate_seek"
                    | "noaccurate_seek"
                    | "hide_banner"
                    | "nostats"
                    | "stats"
            );
            next_value = if takes_value {
                Some(
                    arg.strip_prefix('-')
                        .unwrap_or_default()
                        .split(':')
                        .next()
                        .unwrap_or_default(),
                )
            } else {
                None
            };
            result.push(value);
        }
        if next_value.is_some() {
            return Err(Error::Invalid("FFmpeg option is missing its value"));
        }
        Ok(result)
    }
}

// Restrict option-driven secondary I/O when explicit access rules are configured.
fn policy_option(flag: &str) -> bool {
    let flag = flag.split(':').next().unwrap_or(flag);
    matches!(
        flag,
        "y" | "n"
            | "an"
            | "vn"
            | "sn"
            | "dn"
            | "re"
            | "shortest"
            | "map"
            | "c"
            | "codec"
            | "vcodec"
            | "acodec"
            | "b"
            | "s"
            | "r"
            | "f"
            | "t"
            | "ss"
            | "threads"
            | "preset"
            | "crf"
            | "pix_fmt"
            | "ar"
            | "ac"
            | "loglevel"
            | "stream_loop"
            | "movflags"
            | "g"
            | "maxrate"
            | "bufsize"
    )
}

/// Clean only files selected by a process rule and preserve paths claimed by
/// other configured processes. Mounted files use the same storage boundary as HTTP.
pub async fn clean_outputs(options: &Options, config: &Config, purge: bool, graph: &super::Graph) {
    if !config
        .outputs
        .iter()
        .any(|output| !output.cleanup.is_empty())
    {
        return;
    }
    let graph = graph.read().await;
    let mut secrets = Vec::new();
    let now = jiff::Timestamp::now();
    let mut protected = Vec::new();
    for other in graph.values().map(|job| &job.config) {
        if other.id == config.id && other.domain == config.domain {
            continue;
        }
        for output in &other.outputs {
            let mut expanded = Expansion {
                options,
                config: other,
                now,
                secrets: &mut secrets,
            };
            for value in std::iter::once(&output.address)
                .chain(output.cleanup.iter().map(|rule| &rule.pattern))
            {
                let value = DATE_PLACEHOLDER.replace_all(value, "*");
                if let Ok(value) = expanded.string(&value, Some((output, false)))
                    && let Some((mount, name)) = cleanup_location(options, &value)
                {
                    let name = FF_SEGMENT.replace_all(&name, "*");
                    if let Ok(pattern) = glob::Pattern::new(&name) {
                        protected.push((mount, pattern));
                    }
                }
            }
        }
    }
    let mut expansion = Expansion {
        options,
        config,
        now,
        secrets: &mut secrets,
    };
    for output in &config.outputs {
        for rule in &output.cleanup {
            if (purge && !rule.purge_on_delete)
                || (!purge && rule.max_files == 0 && rule.max_file_age_seconds == 0)
            {
                continue;
            }
            let Ok(value) = expansion.string(&rule.pattern, Some((output, false))) else {
                continue;
            };
            let Some((mount, name)) = cleanup_location(options, &value) else {
                continue;
            };
            let Ok(pattern) = glob::Pattern::new(&name) else {
                continue;
            };
            let Some(backend) = options.media.cleanup_mounts.get(&mount) else {
                continue;
            };
            let Ok(mut files) = backend.list().await else {
                continue;
            };
            let matching = glob::MatchOptions {
                require_literal_separator: true,
                ..Default::default()
            };
            files.retain(|item| {
                pattern.matches_with(item.key.as_str(), matching)
                    && !protected.iter().any(|(name, pattern)| {
                        name == &mount && pattern.matches_with(item.key.as_str(), matching)
                    })
            });
            files.sort_unstable_by_key(|item| std::cmp::Reverse(item.modified));
            for (index, item) in files.into_iter().enumerate() {
                let old = rule.max_file_age_seconds > 0
                    && item
                        .modified
                        .elapsed()
                        .is_ok_and(|age| age.as_secs() >= rule.max_file_age_seconds);
                if (purge || old || (rule.max_files > 0 && index as u64 >= rule.max_files))
                    && backend
                        .stat(&item.key)
                        .await
                        .is_ok_and(|current| current.tag == item.tag)
                {
                    let _ = backend.delete(&item.key).await;
                }
            }
        }
    }
}

static FF_SEGMENT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"%(?:0?\d+)?d|\$Number\$|\$Time\$").expect("fixed segment pattern")
});
static DATE_PLACEHOLDER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"\{date(?:,[^}]*)?\}").expect("fixed date pattern")
});

fn cleanup_location(options: &Options, value: &str) -> Option<(String, String)> {
    if let Ok(url) = url::Url::parse(value) {
        if url.scheme() == "file" {
            let path = url.to_file_path().ok()?;
            return cleanup_location(options, path.to_str()?);
        }
        for (name, base) in options.media.memory.iter().map(|base| ("mem", base)).chain(
            options
                .media
                .remote_storage
                .iter()
                .map(|(name, base)| (name.as_str(), base)),
        ) {
            let base = url::Url::parse(base).ok()?;
            if url.origin() != base.origin() {
                continue;
            }
            if let Some(path) = url
                .path()
                .strip_prefix(base.path())
                .and_then(|path| path.strip_prefix('/'))
            {
                let path = percent_encoding::percent_decode_str(path)
                    .decode_utf8()
                    .ok()?;
                crate::media::storage::Key::parse(&path).ok()?;
                return Some((name.to_owned(), path.into_owned()));
            }
        }
        return None;
    }
    let path = Path::new(value);
    let path = path.strip_prefix(Path::new(&options.disk)).unwrap_or(path);
    if path.is_absolute() {
        return None;
    }
    let path = path.to_str()?;
    crate::media::storage::Key::parse(path).ok()?;
    Some(("disk".into(), path.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_placeholders_are_resolved_only_in_arguments_and_hidden_from_diagnostics() {
        let mut overrides = super::super::Overrides::default();
        overrides
            .values
            .insert("CORE_PROCESS_SECRET_SAMPLE".into(), "fixture-secret".into());
        let options =
            Options::from_document(&crate::config::defaults().unwrap(), &overrides).unwrap();
        let config = Config {
            id: "test".into(),
            inputs: vec![Endpoint {
                id: "source".into(),
                address: "https://example.test/video?token={secret:SAMPLE}".into(),
                options: vec![
                    "-headers".into(),
                    "Authorization: Bearer fixture-header-secret".into(),
                ],
                ..Default::default()
            }],
            outputs: vec![Endpoint {
                id: "output".into(),
                address: "-".into(),
                options: vec!["-f".into(), "null".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let command = options
            .build(&config, &BTreeMap::new(), &BTreeMap::new())
            .unwrap();
        assert!(
            command
                .args
                .iter()
                .any(|arg| arg.ends_with("token=fixture-secret"))
        );
        assert!(
            command.display.iter().all(
                |arg| !arg.contains("fixture-secret") && !arg.contains("fixture-header-secret")
            )
        );
        assert!(
            !command
                .redactor
                .clean("FFmpeg returned fixture-secret")
                .contains("fixture-secret")
        );
    }
}
