use std::{net::SocketAddr, sync::Arc};

use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{
    net::TcpListener,
    sync::{RwLock, mpsc},
};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    auth::{Authentication, hash_password, verify_password},
    config::{self, Environment, SCHEMA, Settings},
    listener::ReloadableListener,
    persistence::{Record, Store},
};

pub type StartupError = Box<dyn std::error::Error + Send + Sync>;

pub struct App {
    pub(crate) store: Store,
    pub(crate) auth: Authentication,
    pub(crate) environment: Environment,
    pub(crate) started_at: OffsetDateTime,
    listening: RwLock<SocketAddr>,
    replacement: Option<mpsc::Sender<TcpListener>>,
    pub(crate) loaded_at: tokio::sync::RwLock<String>,
    pub(crate) requests: tokio::sync::Semaphore,
    pub(crate) writes: tokio::sync::Mutex<()>,
    pub(crate) on_reload: Arc<dyn Fn(&str) + Send + Sync>,
}

impl App {
    pub async fn open(mut environment: Environment) -> Result<Self, StartupError> {
        let store = Store::open(&environment.db).await?;
        let mut tx = store.write().await?;
        let now = OffsetDateTime::now_utc();
        let timestamp = now.format(&Rfc3339)?;
        let mut document = config::defaults()?;
        document["id"] = json!(Uuid::new_v4().to_string());
        if let Some(dir) = environment.db.parent().and_then(|path| path.to_str())
            && !dir.is_empty()
        {
            document["db"]["dir"] = json!(dir);
        }

        let existing = Store::read_for_write(&mut tx).await?;
        if let Some(record) = &existing {
            let mut old: Value = serde_json::from_str(&record.document)?;
            old["version"] = json!(record.schema_version);
            config::merge(
                &mut document,
                config::migrate_patch(old).ok_or("unsupported database configuration version")?,
            )
            .map_err(|_| "invalid database configuration")?;
        }

        let mut password = environment
            .password
            .as_ref()
            .map(|password| Zeroizing::new(password.to_string()));
        if password.is_none() {
            password = take_secret(&mut document, "/api/auth/password");
        } else {
            take_secret(&mut document, "/api/auth/password");
        }
        take_secret(&mut document, "/api/auth/jwt/secret");

        // Bootstrap credentials survive restarts. Environment overlays still win on this run.
        if password.is_some()
            && existing
                .as_ref()
                .and_then(|r| r.password_hash.as_ref())
                .is_none()
        {
            let effective = environment.apply(document.clone());
            let username = effective["api"]["auth"]["username"]
                .as_str()
                .filter(|name| !name.is_empty())
                .unwrap_or("admin");
            document["api"]["auth"]["username"] = json!(username);
            document["api"]["auth"]["enable"] = json!(true);
        }

        let mut has_password = existing.as_ref().is_some_and(|r| r.password_hash.is_some());
        if let Some(password) = password {
            validate_password(&password)
                .map_err(|_| "administrator password must be 12–1024 bytes")?;
            let saved = existing.as_ref().and_then(|r| r.password_hash.clone());
            let unchanged = if let Some(saved) = saved {
                let candidate = password.clone();
                tokio::task::spawn_blocking(move || verify_password(&candidate, &saved)).await?
            } else {
                false
            };
            if !unchanged {
                let hash = hash_password(password).await?;
                Store::save_password(&mut tx, &hash, &Uuid::new_v4().to_string()).await?;
            }
            has_password = true;
        }

        let effective = environment.apply(document.clone());
        let settings = validate(&effective, has_password)
            .map_err(|field| format!("invalid configuration field: {field}"))?;
        let listening = config::address(&settings.address)?;
        if !listening.ip().is_loopback() && !settings.api.auth.enable && !environment.allow_insecure
        {
            return Err("external listeners require administrator authentication or explicit CORE_API_AUTH_ENABLE=false".into());
        }

        if existing.as_ref().is_none_or(|record| {
            record.schema_version != SCHEMA
                || serde_json::from_str::<Value>(&record.document)
                    .ok()
                    .as_ref()
                    != Some(&document)
        }) {
            Store::save(
                &mut tx,
                SCHEMA,
                &document,
                existing.as_ref().map_or(&timestamp, |r| &r.created_at),
                existing.as_ref().map_or(&timestamp, |r| &r.updated_at),
            )
            .await?;
        }
        let mut candidate = Zeroizing::new([0; 32]);
        getrandom::fill(candidate.as_mut()).map_err(|_| "could not generate signing key")?;
        let persisted = Zeroizing::new(Store::key(&mut tx, &candidate).await?);
        let key = environment
            .signing_key
            .as_ref()
            .map_or(persisted, |key| Zeroizing::new(key.as_bytes().to_vec()));
        tx.commit().await?;
        let auth = Authentication::new(key).await?;
        environment.clear_secrets();
        Ok(Self {
            store,
            auth,
            environment,
            started_at: now,
            listening: RwLock::new(listening),
            replacement: None,
            loaded_at: tokio::sync::RwLock::new(timestamp),
            requests: tokio::sync::Semaphore::new(128),
            writes: tokio::sync::Mutex::new(()),
            on_reload: Arc::new(|_| {}),
        })
    }

    pub async fn bind(&mut self) -> std::io::Result<ReloadableListener> {
        let listener = TcpListener::bind(*self.listening.get_mut()).await?;
        let (sender, receiver) = mpsc::channel(1);
        self.replacement = Some(sender);
        Ok(ReloadableListener::new(listener, receiver))
    }

    pub(crate) async fn listening_address(&self) -> SocketAddr {
        *self.listening.read().await
    }

    pub(crate) async fn reload_address(&self, address: SocketAddr) -> Result<(), &'static str> {
        let mut listening = self.listening.write().await;
        if *listening == address {
            return Ok(());
        }
        let replacement = self.replacement.as_ref().ok_or("listener unavailable")?;
        let slot = replacement
            .try_reserve()
            .map_err(|_| "listener cannot be reconfigured yet")?;
        let listener = TcpListener::bind(address)
            .await
            .map_err(|_| "could not bind configured address")?;
        let local = listener
            .local_addr()
            .map_err(|_| "could not inspect configured listener")?;
        slot.send(listener);
        *listening = address;
        tracing::info!(address = %local, "HTTP listener reconfigured");
        Ok(())
    }

    pub(crate) fn effective(&self, record: &Record) -> Result<Value, serde_json::Error> {
        serde_json::from_str(&record.document).map(|document| self.environment.apply(document))
    }

    pub async fn log_level(&self) -> Result<String, StartupError> {
        let record = self.store.read().await?;
        let settings: Settings = serde_json::from_value(self.effective(&record)?)?;
        Ok(settings.log.level)
    }

    pub fn set_reload_hook(&mut self, hook: impl Fn(&str) + Send + Sync + 'static) {
        self.on_reload = Arc::new(hook);
    }

    pub async fn close(&self) {
        self.store.close().await;
    }
}

pub(crate) fn take_secret(document: &mut Value, pointer: &str) -> Option<Zeroizing<String>> {
    let field = document.pointer_mut(pointer)?;
    let Value::String(secret) = std::mem::replace(field, json!("")) else {
        return None;
    };
    (!secret.is_empty()).then_some(Zeroizing::new(secret))
}

pub(crate) fn validate_password(password: &str) -> Result<(), ()> {
    if (12..=1024).contains(&password.len()) {
        Ok(())
    } else {
        Err(())
    }
}

pub(crate) fn validate(document: &Value, has_password: bool) -> Result<Settings, &'static str> {
    let settings: Settings = serde_json::from_value(document.clone()).map_err(|_| "api.access")?;
    if settings.id.is_empty() {
        return Err("id");
    }
    if settings.name.trim().is_empty() || settings.name.len() > 256 {
        return Err("name");
    }
    if config::address(&settings.address).is_err() {
        return Err("address");
    }
    if !matches!(
        settings.log.level.as_str(),
        "silent" | "error" | "warn" | "info" | "debug" | "trace"
    ) {
        return Err("log.level");
    }
    if document["log"]["max_lines"]
        .as_i64()
        .is_none_or(|lines| lines < 0)
    {
        return Err("log.max_lines");
    }
    if document["db"]["dir"].as_str().is_none_or(str::is_empty) {
        return Err("db.dir");
    }
    if settings.api.auth.enable || has_password {
        let username = settings.api.auth.username.as_str();
        if username.is_empty() || username.len() > 128 || username.chars().any(char::is_control) {
            return Err("api.auth.username");
        }
        if !has_password {
            return Err("api.auth.password");
        }
    }
    for field in [
        "max_cpu_usage",
        "max_memory_usage",
        "max_gpu_usage",
        "max_gpu_memory_usage",
    ] {
        if document["resources"][field]
            .as_f64()
            .is_none_or(|value| !(0.0..=100.0).contains(&value))
        {
            return Err("resources");
        }
    }
    for field in ["min_port", "max_port"] {
        if document["playout"][field]
            .as_u64()
            .is_none_or(|port| port > 65535)
        {
            return Err("playout");
        }
    }
    Ok(settings)
}
