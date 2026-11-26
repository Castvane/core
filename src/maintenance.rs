use std::{
    io::{self, Read},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Deserializer, de};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use tempfile::{Builder, NamedTempFile};

use crate::{
    StartupError,
    config::{self, Environment},
    state,
};

const MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();
const CONFIG_LIMIT: u64 = 4 * 1024 * 1024;

pub async fn validate_configuration(
    env: &Environment,
    path: Option<&Path>,
) -> Result<(), StartupError> {
    let (document, has_password) = if let Some(path) = path {
        (
            merge_configuration(read_json(path, CONFIG_LIMIT).await?)?,
            false,
        )
    } else if tokio::fs::try_exists(&env.db).await? {
        let mut connection = readonly(&env.db).await?;
        let (document, version, has_password): (String, i64, bool) = sqlx::query_as(
            "SELECT document, schema_version, EXISTS(SELECT 1 FROM administrator) FROM configuration"
        ).fetch_one(&mut connection).await?;
        let mut parsed: Value = serde_json::from_str(&document)?;
        parsed["version"] = json!(version);
        (merge_configuration(parsed)?, has_password)
    } else {
        (config::defaults()?, false)
    };
    let embedded = document
        .pointer("/api/auth/password")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let will_provision = !has_password && (env.password.is_some() || embedded.is_some());
    if let Some(password) = env.password.as_deref().map(String::as_str).or(embedded) {
        state::validate_password(password)
            .map_err(|_| "administrator password must be 12–1024 bytes")?;
    }
    let mut document = document;
    if document["id"] == "" {
        document["id"] = json!(uuid::Uuid::nil().to_string());
    }
    if will_provision {
        document["api"]["auth"]["enable"] = json!(true);
        if document["api"]["auth"]["username"] == "" {
            document["api"]["auth"]["username"] = json!("admin");
        }
    }
    let effective = env.apply(document);
    let settings = state::validate(&effective, has_password || will_provision)
        .map_err(|field| format!("invalid configuration field: {field}"))?;
    crate::engine::Options::from_document(&effective, &env.engine)?;
    if !config::address(&settings.address)?.ip().is_loopback()
        && !settings.api.auth.enable
        && !env.allow_insecure
    {
        return Err("external listeners require administrator authentication or explicit CORE_API_AUTH_ENABLE=false".into());
    }
    Ok(())
}

pub async fn init_configuration(path: PathBuf) -> Result<(), StartupError> {
    let mut content = serde_json::to_vec_pretty(&config::defaults()?)?;
    content.push(b'\n');
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut output = temporary_for(&path)?;
        output.write_all(&content)?;
        publish(output, &path)
    })
    .await?
}

pub(crate) async fn read_json(path: &Path, limit: u64) -> Result<Value, StartupError> {
    use tokio::io::AsyncReadExt;
    if !tokio::fs::metadata(path).await?.is_file() {
        return Err("expected a regular JSON file".into());
    }
    let file = tokio::fs::File::open(path).await?;
    let mut data = zeroize::Zeroizing::new(Vec::new());
    file.take(limit + 1).read_to_end(&mut data).await?;
    if data.len() as u64 > limit {
        return Err("JSON file exceeds the supported size".into());
    }
    Ok(serde_json::from_slice::<UniqueJson>(&data)?.0)
}

struct UniqueJson(Value);

impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> de::Visitor<'de> for Visitor {
            type Value = UniqueJson;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON value without duplicate keys")
            }

            fn visit_map<A: de::MapAccess<'de>>(
                self,
                mut access: A,
            ) -> Result<Self::Value, A::Error> {
                use serde_json::map::Entry;
                let mut map = serde_json::Map::new();
                while let Some(key) = access.next_key::<String>()? {
                    match map.entry(key) {
                        Entry::Vacant(entry) => {
                            entry.insert(access.next_value::<UniqueJson>()?.0);
                        }
                        Entry::Occupied(_) => {
                            return Err(de::Error::custom("duplicate JSON object key"));
                        }
                    }
                }
                Ok(UniqueJson(Value::Object(map)))
            }

            fn visit_seq<A: de::SeqAccess<'de>>(
                self,
                mut access: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = access.next_element::<UniqueJson>()? {
                    values.push(value.0);
                }
                Ok(UniqueJson(Value::Array(values)))
            }

            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Null))
            }
            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }
            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }
            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(UniqueJson(value.into()))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

pub(crate) fn merge_configuration(patch: Value) -> Result<Value, StartupError> {
    let mut document = config::defaults()?;
    config::merge(
        &mut document,
        config::migrate_patch(patch).ok_or("unsupported configuration schema")?,
    )
    .map_err(|_| "invalid configuration object")?;
    Ok(document)
}

async fn readonly(path: &Path) -> Result<SqliteConnection, StartupError> {
    Ok(SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .read_only(true)
            .busy_timeout(std::time::Duration::from_secs(5))
            .pragma("trusted_schema", "OFF"),
    )
    .await?)
}

async fn validate_database(
    connection: &mut SqliteConnection,
    integrity: bool,
) -> Result<(), StartupError> {
    if integrity {
        let result: String = sqlx::query_scalar("PRAGMA integrity_check(1)")
            .fetch_one(&mut *connection)
            .await?;
        if result != "ok"
            || sqlx::query("PRAGMA foreign_key_check")
                .fetch_optional(&mut *connection)
                .await?
                .is_some()
        {
            return Err("database integrity verification failed".into());
        }
    }
    let versions: Vec<(i64, bool, Vec<u8>)> =
        sqlx::query_as("SELECT version, success, checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&mut *connection)
            .await?;
    if versions.is_empty()
        || !versions
            .iter()
            .zip(MIGRATOR.iter())
            .all(|((version, success, checksum), migration)| {
                *success
                    && *version == migration.version
                    && checksum.as_slice() == migration.checksum.as_ref()
            })
        || versions.len() > MIGRATOR.iter().count()
    {
        return Err("database schema does not match a supported Castvane migration history".into());
    }
    Ok(())
}

pub async fn database_check(path: &Path) -> Result<(), StartupError> {
    let mut connection = readonly(path).await?;
    validate_database(&mut connection, true).await
}

pub async fn backup(source: &Path, output: &Path) -> Result<String, StartupError> {
    let mut connection = readonly(source).await?;
    validate_database(&mut connection, false).await?;
    let destination = temporary_for(output)?;
    let identifier = destination
        .path()
        .to_str()
        .ok_or("backup path must be UTF-8")?;
    sqlx::query("VACUUM INTO ?")
        .bind(identifier)
        .execute(&mut connection)
        .await?;
    database_check(destination.path()).await?;
    let path = output.to_owned();
    tokio::task::spawn_blocking(move || {
        ensure_no_sqlite_sidecars(&path)?;
        let checksum = checksum(destination.path())?;
        publish(destination, &path)?;
        Ok(checksum)
    })
    .await?
}

pub async fn restore(
    source: PathBuf,
    output: PathBuf,
    expected: String,
) -> Result<(), StartupError> {
    let destination = tokio::task::spawn_blocking({
        let output = output.clone();
        move || -> Result<NamedTempFile, StartupError> {
            ensure_no_sqlite_sidecars(&source)?;
            ensure_no_sqlite_sidecars(&output)?;
            let mut input = std::fs::File::open(source)?;
            if !input.metadata()?.is_file() {
                return Err("expected a regular Castvane backup file".into());
            }
            let mut destination = temporary_for(&output)?;
            io::copy(&mut input, &mut destination)?;
            if checksum(destination.path())? != expected.to_ascii_lowercase() {
                return Err("backup SHA-256 does not match".into());
            }
            Ok(destination)
        }
    })
    .await??;
    database_check(destination.path()).await?;
    tokio::task::spawn_blocking(move || {
        ensure_no_sqlite_sidecars(&output)?;
        publish(destination, &output)
    })
    .await?
}

pub(crate) fn ensure_no_sqlite_sidecars(path: &Path) -> io::Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sibling = path.as_os_str().to_owned();
        sibling.push(suffix);
        match std::fs::symlink_metadata(sibling) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "SQLite sidecar exists",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn checksum(path: &Path) -> io::Result<String> {
    let mut reader = io::BufReader::new(std::fs::File::open(path)?);
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let length = reader.read(&mut buffer)?;
        if length == 0 {
            use std::fmt::Write;
            let mut hex = String::with_capacity(64);
            for byte in digest.finalize() {
                write!(&mut hex, "{byte:02x}").map_err(io::Error::other)?;
            }
            return Ok(hex);
        }
        digest.update(&buffer[..length]);
    }
}

pub(crate) fn temporary_for(path: &Path) -> io::Result<NamedTempFile> {
    Builder::new()
        .prefix(".castvane-")
        .permissions(std::fs::Permissions::from_mode(0o600))
        .tempfile_in(parent(path))
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

pub(crate) fn publish(file: NamedTempFile, path: &Path) -> Result<(), StartupError> {
    file.as_file().sync_all()?;
    file.persist_noclobber(path)?;
    std::fs::File::open(parent(path))?.sync_all()?;
    Ok(())
}

pub async fn ffmpeg_diagnostics(env: Environment) -> Result<Value, StartupError> {
    let mut document = config::defaults()?;
    if tokio::fs::try_exists(&env.db).await? {
        let mut connection = readonly(&env.db).await?;
        let (stored, version): (String, i64) =
            sqlx::query_as("SELECT document, schema_version FROM configuration")
                .fetch_one(&mut connection)
                .await?;
        let mut parsed: Value = serde_json::from_str(&stored)?;
        parsed["version"] = json!(version);
        document = merge_configuration(parsed)?;
    }
    let options = crate::engine::Options::from_document(&env.apply(document), &env.engine)?;
    let inventory = crate::engine::inspect_executables(&options).await?;
    Ok(serde_json::to_value(inventory)?)
}
