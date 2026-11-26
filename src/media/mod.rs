pub mod alias;
pub mod playlist;
pub mod sessions;
pub mod storage;
pub mod transport;

use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use serde_json::Value;
use sqlx::SqlitePool;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use self::{
    alias::Aliases,
    sessions::{Limits, Sessions},
    storage::{Disk, Error, Key, Memory, S3, S3Config, Storage},
    transport::Transport,
};

pub struct Mount {
    pub name: String,
    pub kind: &'static str,
    pub prefix: String,
    pub backend: Arc<dyn Storage>,
}

pub struct Media {
    pub mounts: BTreeMap<String, Mount>,
    pub sessions: Arc<Sessions>,
    pub aliases: Aliases,
    pub transport: Option<Transport>,
    pub private: bool,
    pub allowed_origins: Vec<String>,
    pub mime: BTreeMap<String, String>,
    pub memory_user: String,
    pub memory_require_auth: bool,
    memory_password: Zeroizing<String>,
    pub ffmpeg: PathBuf,
    pub launcher: Option<PathBuf>,
    pub cache: Option<Cache>,
    pub transfers: Arc<Semaphore>,
    pub uploads: Arc<Semaphore>,
    pub flv: Arc<Semaphore>,
    pub stop: CancellationToken,
    pub upload_rate: u64,
    pub download_rate: u64,
}

pub struct Cache {
    pub data: Memory,
    pub fills: Semaphore,
    pub ttl: Duration,
    pub size: u64,
    pub allow: Vec<String>,
    pub block: Vec<String>,
}

#[derive(Clone, Default)]
pub struct Endpoints {
    pub http_ready: bool,
    pub memory: Option<String>,
    pub memory_auth: Option<(String, String)>,
    pub rtmp: Option<String>,
    pub rtmp_token: String,
    pub srt_read: Option<String>,
    pub srt_write: Option<String>,
    pub remote_storage: BTreeMap<String, String>,
    pub cleanup_mounts: BTreeMap<String, Arc<dyn Storage>>,
}

impl Media {
    pub async fn open(
        pool: SqlitePool,
        doc: &Value,
        key: Zeroizing<Vec<u8>>,
        launcher: Option<PathBuf>,
    ) -> Result<Self, Error> {
        validate_settings(doc)?;
        let storage = &doc["storage"];
        let disk = &storage["disk"];
        let memory = &storage["memory"];
        let mut mounts = BTreeMap::new();
        let mut prefixes = BTreeSet::from(["/".to_owned(), "/memfs".to_owned()]);
        let disk_root = PathBuf::from(disk["dir"].as_str().ok_or(Error::Invalid)?);
        if disk_root.as_os_str().is_empty() {
            return Err(Error::Invalid);
        }
        let disksize = mbytes(&disk["max_size_mbytes"], 0)?;
        let disk = Disk::open(disk_root, (disksize > 0).then_some(disksize)).await?;
        mounts.insert(
            "disk".into(),
            Mount {
                name: "disk".into(),
                kind: "disk",
                prefix: "/".into(),
                backend: Arc::new(disk),
            },
        );
        let memsize = mbytes(&memory["max_size_mbytes"], 64)?;
        if memsize > 4 * 1024 * 1024 * 1024 {
            return Err(Error::Invalid);
        }
        let mem = Memory::new(memsize, memory["purge"].as_bool().unwrap_or(false));
        mounts.insert(
            "mem".into(),
            Mount {
                name: "mem".into(),
                kind: "mem",
                prefix: "/memfs".into(),
                backend: Arc::new(mem),
            },
        );
        let s3 = storage["s3"].as_array().into_iter().flatten();
        for entry in s3.take(32) {
            let cfg: S3Config =
                serde_json::from_value(entry.clone()).map_err(|_| Error::Invalid)?;
            valid_mount(&cfg.mountpoint)?;
            if cfg.name.is_empty()
                || cfg.name.len() > 64
                || !cfg
                    .name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
                || !prefixes.insert(cfg.mountpoint.clone())
                || mounts.contains_key(&cfg.name)
            {
                return Err(Error::Invalid);
            }
            let backend = S3::open(&cfg)?;
            mounts.insert(
                cfg.name.clone(),
                Mount {
                    name: cfg.name,
                    kind: "s3",
                    prefix: cfg.mountpoint,
                    backend: Arc::new(backend),
                },
            );
        }
        let limits = Limits::from_document(&doc["sessions"])?;
        let transport = Transport::open(doc, &limits, launcher.as_deref()).await?;
        if let Some(flv) = transport.as_ref().and_then(Transport::flv_mount)
            && prefixes.contains(flv)
        {
            return Err(Error::Invalid);
        }
        let cache = &storage["disk"]["cache"];
        let cache = if cache["enable"].as_bool() == Some(true) {
            let capacity = mbytes(&cache["max_size_mbytes"], 32)?.min(256 * 1024 * 1024);
            Some(Cache {
                data: Memory::new(capacity, true),
                fills: Semaphore::new(8),
                ttl: Duration::from_secs(cache["ttl_seconds"].as_u64().unwrap_or(300).min(86400)),
                size: mbytes(&cache["max_file_size_mbytes"], 1)?.min(8 * 1024 * 1024),
                allow: strings(&cache["types"]["allow"]),
                block: strings(&cache["types"]["block"]),
            })
        } else {
            None
        };
        let password = memory["auth"]["password"]
            .as_str()
            .filter(|value| !value.is_empty())
            .map_or_else(
                || {
                    format!(
                        "{}{}",
                        uuid::Uuid::new_v4().simple(),
                        uuid::Uuid::new_v4().simple()
                    )
                },
                str::to_owned,
            );
        Ok(Self {
            mounts,
            aliases: Aliases::open(pool).await?,
            sessions: Arc::new(Sessions::new(limits, key)),
            transport,
            private: storage["private"].as_bool().unwrap_or(false),
            allowed_origins: strings(&storage["cors"]["origins"]),
            mime: serde_json::from_value(storage["mimetypes"].clone()).unwrap_or_default(),
            memory_require_auth: memory["auth"]["enable"].as_bool().unwrap_or(true),
            memory_user: memory["auth"]["username"]
                .as_str()
                .unwrap_or("admin")
                .into(),
            memory_password: Zeroizing::new(password),
            ffmpeg: PathBuf::from(doc["ffmpeg"]["binary"].as_str().unwrap_or("ffmpeg")),
            launcher,
            cache,
            transfers: Arc::new(Semaphore::new(256)),
            uploads: Arc::new(Semaphore::new(16)),
            flv: Arc::new(Semaphore::new(16)),
            stop: CancellationToken::new(),
            upload_rate: storage["upload_bitrate_kbit"]
                .as_u64()
                .unwrap_or(0)
                .saturating_mul(125),
            download_rate: storage["download_bitrate_kbit"]
                .as_u64()
                .unwrap_or(0)
                .saturating_mul(125),
        })
    }

    pub fn get(&self, name: &str) -> Result<&Mount, Error> {
        self.mounts.get(name).ok_or(Error::Missing)
    }

    pub fn mounted(&self, path: &str) -> Result<(&Mount, Key), Error> {
        let found = self
            .mounts
            .values()
            .filter(|mount| {
                mount.prefix != "/"
                    && path
                        .strip_prefix(&mount.prefix)
                        .is_some_and(|tail| tail.starts_with('/'))
            })
            .max_by_key(|mount| mount.prefix.len());
        match found {
            Some(mount) => Ok((mount, Key::parse(&path[mount.prefix.len()..])?)),
            None => Ok((self.get("disk")?, Key::parse(path)?)),
        }
    }

    pub fn basic(&self, user: &str, password: &str) -> bool {
        transport::eq(&self.memory_user, user) && transport::eq(&self.memory_password, password)
    }

    pub fn endpoints(&self, address: SocketAddr, http_ready: bool) -> Endpoints {
        let local = if address.ip().is_unspecified() {
            if address.is_ipv4() {
                SocketAddr::from(([127, 0, 0, 1], address.port()))
            } else {
                SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, address.port()))
            }
        } else {
            address
        };
        let transport = self.transport.as_ref();
        Endpoints {
            http_ready,
            memory: Some(format!("http://{local}/memfs")),
            memory_auth: Some((self.memory_user.clone(), self.memory_password.to_string())),
            rtmp: transport.and_then(Transport::base_rtmp),
            rtmp_token: transport.map_or_else(String::new, |t| t.rtmp_token().to_owned()),
            srt_read: transport.and_then(|t| t.srt_url("COREMEDIAKEY", false).ok()),
            srt_write: transport.and_then(|t| t.srt_url("COREMEDIAKEY", true).ok()),
            cleanup_mounts: self
                .mounts
                .iter()
                .map(|(name, mount)| (name.clone(), mount.backend.clone()))
                .collect(),
            remote_storage: self
                .mounts
                .values()
                .filter(|m| m.kind == "s3")
                .map(|m| (m.name.clone(), format!("http://{local}{}", m.prefix)))
                .collect(),
        }
    }

    pub async fn close(&self) {
        self.stop.cancel();
        if let Some(transport) = &self.transport {
            transport.close().await;
        }
    }
}

pub fn validate_settings(doc: &Value) -> Result<(), Error> {
    let storage = &doc["storage"];
    if storage["disk"]["dir"].as_str().is_none_or(str::is_empty)
        || mbytes(&storage["memory"]["max_size_mbytes"], 64)? > 4 * 1024 * 1024 * 1024
    {
        return Err(Error::Invalid);
    }
    mbytes(&storage["disk"]["max_size_mbytes"], 0)?;
    mbytes(&storage["disk"]["cache"]["max_size_mbytes"], 32)?;
    mbytes(&storage["disk"]["cache"]["max_file_size_mbytes"], 1)?;
    Limits::from_document(&doc["sessions"])?;
    if let Some(buckets) = storage["s3"].as_array()
        && buckets.len() > 32
    {
        return Err(Error::Invalid);
    }
    if doc["srt"]["enable"].as_bool() == Some(true) {
        let passphrase = doc["srt"]["passphrase"].as_str().unwrap_or_default();
        if !passphrase.is_empty() && !(10..=79).contains(&passphrase.len()) {
            return Err(Error::Invalid);
        }
    }
    for (service, setting, enabled) in [
        ("rtmp", "address", "enable"),
        ("rtmp", "address_tls", "enable_tls"),
        ("srt", "address", "enable"),
        ("tls", "address", "enable"),
    ] {
        if doc[service][enabled].as_bool() == Some(true)
            && doc[service][setting]
                .as_str()
                .is_none_or(|address| crate::config::address(address).is_err())
        {
            return Err(Error::Invalid);
        }
    }
    Ok(())
}

pub fn valid_mount(mount: &str) -> Result<(), Error> {
    if !mount.starts_with('/')
        || mount.ends_with('/')
        || mount.contains(['%', '?', '#'])
        || mount
            .split('/')
            .nth(1)
            .is_some_and(|segment| ["api", "ping"].contains(&segment))
    {
        return Err(Error::Invalid);
    }
    Key::parse(mount)?;
    Ok(())
}

fn mbytes(value: &Value, default: u64) -> Result<u64, Error> {
    if value.is_number() && value.as_u64().is_none() {
        return Err(Error::Invalid);
    }
    value
        .as_u64()
        .filter(|value| *value > 0)
        .unwrap_or(default)
        .checked_mul(1024 * 1024)
        .ok_or(Error::Invalid)
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .take(128)
        .map(str::to_owned)
        .collect()
}

pub fn content_type<'a>(mime: &'a BTreeMap<String, String>, path: &str) -> &'a str {
    let extension = Path::new(path)
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "mpd" => "application/dash+xml",
        "m4s" => "video/iso.segment",
        "flv" => "video/x-flv",
        _ => mime
            .get(&format!(".{extension}"))
            .map(String::as_str)
            .unwrap_or("application/octet-stream"),
    }
}
