pub(crate) mod diagnostics;
mod files;
pub mod model;
mod network;
mod storage;
pub(crate) mod work;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use openraft::{
    ServerState,
    alias::LogIdOf,
    impls::BasicNode,
    raft::ReadPolicy,
    rt::{Instant as _, WatchReceiver},
};
use rustls::pki_types::{CertificateDer, pem::PemObject};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Semaphore};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::{StartupError, persistence::Store};
use model::{Action, Applied, Collection, Command, Data, Node, Role, valid_key};
use network::Network;
use storage::SqliteStore;

openraft::declare_raft_types!(pub Types: D = Command, R = Applied);
pub type Raft = openraft::Raft<Types, SqliteStore>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub cluster: String,
    pub id: u64,
    pub bind: std::net::SocketAddr,
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    pub ca: PathBuf,
    pub peers: BTreeMap<u64, PeerConfig>,
    #[serde(default)]
    pub bootstrap: BTreeSet<u64>,
    #[serde(default = "lease_default")]
    pub lease_ms: u64,
    #[serde(default)]
    pub rebalance: Option<Rebalance>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rebalance {
    pub above_percent: u8,
    pub below_percent: u8,
    pub cooldown_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerConfig {
    address: String,
    playback_origin: Option<String>,
    certificate: PathBuf,
    #[serde(default)]
    node: Node,
}

pub struct Peer {
    pub address: String,
    pub playback_origin: Option<String>,
    pub certificate: CertificateDer<'static>,
    pub node: Node,
}

pub struct Cluster {
    pub id: u64,
    pub name: String,
    pub raft: Raft,
    pub peers: Arc<BTreeMap<u64, Peer>>,
    store: SqliteStore,
    stop: CancellationToken,
    network: Network,
    telemetry: work::SharedTelemetry,
    file_listings: Semaphore,
    lease_ms: u64,
    rebalance: Option<Rebalance>,
}

async fn bounded_file(path: &Path) -> Result<Vec<u8>, StartupError> {
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(path).await?;
    let mut bytes = Vec::new();
    file.take(262_145).read_to_end(&mut bytes).await?;
    if bytes.len() > 262_144 {
        return Err("cluster configuration file exceeds 256 KiB".into());
    }
    Ok(bytes)
}

impl Settings {
    pub async fn read(path: &Path) -> Result<Self, StartupError> {
        let settings: Self = serde_json::from_slice(&bounded_file(path).await?)?;
        if !valid_key(&settings.cluster)
            || !(8000..=30000).contains(&settings.lease_ms)
            || settings.rebalance.as_ref().is_some_and(|rule| {
                rule.above_percent > 100
                    || rule.below_percent >= rule.above_percent
                    || !(30_000..=3_600_000).contains(&rule.cooldown_ms)
            })
            || settings.id == 0
            || settings.peers.contains_key(&0)
            || !settings.peers.contains_key(&settings.id)
            || settings.peers.len() > 32
            || !settings.bootstrap.is_empty()
                && (!settings.bootstrap.contains(&settings.id)
                    || settings.bootstrap.iter().any(|id| {
                        settings
                            .peers
                            .get(id)
                            .is_none_or(|peer| peer.node.role == Role::Worker)
                    }))
        {
            return Err("invalid cluster identity, peer list, or bootstrap membership".into());
        }
        Ok(settings)
    }
}

impl Cluster {
    pub async fn start(
        settings: Settings,
        store: &Store,
        media_root: &Path,
        worker: diagnostics::Worker,
    ) -> Result<Arc<Self>, StartupError> {
        let database = SqliteStore(store.0.clone());
        let key_path = tokio::fs::canonicalize(&settings.private_key).await?;
        #[cfg(unix)]
        let metadata = tokio::fs::metadata(&key_path).await?;
        #[cfg(windows)]
        let metadata = tokio::fs::symlink_metadata(&settings.private_key).await?;
        if !metadata.is_file()
            || !crate::platform::private_metadata(&metadata)
            || key_path.starts_with(media_root)
        {
            return Err(
                "cluster private key must be a private regular file outside public media".into(),
            );
        }
        let cert = bounded_file(&settings.certificate).await?;
        let key = Zeroizing::new(bounded_file(&key_path).await?);
        let ca = bounded_file(&settings.ca).await?;
        let local = CertificateDer::from_pem_slice(&cert)?;
        let mut identity = Zeroizing::new(cert.clone());
        identity.push(b'\n');
        identity.extend_from_slice(&key);
        let (tls, client) = network::tls(&ca, &cert, &key, &identity)?;
        let mut peers = BTreeMap::new();
        let mut origins = BTreeSet::new();
        let mut certificates = BTreeSet::new();
        for (id, peer) in settings.peers {
            let address = url::Url::parse(&peer.address)?;
            if address.scheme() != "https"
                || address.port().is_none()
                || address.path() != "/"
                || address.host().is_none()
                || !address.username().is_empty()
                || address.password().is_some()
                || address.query().is_some()
                || address.fragment().is_some()
                || !origins.insert(address.clone())
            {
                return Err(
                    "cluster peer origins must be unique HTTPS addresses with explicit ports"
                        .into(),
                );
            }
            let certificate =
                CertificateDer::from_pem_slice(&bounded_file(&peer.certificate).await?)?;
            if !certificates.insert(certificate.as_ref().to_vec())
                || id == settings.id && certificate != local
            {
                return Err(
                    "cluster node certificates must be unique and the local identity must match"
                        .into(),
                );
            }
            let playback_origin = peer
                .playback_origin
                .as_deref()
                .map(url::Url::parse)
                .transpose()?;
            if playback_origin.as_ref().is_some_and(|origin| {
                origin.scheme() != "https"
                    || origin.host().is_none()
                    || origin.path() != "/"
                    || !origin.username().is_empty()
                    || origin.password().is_some()
                    || origin.query().is_some()
                    || origin.fragment().is_some()
            }) {
                return Err("cluster playback origin must be an HTTPS origin".into());
            }
            peers.insert(
                id,
                Peer {
                    address: address.to_string().trim_end_matches('/').into(),
                    playback_origin: playback_origin
                        .map(|origin| origin.origin().ascii_serialization()),
                    certificate,
                    node: peer.node,
                },
            );
        }
        let identity = serde_json::to_vec(&(settings.id, &settings.cluster))?;
        let mut tx = store.write().await?;
        sqlx::query("INSERT OR IGNORE INTO cluster_raft_state(key,data) VALUES('identity',?)")
            .bind(&identity)
            .execute(&mut *tx)
            .await?;
        let persisted: Vec<u8> =
            sqlx::query_scalar("SELECT data FROM cluster_raft_state WHERE key='identity'")
                .fetch_one(&mut *tx)
                .await?;
        if persisted != identity {
            return Err("database belongs to another cluster node".into());
        }
        tx.commit().await?;
        let listener = TcpListener::bind(settings.bind).await?;
        let peers = Arc::new(peers);
        let network = Network {
            client,
            peers: peers.clone(),
            cluster: Arc::from(settings.cluster.as_str()),
        };
        let config = openraft::Config {
            cluster_name: settings.cluster.clone(),
            election_timeout_min: 800,
            election_timeout_max: 1600,
            heartbeat_interval: 200,
            install_snapshot_timeout: 3000,
            max_payload_entries: 8,
            snapshot_max_chunk_size: 64 * 1024,
            snapshot_policy: openraft::SnapshotPolicy::LogsSinceLast(64),
            replication_lag_threshold: 128,
            max_in_snapshot_log_to_keep: 4,
            ..Default::default()
        }
        .validate()?;
        let raft = Raft::new(
            settings.id,
            Arc::new(config),
            network.clone(),
            database.clone(),
            database.clone(),
        )
        .await?;
        if !settings.bootstrap.is_empty() && !raft.is_initialized().await? {
            let nodes: BTreeMap<u64, BasicNode> = settings
                .bootstrap
                .into_iter()
                .filter_map(|id| {
                    peers
                        .get(&id)
                        .map(|peer| (id, BasicNode::new(peer.address.clone())))
                })
                .collect();
            raft.initialize(nodes).await?;
        }
        let stop = CancellationToken::new();
        let telemetry = Arc::new(tokio::sync::RwLock::new(None));
        tokio::spawn(network::serve(
            listener,
            tls,
            network.clone(),
            raft.clone(),
            stop.clone(),
            telemetry.clone(),
            worker,
        ));
        Ok(Arc::new(Self {
            id: settings.id,
            name: settings.cluster,
            raft,
            peers,
            store: database,
            stop,
            network,
            telemetry,
            file_listings: Semaphore::new(4),
            lease_ms: settings.lease_ms,
            rebalance: settings.rebalance,
        }))
    }

    pub fn readiness(&self) -> (bool, bool) {
        let metrics = self.raft.metrics();
        let metrics = metrics.borrow_watched();
        let joined = metrics.current_leader.is_some()
            && metrics
                .membership_config
                .membership()
                .get_node(&self.id)
                .is_some();
        let runs_media = self
            .peers
            .get(&self.id)
            .is_some_and(|node| node.node.role != Role::Control);
        (joined, runs_media)
    }

    pub async fn close(&self) {
        self.stop.cancel();
        let _ = self.raft.shutdown().await;
    }

    pub async fn committed(&self) -> Result<Data, Failure> {
        self.quorum().await?;
        self.local().await
    }

    pub async fn local(&self) -> Result<Data, Failure> {
        Ok(self
            .store
            .machine()
            .await
            .map_err(|_| Failure::Unavailable)?
            .data)
    }

    pub async fn identity(&self, name: &str) -> Result<Option<model::ClusterAccount>, Failure> {
        let state = self.authenticated_state().await?;
        state
            .get(Collection::Identity, name)
            .map(|record| {
                serde_json::from_value(record.value.clone()).map_err(|_| Failure::Unavailable)
            })
            .transpose()
    }

    pub async fn authenticated_state(&self) -> Result<Data, Failure> {
        if self.raft.current_leader().await == Some(self.id) {
            return self.committed().await;
        }
        let leader = self
            .raft
            .current_leader()
            .await
            .ok_or(Failure::Unavailable)?;
        let peer = self.peers.get(&leader).ok_or(Failure::Unavailable)?;
        let mut response = self
            .network
            .client
            .post(format!("{}/internal/cluster/v1/read-fence", peer.address))
            .header("x-castvane-cluster", self.name.as_str())
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .map_err(|_| Failure::Unavailable)?
            .error_for_status()
            .map_err(|_| Failure::Unavailable)?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Failure::Unavailable)? {
            if bytes.len() + chunk.len() > 4096 {
                return Err(Failure::Unavailable);
            }
            bytes.extend_from_slice(&chunk);
        }
        let fence: Option<LogIdOf<Types>> =
            serde_json::from_slice(&bytes).map_err(|_| Failure::Unavailable)?;
        let machine = self
            .store
            .machine()
            .await
            .map_err(|_| Failure::Unavailable)?;
        if machine.last.map(|entry| entry.index) < fence.map(|entry| entry.index) {
            return Err(Failure::Unavailable);
        }
        Ok(machine.data)
    }

    pub async fn quorum(&self) -> Result<(), Failure> {
        tokio::time::timeout(
            Duration::from_secs(3),
            self.raft.ensure_linearizable(ReadPolicy::ReadIndex),
        )
        .await
        .map_err(|_| Failure::Unavailable)?
        .map_err(|_| Failure::Unavailable)?;
        Ok(())
    }

    pub async fn write(&self, action: Action) -> Result<u64, Failure> {
        let reply = tokio::time::timeout(
            Duration::from_secs(4),
            self.raft.client_write(Command { version: 1, action }),
        )
        .await
        .map_err(|_| Failure::Uncertain)?
        .map_err(|_| Failure::Unavailable)?;
        match reply.data {
            Applied::Updated(index) => Ok(index),
            Applied::Missing => Err(Failure::Missing),
            Applied::Conflict => Err(Failure::Conflict),
            Applied::Invalid => Err(Failure::Invalid),
            Applied::Full => Err(Failure::Full),
        }
    }

    pub fn about(&self) -> Value {
        let receiver = self.raft.metrics();
        let metrics = receiver.borrow_watched();
        let leader = metrics.current_leader;
        let last_contact = metrics
            .last_quorum_acked
            .map(|ack| ack.into_inner().elapsed().as_millis() as u64);
        let state = if metrics.running_state.is_err() || leader.is_none() {
            "unavailable"
        } else if metrics.state != ServerState::Leader {
            "read-only"
        } else if last_contact.is_some_and(|elapsed| elapsed > 1600) {
            "degraded"
        } else {
            "healthy"
        };
        let voters: BTreeSet<_> = metrics.membership_config.voter_ids().collect();
        let nodes:Vec<_> = metrics.membership_config.nodes().map(|(id,node)| json!({
            "id": id.to_string(),"leader":leader.as_ref() == Some(id),"address":node.addr,
            "voter":voters.contains(id),"name":self.peers.get(id).map(|peer| &peer.node.name)
        })).collect();
        json!({
            "id":self.name, "status":state,"degraded":state=="degraded","domains":[],
            "leader":{"id":leader.map(|id|id.to_string()).unwrap_or_default(),
                "address":leader.and_then(|id|self.peers.get(&id)).map(|peer|&peer.address),"elected_since":0},
            "raft":{"address":self.peers.get(&self.id).map(|peer| &peer.address),
                "state":metrics.state,"num_peers":voters.len(),"last_contact":last_contact,
                "log_term":metrics.current_term,"log_index":metrics.last_log_index,"snapshot_index":metrics.snapshot.map(|log|log.index)},
            "nodes":nodes,"version":env!("CARGO_PKG_VERSION")
        })
    }

    pub fn prometheus(&self) -> String {
        let receiver = self.raft.metrics();
        let metrics = receiver.borrow_watched();
        format!(
            "# TYPE castvane_cluster_is_leader gauge\ncastvane_cluster_is_leader {}\n# TYPE castvane_cluster_voters gauge\ncastvane_cluster_voters {}\n# TYPE castvane_cluster_log_index gauge\ncastvane_cluster_log_index {}\n# TYPE castvane_cluster_applied_index gauge\ncastvane_cluster_applied_index {}\n",
            u8::from(metrics.state == ServerState::Leader),
            metrics.membership_config.voter_ids().count(),
            metrics.last_log_index.unwrap_or(0),
            metrics.last_applied.map_or(0, |log| log.index),
        )
    }

    pub async fn enroll(&self, id: u64) -> Result<(), Failure> {
        let peer = self.peers.get(&id).ok_or(Failure::Invalid)?;
        self.quorum().await?;
        tokio::time::timeout(
            Duration::from_secs(4),
            self.raft
                .add_learner(id, BasicNode::new(peer.address.clone()), false),
        )
        .await
        .map_err(|_| Failure::Uncertain)?
        .map_err(|_| Failure::Unavailable)?;
        if self
            .local()
            .await?
            .get(Collection::Node, &id.to_string())
            .is_some()
        {
            return Ok(());
        }
        self.write(Action::Put {
            collection: Collection::Node,
            key: id.to_string(),
            value: json!(peer.node),
            expected: Some(0),
        })
        .await?;
        Ok(())
    }

    pub async fn voters(&self, ids: BTreeSet<u64>) -> Result<(), Failure> {
        if ids.is_empty()
            || ids.iter().any(|id| {
                self.peers
                    .get(id)
                    .is_none_or(|peer| peer.node.role == Role::Worker)
            })
        {
            return Err(Failure::Invalid);
        }
        self.quorum().await?;
        tokio::time::timeout(
            Duration::from_secs(8),
            self.raft.change_membership(ids, false),
        )
        .await
        .map_err(|_| Failure::Uncertain)?
        .map_err(|_| Failure::Unavailable)?;
        Ok(())
    }

    pub async fn transfer_leadership(&self, target: u64) -> Result<(), Failure> {
        self.quorum().await?;
        if !self.peers.contains_key(&target) {
            return Err(Failure::Missing);
        }
        let voter = self
            .raft
            .metrics()
            .borrow_watched()
            .membership_config
            .voter_ids()
            .any(|id| id == target);
        if !voter {
            return Err(Failure::Invalid);
        }
        if target == self.id {
            return Ok(());
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            self.raft
                .trigger()
                .transfer_leader(target)
                .await
                .map_err(|_| Failure::Unavailable)?;
            self.raft
                .wait(None)
                .current_leader(target, "leadership transfer")
                .await
                .map_err(|_| Failure::Unavailable)?;
            Ok(())
        })
        .await
        .map_err(|_| Failure::Uncertain)?
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Failure {
    Invalid,
    Missing,
    Conflict,
    Full,
    Unavailable,
    Uncertain,
}

fn lease_default() -> u64 {
    15_000
}
