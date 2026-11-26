use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf};

use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zeroize::Zeroizing;

pub const SCHEMA: i64 = 3;

#[derive(Deserialize)]
pub struct Settings {
    pub id: String,
    pub name: String,
    pub address: String,
    pub log: LogSettings,
    pub api: ApiSettings,
}

#[derive(Deserialize)]
pub struct LogSettings {
    pub level: String,
}

#[derive(Deserialize)]
pub struct ApiSettings {
    pub read_only: bool,
    pub access: AccessSettings,
    pub auth: AuthSettings,
}

#[derive(Deserialize)]
pub struct AccessSettings {
    #[serde(default)]
    pub trusted_proxies: Vec<ipnet::IpNet>,
    pub http: NetworkPolicy,
    pub https: NetworkPolicy,
}

#[derive(Deserialize)]
pub struct NetworkPolicy {
    pub allow: Option<Vec<IpNet>>,
    pub block: Option<Vec<IpNet>>,
}

#[derive(Deserialize)]
pub struct AuthSettings {
    pub enable: bool,
    pub disable_localhost: bool,
    pub username: String,
    pub jwt: JwtSettings,
}

#[derive(Deserialize)]
pub struct JwtSettings {
    pub access_ttl_sec: i64,
    pub refresh_ttl_sec: i64,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct StorageBucket {
    name: String,
    mountpoint: String,
    auth: StorageCredentials,
    endpoint: String,
    access_key_id: String,
    secret_access_key: String,
    bucket: String,
    region: String,
    use_ssl: bool,
    virtual_hosted_style: bool,
    required: bool,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct StorageCredentials {
    enable: bool,
    username: String,
    password: String,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct AuthTenant {
    domain: String,
    audience: String,
    clientid: String,
    users: Option<Vec<String>>,
}

pub struct Environment {
    pub(crate) engine: crate::engine::Overrides,
    pub db: PathBuf,
    pub password: Option<Zeroizing<String>>,
    pub signing_key: Option<Zeroizing<String>>,
    pub allow_insecure: bool,
    password_override: bool,
    signing_key_override: bool,
    overrides: BTreeMap<&'static str, Value>,
}

impl Environment {
    pub fn read() -> Result<Self, &'static str> {
        let mut environment = Self::from_lookup(|key| std::env::var(key).ok())?;
        environment.engine.values = std::env::vars_os()
            .filter_map(|(key, value)| {
                let key = key.into_string().ok()?;
                if key.starts_with("CORE_PROCESS_VALUE_") || key.starts_with("CORE_PROCESS_SECRET_")
                {
                    return value.into_string().ok().map(|value| (key, value));
                }
                None
            })
            .collect();
        Ok(environment)
    }

    /// Select the standalone Core executable as the Linux media child launcher.
    pub fn with_media_launcher(mut self, executable: PathBuf) -> Self {
        self.engine.launcher = Some(executable);
        self
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, &'static str> {
        let db_dir = get("CORE_DB_DIR");
        let db = get("CORE_DATABASE_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(db_dir.as_deref().unwrap_or("./config")).join("restreamer.sqlite3")
            });
        let password = get("CORE_API_AUTH_PASSWORD")
            .or_else(|| get("RS_PASSWORD"))
            .filter(|value| !value.is_empty())
            .map(Zeroizing::new);
        let signing_key = get("CORE_API_AUTH_JWT_SECRET")
            .filter(|value| !value.is_empty())
            .map(Zeroizing::new);
        if signing_key.as_ref().is_some_and(|key| key.len() < 32) {
            return Err("CORE_API_AUTH_JWT_SECRET requires at least 32 bytes");
        }

        let allow_insecure = get("CORE_API_AUTH_ENABLE")
            .map(|value| parse_flag(&value))
            .transpose()?
            .is_some_and(|flag| !flag);
        let mut overrides = BTreeMap::new();
        for (path, keys) in [
            ("id", &["CORE_ID"][..]),
            ("name", &["CORE_NAME"][..]),
            ("address", &["CORE_BIND_ADDR", "CORE_ADDRESS"][..]),
            ("log.level", &["CORE_LOG_LEVEL"][..]),
            ("db.dir", &["CORE_DB_DIR"][..]),
            (
                "api.auth.username",
                &["CORE_API_AUTH_USERNAME", "RS_USERNAME"][..],
            ),
            ("ffmpeg.binary", &["CORE_FFMPEG_BINARY"][..]),
            ("ffmpeg.probe_binary", &["CORE_FFPROBE_BINARY"][..]),
            ("storage.disk.dir", &["CORE_STORAGE_DISK_DIR"][..]),
            (
                "rtmp.transport_binary",
                &["CORE_MEDIA_TRANSPORT_BINARY"][..],
            ),
            ("rtmp.address", &["CORE_RTMP_ADDRESS"][..]),
            ("rtmp.address_tls", &["CORE_RTMP_ADDRESS_TLS"][..]),
            ("rtmp.app", &["CORE_RTMP_APP"][..]),
            ("rtmp.token", &["CORE_RTMP_TOKEN"][..]),
            ("rtmp.httpflv.mount", &["CORE_RTMP_HTTPFLV_MOUNT"][..]),
            ("srt.address", &["CORE_SRT_ADDRESS"][..]),
            ("srt.passphrase", &["CORE_SRT_PASSPHRASE"][..]),
            ("srt.token", &["CORE_SRT_TOKEN"][..]),
            ("tls.trust_file", &["CORE_TLS_TRUST_FILE"][..]),
            ("log.format", &["CORE_LOG_FORMAT"][..]),
            ("tls.address", &["CORE_TLS_ADDRESS"][..]),
            ("tls.email", &["CORE_TLS_EMAIL"][..]),
            ("tls.acme_directory", &["CORE_TLS_ACME_DIRECTORY"][..]),
            ("tls.acme_storage", &["CORE_TLS_ACME_STORAGE"][..]),
            ("tls.acme_ca_file", &["CORE_TLS_ACME_CA_FILE"][..]),
            (
                "tls.cert_file",
                &["CORE_TLS_CERT_FILE", "CORE_TLS_CERTFILE"][..],
            ),
            (
                "tls.key_file",
                &["CORE_TLS_KEY_FILE", "CORE_TLS_KEYFILE"][..],
            ),
            (
                "storage.memory.auth.username",
                &["CORE_STORAGE_MEMORY_AUTH_USERNAME", "CORE_MEMFS_USERNAME"][..],
            ),
            (
                "storage.memory.auth.password",
                &["CORE_STORAGE_MEMORY_AUTH_PASSWORD", "CORE_MEMFS_PASSWORD"][..],
            ),
            ("router.ui_path", &["CORE_ROUTER_UI_PATH"][..]),
        ] {
            if let Some(value) = keys.iter().find_map(|key| get(key)) {
                overrides.insert(path, json!(value));
            }
        }
        for (path, key) in [
            ("update_check", "CORE_UPDATE_CHECK"),
            ("host.auto", "CORE_HOST_AUTO"),
            ("api.read_only", "CORE_API_READ_ONLY"),
            ("api.auth.enable", "CORE_API_AUTH_ENABLE"),
            (
                "api.auth.disable_localhost",
                "CORE_API_AUTH_DISABLE_LOCALHOST",
            ),
            ("api.auth.auth0.enable", "CORE_API_AUTH_AUTH0_ENABLE"),
            ("rtmp.enable", "CORE_RTMP_ENABLE"),
            ("rtmp.required", "CORE_RTMP_REQUIRED"),
            ("rtmp.enable_tls", "CORE_RTMP_ENABLE_TLS"),
            ("rtmp.httpflv.enable", "CORE_RTMP_HTTPFLV_ENABLE"),
            (
                "storage.disk.cache.enable",
                "CORE_STORAGE_DISK_CACHE_ENABLE",
            ),
            ("storage.private", "CORE_STORAGE_PRIVATE"),
            ("storage.memory.purge", "CORE_STORAGE_MEMORY_PURGE"),
            (
                "storage.memory.auth.enable",
                "CORE_STORAGE_MEMORY_AUTH_ENABLE",
            ),
            ("sessions.enable", "CORE_SESSIONS_ENABLE"),
            ("srt.enable", "CORE_SRT_ENABLE"),
            ("srt.required", "CORE_SRT_REQUIRED"),
            ("playout.enable", "CORE_PLAYOUT_ENABLE"),
            ("tls.enable", "CORE_TLS_ENABLE"),
            ("tls.auto", "CORE_TLS_AUTO"),
            ("tls.staging", "CORE_TLS_STAGING"),
            ("tls.acme_agree", "CORE_TLS_ACME_AGREE"),
            ("metrics.enable", "CORE_METRICS_ENABLE"),
            (
                "metrics.enable_prometheus",
                "CORE_METRICS_ENABLE_PROMETHEUS",
            ),
            ("cluster.enable", "CORE_CLUSTER_ENABLE"),
        ] {
            if let Some(value) = get(key) {
                overrides.insert(path, json!(parse_flag(&value)?));
            }
        }
        if password.is_some() && !overrides.contains_key("api.auth.enable") {
            overrides.insert("api.auth.enable", json!(true));
        }
        for (path, key) in [
            ("host.name", "CORE_HOST_NAME"),
            (
                "api.access.trusted_proxies",
                "CORE_API_ACCESS_TRUSTED_PROXIES",
            ),
            ("log.topics", "CORE_LOG_TOPICS"),
            ("api.access.http.allow", "CORE_API_ACCESS_HTTP_ALLOW"),
            ("api.access.http.block", "CORE_API_ACCESS_HTTP_BLOCK"),
            ("api.access.https.allow", "CORE_API_ACCESS_HTTPS_ALLOW"),
            ("api.access.https.block", "CORE_API_ACCESS_HTTPS_BLOCK"),
            ("storage.cors.origins", "CORE_STORAGE_CORS_ORIGINS"),
        ] {
            if let Some(value) = get(key) {
                overrides.insert(
                    path,
                    json!(
                        value
                            .split(',')
                            .map(str::trim)
                            .filter(|item| !item.is_empty())
                            .collect::<Vec<_>>()
                    ),
                );
            }
        }
        if let Some(value) = get("CORE_LOG_MAX_LINES").or_else(|| get("CORE_LOG_MAXLINES")) {
            overrides.insert(
                "log.max_lines",
                json!(
                    value
                        .parse::<i64>()
                        .map_err(|_| "invalid CORE_LOG_MAX_LINES")?
                ),
            );
        }
        for (path, keys) in [
            (
                "ffmpeg.probe_timeout_seconds",
                &["CORE_FFMPEG_PROBE_TIMEOUT_SECONDS"][..],
            ),
            (
                "ffmpeg.stop_timeout_seconds",
                &["CORE_FFMPEG_STOP_TIMEOUT_SECONDS"][..],
            ),
            ("ffmpeg.max_retries", &["CORE_FFMPEG_MAX_RETRIES"][..]),
            (
                "ffmpeg.max_processes",
                &["CORE_FFMPEG_MAX_PROCESSES", "CORE_FFMPEG_MAXPROCESSES"][..],
            ),
            (
                "ffmpeg.log.max_lines",
                &["CORE_FFMPEG_LOG_MAX_LINES", "CORE_FFMPEG_LOG_MAXLINES"][..],
            ),
            (
                "ffmpeg.log.max_history",
                &["CORE_FFMPEG_LOG_MAX_HISTORY", "CORE_FFMPEG_LOG_MAXHISTORY"][..],
            ),
        ] {
            if let Some(value) = keys.iter().find_map(|key| get(key)) {
                overrides.insert(
                    path,
                    json!(
                        value
                            .parse::<u64>()
                            .map_err(|_| "invalid FFmpeg environment setting")?
                    ),
                );
            }
        }
        for (path, key) in [
            (
                "ffmpeg.access.input.allow",
                "CORE_FFMPEG_ACCESS_INPUT_ALLOW",
            ),
            (
                "ffmpeg.access.input.block",
                "CORE_FFMPEG_ACCESS_INPUT_BLOCK",
            ),
            (
                "ffmpeg.access.output.allow",
                "CORE_FFMPEG_ACCESS_OUTPUT_ALLOW",
            ),
            (
                "ffmpeg.access.output.block",
                "CORE_FFMPEG_ACCESS_OUTPUT_BLOCK",
            ),
        ] {
            if let Some(value) = get(key) {
                overrides.insert(path, json!(value.split_whitespace().collect::<Vec<_>>()));
            }
        }
        for (path, keys) in [
            (
                "storage.disk.max_size_mbytes",
                &[
                    "CORE_STORAGE_DISK_MAX_SIZE_MBYTES",
                    "CORE_STORAGE_DISK_MAXSIZEMBYTES",
                ][..],
            ),
            (
                "storage.memory.max_size_mbytes",
                &[
                    "CORE_STORAGE_MEMORY_MAX_SIZE_MBYTES",
                    "CORE_STORAGE_MEMORY_MAXSIZEMBYTES",
                ][..],
            ),
            (
                "storage.disk.cache.max_size_mbytes",
                &[
                    "CORE_STORAGE_DISK_CACHE_MAX_SIZE_MBYTES",
                    "CORE_STORAGE_DISK_CACHE_MAXSIZEMBYTES",
                ][..],
            ),
            (
                "storage.disk.cache.max_file_size_mbytes",
                &[
                    "CORE_STORAGE_DISK_CACHE_MAX_FILE_SIZE_MBYTES",
                    "CORE_STORAGE_DISK_CACHE_MAXFILESIZEMBYTES",
                ][..],
            ),
            (
                "storage.disk.cache.ttl_seconds",
                &[
                    "CORE_STORAGE_DISK_CACHE_TTL_SECONDS",
                    "CORE_STORAGE_DISK_CACHE_TTLSECONDS",
                ][..],
            ),
            ("api.max_requests", &["CORE_API_MAX_REQUESTS"][..]),
            ("api.max_connections", &["CORE_API_MAX_CONNECTIONS"][..]),
            ("api.timeout_seconds", &["CORE_API_TIMEOUT_SECONDS"][..]),
            (
                "api.auth.jwt.access_ttl_sec",
                &["CORE_API_AUTH_JWT_ACCESS_TTL_SEC"][..],
            ),
            (
                "api.auth.jwt.refresh_ttl_sec",
                &["CORE_API_AUTH_JWT_REFRESH_TTL_SEC"][..],
            ),
            (
                "tls.acme_renew_before_seconds",
                &["CORE_TLS_ACME_RENEW_BEFORE_SECONDS"][..],
            ),
            (
                "storage.upload_bitrate_kbit",
                &["CORE_STORAGE_UPLOAD_BITRATE_KBIT"][..],
            ),
            (
                "storage.download_bitrate_kbit",
                &["CORE_STORAGE_DOWNLOAD_BITRATE_KBIT"][..],
            ),
            (
                "sessions.session_timeout_sec",
                &["CORE_SESSIONS_SESSION_TIMEOUT_SEC"][..],
            ),
            (
                "sessions.max_sessions",
                &["CORE_SESSIONS_MAX_SESSIONS", "CORE_SESSIONS_MAXSESSIONS"][..],
            ),
            (
                "sessions.max_viewers_per_stream",
                &["CORE_SESSIONS_MAX_VIEWERS_PER_STREAM"][..],
            ),
            (
                "sessions.max_bitrate_mbit",
                &[
                    "CORE_SESSIONS_MAX_BITRATE_MBIT",
                    "CORE_SESSIONS_MAXBITRATE_MBIT",
                ][..],
            ),
            (
                "sessions.stream_bitrate_kbit",
                &["CORE_SESSIONS_STREAM_BITRATE_KBIT"][..],
            ),
            (
                "sessions.session_bitrate_kbit",
                &["CORE_SESSIONS_SESSION_BITRATE_KBIT"][..],
            ),
        ] {
            if let Some(value) = keys.iter().find_map(|key| get(key)) {
                overrides.insert(
                    path,
                    json!(value.parse::<u64>().map_err(|_| "invalid media limit")?),
                );
            }
        }
        for (path, keys) in [
            (
                "storage.disk.cache.types.allow",
                &[
                    "CORE_STORAGE_DISK_CACHE_TYPES_ALLOW",
                    "CORE_STORAGE_DISK_CACHE_TYPES",
                ][..],
            ),
            (
                "storage.disk.cache.types.block",
                &["CORE_STORAGE_DISK_CACHE_TYPES_BLOCK"][..],
            ),
        ] {
            if let Some(value) = keys.iter().find_map(|key| get(key)) {
                overrides.insert(path, json!(value.split_whitespace().collect::<Vec<_>>()));
            }
        }
        if let Some(value) = get("CORE_STORAGE_S3_JSON") {
            let stores: Value =
                serde_json::from_str(&value).map_err(|_| "invalid CORE_STORAGE_S3_JSON")?;
            if !stores.is_array() {
                return Err("CORE_STORAGE_S3_JSON must be an array");
            }
            overrides.insert("storage.s3", stores);
        } else if let Some(value) = get("CORE_STORAGE_S3") {
            overrides.insert("storage.s3", parse_legacy_s3(&value)?);
        }
        Ok(Self {
            engine: crate::engine::Overrides {
                launcher: None,
                cgroup: get("CORE_PROCESS_CGROUP_ROOT")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from),
                values: BTreeMap::new(),
            },
            password_override: password.is_some(),
            signing_key_override: signing_key.is_some(),
            db,
            password,
            signing_key,
            allow_insecure,
            overrides,
        })
    }

    pub fn apply(&self, mut document: Value) -> Value {
        for (path, value) in &self.overrides {
            let pointer = format!("/{}", path.replace('.', "/"));
            if let Some(field) = document.pointer_mut(&pointer) {
                *field = value.clone();
            }
        }
        document
    }

    pub(crate) fn clear_secrets(&mut self) {
        self.password = None;
        self.signing_key = None;
    }

    pub(crate) fn signing_key_is_managed(&self) -> bool {
        self.signing_key_override
    }

    pub(crate) fn password_is_managed(&self) -> bool {
        self.password_override
    }

    pub fn override_names(&self) -> Vec<&str> {
        let mut names: Vec<_> = self.overrides.keys().copied().collect();
        if self.password_override {
            names.push("api.auth.password");
        }
        if self.signing_key_override {
            names.push("api.auth.jwt.secret");
        }
        names.sort_unstable();
        names
    }
}

fn parse_flag(value: &str) -> Result<bool, &'static str> {
    match value {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Ok(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Ok(false),
        _ => Err("invalid boolean environment override"),
    }
}

pub fn defaults() -> Result<Value, serde_json::Error> {
    serde_json::from_str(include_str!("config-defaults.json"))
}

pub fn address(value: &str) -> Result<SocketAddr, std::net::AddrParseError> {
    if value.starts_with(':') {
        return format!("0.0.0.0{value}").parse();
    }
    value.parse()
}

/// Normalize legacy on-disk documents and versioned API updates to the current layout.
pub fn migrate_patch(mut patch: Value) -> Option<Value> {
    let version = patch.get("version")?.as_i64()?;
    match version {
        1 => {
            if let Some(rtmp) = patch.get_mut("rtmp").and_then(Value::as_object_mut) {
                if let Some(app) = rtmp.get("app").and_then(Value::as_str)
                    && !app.starts_with('/')
                {
                    rtmp.insert("app".into(), json!(format!("/{app}")));
                }
                if rtmp.get("enable_tls").and_then(Value::as_bool) == Some(true) {
                    rtmp.insert("enable".into(), json!(true));
                    if let Some(listen) = rtmp.get("address").and_then(Value::as_str) {
                        let (host, port) = listen.rsplit_once(':')?;
                        let port = port.parse::<u16>().ok()?.checked_sub(1)?;
                        let plain = format!("{host}:{port}");
                        rtmp.insert("address_tls".into(), json!(listen));
                        rtmp.insert("address".into(), json!(plain));
                    }
                }
            }
        }
        2 | 3 => {}
        _ => return None,
    }
    if version < 3
        && let Some(types) = patch.pointer_mut("/storage/disk/cache/types")
        && (types.is_array() || types.is_null())
    {
        *types = json!({"allow":types.take()});
    }
    if let Some(stats) = patch.as_object_mut()?.remove("stats") {
        patch.as_object_mut()?.entry("sessions").or_insert(stats);
    }
    patch["version"] = json!(SCHEMA);
    Some(patch)
}

/// JSON null matches Go's partial update semantics for scalar and struct fields.
pub(crate) struct InvalidField;

pub(crate) fn merge(target: &mut Value, patch: Value) -> Result<(), InvalidField> {
    let schema = defaults().map_err(|_| InvalidField)?;
    merge_fields(target, patch, &schema, "")
}

fn merge_fields(
    target: &mut Value,
    patch: Value,
    schema: &Value,
    field: &str,
) -> Result<(), InvalidField> {
    let string_map = matches!(field, "mimetypes" | "routes") && schema.is_object();
    match (schema, patch) {
        (Value::Object(shape), Value::Object(patch)) if !string_map => {
            let target = target.as_object_mut().ok_or(InvalidField)?;
            for (key, value) in patch {
                if let (Some(field), Some(schema)) = (target.get_mut(&key), shape.get(&key)) {
                    merge_fields(field, value, schema, &key)?;
                }
            }
        }
        (Value::Object(_), Value::Object(values)) if string_map => {
            if !values.values().all(Value::is_string) {
                return Err(InvalidField);
            }
            if target.is_null() {
                *target = json!({});
            }
            target.as_object_mut().ok_or(InvalidField)?.extend(values);
        }
        (Value::Array(_), Value::Null) => *target = Value::Null,
        (Value::Object(_), Value::Null) if string_map => *target = Value::Null,
        (_, Value::Null) => {}
        (Value::Array(_), value @ Value::Array(_)) if field == "s3" => {
            let entries: Vec<StorageBucket> =
                serde_json::from_value(value).map_err(|_| InvalidField)?;
            *target = serde_json::to_value(entries).map_err(|_| InvalidField)?;
        }
        (Value::Array(_), value @ Value::Array(_)) if field == "tenants" => {
            let entries: Vec<AuthTenant> =
                serde_json::from_value(value).map_err(|_| InvalidField)?;
            *target = serde_json::to_value(entries).map_err(|_| InvalidField)?;
        }
        (Value::Array(_), Value::Array(items)) => {
            if !items.iter().all(Value::is_string) {
                return Err(InvalidField);
            }
            *target = Value::Array(items);
        }
        (Value::Number(_), value @ Value::Number(_)) => {
            let floating = matches!(
                field,
                "max_cpu_usage" | "max_memory_usage" | "max_gpu_usage" | "max_gpu_memory_usage"
            );
            if !floating && !value.is_i64() && !value.is_u64() {
                return Err(InvalidField);
            }
            *target = value;
        }
        (Value::Bool(_), value @ Value::Bool(_)) | (Value::String(_), value @ Value::String(_)) => {
            *target = value;
        }
        _ => return Err(InvalidField),
    }
    Ok(())
}

pub fn redact(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            for (key, field) in fields {
                match key.as_str() {
                    "password" | "secret" | "token" | "passphrase" | "secret_access_key"
                    | "access_key_id" => *field = json!(""),
                    _ => redact(field),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact),
        _ => {}
    }
}

fn parse_legacy_s3(value: &str) -> Result<Value, &'static str> {
    let invalid = "invalid CORE_STORAGE_S3";
    let mut stores = Vec::new();
    for address in value.split('|').filter(|value| !value.is_empty()) {
        let url = url::Url::parse(address).map_err(|_| invalid)?;
        let host = url.host_str().ok_or(invalid)?;
        let (region, host) = host.split_once('.').ok_or(invalid)?;
        let endpoint = if let Some(port) = url.port() {
            format!("{host}:{port}")
        } else {
            host.to_owned()
        };
        let pairs: BTreeMap<_, _> = url.query_pairs().collect();
        let decode = |value: &str| {
            percent_encoding::percent_decode_str(value)
                .decode_utf8()
                .map(|v| v.into_owned())
                .map_err(|_| invalid)
        };
        stores.push(json!({
            "name":pairs.get("name"),"mountpoint":pairs.get("mount"),"region":region,"endpoint":endpoint,
            "access_key_id":decode(url.username())?,"secret_access_key":decode(url.password().ok_or(invalid)?)?,
            "bucket":url.path().trim_start_matches('/'),"use_ssl":url.scheme()=="https"
        }));
    }
    Ok(json!(stores))
}
