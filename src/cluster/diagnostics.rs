use std::{sync::Arc, time::Duration};

use axum::{
    Extension, Json, Router,
    extract::{State, rejection::JsonRejection},
    routing::post,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Cluster, Failure, Raft, work::Gate};
use crate::{
    api::{ApiError, process_dto as dto},
    engine::Engine,
    media::Media,
};

#[derive(Clone)]
pub struct Worker {
    pub engine: Arc<Engine>,
    pub media: Arc<Media>,
    pub gate: Arc<Gate>,
}

#[derive(Deserialize, Serialize)]
pub enum Query {
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

impl Cluster {
    pub async fn diagnostics(&self, id: u64, query: Query) -> Result<Value, Failure> {
        self.quorum().await?;
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
            _ => return Err(Failure::Unavailable),
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Failure::Unavailable)? {
            if bytes.len() + chunk.len() > 1_048_576 {
                return Err(Failure::Full);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| Failure::Unavailable)
    }
}
