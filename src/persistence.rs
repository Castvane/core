use std::{path::Path, time::Duration};

use serde_json::Value;
use sqlx::{
    FromRow, Sqlite, SqlitePool, Transaction,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};

pub struct Store(pub(crate) SqlitePool);

#[derive(FromRow)]
pub struct Record {
    pub schema_version: i64,
    pub document: String,
    pub created_at: String,
    pub updated_at: String,
    pub password_hash: Option<String>,
    pub generation: Option<String>,
}

impl Store {
    pub async fn open(path: &Path) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        // SQLite opens a pre-created private file and derives sidecar permissions from it.
        let mut file = tokio::fs::OpenOptions::new();
        file.write(true).create(true).truncate(false);
        #[cfg(unix)]
        file.mode(0o600);
        let metadata = file.open(path).await?.metadata().await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err("database file must be private (chmod 600)".into());
            }
        }

        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(path)
                    .journal_mode(SqliteJournalMode::Wal)
                    .busy_timeout(Duration::from_secs(5))
                    .foreign_keys(true)
                    .pragma("trusted_schema", "OFF")
                    .disable_statement_logging(),
            )
            .await?;
        if let Err(error) = sqlx::migrate!().run(&pool).await {
            pool.close().await;
            return Err(error.into());
        }
        Ok(Self(pool))
    }

    pub async fn close(&self) {
        self.0.close().await;
    }

    pub async fn write(&self) -> Result<Transaction<'static, Sqlite>, sqlx::Error> {
        self.0.begin_with("BEGIN IMMEDIATE").await
    }

    pub async fn read(&self) -> Result<Record, sqlx::Error> {
        sqlx::query_as(SELECT).fetch_one(&self.0).await
    }

    pub async fn read_for_write(
        tx: &mut Transaction<'_, Sqlite>,
    ) -> Result<Option<Record>, sqlx::Error> {
        sqlx::query_as(SELECT).fetch_optional(&mut **tx).await
    }

    pub async fn save(
        tx: &mut Transaction<'_, Sqlite>,
        version: i64,
        document: &Value,
        created_at: &str,
        updated_at: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO configuration (singleton, schema_version, document, created_at, updated_at)
             VALUES (1, ?, ?, ?, ?)
             ON CONFLICT(singleton) DO UPDATE SET schema_version = excluded.schema_version,
             document = excluded.document, updated_at = excluded.updated_at,
             revision = configuration.revision + 1",
        )
        .bind(version).bind(document.to_string()).bind(created_at).bind(updated_at)
        .execute(&mut **tx).await?;
        Ok(())
    }

    pub async fn save_password(
        tx: &mut Transaction<'_, Sqlite>,
        hash: &str,
        generation: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO administrator VALUES (1, ?, ?)
             ON CONFLICT(singleton) DO UPDATE SET password_hash = excluded.password_hash,
             generation = excluded.generation",
        )
        .bind(hash)
        .bind(generation)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    pub async fn key(
        tx: &mut Transaction<'_, Sqlite>,
        candidate: &[u8; 32],
    ) -> Result<Vec<u8>, sqlx::Error> {
        sqlx::query("INSERT OR IGNORE INTO signing_key VALUES (1, ?)")
            .bind(candidate.as_slice())
            .execute(&mut **tx)
            .await?;
        sqlx::query_scalar("SELECT secret FROM signing_key WHERE singleton = 1")
            .fetch_one(&mut **tx)
            .await
    }
}

const SELECT: &str =
    "SELECT schema_version, document, created_at, updated_at, password_hash, generation
     FROM configuration LEFT JOIN administrator ON administrator.singleton = 1
     WHERE configuration.singleton = 1";

use sqlx::ConnectOptions;
