use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_VALUE: usize = 32 * 1024;
pub const MAX_STATE: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    #[default]
    Combined,
    Control,
    Worker,
}

impl Role {
    pub fn runs_media(self) -> bool {
        self != Self::Control
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Node {
    pub name: String,
    pub role: Role,
    pub zones: BTreeSet<String>,
    pub labels: BTreeMap<String, String>,
    pub hardware_encoders: BTreeSet<String>,
    pub draining: bool,
    pub disabled: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Capacity {
    pub cpu: u64,
    pub memory: u64,
    pub processes: u64,
    pub gpu_memory: BTreeMap<String, u64>,
}

impl Capacity {
    pub fn reserve(&mut self, request: &Self) -> Option<()> {
        let cpu = self.cpu.checked_sub(request.cpu)?;
        let memory = self.memory.checked_sub(request.memory)?;
        let processes = self.processes.checked_sub(request.processes)?;
        let mut gpu_memory = self.gpu_memory.clone();
        for (device, required) in &request.gpu_memory {
            let available = gpu_memory.get_mut(device)?;
            *available = available.checked_sub(*required)?;
        }
        *self = Self {
            cpu,
            memory,
            processes,
            gpu_memory,
        };
        Some(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Observation {
    pub capacity: Capacity,
    pub cpu_pressure: Option<f64>,
    pub protocols: BTreeSet<String>,
    pub storage: BTreeSet<String>,
    pub encoders: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Requirements {
    pub capacity: Capacity,
    pub node: Option<u64>,
    pub preferred: Option<u64>,
    pub zone: String,
    pub labels: BTreeMap<String, String>,
    pub protocols: BTreeSet<String>,
    pub storage: BTreeSet<String>,
    pub encoders: BTreeSet<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Job {
    pub config: Value,
    #[serde(default)]
    pub requirements: Requirements,
    #[serde(default)]
    pub desired: bool,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
    #[serde(default)]
    pub assignment: Option<Assignment>,
    #[serde(default)]
    pub epoch: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Assignment {
    pub node: u64,
    pub epoch: u64,
    pub expires_ms: u64,
    pub resources: Capacity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relocate_to: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Action {
    Put {
        collection: Collection,
        key: String,
        value: Value,
        expected: Option<u64>,
    },
    Delete {
        collection: Collection,
        key: String,
        expected: Option<u64>,
    },
    Assign {
        key: String,
        revision: u64,
        node: u64,
        expires_ms: u64,
        now_ms: u64,
        capacity: Capacity,
    },
    Rebalance {
        key: String,
        revision: u64,
        destination: u64,
        now_ms: u64,
        cooldown_ms: u64,
    },
    Lock {
        key: String,
        owner: u64,
        token: Option<u64>,
        now_ms: u64,
        expires_ms: u64,
    },
    Unlock {
        key: String,
        owner: u64,
        token: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Collection {
    Node,
    Process,
    Configuration,
    Identity,
    Policy,
    Zone,
    Alias,
    Kv,
    Certificate,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Command {
    pub version: u16,
    pub action: Action,
}

impl std::fmt::Debug for Command {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Command")
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Applied {
    Updated(u64),
    Missing,
    Conflict,
    Invalid,
    Full,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Record {
    pub revision: u64,
    pub value: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Lease {
    pub owner: u64,
    pub token: u64,
    pub expires_ms: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Data {
    pub collections: BTreeMap<Collection, BTreeMap<String, Record>>,
    pub locks: BTreeMap<String, Lease>,
    #[serde(default)]
    pub last_rebalance_ms: u64,
}

impl Data {
    pub fn records(&self, collection: Collection) -> impl Iterator<Item = (&String, &Record)> {
        self.collections.get(&collection).into_iter().flatten()
    }

    pub fn get(&self, collection: Collection, key: &str) -> Option<&Record> {
        self.collections.get(&collection)?.get(key)
    }

    pub fn apply(&mut self, revision: u64, command: &Command) -> Applied {
        if command.version != 1 {
            return Applied::Invalid;
        }
        let mut candidate = self.clone();
        let result = candidate.mutate(revision, &command.action);
        if matches!(result, Applied::Updated(_)) {
            if serde_json::to_vec(&candidate).map_or(true, |bytes| bytes.len() > MAX_STATE) {
                return Applied::Full;
            }
            *self = candidate;
        }
        result
    }

    fn mutate(&mut self, revision: u64, action: &Action) -> Applied {
        match action {
            Action::Put {
                collection,
                key,
                value,
                expected,
            } => {
                if !valid_key(key)
                    || serde_json::to_vec(value).map_or(true, |bytes| bytes.len() > MAX_VALUE)
                {
                    return Applied::Invalid;
                }
                let old = self.get(*collection, key);
                if expected.is_some_and(|wanted| wanted != old.map_or(0, |record| record.revision))
                {
                    return Applied::Conflict;
                }
                let mut value = value.clone();
                if *collection == Collection::Process {
                    let Ok(mut job) = serde_json::from_value::<Job>(value) else {
                        return Applied::Invalid;
                    };
                    let configured_zone = job.config["zone"].as_str().unwrap_or_default();
                    if [configured_zone, job.requirements.zone.as_str()]
                        .into_iter()
                        .any(|zone| {
                            !zone.is_empty()
                                && zone != "$default"
                                && self.get(Collection::Zone, zone).is_none()
                        })
                    {
                        return Applied::Invalid;
                    }
                    job.requirements.capacity.processes = 1;
                    if let Some(previous) = old
                        .and_then(|record| serde_json::from_value::<Job>(record.value.clone()).ok())
                    {
                        if let Some(lease) = &previous.assignment
                            && job.requirements.capacity != previous.requirements.capacity
                            && lease.resources != job.requirements.capacity
                        {
                            return Applied::Conflict;
                        }
                        job.epoch = previous.epoch;
                        job.assignment = previous.assignment;
                    } else {
                        job.epoch = 0;
                        job.assignment = None;
                    }
                    let Ok(document) = serde_json::to_value(job) else {
                        return Applied::Invalid;
                    };
                    value = document;
                }
                let records = self.collections.entry(*collection).or_default();
                if records.len() >= 2048 && !records.contains_key(key) {
                    return Applied::Full;
                }
                records.insert(key.clone(), Record { revision, value });
            }
            Action::Delete {
                collection,
                key,
                expected,
            } => {
                let Some(record) = self.get(*collection, key) else {
                    return Applied::Missing;
                };
                if expected.is_some_and(|wanted| wanted != record.revision) {
                    return Applied::Conflict;
                }
                if *collection == Collection::Process
                    && serde_json::from_value::<Job>(record.value.clone())
                        .is_ok_and(|job| job.assignment.is_some())
                {
                    return Applied::Conflict;
                }
                if *collection == Collection::Zone
                    && (key == "$default"
                        || self.records(Collection::Process).any(|(_, record)| {
                            record.value["config"]["zone"] == *key
                                || record.value["requirements"]["zone"] == *key
                        }))
                {
                    return Applied::Conflict;
                }
                if let Some(records) = self.collections.get_mut(collection) {
                    records.remove(key);
                }
            }
            Action::Assign {
                key,
                revision: current,
                node,
                expires_ms,
                now_ms,
                capacity,
            } => {
                let Some(record) = self.get(Collection::Process, key) else {
                    return Applied::Missing;
                };
                let Ok(mut job) = serde_json::from_value::<Job>(record.value.clone()) else {
                    return Applied::Invalid;
                };
                if record.revision != *current
                    || *node != 0
                        && (!job.desired || *expires_ms <= *now_ms || expires_ms - now_ms > 30_000)
                {
                    return Applied::Conflict;
                }
                let same = job
                    .assignment
                    .as_ref()
                    .is_some_and(|old| old.node == *node && old.expires_ms > *now_ms);
                if !same
                    && job
                        .assignment
                        .as_ref()
                        .is_some_and(|old| old.expires_ms.saturating_add(3000) > *now_ms)
                {
                    return Applied::Conflict;
                }
                if *node == 0 {
                    job.assignment = None;
                } else {
                    let Some(details) =
                        self.get(Collection::Node, &node.to_string())
                            .and_then(|record| {
                                serde_json::from_value::<Node>(record.value.clone()).ok()
                            })
                    else {
                        return Applied::Missing;
                    };
                    if !eligible(*node, &details, &job.requirements) {
                        return Applied::Conflict;
                    }
                    let mut free = capacity.clone();
                    for (other, entry) in self.records(Collection::Process) {
                        if other == key {
                            continue;
                        }
                        let reservation = serde_json::from_value::<Job>(entry.value.clone())
                            .ok()
                            .and_then(|j| j.assignment)
                            .filter(|a| a.node == *node);
                        if let Some(reservation) = reservation
                            && free.reserve(&reservation.resources).is_none()
                        {
                            return Applied::Conflict;
                        }
                    }
                    if free.reserve(&job.requirements.capacity).is_none() {
                        return Applied::Conflict;
                    }
                    if !same {
                        job.epoch = revision;
                    }
                    job.assignment = Some(Assignment {
                        node: *node,
                        epoch: job.epoch,
                        expires_ms: *expires_ms,
                        resources: job.requirements.capacity.clone(),
                        relocate_to: None,
                    });
                }
                let Ok(value) = serde_json::to_value(job) else {
                    return Applied::Invalid;
                };
                self.collections
                    .entry(Collection::Process)
                    .or_default()
                    .insert(key.clone(), Record { revision, value });
            }
            Action::Rebalance {
                key,
                revision: current,
                destination,
                now_ms,
                cooldown_ms,
            } => {
                if self.last_rebalance_ms.saturating_add(*cooldown_ms) > *now_ms
                    || self
                        .records(Collection::Process)
                        .any(|(_, record)| !record.value["assignment"]["relocate_to"].is_null())
                {
                    return Applied::Conflict;
                }
                let Some(record) = self.get(Collection::Process, key) else {
                    return Applied::Missing;
                };
                let Ok(mut job) = serde_json::from_value::<Job>(record.value.clone()) else {
                    return Applied::Invalid;
                };
                let Some(assignment) = &mut job.assignment else {
                    return Applied::Conflict;
                };
                if record.revision != *current
                    || !job.desired
                    || job.requirements.node.is_some()
                    || assignment.node == *destination
                    || assignment.expires_ms <= *now_ms
                {
                    return Applied::Conflict;
                }
                assignment.relocate_to = Some(*destination);
                let Ok(value) = serde_json::to_value(job) else {
                    return Applied::Invalid;
                };
                self.collections
                    .entry(Collection::Process)
                    .or_default()
                    .insert(key.clone(), Record { revision, value });
                self.last_rebalance_ms = *now_ms;
            }
            Action::Lock {
                key,
                owner,
                token,
                now_ms,
                expires_ms,
            } => {
                if !valid_key(key)
                    || *owner == 0
                    || *expires_ms <= *now_ms
                    || expires_ms - now_ms > 300_000
                {
                    return Applied::Invalid;
                }
                let existing = self.locks.get(key);
                let fencing = match (existing, token) {
                    (Some(held), Some(token))
                        if held.token == *token
                            && held.owner == *owner
                            && held.expires_ms > *now_ms =>
                    {
                        *token
                    }
                    (_, Some(_)) => return Applied::Conflict,
                    (Some(held), None) if held.expires_ms > *now_ms => return Applied::Conflict,
                    _ => revision,
                };
                if self.locks.len() >= 2048 && !self.locks.contains_key(key) {
                    self.locks.retain(|_, lease| lease.expires_ms > *now_ms);
                    if self.locks.len() >= 2048 {
                        return Applied::Full;
                    }
                }
                self.locks.insert(
                    key.clone(),
                    Lease {
                        owner: *owner,
                        token: fencing,
                        expires_ms: *expires_ms,
                    },
                );
                return Applied::Updated(fencing);
            }
            Action::Unlock { key, owner, token } => {
                let Some(lease) = self.locks.get(key) else {
                    return Applied::Missing;
                };
                if lease.owner != *owner || lease.token != *token {
                    return Applied::Conflict;
                }
                self.locks.remove(key);
            }
        }
        Applied::Updated(revision)
    }
}

pub fn eligible(id: u64, node: &Node, request: &Requirements) -> bool {
    node.role.runs_media()
        && !node.disabled
        && !node.draining
        && request.node.is_none_or(|required| required == id)
        && (request.zone.is_empty() || node.zones.contains(&request.zone))
        && request
            .labels
            .iter()
            .all(|(key, value)| node.labels.get(key) == Some(value))
}

pub fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 256 && !key.chars().any(char::is_control)
}

#[derive(Serialize, Deserialize)]
pub struct ClusterAccount {
    pub account: crate::iam::Account,
    pub rules: Vec<crate::iam::Rule>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(data: &mut Data, index: u64, action: Action) -> Applied {
        data.apply(index, &Command { version: 1, action })
    }

    #[test]
    fn placements_reserve_capacity_and_wait_for_the_previous_epoch() {
        let mut data = Data::default();
        for node in [1, 2] {
            assert!(matches!(
                apply(
                    &mut data,
                    node,
                    Action::Put {
                        collection: Collection::Node,
                        key: node.to_string(),
                        value: serde_json::to_value(Node::default()).unwrap(),
                        expected: Some(0)
                    }
                ),
                Applied::Updated(_)
            ));
        }
        for (index, key) in [(3, "first"), (4, "second")] {
            assert!(matches!(
                apply(
                    &mut data,
                    index,
                    Action::Put {
                        collection: Collection::Process,
                        key: key.into(),
                        value: serde_json::json!({
                            "config":{"id":key},"desired":true,"requirements":{"capacity":{"cpu":600}}
                        }),
                        expected: Some(0)
                    }
                ),
                Applied::Updated(_)
            ));
        }
        let capacity = Capacity {
            cpu: 1000,
            processes: 2,
            ..Capacity::default()
        };
        let assignment = |key: &str, revision, node, now| Action::Assign {
            key: key.into(),
            revision,
            node,
            now_ms: now,
            expires_ms: now + 10000,
            capacity: capacity.clone(),
        };
        assert!(matches!(
            apply(&mut data, 5, assignment("first", 3, 1, 1000)),
            Applied::Updated(5)
        ));
        assert!(matches!(
            apply(&mut data, 6, assignment("second", 4, 1, 1001)),
            Applied::Conflict
        ));
        assert!(matches!(
            apply(&mut data, 7, assignment("first", 5, 2, 13999)),
            Applied::Conflict
        ));
        assert!(matches!(
            apply(&mut data, 8, assignment("first", 5, 2, 14000)),
            Applied::Updated(8)
        ));
        assert_eq!(
            data.get(Collection::Process, "first").unwrap().value["assignment"]["epoch"],
            8
        );
        assert!(matches!(
            apply(&mut data, 9, assignment("first", 5, 1, 14001)),
            Applied::Conflict
        ));
    }

    #[test]
    fn rebalance_records_one_destination_without_changing_current_ownership() {
        let mut state = Data::default();
        let processes = state.collections.entry(Collection::Process).or_default();
        for key in ["stream", "other"] {
            processes.insert(
                key.into(),
                Record {
                    revision: 1,
                    value: serde_json::json!({
                        "config":{"id":key},"desired":true,"assignment":{
                            "node":1,"epoch":10,"expires_ms":120_000,"resources":{}
                        }
                    }),
                },
            );
        }
        let move_to = |key: &str| Action::Rebalance {
            key: key.into(),
            revision: 1,
            destination: 2,
            now_ms: 110_000,
            cooldown_ms: 30_000,
        };
        assert!(matches!(
            apply(&mut state, 2, move_to("stream")),
            Applied::Updated(2)
        ));
        assert!(matches!(
            apply(&mut state, 3, move_to("other")),
            Applied::Conflict
        ));
        let entry = state.get(Collection::Process, "stream").unwrap();
        assert_eq!(entry.value["assignment"]["node"], 1);
        assert_eq!(entry.value["assignment"]["epoch"], 10);
        assert_eq!(entry.value["assignment"]["relocate_to"], 2);
    }

    #[test]
    fn gpu_reservation_requires_a_matching_device_without_partial_charges() {
        let mut available = Capacity {
            cpu: 1000,
            memory: 1024,
            processes: 2,
            gpu_memory: BTreeMap::from([("nvidia-test".into(), 128)]),
        };
        let before = available.clone();
        let mut request = Capacity {
            cpu: 500,
            memory: 16,
            processes: 1,
            gpu_memory: BTreeMap::from([("intel-other".into(), 64)]),
        };
        assert!(available.reserve(&request).is_none());
        assert_eq!(available, before);
        request.gpu_memory = BTreeMap::from([("nvidia-test".into(), 96)]);
        available.reserve(&request).unwrap();
        assert_eq!(available.gpu_memory["nvidia-test"], 32);
        assert!(available.reserve(&request).is_none());
    }
}
