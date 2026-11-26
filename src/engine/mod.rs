mod command;
pub(crate) mod executable;
mod model;
mod probe;
mod progress;
mod repository;
mod resources;
mod scheduler;
mod skills;
mod supervisor;

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use serde_json::Value;
use sqlx::SqlitePool;
use tokio::{
    sync::{RwLock, Semaphore, broadcast, watch},
    task::JoinHandle,
};

pub(crate) use command::{Options, sanitize};
pub(crate) use model::*;
pub(crate) use probe::ResultSet;
use progress::lock;
use repository::Repository;
pub(crate) use skills::Inventory;

pub const MAX_JOBS: usize = 128;
pub type Key = (String, String);
pub type Metadata = BTreeMap<String, Value>;
type Graph = RwLock<BTreeMap<Key, Arc<Job>>>;
type LiveOutputs = Mutex<BTreeMap<String, BTreeMap<String, String>>>;

#[derive(Debug)]
pub enum Error {
    Invalid(&'static str),
    Forbidden(&'static str),
    Missing(&'static str),
    Conflict(&'static str),
    Unavailable(&'static str),
    Internal,
}

impl Error {
    pub fn public(&self) -> &'static str {
        match self {
            Self::Invalid(message)
            | Self::Forbidden(message)
            | Self::Missing(message)
            | Self::Conflict(message)
            | Self::Unavailable(message) => message,
            Self::Internal => "process engine internal error",
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.public())
    }
}

impl std::error::Error for Error {}

impl From<sqlx::Error> for Error {
    fn from(error: sqlx::Error) -> Self {
        if error
            .as_database_error()
            .is_some_and(|e| e.is_unique_violation())
        {
            Self::Conflict("process already exists")
        } else {
            Self::Internal
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self::Internal
    }
}

#[derive(Default)]
pub struct Overrides {
    pub launcher: Option<PathBuf>,
    pub cgroup: Option<PathBuf>,
    pub values: BTreeMap<String, String>,
}

#[derive(Clone)]
struct Intent {
    job: Arc<Job>,
    reason: Arc<str>,
    immediate: bool,
    removed: bool,
}

pub struct Entry {
    updates: watch::Sender<Intent>,
    runtime: Arc<Mutex<Runtime>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl Entry {
    pub fn job(&self) -> Arc<Job> {
        self.updates.borrow().job.clone()
    }
    pub fn snapshot(&self) -> Runtime {
        lock(&self.runtime).clone()
    }
}

#[derive(Clone)]
pub struct Event {
    pub process: String,
    pub execution: Execution,
}

pub struct Engine {
    repository: Repository,
    index: RwLock<BTreeMap<Key, Arc<Entry>>>,
    graph: Arc<Graph>,
    live: Arc<LiveOutputs>,
    mutations: tokio::sync::Mutex<()>,
    queued_mutations: Arc<Semaphore>,
    options: watch::Sender<Arc<Options>>,
    shutdown: watch::Sender<bool>,
    slots: Arc<Semaphore>,
    probes: Semaphore,
    events: broadcast::Sender<Event>,
    skills: tokio::sync::Mutex<Option<SkillsCache>>,
}

struct SkillsCache {
    options: Arc<Options>,
    inventory: Arc<Inventory>,
}

impl Engine {
    pub async fn open(pool: SqlitePool, options: Options) -> Result<Arc<Self>, Error> {
        let repository = Repository(pool);
        let jobs = repository.all().await?;
        if jobs.len() > MAX_JOBS {
            return Err(Error::Invalid("stored process count exceeds node capacity"));
        }
        let (options, _) = watch::channel(Arc::new(options));
        let (shutdown, _) = watch::channel(false);
        let (events, mut event_log) = broadcast::channel::<Event>(256);
        let mut stop = shutdown.subscribe();
        // A bounded best-effort consumer keeps lifecycle events out of the command path.
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stop.changed() => return,
                    result = event_log.recv() => match result {
                        Ok(event) => tracing::debug!(process = %event.process, execution = event.execution.wire(), "process execution changed"),
                        Err(broadcast::error::RecvError::Lagged(_)) => {},
                        Err(broadcast::error::RecvError::Closed) => return,
                    }
                }
            }
        });
        let engine = Arc::new(Self {
            repository,
            index: RwLock::new(BTreeMap::new()),
            graph: Arc::new(RwLock::new(BTreeMap::new())),
            live: Arc::new(Mutex::new(BTreeMap::new())),
            mutations: tokio::sync::Mutex::new(()),
            queued_mutations: Arc::new(Semaphore::new(32)),
            options,
            shutdown,
            slots: Arc::new(Semaphore::new(MAX_JOBS)),
            probes: Semaphore::new(4),
            events,
            skills: tokio::sync::Mutex::new(None),
        });
        let mut graph = engine.graph.write().await;
        for job in jobs {
            let key = key(&job.config);
            graph.insert(key, Arc::new(job));
        }
        let mut index = engine.index.write().await;
        for (key, job) in graph.iter() {
            let command = engine
                .options
                .borrow()
                .build(&job.config, &graph, &lock(&engine.live))
                .map_or_else(|_| Vec::new(), |line| line.display);
            index.insert(key.clone(), engine.attach(job.clone(), command));
        }
        drop(index);
        drop(graph);
        Ok(engine)
    }

    fn attach(&self, job: Arc<Job>, command: Vec<String>) -> Arc<Entry> {
        let (updates, receiver) = watch::channel(Intent {
            job,
            reason: "process configuration changed".into(),
            immediate: false,
            removed: false,
        });
        let entry = Arc::new(Entry {
            updates,
            runtime: Arc::new(Mutex::new(Runtime {
                command,
                ..Runtime::default()
            })),
            task: Mutex::new(None),
        });
        let worker = supervisor::Supervisor {
            graph: Arc::downgrade(&self.graph),
            live: Arc::downgrade(&self.live),
            repository: self.repository.clone(),
            options: self.options.subscribe(),
            shutdown: self.shutdown.subscribe(),
            slots: self.slots.clone(),
            events: self.events.clone(),
        };
        *lock(&entry.task) = Some(worker.spawn(&entry, receiver));
        entry
    }

    pub async fn close(&self) {
        {
            let _mutation = self.mutations.lock().await;
            self.shutdown.send_replace(true);
        }
        let entries = self.list().await;
        for entry in entries {
            let task = lock(&entry.task).take();
            if let Some(task) = task {
                let _ = task.await;
            }
        }
    }

    pub fn update_media(&self, endpoints: crate::media::Endpoints) {
        let current = self.options.borrow().clone();
        self.options
            .send_replace(Arc::new((*current).clone().with_media(endpoints)));
    }

    pub fn options(&self, options: Options) {
        self.options.send_replace(Arc::new(options));
    }

    fn queue_mutation(&self) -> Result<tokio::sync::OwnedSemaphorePermit, Error> {
        self.queued_mutations
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Unavailable("process mutation capacity reached"))
    }

    pub async fn get(&self, key: &Key) -> Result<Arc<Entry>, Error> {
        self.index
            .read()
            .await
            .get(key)
            .cloned()
            .ok_or(Error::Missing("unknown process"))
    }

    pub async fn list(&self) -> Vec<Arc<Entry>> {
        self.index.read().await.values().cloned().collect()
    }

    pub async fn edit(
        self: &Arc<Self>,
        key: Option<Key>,
        force: bool,
        change: impl FnOnce(Option<&Config>) -> Result<(Config, Metadata), Error> + Send + 'static,
    ) -> Result<Config, Error> {
        let queued = self.queue_mutation()?;
        let engine = self.clone();
        // Cancellation of the caller must not split SQLite commit and publication to the supervisor.
        tokio::spawn(async move {
            let _queued = queued;
            let _mutation = engine.mutations.lock().await;
            if *engine.shutdown.borrow() {
                return Err(Error::Unavailable("process engine is shutting down"));
            }
            let existing = match &key {
                Some(key) => Some(engine.get(key).await?),
                None => None,
            };
            let previous = existing.as_ref().map(|entry| entry.job());
            let (config, metadata) = change(previous.as_deref().map(|job| &job.config))?;
            let new_key = self::key(&config);
            let options = engine.options.borrow().clone();
            let graph = engine.graph.read().await;
            if key.as_ref() != Some(&new_key) && graph.contains_key(&new_key) {
                return Err(Error::Conflict("process already exists"));
            }
            if previous.is_none() && graph.len() >= MAX_JOBS {
                return Err(Error::Unavailable("node process capacity reached"));
            }
            let mut proposed = graph.clone();
            drop(graph);
            if let Some(key) = &key {
                proposed.remove(key);
            }
            let now = unix_now();
            let job = Arc::new(Job {
                uid: previous
                    .as_ref()
                    .map_or_else(|| uuid::Uuid::new_v4().to_string(), |p| p.uid.clone()),
                desired: previous.as_ref().map_or(config.autostart, |p| p.desired),
                created: previous.as_ref().map_or(now, |p| p.created),
                updated: now,
                config,
            });
            let command = options
                .build(&job.config, &proposed, &lock(&engine.live))?
                .display;
            proposed.insert(new_key.clone(), job.clone());
            // Revalidate dependencies when an endpoint, ID or domain changes.
            if let Some(old) = &previous
                && (old.config.id != job.config.id
                    || old.config.domain != job.config.domain
                    || old.config.outputs != job.config.outputs)
            {
                for other in proposed.values().filter(|other| other.uid != job.uid) {
                    command::validate_references(&other.config, &proposed)?;
                }
            }
            if previous
                .as_ref()
                .is_some_and(|old| old.config == job.config)
                && !force
            {
                engine
                    .repository
                    .save(previous.as_ref().ok_or(Error::Internal)?, metadata)
                    .await?;
                return Ok(job.config.clone());
            }
            engine.repository.save(&job, metadata).await?;
            {
                let mut graph = engine.graph.write().await;
                if previous
                    .as_ref()
                    .is_some_and(|old| old.config != job.config)
                {
                    lock(&engine.live).remove(&job.uid);
                }
                *graph = proposed;
            }
            let mut index = engine.index.write().await;
            if let (Some(previous_key), Some(entry)) = (key, existing) {
                index.remove(&previous_key);
                if !job.desired {
                    lock(&entry.runtime).command = command;
                }
                entry.updates.send_replace(Intent {
                    job: job.clone(),
                    reason: "process configuration changed".into(),
                    immediate: false,
                    removed: false,
                });
                index.insert(new_key, entry);
            } else {
                index.insert(new_key, engine.attach(job.clone(), command));
            }
            Ok(job.config.clone())
        })
        .await
        .map_err(|_| Error::Internal)?
    }

    pub async fn command(
        self: &Arc<Self>,
        key: Key,
        command: Control,
        reason: String,
    ) -> Result<(), Error> {
        let queued = self.queue_mutation()?;
        let engine = self.clone();
        tokio::spawn(async move {
            let _queued = queued;
            let _mutation = engine.mutations.lock().await;
            if *engine.shutdown.borrow() {
                return Err(Error::Unavailable("process engine is shutting down"));
            }
            let entry = engine.get(&key).await?;
            let old = entry.job();
            let mut job = (*old).clone();
            match command {
                Control::Start => {
                    let state = lock(&entry.runtime);
                    if job.desired
                        && (state.reconnect.is_some()
                            || matches!(state.execution, Execution::Starting | Execution::Running))
                    {
                        return Ok(());
                    }
                    job.desired = true;
                }
                Control::Stop => {
                    if !job.desired {
                        return Ok(());
                    }
                    job.desired = false;
                }
                Control::Restart | Control::Reload => {
                    if !job.desired {
                        return Ok(());
                    }
                }
            }
            let job = Arc::new(job);
            if job.desired != old.desired {
                engine.repository.save(&job, Metadata::new()).await?;
            }
            if matches!(command, Control::Start) {
                lock(&entry.runtime).execution = Execution::Starting;
            }
            let default_reason = match command {
                Control::Stop => "process stopped by request",
                Control::Reload => "process reloaded by request",
                Control::Start | Control::Restart => "process restarted by request",
            };
            engine.graph.write().await.insert(key, job.clone());
            entry.updates.send_replace(Intent {
                job: job.clone(),
                reason: if reason.trim().is_empty() {
                    default_reason.into()
                } else {
                    sanitize(&reason).into()
                },
                immediate: matches!(command, Control::Restart | Control::Reload),
                removed: false,
            });
            Ok(())
        })
        .await
        .map_err(|_| Error::Internal)?
    }

    pub async fn delete(self: &Arc<Self>, key: Key, purge: bool) -> Result<(), Error> {
        let queued = self.queue_mutation()?;
        let engine = self.clone();
        tokio::spawn(async move {
            let _queued = queued;
            let _mutation = engine.mutations.lock().await;
            if *engine.shutdown.borrow() {
                return Err(Error::Unavailable("process engine is shutting down"));
            }
            let entry = engine.get(&key).await?;
            let mut job = (*entry.job()).clone();
            job.desired = false;
            engine.repository.save(&job, Metadata::new()).await?;
            let job = Arc::new(job);
            entry.updates.send_replace(Intent {
                job: job.clone(),
                reason: "process deleted".into(),
                immediate: false,
                removed: true,
            });
            let task = lock(&entry.task).take();
            if let Some(task) = task {
                let _ = task.await;
            }
            if purge {
                let options = engine.options.borrow().clone();
                if tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    command::clean_outputs(&options, &job.config, true, &engine.graph),
                )
                .await
                .is_err()
                {
                    tracing::warn!("process output purge reached its time budget");
                }
            }
            engine.repository.delete(&job.uid).await?;
            engine.index.write().await.remove(&key);
            engine.graph.write().await.remove(&key);
            lock(&engine.live).remove(&job.uid);
            Ok(())
        })
        .await
        .map_err(|_| Error::Internal)?
    }

    pub async fn validate(&self, config: &Config) -> Result<(), Error> {
        let options = self.options.borrow().clone();
        let graph = self.graph.read().await;
        options.build(config, &graph, &lock(&self.live))?;
        Ok(())
    }

    pub async fn probe(&self, config: &Config) -> Result<ResultSet, Error> {
        let _permit = self
            .probes
            .try_acquire()
            .map_err(|_| Error::Unavailable("probe capacity reached"))?;
        let options = self.options.borrow().clone();
        let mut config = config.clone();
        config.outputs = vec![Endpoint {
            id: "probe".into(),
            address: "-".into(),
            options: vec!["-f".into(), "null".into()],
            cleanup: Vec::new(),
        }];
        let mut options = (*options).clone();
        options.for_probe();
        let command = {
            let graph = self.graph.read().await;
            options.build(&config, &graph, &lock(&self.live))?
        };
        probe::inspect(&options, command).await
    }

    pub async fn skills(&self, reload: bool) -> Result<Arc<Inventory>, Error> {
        let mut cache = self.skills.lock().await;
        let options = self.options.borrow().clone();
        if !reload
            && let Some(cache) = &*cache
            && Arc::ptr_eq(&cache.options, &options)
        {
            return Ok(cache.inventory.clone());
        }
        let fresh = Arc::new(skills::inspect(&options).await?);
        *cache = Some(SkillsCache {
            options,
            inventory: fresh.clone(),
        });
        Ok(fresh)
    }

    pub async fn metadata(&self, uid: Option<&str>) -> Result<Metadata, Error> {
        self.repository.metadata(uid).await
    }

    pub async fn set_metadata(
        &self,
        uid: Option<&str>,
        key: &str,
        value: &Value,
    ) -> Result<(), Error> {
        self.repository.set_metadata(uid, key, value).await
    }

    pub async fn history(&self, uid: &str) -> Result<Vec<Run>, Error> {
        self.repository.history(uid).await
    }

    pub async fn set_history(&self, uid: &str, history: &[Run]) -> Result<(), Error> {
        let max = self.options.borrow().history as usize;
        self.repository
            .replace_history(uid, &history[..history.len().min(max)])
            .await
    }

    pub fn diagnostics(&self) -> (usize, bool) {
        let options = self.options.borrow();
        (options.max_children, options.cgroup.is_some())
    }
}

#[derive(Clone, Copy)]
pub enum Control {
    Start,
    Stop,
    Restart,
    Reload,
}

fn key(config: &Config) -> Key {
    (config.domain.clone(), config.id.clone())
}
fn unix_now() -> i64 {
    jiff::Timestamp::now().as_second()
}
