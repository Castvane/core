use serde::{Deserialize, Serialize};
use sqlx::SqliteConnection;

use crate::engine::{Error, Job};

pub const DEFAULT: &str = "$default";

pub fn canonical(id: &str) -> &str {
    if id.is_empty() { DEFAULT } else { id }
}

pub fn valid_id(id: &str) -> bool {
    id == DEFAULT
        || (!id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Resources {
    cpu_usage: f64,
    memory_mbytes: u64,
    gpu_usage: f64,
    gpu_encoder: f64,
    gpu_decoder: f64,
    gpu_memory_mbytes: u64,
}

impl Resources {
    pub fn valid(self) -> bool {
        [
            self.cpu_usage,
            self.gpu_usage,
            self.gpu_encoder,
            self.gpu_decoder,
        ]
        .iter()
        .all(|v| v.is_finite() && *v >= 0.0)
    }

    pub fn claim(job: &Job) -> Self {
        let limits = &job.config.limits;
        Self {
            cpu_usage: limits.cpu_usage,
            memory_mbytes: limits.memory_mbytes,
            gpu_usage: limits.gpu_usage,
            gpu_encoder: limits.gpu_encoder,
            gpu_decoder: limits.gpu_decoder,
            gpu_memory_mbytes: limits.gpu_memory_mbytes,
        }
    }

    pub fn plus(self, rhs: Self) -> Self {
        Self {
            cpu_usage: self.cpu_usage + rhs.cpu_usage,
            memory_mbytes: self.memory_mbytes.saturating_add(rhs.memory_mbytes),
            gpu_usage: self.gpu_usage + rhs.gpu_usage,
            gpu_encoder: self.gpu_encoder + rhs.gpu_encoder,
            gpu_decoder: self.gpu_decoder + rhs.gpu_decoder,
            gpu_memory_mbytes: self.gpu_memory_mbytes.saturating_add(rhs.gpu_memory_mbytes),
        }
    }

    fn exceeds(self, current: Self, previous: Self) -> bool {
        [
            (self.cpu_usage, current.cpu_usage, previous.cpu_usage),
            (
                self.memory_mbytes as f64,
                current.memory_mbytes as f64,
                previous.memory_mbytes as f64,
            ),
            (self.gpu_usage, current.gpu_usage, previous.gpu_usage),
            (self.gpu_encoder, current.gpu_encoder, previous.gpu_encoder),
            (self.gpu_decoder, current.gpu_decoder, previous.gpu_decoder),
            (
                self.gpu_memory_mbytes as f64,
                current.gpu_memory_mbytes as f64,
                previous.gpu_memory_mbytes as f64,
            ),
        ]
        .into_iter()
        .any(|(limit, current, previous)| limit > 0.0 && current > limit && current > previous)
    }
}

pub async fn validate_claim(conn: &mut SqliteConnection, job: &Job) -> Result<(), Error> {
    let zone = canonical(&job.config.zone);
    let document: Option<String> = sqlx::query_scalar("SELECT document FROM zone WHERE id=?")
        .bind(zone)
        .fetch_optional(&mut *conn)
        .await?;
    let limits: Resources = serde_json::from_str(&document.ok_or(Error::Invalid("unknown zone"))?)?;
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT uid, coalesce(json_extract(configuration,'$.limits'),'{}') FROM process_jobs
        WHERE coalesce(nullif(json_extract(configuration, '$.zone'),''),'$default') = ?",
    )
    .bind(zone)
    .fetch_all(conn)
    .await?;
    let mut other = Resources::default();
    let mut previous = Resources::default();
    for (uid, configuration) in rows {
        let claim: Resources = serde_json::from_str(&configuration)?;
        if uid == job.uid {
            previous = claim;
        } else {
            other = other.plus(claim);
        }
    }
    if limits.exceeds(other.plus(Resources::claim(job)), other.plus(previous)) {
        return Err(Error::Conflict(
            "zone resource reservation exceeds configured limit",
        ));
    }
    Ok(())
}
