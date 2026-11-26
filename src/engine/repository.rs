use std::collections::BTreeMap;

use serde_json::Value;
use sqlx::{FromRow, SqlitePool};

use super::{Error, Job, Run};

#[derive(Clone)]
pub struct Repository(pub SqlitePool);

#[derive(FromRow)]
struct Row {
    uid: String,
    configuration: String,
    desired: bool,
    created_at: i64,
    updated_at: i64,
}

impl Repository {
    pub async fn all(&self) -> Result<Vec<Job>, Error> {
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT uid, configuration, desired, created_at, updated_at FROM process_jobs ORDER BY domain, id",
        ).fetch_all(&self.0).await?;
        rows.into_iter()
            .map(|row| {
                Ok(Job {
                    uid: row.uid,
                    config: serde_json::from_str(&row.configuration)?,
                    desired: row.desired,
                    created: row.created_at,
                    updated: row.updated_at,
                })
            })
            .collect()
    }

    pub async fn save(&self, job: &Job, metadata: BTreeMap<String, Value>) -> Result<(), Error> {
        let mut tx = self.0.begin_with("BEGIN IMMEDIATE").await?;
        crate::zone::validate_claim(&mut tx, job).await?;
        sqlx::query(
            "INSERT INTO process_jobs VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(uid) DO UPDATE SET domain=excluded.domain, id=excluded.id,
             configuration=excluded.configuration, desired=excluded.desired, updated_at=excluded.updated_at",
        ).bind(&job.uid).bind(&job.config.domain).bind(&job.config.id)
            .bind(serde_json::to_string(&job.config)?).bind(job.desired)
            .bind(job.created).bind(job.updated).execute(&mut *tx).await?;
        for (key, value) in metadata {
            write_metadata(&mut tx, Some(&job.uid), &key, &value).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete(&self, uid: &str) -> Result<(), Error> {
        sqlx::query("DELETE FROM process_jobs WHERE uid=?")
            .bind(uid)
            .execute(&self.0)
            .await?;
        Ok(())
    }

    pub async fn metadata(&self, uid: Option<&str>) -> Result<BTreeMap<String, Value>, Error> {
        let rows: Vec<(String, String)> = match uid {
            Some(uid) => {
                sqlx::query_as("SELECT key,value FROM process_metadata WHERE uid=?")
                    .bind(uid)
                    .fetch_all(&self.0)
                    .await?
            }
            None => {
                sqlx::query_as("SELECT key,value FROM core_metadata")
                    .fetch_all(&self.0)
                    .await?
            }
        };
        rows.into_iter()
            .map(|(key, value)| Ok((key, serde_json::from_str(&value)?)))
            .collect()
    }

    pub async fn set_metadata(
        &self,
        uid: Option<&str>,
        key: &str,
        value: &Value,
    ) -> Result<(), Error> {
        let mut tx = self.0.begin_with("BEGIN IMMEDIATE").await?;
        write_metadata(&mut tx, uid, key, value).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn history(&self, uid: &str) -> Result<Vec<Run>, Error> {
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT document FROM process_reports WHERE uid=? ORDER BY sequence DESC",
        )
        .bind(uid)
        .fetch_all(&self.0)
        .await?;
        rows.into_iter()
            .map(|r| Ok(serde_json::from_str(&r)?))
            .collect()
    }

    pub async fn record(&self, uid: &str, run: &Run, keep: u32) -> Result<(), Error> {
        let mut tx = self.0.begin().await?;
        insert_report(&mut tx, uid, run).await?;
        sqlx::query(
            "DELETE FROM process_reports WHERE uid=? AND sequence IN
            (SELECT sequence FROM process_reports WHERE uid=? ORDER BY sequence DESC LIMIT -1 OFFSET ?)",
        ).bind(uid).bind(uid).bind(keep).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn replace_history(&self, uid: &str, history: &[Run]) -> Result<(), Error> {
        let mut tx = self.0.begin().await?;
        sqlx::query("DELETE FROM process_reports WHERE uid=?")
            .bind(uid)
            .execute(&mut *tx)
            .await?;
        for run in history.iter().rev() {
            insert_report(&mut tx, uid, run).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

async fn insert_report(tx: &mut sqlx::SqliteConnection, uid: &str, run: &Run) -> Result<(), Error> {
    sqlx::query("INSERT INTO process_reports (uid,document) VALUES (?,?)")
        .bind(uid)
        .bind(serde_json::to_string(run)?)
        .execute(tx)
        .await?;
    Ok(())
}

async fn write_metadata(
    tx: &mut sqlx::SqliteConnection,
    uid: Option<&str>,
    key: &str,
    value: &Value,
) -> Result<(), Error> {
    if key.is_empty() || key.len() > 256 || value.to_string().len() > 65_536 {
        return Err(Error::Invalid("invalid metadata key or value size"));
    }
    match (uid, value.is_null()) {
        (Some(uid), true) => {
            sqlx::query("DELETE FROM process_metadata WHERE uid=? AND key=?")
                .bind(uid)
                .bind(key)
                .execute(tx)
                .await?;
        }
        (None, true) => {
            sqlx::query("DELETE FROM core_metadata WHERE key=?")
                .bind(key)
                .execute(tx)
                .await?;
        }
        (Some(uid), false) => {
            if sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM process_metadata WHERE uid=? AND key<>?",
            )
            .bind(uid)
            .bind(key)
            .fetch_one(&mut *tx)
            .await?
                >= 32
            {
                return Err(Error::Invalid("process metadata capacity reached"));
            }
            sqlx::query("INSERT INTO process_metadata VALUES (?,?,?) ON CONFLICT(uid,key) DO UPDATE SET value=excluded.value")
                .bind(uid).bind(key).bind(value.to_string()).execute(tx).await?;
        }
        (None, false) => {
            if sqlx::query_scalar::<_, i64>("SELECT count(*) FROM core_metadata WHERE key<>?")
                .bind(key)
                .fetch_one(&mut *tx)
                .await?
                >= 128
            {
                return Err(Error::Invalid("core metadata capacity reached"));
            }
            sqlx::query("INSERT INTO core_metadata VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
                .bind(key).bind(value.to_string()).execute(tx).await?;
        }
    }
    Ok(())
}
