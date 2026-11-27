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
    engine::{Engine, Options},
    listener::ReloadableListener,
    media::Media,
    persistence::{Record, Store},
};

pub type StartupError = Box<dyn std::error::Error + Send + Sync>;

pub struct App {
    pub(crate) store: Store,
    pub(crate) cluster: Option<Arc<crate::cluster::Cluster>>,
    worker: Option<(Store, tempfile::TempDir)>,
    pub(crate) auth: Authentication,
    pub(crate) iam: crate::iam::Iam,
    pub(crate) ops: Arc<crate::ops::Operations>,
    pub(crate) certificates: Arc<crate::certificates::Certificates>,
    pub(crate) media: Arc<Media>,
    pub(crate) processes: Arc<Engine>,
    pub(crate) environment: Environment,
    pub(crate) started_at: OffsetDateTime,
    listening: RwLock<SocketAddr>,
    local_address: RwLock<SocketAddr>,
    replacement: Option<mpsc::Sender<TcpListener>>,
    pub(crate) loaded_at: tokio::sync::RwLock<String>,
    pub(crate) requests: tokio::sync::Semaphore,
    pub(crate) trusted_proxies: Vec<ipnet::IpNet>,
    pub(crate) request_timeout: std::time::Duration,
    max_connections: usize,
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
        let collides: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM iam_identity WHERE name = ?)")
                .bind(&settings.api.auth.username)
                .fetch_one(&mut *tx)
                .await?;
        if collides {
            return Err("bootstrap administrator name conflicts with a managed identity".into());
        }
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
        let root_path = effective["storage"]["disk"]["dir"]
            .as_str()
            .ok_or("invalid storage directory")?;
        if !root_path.is_empty() {
            tokio::fs::create_dir_all(root_path).await?;
            let root = tokio::fs::canonicalize(root_path).await?;
            let database = tokio::fs::canonicalize(&environment.db).await?;
            if database.starts_with(&root) {
                return Err("SQLite database must be outside the public storage directory".into());
            }
            if let Some(private_key) = effective["tls"]["key_file"].as_str()
                && !private_key.is_empty()
                && tokio::fs::canonicalize(private_key)
                    .await
                    .is_ok_and(|key| key.starts_with(&root))
            {
                return Err("TLS private key must be outside the public storage directory".into());
            }
        }
        let media = Arc::new(
            Media::open(
                store.0.clone(),
                &effective,
                key.clone(),
                environment.engine.launcher.clone(),
            )
            .await?,
        );
        let auth = Authentication::new(key).await?;
        environment.clear_secrets();
        let disk = std::path::PathBuf::from(root_path);
        let initial = {
            let disk = disk.clone();
            tokio::task::spawn_blocking(move || {
                crate::ops::resources::Sampler::default().sample(&disk, Vec::new())
            })
            .await?
        };
        let budget = crate::engine::Budget::from_host(&effective, &initial)?;
        let settings_file = if effective["cluster"]["enable"] == true {
            if !settings.api.auth.enable || settings.api.auth.disable_localhost {
                return Err("cluster mode requires API authentication for all clients".into());
            }
            let file = environment
                .cluster_file
                .as_deref()
                .ok_or("cluster mode requires CORE_CLUSTER_CONFIG")?;
            Some(crate::cluster::Settings::read(file).await?)
        } else {
            None
        };
        let worker = if settings_file.is_some() {
            let parent = tokio::fs::canonicalize(&environment.db).await?;
            let directory = tempfile::Builder::new()
                .prefix("cluster-worker-")
                .tempdir_in(
                    parent
                        .parent()
                        .ok_or("database must have a parent directory")?,
                )?;
            let store = Store::open(&directory.path().join("worker.sqlite")).await?;
            Some((store, directory))
        } else {
            None
        };
        let gate = settings_file
            .as_ref()
            .map(|_| Arc::new(crate::cluster::work::Gate::default()));
        let processes = Engine::open(
            worker
                .as_ref()
                .map_or_else(|| store.0.clone(), |(store, _)| store.0.clone()),
            Options::from_document(&effective, &environment.engine)?
                .with_media(media.endpoints(listening, false)),
            budget,
            gate.clone(),
        )
        .await?;
        let ops = crate::ops::Operations::new(&effective, initial);
        let cluster = if let (Some(cluster_settings), Some(gate)) = (settings_file, gate) {
            let media_root = tokio::fs::canonicalize(root_path).await?;
            let worker = crate::cluster::diagnostics::Worker {
                core_id: settings.id.clone(),
                media: media.clone(),
                engine: processes.clone(),
                gate: gate.clone(),
            };
            let cluster =
                crate::cluster::Cluster::start(cluster_settings, &store, &media_root, worker)
                    .await?;
            cluster.run_workers(processes.clone(), ops.clone(), media.clone(), gate);
            Some(cluster)
        } else {
            None
        };
        ops.start(&processes, &media, disk);
        ops.emit("application", "ready", "", "");
        Ok(Self {
            cluster,
            worker,
            ops,
            certificates: crate::certificates::Certificates::new(),
            iam: crate::iam::Iam(store.0.clone()),
            store,
            auth,
            media,
            processes,
            environment,
            started_at: now,
            listening: RwLock::new(listening),
            local_address: RwLock::new(listening),
            replacement: None,
            loaded_at: tokio::sync::RwLock::new(timestamp),
            requests: tokio::sync::Semaphore::new(
                effective["api"]["max_requests"].as_u64().unwrap_or(128) as usize,
            ),
            trusted_proxies: settings.api.access.trusted_proxies,
            request_timeout: std::time::Duration::from_secs(
                effective["api"]["timeout_seconds"].as_u64().unwrap_or(15),
            ),
            max_connections: effective["api"]["max_connections"].as_u64().unwrap_or(1024) as usize,
            writes: tokio::sync::Mutex::new(()),
            on_reload: Arc::new(|_| {}),
        })
    }

    /// Run an HTTP listener with this node's configured connection budget.
    pub async fn serve<L>(self: Arc<Self>, listener: L, stop: tokio_util::sync::CancellationToken)
    where
        L: axum::serve::Listener,
        L::Addr: Clone + Send + Sync + 'static,
    {
        crate::http_server::serve_http_with_limits(
            listener,
            crate::router(self.clone()),
            stop,
            self.max_connections,
            Some(self.ops.clone()),
        )
        .await;
    }

    pub async fn bind(&mut self) -> std::io::Result<ReloadableListener> {
        let listener = TcpListener::bind(*self.listening.get_mut()).await?;
        let local = listener.local_addr()?;
        *self.local_address.get_mut() = local;
        self.processes
            .update_media(self.media.endpoints(local, true));
        let (sender, receiver) = mpsc::channel(1);
        self.replacement = Some(sender);
        Ok(ReloadableListener::new(listener, receiver))
    }

    pub(crate) async fn listening_address(&self) -> SocketAddr {
        *self.listening.read().await
    }

    pub(crate) async fn media_address(&self) -> SocketAddr {
        *self.local_address.read().await
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
        *self.local_address.write().await = local;
        tracing::info!(address = %local, "HTTP listener reconfigured");
        Ok(())
    }

    pub(crate) fn effective(&self, record: &Record) -> Result<Value, serde_json::Error> {
        serde_json::from_str(&record.document).map(|document| self.environment.apply(document))
    }

    pub fn capture_logs<S>(&self) -> impl tracing_subscriber::Layer<S>
    where
        S: tracing::Subscriber,
    {
        crate::ops::logs::Capture(self.ops.clone())
    }

    pub async fn log_json(&self) -> Result<bool, StartupError> {
        Ok(self.effective(&self.store.read().await?)?["log"]["format"] == "json")
    }

    pub async fn log_level(&self) -> Result<String, StartupError> {
        let record = self.store.read().await?;
        let settings: Settings = serde_json::from_value(self.effective(&record)?)?;
        Ok(settings.log.level)
    }

    pub fn set_reload_hook(&mut self, hook: impl Fn(&str) + Send + Sync + 'static) {
        self.on_reload = Arc::new(hook);
    }

    pub async fn bind_tls(&self) -> Result<Option<crate::tls::TlsListener>, StartupError> {
        let record = self.store.read().await?;
        let doc = self.effective(&record)?;
        if doc["tls"]["enable"].as_bool() == Some(true)
            && doc["tls"]["address"]
                .as_str()
                .and_then(|address| config::address(address).ok())
                .is_some_and(|address| !address.ip().is_loopback())
            && doc["api"]["auth"]["enable"].as_bool() != Some(true)
            && !self.environment.allow_insecure
        {
            return Err("authentication is required for the external HTTPS listener".into());
        }
        let acme = crate::certificates::Acme::parse(&doc, &self.environment.db)?;
        if self.cluster.is_some() && acme.is_some() {
            return Err(
                "cluster mode requires externally coordinated public TLS certificates".into(),
            );
        }
        if let Some(acme) = &acme {
            let root = doc["storage"]["disk"]["dir"]
                .as_str()
                .ok_or("invalid storage directory")?;
            acme.check_storage(&tokio::fs::canonicalize(root).await?)
                .await?;
        }
        crate::tls::TlsListener::bind(&doc["tls"], acme, self.certificates.clone()).await
    }

    pub fn stop_media_delivery(&self) {
        self.ops.emit("application", "shutdown", "", "");
        self.ops.stop.cancel();
        self.media.stop.cancel();
    }

    pub async fn close(&self) {
        self.ops.stop.cancel();
        if let Some(cluster) = &self.cluster {
            cluster.close().await;
        }
        self.processes.close().await;
        self.media.close().await;
        if let Some((worker, _)) = &self.worker {
            worker.close().await;
        }
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
    if !matches!(document["log"]["format"].as_str(), Some("console" | "json")) {
        return Err("log.format");
    }
    for (field, maximum) in [
        ("max_requests", 4096),
        ("max_connections", 8192),
        ("timeout_seconds", 120),
    ] {
        if document["api"][field]
            .as_u64()
            .is_none_or(|value| value == 0 || value > maximum)
        {
            return Err("api");
        }
    }
    if settings.api.access.trusted_proxies.len() > 64 {
        return Err("api.access.trusted_proxies");
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
    let jwt = &settings.api.auth.jwt;
    if !(30..=86_400).contains(&jwt.access_ttl_sec)
        || !(jwt.access_ttl_sec..=2_592_000).contains(&jwt.refresh_ttl_sec)
    {
        return Err("api.auth.jwt");
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
