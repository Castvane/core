use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    api::process_dto::Configuration,
    engine::{Config, Control, Engine, Key},
    media::Media,
    ops::Operations,
};

use super::{
    Cluster,
    model::{Action, Capacity, Collection, Data, Job, Node, Observation, Requirements, eligible},
    network::Network,
};

#[derive(Default)]
pub struct Gate(RwLock<BTreeMap<Key, (Instant, u64)>>);

impl Gate {
    pub fn allows(&self, config: &Config) -> bool {
        self.0.read().ok().is_some_and(|leases| {
            leases.iter().any(|((domain, id), (expiry, _))| {
                domain == &config.domain && id == &config.id && *expiry > Instant::now()
            })
        })
    }

    pub fn owns(&self, key: &Key, epoch: u64) -> bool {
        self.0.read().ok().is_some_and(|leases| {
            leases
                .get(key)
                .is_some_and(|(expiry, owned)| *owned == epoch && *expiry > Instant::now())
        })
    }

    fn replace(&self, leases: BTreeMap<Key, (Instant, u64)>) {
        if let Ok(mut current) = self.0.write() {
            *current = leases;
        }
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Telemetry {
    pub observation: Observation,
    pub observed_ms: u64,
    pub runtime: BTreeMap<String, Runtime>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Runtime {
    pub state: String,
    pub epoch: u64,
    pub frame: u64,
}

struct Deployment {
    key: Key,
    epoch: u64,
    config: Value,
}

pub type SharedTelemetry = Arc<tokio::sync::RwLock<Option<Telemetry>>>;

pub fn now_ms() -> u64 {
    u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

impl Cluster {
    pub fn run_workers(
        self: &Arc<Self>,
        engine: Arc<Engine>,
        ops: Arc<Operations>,
        media: Arc<Media>,
        gate: Arc<Gate>,
    ) {
        let node = self.clone();
        tokio::spawn(async move {
            let mut active = BTreeMap::<String, Deployment>::new();
            let mut tick = tokio::time::interval(Duration::from_millis(500));
            let mut high_water = 0;
            let mut aliases = None;
            let mut zones = BTreeMap::new();
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = node.stop.cancelled() => break,
                    _ = tick.tick() => {
                        let now = now_ms();
                        high_water = now.max(high_water);
                        let Ok(state) = node.local().await else { continue; };
                        if let Some(record) = state.get(Collection::Alias,"rules")
                            && aliases != Some(record.revision)
                            && let Ok(rules) = serde_json::from_value(record.value.clone())
                            && media.aliases.cluster_rules(rules).await.is_ok()
                        { aliases = Some(record.revision); }
                        for (id,record) in state.records(Collection::Zone) {
                            if zones.get(id) != Some(&record.revision)
                                && engine.cluster_zone(id,&record.value["limits"]).await.is_ok()
                            { zones.insert(id.clone(),record.revision); }
                        }
                        let mut targets = BTreeMap::new();
                        let mut deadlines = BTreeMap::new();
                        if now.abs_diff(high_water) <= 500 && let Some(descriptor) = read_node(&state,node.id) {
                            for (id, job) in jobs(&state) {
                                if !job.desired || !eligible(node.id,&descriptor,&job.requirements) { continue; }
                                if let Some(assignment) = &job.assignment
                                    && assignment.node == node.id
                                    && let Some(remaining) = assignment.expires_ms.checked_sub(high_water.saturating_add(1500))
                                    && (200..=30_000).contains(&remaining)
                                {
                                    let domain = job.config["domain"].as_str().unwrap_or_default().to_owned();
                                    deadlines.insert((domain,id.clone()),(Instant::now() + Duration::from_millis(remaining),assignment.epoch));
                                    targets.insert(id,job);
                                }
                            }
                        }
                        gate.replace(deadlines);
                        reconcile(&engine,&gate,&mut active,targets).await;
                        let (ready,_) = ops.components();
                        if ready {
                            let telemetry = telemetry(&engine,&ops,&media,&active,read_node(&state,node.id).as_ref()).await;
                            *node.telemetry.write().await = Some(telemetry);
                        } else {
                            *node.telemetry.write().await = None;
                        }
                    }
                }
            }
            gate.replace(BTreeMap::new());
        });

        let node = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = node.stop.cancelled() => return,
                    _ = tick.tick() => {
                        if node.raft.current_leader().await == Some(node.id) {
                            node.schedule().await;
                        }
                    }
                }
            }
        });
    }

    async fn schedule(&self) {
        let Ok(state) = self.local().await else {
            return;
        };
        let membership = self.raft.metrics().borrow().membership_config.clone();
        for (id, _) in membership.nodes() {
            if state.get(Collection::Node, &id.to_string()).is_none() {
                if let Some(peer) = self.peers.get(id) {
                    let _ = self
                        .write(Action::Put {
                            collection: Collection::Node,
                            key: id.to_string(),
                            value: json!(peer.node),
                            expected: Some(0),
                        })
                        .await;
                }
                return;
            }
        }
        let ids: Vec<_> = membership.nodes().map(|(id, _)| *id).collect();
        let observations: BTreeMap<_, _> = stream::iter(ids)
            .map(|id| async move { self.report(id).await.map(|report| (id, report.observation)) })
            .buffer_unordered(8)
            .filter_map(|result| async move { result })
            .collect()
            .await;
        let now = now_ms();
        self.rebalance(&state, &observations, now).await;
        let mut work: Vec<_> = jobs(&state).collect();
        work.sort_by_key(|(_, job)| {
            job.assignment
                .as_ref()
                .map_or(u64::MAX, |lease| lease.expires_ms)
        });
        for (id, job) in work.into_iter().take(256) {
            let Some(record) = state.get(Collection::Process, &id) else {
                continue;
            };
            let observation = |id| {
                observations.get(&id).filter(|obs| {
                    read_node(&state, id).is_some_and(|node| eligible(id, &node, &job.requirements))
                        && supplies(obs, &job.requirements)
                })
            };
            let existing = job.assignment.as_ref();
            let target = if let Some(lease) = existing
                && lease.expires_ms.saturating_add(3000) > now
            {
                if !job.desired
                    || lease.relocate_to.is_some()
                    || lease.expires_ms > now.saturating_add(self.lease_ms / 2)
                {
                    continue;
                }
                if lease.expires_ms <= now {
                    continue;
                }
                observation(lease.node).map(|obs| (lease.node, obs))
            } else if job.desired {
                observations
                    .keys()
                    .filter_map(|id| {
                        let obs = observation(*id)?;
                        let mut free = obs.capacity.clone();
                        for (key, other) in jobs(&state) {
                            if key != id_string(&job)
                                && let Some(assigned) = other.assignment
                                && assigned.node == *id
                                && free.reserve(&assigned.resources).is_none()
                            {
                                return None;
                            }
                        }
                        free.reserve(&job.requirements.capacity)?;
                        Some((*id, obs, free.processes))
                    })
                    .max_by_key(|(id, _, free)| {
                        (
                            existing.and_then(|lease| lease.relocate_to) == Some(*id),
                            job.requirements.preferred == Some(*id),
                            *free,
                            std::cmp::Reverse(*id),
                        )
                    })
                    .map(|(id, obs, _)| (id, obs))
            } else {
                None
            };
            let (node, capacity) = match target {
                Some((node, observation)) => (node, observation.capacity.clone()),
                None if existing
                    .is_some_and(|lease| lease.expires_ms.saturating_add(3000) <= now) =>
                {
                    (0, Capacity::default())
                }
                None => continue,
            };
            let result = self
                .write(Action::Assign {
                    key: id,
                    revision: record.revision,
                    node,
                    expires_ms: now.saturating_add(self.lease_ms),
                    now_ms: now,
                    capacity,
                })
                .await;
            if matches!(
                result,
                Err(super::Failure::Unavailable | super::Failure::Uncertain)
            ) {
                break;
            }
        }
    }

    async fn rebalance(&self, state: &Data, observations: &BTreeMap<u64, Observation>, now: u64) {
        let Some(policy) = &self.rebalance else {
            return;
        };
        if state.last_rebalance_ms.saturating_add(policy.cooldown_ms) > now {
            return;
        }
        for (key, job) in jobs(state) {
            let Some(current) = job.assignment.as_ref() else {
                continue;
            };
            if job.requirements.node.is_some()
                || !job.desired
                || observations
                    .get(&current.node)
                    .and_then(|node| node.cpu_pressure)
                    .is_none_or(|pressure| pressure < f64::from(policy.above_percent))
            {
                continue;
            }
            let destination = observations.iter().find(|(id, observed)| {
                let id = **id;
                if id == current.node
                    || observed
                        .cpu_pressure
                        .is_none_or(|pressure| pressure > f64::from(policy.below_percent))
                    || !read_node(state, id)
                        .is_some_and(|node| eligible(id, &node, &job.requirements))
                    || !supplies(observed, &job.requirements)
                {
                    return false;
                }
                let mut free = observed.capacity.clone();
                for (_, other) in jobs(state) {
                    if let Some(lease) = other.assignment
                        && lease.node == id
                        && free.reserve(&lease.resources).is_none()
                    {
                        return false;
                    }
                }
                free.reserve(&job.requirements.capacity).is_some()
            });
            if let Some((&destination, _)) = destination
                && let Some(record) = state.get(Collection::Process, &key)
            {
                let _ = self
                    .write(Action::Rebalance {
                        key,
                        revision: record.revision,
                        destination,
                        now_ms: now,
                        cooldown_ms: policy.cooldown_ms,
                    })
                    .await;
                return;
            }
        }
    }

    pub async fn report(&self, id: u64) -> Option<Telemetry> {
        let report = if self.id == id {
            self.telemetry.read().await.clone()
        } else {
            self.network.telemetry(id).await
        }?;
        (report.observed_ms.abs_diff(now_ms()) < 4000).then_some(report)
    }
}

fn id_string(job: &Job) -> &str {
    job.config["id"].as_str().unwrap_or_default()
}

fn supplies(observed: &Observation, requirements: &Requirements) -> bool {
    requirements.protocols.is_subset(&observed.protocols)
        && requirements.storage.is_subset(&observed.storage)
        && requirements.encoders.is_subset(&observed.encoders)
}

fn read_node(state: &Data, id: u64) -> Option<Node> {
    serde_json::from_value(state.get(Collection::Node, &id.to_string())?.value.clone()).ok()
}

fn jobs(state: &Data) -> impl Iterator<Item = (String, Job)> + '_ {
    state
        .records(Collection::Process)
        .filter_map(|(id, record)| {
            serde_json::from_value(record.value.clone())
                .ok()
                .map(|job| (id.clone(), job))
        })
}

async fn reconcile(
    engine: &Arc<Engine>,
    gate: &Gate,
    active: &mut BTreeMap<String, Deployment>,
    targets: BTreeMap<String, Job>,
) {
    let stale: Vec<_> = active
        .keys()
        .filter(|name| !targets.contains_key(*name))
        .cloned()
        .collect();
    for name in stale.into_iter().take(4) {
        if let Some(deployed) = active.get(&name)
            && engine.delete(deployed.key.clone(), false).await.is_ok()
        {
            active.remove(&name);
        }
    }
    for (id, job) in targets
        .into_iter()
        .filter(|(id, job)| {
            active.get(id).is_none_or(|item| {
                item.config != job.config
                    || job
                        .assignment
                        .as_ref()
                        .is_some_and(|assignment| assignment.epoch != item.epoch)
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .take(4)
    {
        let Some(assignment) = &job.assignment else {
            continue;
        };
        let mut config = job.config.clone();
        config["autostart"] = json!(false);
        let Ok(config) = Configuration::from_patch(None, config, "$cluster", false) else {
            continue;
        };
        let (config, _) = config.into_config();
        if !gate.allows(&config) {
            continue;
        }
        let key = (config.domain.clone(), config.id.clone());
        let existing = active.get(&id);
        if let Some(old) = existing
            && old.key != key
        {
            if engine.delete(old.key.clone(), false).await.is_err() {
                continue;
            }
            active.remove(&id);
        }
        if engine
            .edit(
                active.get(&id).map(|item| item.key.clone()),
                true,
                move |_| Ok((config, Default::default())),
            )
            .await
            .is_ok()
        {
            let _ = engine
                .command(key.clone(), Control::Start, "cluster assignment".into())
                .await;
            active.insert(
                id,
                Deployment {
                    key,
                    epoch: assignment.epoch,
                    config: job.config,
                },
            );
        }
    }
}

async fn telemetry(
    engine: &Engine,
    ops: &Operations,
    media: &Media,
    active: &BTreeMap<String, Deployment>,
    node: Option<&Node>,
) -> Telemetry {
    let host = ops.host();
    let (max, _) = engine.diagnostics();
    let mut gpu_memory = BTreeMap::new();
    let mut encoders = BTreeSet::new();
    if !host.gpus.is_empty()
        && let Some(node) = node
        && !node.hardware_encoders.is_empty()
        && let Ok(Ok(skills)) =
            tokio::time::timeout(Duration::from_millis(400), engine.skills(false)).await
    {
        encoders.extend(
            node.hardware_encoders
                .iter()
                .filter(|encoder| skills.has_video_encoder(encoder))
                .cloned(),
        );
        for gpu in host.gpus.iter().take(16) {
            if let Some(size) = gpu.memory_bytes {
                gpu_memory.insert(gpu.identifier.clone(), size);
            }
        }
    }
    let mut protocols = BTreeSet::from(["file".to_owned(), "http".to_owned()]);
    if let Some(transport) = &media.transport
        && transport.metrics().available
    {
        protocols.extend(
            ["rtmp", "srt"]
                .into_iter()
                .filter(|protocol| transport.enabled(protocol))
                .map(str::to_owned),
        );
    }
    let observation = Observation {
        cpu_pressure: host
            .cgroup_cpu_cores
            .zip(host.effective_cpus)
            .filter(|(_, capacity)| *capacity > 0.0)
            .map(|(used, capacity)| used * 100.0 / capacity)
            .or(host.host_cpu_percent),
        capacity: Capacity {
            cpu: host.effective_cpus.map_or(0, |cpu| (cpu * 1000.0) as u64),
            memory: host
                .total_memory
                .into_iter()
                .chain(host.cgroup_memory_limit)
                .min()
                .unwrap_or(0),
            processes: max as u64,
            gpu_memory,
        },
        storage: media.mounts.keys().cloned().collect(),
        protocols,
        encoders,
    };
    let mut runtime = BTreeMap::new();
    for (id, deployment) in active {
        if let Ok(entry) = engine.get(&deployment.key).await {
            let (state, _, _, _, progress) = entry.telemetry();
            runtime.insert(
                id.clone(),
                Runtime {
                    state: state.wire().into(),
                    epoch: deployment.epoch,
                    frame: progress.frame,
                },
            );
        }
    }
    Telemetry {
        observation,
        observed_ms: now_ms(),
        runtime,
    }
}

impl Network {
    pub async fn telemetry(&self, id: u64) -> Option<Telemetry> {
        let peer = self.peers.get(&id)?;
        let mut response = self
            .client
            .post(format!("{}/internal/cluster/v1/telemetry", peer.address))
            .header("x-castvane-cluster", self.cluster.as_ref())
            .timeout(Duration::from_millis(700))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if bytes.len() + chunk.len() > 128 * 1024 {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).ok()
    }
}
