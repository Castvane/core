mod disk;
mod memory;
mod s3;

use std::{
    ops::Range,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{StreamExt, stream::BoxStream};
use serde::Serialize;

pub use disk::Disk;
pub use memory::Memory;
pub use s3::{S3, S3Config};

pub const MAX_UPLOAD: u64 = 4 * 1024 * 1024 * 1024;
pub const MAX_LIST: usize = 50_000;
pub type Stream = BoxStream<'static, Result<Bytes, Error>>;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Key(String);

impl Key {
    pub fn parse(value: &str) -> Result<Self, Error> {
        let value = value.strip_prefix('/').unwrap_or(value);
        if value.is_empty()
            || value.len() > 2048
            || value.contains('\\')
            || value.chars().any(char::is_control)
            || value.split('/').any(|part| {
                part.is_empty()
                    || part == "."
                    || part == ".."
                    || part.len() > 255
                    || part.starts_with(".core-upload-")
                    || (cfg!(windows) && windows_reserved(part))
            })
        {
            return Err(Error::Invalid);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn windows_reserved(part: &str) -> bool {
    if part.ends_with(['.', ' '])
        || part.contains([':', '*', '?', '"', '<', '>', '|'])
        || part.to_ascii_lowercase().starts_with(".core-upload-")
    {
        return true;
    }
    let stem = part
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    })
}

#[derive(Clone)]
pub struct Meta {
    pub key: Key,
    pub size: u64,
    pub modified: SystemTime,
    pub tag: String,
}

#[derive(Serialize)]
pub struct Usage {
    pub bytes: u64,
    pub resident_bytes: u64,
    pub objects: usize,
    pub capacity_bytes: Option<u64>,
}

pub fn now_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone, Copy, Debug)]
pub enum Error {
    Invalid,
    Missing,
    Forbidden,
    Full,
    Changed,
    Unavailable,
    Interrupted,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "invalid media path or setting",
            Self::Missing => "file not found",
            Self::Forbidden => "access denied",
            Self::Full => "storage capacity exceeded",
            Self::Changed => "object changed during request",
            Self::Unavailable => "media service unavailable",
            Self::Interrupted => "media transfer interrupted",
        })
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        if value.raw_os_error() == Some(28) || value.raw_os_error() == Some(122) {
            return Self::Full;
        }
        match value.kind() {
            std::io::ErrorKind::NotFound => Self::Missing,
            std::io::ErrorKind::PermissionDenied => Self::Forbidden,
            _ => Self::Unavailable,
        }
    }
}

#[async_trait]
pub trait Storage: Send + Sync {
    async fn health(&self) -> Result<(), Error> {
        Ok(())
    }
    async fn stat(&self, key: &Key) -> Result<Meta, Error>;
    async fn read(&self, meta: &Meta, range: Range<u64>) -> Result<Stream, Error>;
    async fn put(&self, key: &Key, stream: Stream) -> Result<bool, Error>;
    async fn delete(&self, key: &Key) -> Result<(), Error>;
    async fn list(&self) -> Result<Vec<Meta>, Error>;
    async fn usage(&self) -> Result<Usage, Error>;
}

pub fn bounded(stream: Stream, maximum: u64) -> Stream {
    Box::pin(async_stream::try_stream! {
        let mut stream = stream;
        let mut count = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            count = count.saturating_add(chunk.len() as u64);
            if count > maximum { Err(Error::Full)?; }
            yield chunk;
        }
    })
}
