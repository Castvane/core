use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, SystemTime},
};

use axum::{
    Extension, Json, Router,
    extract::{Query as AxumQuery, State, rejection::JsonRejection},
    http::Uri,
    routing::post,
};
use futures_util::{StreamExt, stream};
use openraft::rt::WatchReceiver;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Cluster, Failure, Raft, work::Gate};
use crate::{
    api::{ApiError, process, process_dto as dto},
    auth::Identity,
    engine::Engine,
    media::{Media, storage::now_secs},
};

const MAX_DIAGNOSTIC: usize = 1_048_576;

#[derive(Clone)]
pub struct Worker {
    pub core_id: String,
    pub engine: Arc<Engine>,
    pub media: Arc<Media>,
    pub gate: Arc<Gate>,
}

#[derive(Clone, Deserialize, Serialize)]
pub enum Query {
    Version,
    Files,
    Processes {
        query: Option<String>,
    },
    StorageFiles {
        storage: String,
        query: Option<String>,
    },
    Report {
        domain: String,
        id: String,
        epoch: u64,
    },
    Probe {
        domain: String,
        id: String,
        epoch: u64,
    },
    ProbeConfiguration(Value),
}

type Context = State<(Raft, Worker)>;

pub fn routes(raft: Raft, worker: Worker) -> Router {
    Router::new()
        .route("/diagnostics", post(run))
        .with_state((raft, worker))
}

async fn run(
    State((raft, worker)): Context,
    Extension(peer): Extension<u64>,
    request: Result<Json<Query>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    if raft.current_leader().await != Some(peer) {
        return Err(ApiError::forbidden("the current leader is required"));
    }
    let Json(query) = request.map_err(|_| Failure::Invalid)?;
    let value = match query {
        Query::Version => serde_json::to_value(crate::api::dto::Version::full())?,
        Query::Files => node_files(&worker.media).await?,
        Query::Processes { query } => {
            let AxumQuery(params) = AxumQuery::<process::Params>::try_from_uri(&query_uri(query)?)
                .map_err(|_| Failure::Invalid)?;
            let Json(processes) = process::process_list_from_engine(
                &worker.engine,
                &worker.core_id,
                &Identity::Administrator,
                params,
                MAX_DIAGNOSTIC,
            )
            .await?;
            processes
        }
        Query::StorageFiles { storage, query } => {
            let uri = query_uri(query)?;
            let files = crate::api::media::select(worker.media.get(&storage)?, &uri, false).await?;
            let mut budget = LimitedListing::new();
            let mut listing = Vec::new();
            for file in files {
                let file = FileInfo {
                    name: format!("/{}", file.key.as_str()),
                    size_bytes: file.size,
                    last_modified: now_secs(file.modified),
                };
                budget.add(&file.name)?;
                listing.push(file);
            }
            serde_json::to_value(listing)?
        }
        Query::Report { domain, id, epoch } => {
            let key = (domain, id);
            if !worker.gate.owns(&key, epoch) {
                return Err(Failure::Conflict.into());
            }
            let entry = worker.engine.get(&key).await?;
            let job = entry.job();
            dto::report(
                &entry.snapshot().active,
                &worker.engine.history(&job.uid).await?,
                &job.config.limits,
            )
        }
        Query::Probe { domain, id, epoch } => {
            let key = (domain, id);
            if !worker.gate.owns(&key, epoch) {
                return Err(Failure::Conflict.into());
            }
            let entry = worker.engine.get(&key).await?;
            dto::probe(worker.engine.probe(&entry.job().config).await?)
        }
        Query::ProbeConfiguration(value) => {
            let cfg = dto::Configuration::from_patch(None, value, "$cluster", false)?
                .into_config()
                .0;
            dto::probe(worker.engine.probe(&cfg).await?)
        }
    };
    Ok(Json(value))
}

fn query_uri(query: Option<String>) -> Result<Uri, Failure> {
    query
        .map_or(Ok(Uri::default()), |query| format!("/?{query}").parse())
        .map_err(|_| Failure::Invalid)
}

#[derive(Deserialize, Serialize)]
pub struct FileInfo {
    pub name: String,
    pub size_bytes: u64,
    pub last_modified: u64,
}

struct LimitedListing(usize);

impl LimitedListing {
    fn new() -> Self {
        Self(0)
    }

    fn add(&mut self, name: &str) -> Result<(), Failure> {
        // Allow for JSON escaping, field names, and two maximum-size numeric fields.
        self.0 += name.len() * 2 + 84;
        if self.0 > MAX_DIAGNOSTIC - 4096 {
            return Err(Failure::Full);
        }
        Ok(())
    }
}

async fn node_files(media: &Media) -> Result<Value, ApiError> {
    let mut budget = LimitedListing::new();
    let mut mounts = BTreeMap::new();
    for mount in media.mounts.values() {
        let files = mount.backend.list().await;
        let files = match files {
            Ok(files) => files,
            Err(_) if !mount.required => continue,
            Err(error) => return Err(error.into()),
        };
        let mut names = Vec::with_capacity(files.len().min(4096));
        for file in files {
            budget.add(file.key.as_str())?;
            names.push(format!("/{}", file.key.as_str()));
        }
        if !names.is_empty() {
            names.sort_unstable();
            mounts.insert(&mount.name, names);
        }
    }
    Ok(serde_json::json!({"last_update": now_secs(SystemTime::now()), "files": mounts}))
}

impl Cluster {
    pub async fn storage_files(
        &self,
        storage: String,
        query: Option<String>,
    ) -> Result<(Vec<FileInfo>, usize), Failure> {
        let _permit = self
            .file_listings
            .try_acquire()
            .map_err(|_| Failure::Unavailable)?;
        self.quorum().await?;
        let receiver = self.raft.metrics();
        let members: Vec<_> = receiver
            .borrow_watched()
            .membership_config
            .nodes()
            .map(|(id, _)| *id)
            .collect();
        let request = Query::StorageFiles {
            storage,
            query: query.clone(),
        };
        let mut responses = stream::iter(
            members
                .into_iter()
                .map(|id| self.peer_diagnostics(id, request.clone())),
        )
        .buffer_unordered(8);
        let mut budget = LimitedListing::new();
        let mut files = Vec::<FileInfo>::new();
        let mut unavailable = 0;
        while let Some(result) = responses.next().await {
            match result {
                Ok(value) => {
                    let peer_files: Vec<FileInfo> =
                        serde_json::from_value(value).map_err(|_| Failure::Unavailable)?;
                    for file in peer_files {
                        budget.add(&file.name)?;
                        files.push(file);
                    }
                }
                Err(Failure::Missing) => (),
                Err(Failure::Unavailable) => unavailable += 1,
                Err(error) => return Err(error),
            }
        }
        let query = query.unwrap_or_default();
        let mut params = url::form_urlencoded::parse(query.as_bytes());
        let sort = params.clone().find(|(key, _)| key == "sort");
        let descending = params.any(|(key, value)| key == "order" && value == "desc");
        match sort.as_ref().map(|(_, value)| value.as_ref()) {
            Some("name") => files.sort_unstable_by(|a, b| a.name.cmp(&b.name)),
            Some("size") => files.sort_unstable_by_key(|file| file.size_bytes),
            _ => files.sort_unstable_by_key(|file| file.last_modified),
        }
        if descending {
            files.reverse();
        }
        Ok((files, unavailable))
    }

    pub async fn diagnostics(&self, id: u64, query: Query) -> Result<Value, Failure> {
        self.quorum().await?;
        if !self
            .raft
            .metrics()
            .borrow_watched()
            .membership_config
            .nodes()
            .any(|(node, _)| *node == id)
        {
            return Err(Failure::Missing);
        }
        self.peer_diagnostics(id, query).await
    }

    async fn peer_diagnostics(&self, id: u64, query: Query) -> Result<Value, Failure> {
        let peer = self.peers.get(&id).ok_or(Failure::Missing)?;
        let mut response = self
            .network
            .client
            .post(format!("{}/internal/cluster/v1/diagnostics", peer.address))
            .header("x-castvane-cluster", self.name.as_str())
            .timeout(Duration::from_secs(4))
            .json(&query)
            .send()
            .await
            .map_err(|_| Failure::Unavailable)?;
        match response.status().as_u16() {
            200 => (),
            400 => return Err(Failure::Invalid),
            404 => return Err(Failure::Missing),
            409 => return Err(Failure::Conflict),
            413 | 507 => return Err(Failure::Full),
            _ => return Err(Failure::Unavailable),
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Failure::Unavailable)? {
            if bytes.len() + chunk.len() > MAX_DIAGNOSTIC {
                return Err(Failure::Full);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| Failure::Unavailable)
    }
}
