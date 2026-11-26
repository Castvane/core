use std::{collections::BTreeSet, sync::Arc};

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::StatusCode,
    routing::{get, put},
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::ApiError;
use crate::{
    App,
    auth::Identity,
    cluster::{
        Cluster, Failure,
        model::{Action, Collection, Job, Node, Requirements},
    },
};

type Reply = Result<Json<Value>, ApiError>;
type StateArg = State<Arc<App>>;
type Actor = Extension<Identity>;

pub fn routes() -> Router<Arc<App>> {
    Router::new().nest(
        "/cluster",
        Router::new()
            .route("/", get(about))
            .route("/healthy", get(health))
            .route("/snapshot", get(snapshot))
            .route("/reallocation", put(relocate))
            .route("/node", get(nodes))
            .route("/node/{id}", get(node).post(enroll))
            .route("/node/{id}/fs/{storage}", get(node_files))
            .route(
                "/node/{id}/fs/{storage}/{*path}",
                get(node_file)
                    .head(node_file)
                    .put(node_file)
                    .delete(node_file),
            )
            .route("/node/{id}/state", get(node_state).put(set_node_state))
            .route("/iam/user", get(users).post(user_add))
            .route(
                "/iam/user/{name}",
                get(user_get).put(user_update).delete(user_remove),
            )
            .route("/iam/user/{name}/policy", put(user_policies))
            .route("/iam/policies", get(policy_list))
            .route("/zone", get(zones).post(zone_add))
            .route("/zone/{id}", get(zone).put(zone_update).delete(zone_delete))
            .route(
                "/alias",
                get(aliases).put(aliases_replace).post(aliases_add),
            )
            .route(
                "/alias/rule",
                get(alias_get).put(alias_update).delete(alias_delete),
            )
            .route("/membership", put(voters))
            .route("/db/kv", get(kv_list))
            .route("/db/kv/{key}", get(kv_get).put(kv_put).delete(kv_delete))
            .route("/db/locks", get(locks))
            .route("/db/locks/{key}", put(lock).delete(unlock))
            .route("/process", get(process_list).post(process_add))
            .route(
                "/process/{id}",
                get(process_get).put(process_update).delete(process_remove),
            )
            .route("/process/{id}/config", get(process_config))
            .route("/process/{id}/command", put(process_command))
            .route("/process/{id}/metadata", get(process_metadata))
            .route(
                "/process/{id}/metadata/{key}",
                get(process_metadata_get).put(process_metadata_put),
            )
            .route("/process/{id}/state", get(process_state))
            .route("/deployments", get(deployments)),
    )
}

impl From<Failure> for ApiError {
    fn from(error: Failure) -> Self {
        match error {
            Failure::Invalid => Self::bad_request("invalid cluster operation"),
            Failure::Missing => Self::not_found(),
            Failure::Conflict => Self::Http(
                StatusCode::CONFLICT,
                "cluster revision or resource conflict",
            ),
            Failure::Full => {
                Self::Http(StatusCode::INSUFFICIENT_STORAGE, "cluster capacity reached")
            }
            Failure::Unavailable => Self::Http(
                StatusCode::SERVICE_UNAVAILABLE,
                "contact the cluster leader with an available quorum",
            ),
            Failure::Uncertain => Self::Http(
                StatusCode::SERVICE_UNAVAILABLE,
                "cluster operation timed out, read the current revision before retrying",
            ),
        }
    }
}

fn authorized<'a>(app: &'a App, actor: &Identity) -> Result<&'a Cluster, ApiError> {
    let cluster = app.cluster.as_deref().ok_or_else(ApiError::not_found)?;
    if !actor.administrator() {
        return Err(ApiError::forbidden(
            "cluster administration requires an administrator",
        ));
    }
    Ok(cluster)
}

fn body<T>(data: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    data.map(|Json(value)| value)
        .map_err(|_| ApiError::bad_request("invalid cluster JSON"))
}

async fn about(State(app): StateArg, Extension(actor): Actor) -> Reply {
    Ok(Json(authorized(&app, &actor)?.about()))
}

async fn health(State(app): StateArg, Extension(actor): Actor) -> Reply {
    Ok(Json(json!(
        authorized(&app, &actor)?.quorum().await.is_ok()
    )))
}

async fn nodes(State(app): StateArg, Extension(actor): Actor) -> Reply {
    Ok(Json(authorized(&app, &actor)?.about()["nodes"].clone()))
}

async fn node(State(app): StateArg, Extension(actor): Actor, Path(id): Path<u64>) -> Reply {
    let info = authorized(&app, &actor)?.about();
    let id = id.to_string();
    let result = info["nodes"]
        .as_array()
        .and_then(|nodes| nodes.iter().find(|node| node["id"] == id));
    result.cloned().map(Json).ok_or_else(ApiError::not_found)
}

async fn enroll(State(app): StateArg, Extension(actor): Actor, Path(id): Path<u64>) -> Reply {
    authorized(&app, &actor)?.enroll(id).await?;
    Ok(Json(json!("OK")))
}

async fn voters(
    State(app): StateArg,
    Extension(actor): Actor,
    data: Result<Json<BTreeSet<u64>>, JsonRejection>,
) -> Reply {
    authorized(&app, &actor)?.voters(body(data)?).await?;
    Ok(Json(json!("OK")))
}

async fn node_state(State(app): StateArg, Extension(actor): Actor, Path(id): Path<u64>) -> Reply {
    let cluster = authorized(&app, &actor)?;
    let state = cluster.committed().await?;
    let record = state
        .get(Collection::Node, &id.to_string())
        .ok_or(Failure::Missing)?;
    Ok(Json(record.value.clone()))
}

#[derive(Deserialize)]
struct NodeChange {
    draining: bool,
    disabled: bool,
}

async fn set_node_state(
    State(app): StateArg,
    Extension(actor): Actor,
    Path(id): Path<u64>,
    data: Result<Json<NodeChange>, JsonRejection>,
) -> Reply {
    let cluster = authorized(&app, &actor)?;
    let data = body(data)?;
    let key = id.to_string();
    let state = cluster.committed().await?;
    let record = state.get(Collection::Node, &key).ok_or(Failure::Missing)?;
    let mut node: Node =
        serde_json::from_value(record.value.clone()).map_err(|_| Failure::Invalid)?;
    node.draining = data.draining;
    node.disabled = data.disabled;
    cluster
        .write(Action::Put {
            collection: Collection::Node,
            key,
            value: json!(node),
            expected: Some(record.revision),
        })
        .await?;
    Ok(Json(json!("OK")))
}

#[derive(Deserialize)]
struct WriteValue {
    value: Value,
    expected: Option<u64>,
}
#[derive(Deserialize)]
struct Revision {
    expected: Option<u64>,
}

async fn kv_list(State(app): StateArg, Extension(actor): Actor) -> Reply {
    let state = authorized(&app, &actor)?.committed().await?;
    Ok(Json(json!(
        state
            .records(Collection::Kv)
            .collect::<std::collections::BTreeMap<_, _>>()
    )))
}

async fn kv_get(State(app): StateArg, Extension(actor): Actor, Path(key): Path<String>) -> Reply {
    let state = authorized(&app, &actor)?.committed().await?;
    Ok(Json(json!(
        state.get(Collection::Kv, &key).ok_or(Failure::Missing)?
    )))
}

async fn kv_put(
    State(app): StateArg,
    Extension(actor): Actor,
    Path(key): Path<String>,
    data: Result<Json<WriteValue>, JsonRejection>,
) -> Reply {
    let input = body(data)?;
    let revision = authorized(&app, &actor)?
        .write(Action::Put {
            collection: Collection::Kv,
            key,
            value: input.value,
            expected: input.expected,
        })
        .await?;
    Ok(Json(json!({"revision":revision})))
}

async fn kv_delete(
    State(app): StateArg,
    Extension(actor): Actor,
    Path(key): Path<String>,
    Query(rev): Query<Revision>,
) -> Reply {
    authorized(&app, &actor)?
        .write(Action::Delete {
            collection: Collection::Kv,
            key,
            expected: rev.expected,
        })
        .await?;
    Ok(Json(json!("OK")))
}

async fn locks(State(app): StateArg, Extension(actor): Actor) -> Reply {
    let state = authorized(&app, &actor)?.committed().await?;
    let now = super::cluster::millis();
    Ok(Json(json!(
        state
            .locks
            .into_iter()
            .filter(|(_, lease)| lease.expires_ms > now)
            .collect::<std::collections::BTreeMap<_, _>>()
    )))
}

pub(crate) fn millis() -> u64 {
    u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

#[derive(Deserialize)]
struct LockChange {
    owner: u64,
    token: Option<u64>,
    ttl_ms: u64,
}
#[derive(Deserialize)]
struct Unlock {
    owner: u64,
    token: u64,
}

async fn lock(
    State(app): StateArg,
    Extension(actor): Actor,
    Path(key): Path<String>,
    data: Result<Json<LockChange>, JsonRejection>,
) -> Reply {
    let input = body(data)?;
    let now_ms = millis();
    let expires_ms = now_ms.saturating_add(input.ttl_ms);
    let token = authorized(&app, &actor)?
        .write(Action::Lock {
            key,
            owner: input.owner,
            token: input.token,
            now_ms,
            expires_ms,
        })
        .await?;
    Ok(Json(json!({"token":token,"expires_ms":expires_ms})))
}

async fn unlock(
    State(app): StateArg,
    Extension(actor): Actor,
    Path(key): Path<String>,
    Query(input): Query<Unlock>,
) -> Reply {
    authorized(&app, &actor)?
        .write(Action::Unlock {
            key,
            owner: input.owner,
            token: input.token,
        })
        .await?;
    Ok(Json(json!("OK")))
}

async fn process_list(State(app): StateArg, Extension(actor): Actor) -> Reply {
    let state = authorized(&app, &actor)?.committed().await?;
    Ok(Json(json!(
        state
            .records(Collection::Process)
            .map(|(id, record)| json!({
                "id":id,"config":record.value["config"],"metadata":record.value["metadata"],
                "desired":record.value["desired"],"assignment":record.value["assignment"]
            }))
            .collect::<Vec<_>>()
    )))
}

async fn find_job(cluster: &Cluster, key: &str) -> Result<(u64, Job), ApiError> {
    let state = cluster.committed().await?;
    let record = state
        .get(Collection::Process, key)
        .ok_or(Failure::Missing)?;
    Ok((
        record.revision,
        serde_json::from_value(record.value.clone())?,
    ))
}

async fn process_get(
    State(app): StateArg,
    Extension(actor): Actor,
    Path(id): Path<String>,
) -> Reply {
    let (_, job) = find_job(authorized(&app, &actor)?, &id).await?;
    Ok(Json(
        json!({"id":id,"config":job.config,"metadata":job.metadata,"desired":job.desired,"assignment":job.assignment}),
    ))
}

async fn process_config(
    State(app): StateArg,
    Extension(actor): Actor,
    Path(id): Path<String>,
) -> Reply {
    let (_, job) = find_job(authorized(&app, &actor)?, &id).await?;
    Ok(Json(job.config))
}

async fn save_job(cluster: &Cluster, key: String, job: Job, expected: u64) -> Reply {
    let config = job.config.clone();
    cluster
        .write(Action::Put {
            collection: Collection::Process,
            key,
            value: json!(job),
            expected: Some(expected),
        })
        .await?;
    Ok(Json(config))
}

async fn process_add(
    State(app): StateArg,
    Extension(actor): Actor,
    data: Result<Json<Value>, JsonRejection>,
) -> Reply {
    let cluster = authorized(&app, &actor)?;
    let config = body(data)?;
    let id = config["id"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 128 && !id.contains('/'))
        .ok_or(Failure::Invalid)?
        .to_string();
    let mut requirements: Requirements = config
        .get("placement")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    let config = normalize(config)?;
    reserve_config(&config, &mut requirements);
    let desired = config["autostart"].as_bool().unwrap_or(false);
    let job = Job {
        config,
        requirements,
        desired,
        metadata: Default::default(),
        assignment: None,
        epoch: 0,
    };
    save_job(cluster, id, job, 0).await
}

async fn process_update(
    State(app): StateArg,
    Extension(actor): Actor,
    Path(id): Path<String>,
    data: Result<Json<Value>, JsonRejection>,
) -> Reply {
    let cluster = authorized(&app, &actor)?;
    let input = body(data)?;
    let fields = input.as_object().ok_or(Failure::Invalid)?;
    let (revision, mut job) = find_job(cluster, &id).await?;
    if fields.get("id").is_some_and(|value| value != &id) {
        return Err(Failure::Invalid.into());
    }
    if let Some(value) = fields.get("placement") {
        job.requirements = serde_json::from_value::<Requirements>(value.clone())?;
    }
    let old = job.config.as_object_mut().ok_or(Failure::Invalid)?;
    old.extend(fields.clone());
    job.config = normalize(job.config)?;
    reserve_config(&job.config, &mut job.requirements);
    save_job(cluster, id, job, revision).await
}

async fn process_remove(
    State(app): StateArg,
    Extension(actor): Actor,
    Path(id): Path<String>,
) -> Reply {
    authorized(&app, &actor)?
        .write(Action::Delete {
            collection: Collection::Process,
            key: id,
            expected: None,
        })
        .await?;
    Ok(Json(json!("OK")))
}

#[derive(Deserialize)]
struct ProcessCommand {
    command: String,
}

async fn process_command(
    State(app): StateArg,
    Extension(actor): Actor,
    Path(id): Path<String>,
    data: Result<Json<ProcessCommand>, JsonRejection>,
) -> Reply {
    let cluster = authorized(&app, &actor)?;
    let (revision, mut job) = find_job(cluster, &id).await?;
    job.desired = match body(data)?.command.as_str() {
        "start" => true,
        "stop" => false,
        "restart" | "reload" => {
            job.config["castvane_restart_revision"] = json!(revision);
            true
        }
        _ => return Err(Failure::Invalid.into()),
    };
    let _ = save_job(cluster, id, job, revision).await?;
    Ok(Json(json!("OK")))
}

async fn process_metadata(
    State(app): StateArg,
    Extension(actor): Actor,
    Path(id): Path<String>,
) -> Reply {
    let (_, job) = find_job(authorized(&app, &actor)?, &id).await?;
    Ok(Json(json!(job.metadata)))
}

async fn process_metadata_get(
    State(app): StateArg,
    Extension(actor): Actor,
    Path((id, key)): Path<(String, String)>,
) -> Reply {
    let (_, mut job) = find_job(authorized(&app, &actor)?, &id).await?;
    job.metadata
        .remove(&key)
        .map(Json)
        .ok_or_else(ApiError::not_found)
}

async fn process_metadata_put(
    State(app): StateArg,
    Extension(actor): Actor,
    Path((id, key)): Path<(String, String)>,
    data: Result<Json<Value>, JsonRejection>,
) -> Reply {
    let cluster = authorized(&app, &actor)?;
    let (revision, mut job) = find_job(cluster, &id).await?;
    let value = body(data)?;
    job.metadata.insert(key, value.clone());
    let _ = save_job(cluster, id, job, revision).await?;
    Ok(Json(value))
}

async fn deployments(State(app): StateArg, Extension(actor): Actor) -> Reply {
    let state = authorized(&app, &actor)?.committed().await?;
    Ok(Json(json!(
        state
            .records(Collection::Process)
            .map(|(id, record)| json!({
                "id":id,"desired":record.value["desired"],"assignment":record.value["assignment"]
            }))
            .collect::<Vec<_>>()
    )))
}

fn reserve_config(config: &Value, requirements: &mut Requirements) {
    let cpu = config["limits"]["cpu_usage"].as_f64().unwrap_or(0.0);
    let memory = config["limits"]["memory_mbytes"].as_u64().unwrap_or(0);
    requirements.capacity.cpu = requirements.capacity.cpu.max((cpu * 10.0).ceil() as u64);
    requirements.capacity.memory = requirements
        .capacity
        .memory
        .max(memory.saturating_mul(1024 * 1024));
    requirements.capacity.processes = 1;
}

fn normalize(value: Value) -> Result<Value, ApiError> {
    let dto = super::process_dto::Configuration::from_patch(None, value, "$cluster", false)
        .map_err(|_| Failure::Invalid)?;
    let serialized = serde_json::to_value(&dto)?;
    let config = dto.into_config().0;
    if config.inputs.is_empty() || config.outputs.is_empty() || config.id.is_empty() {
        return Err(Failure::Invalid.into());
    }
    Ok(serialized)
}

async fn snapshot(State(app): StateArg, Extension(actor): Actor) -> Reply {
    let cluster = authorized(&app, &actor)?;
    cluster.quorum().await?;
    let metrics = cluster.raft.metrics();
    let metrics = metrics.borrow();
    Ok(Json(
        json!({"snapshot_index":metrics.snapshot.map(|log|log.index),"applied_index":metrics.last_applied.map(|log|log.index)}),
    ))
}

async fn process_state(
    State(app): StateArg,
    Extension(actor): Actor,
    Path(id): Path<String>,
) -> Reply {
    let cluster = authorized(&app, &actor)?;
    let (_, job) = find_job(cluster, &id).await?;
    let report = if let Some(assigned) = &job.assignment {
        cluster
            .report(assigned.node)
            .await
            .and_then(|mut report| report.runtime.remove(&id))
            .filter(|report| report.epoch == assigned.epoch)
    } else {
        None
    };
    Ok(Json(json!({
        "order": if job.desired {"start"} else {"stop"},
        "exec":report.as_ref().map(|runtime|runtime.state.as_str()),
        "progress":report.as_ref().map(|runtime|json!({"frame":runtime.frame})),
        "assignment":job.assignment,
        "placement":if report.is_some() {"observed"} else if job.assignment.is_some() {"assigned"} else if job.desired {"pending"} else {"unassigned"}
    })))
}

#[derive(Deserialize)]
struct Relocation {
    id: String,
    node: Option<u64>,
}

async fn relocate(
    State(app): StateArg,
    Extension(actor): Actor,
    data: Result<Json<Relocation>, JsonRejection>,
) -> Reply {
    let cluster = authorized(&app, &actor)?;
    let relocation = body(data)?;
    if relocation
        .node
        .is_some_and(|node| !cluster.peers.contains_key(&node))
    {
        return Err(Failure::Invalid.into());
    }
    let (revision, mut job) = find_job(cluster, &relocation.id).await?;
    job.requirements.node = relocation.node;
    let _ = save_job(cluster, relocation.id, job, revision).await?;
    Ok(Json(json!("OK")))
}

#[derive(Deserialize)]
struct ZoneWrite {
    #[serde(default)]
    id: String,
    #[serde(default)]
    limits: crate::zone::Resources,
}

async fn zones(State(app): StateArg, Extension(actor): Actor) -> Reply {
    let state = authorized(&app, &actor)?.committed().await?;
    let result: Vec<_> = state
        .records(Collection::Zone)
        .map(|(_, record)| &record.value)
        .collect();
    Ok(Json(json!(result)))
}

async fn zone(State(app): StateArg, Extension(actor): Actor, Path(id): Path<String>) -> Reply {
    let state = authorized(&app, &actor)?.committed().await?;
    Ok(Json(
        state
            .get(Collection::Zone, &id)
            .ok_or(Failure::Missing)?
            .value
            .clone(),
    ))
}

async fn zone_add(
    State(app): StateArg,
    Extension(actor): Actor,
    data: Result<Json<ZoneWrite>, JsonRejection>,
) -> Reply {
    let input = body(data)?;
    zone_write(authorized(&app, &actor)?, input.id.clone(), input, 0).await
}

async fn zone_update(
    State(app): StateArg,
    Extension(actor): Actor,
    Path(id): Path<String>,
    data: Result<Json<ZoneWrite>, JsonRejection>,
) -> Reply {
    let cluster = authorized(&app, &actor)?;
    let state = cluster.committed().await?;
    let revision = state
        .get(Collection::Zone, &id)
        .ok_or(Failure::Missing)?
        .revision;
    zone_write(cluster, id, body(data)?, revision).await
}

async fn zone_write(cluster: &Cluster, id: String, input: ZoneWrite, revision: u64) -> Reply {
    if !crate::zone::valid_id(&id)
        || !input.limits.valid()
        || (!input.id.is_empty() && input.id != id)
    {
        return Err(Failure::Invalid.into());
    }
    let value = json!({"id":id,"limits":input.limits});
    cluster
        .write(Action::Put {
            collection: Collection::Zone,
            key: id,
            value: value.clone(),
            expected: Some(revision),
        })
        .await?;
    Ok(Json(value))
}

async fn zone_delete(
    State(app): StateArg,
    Extension(actor): Actor,
    Path(id): Path<String>,
) -> Reply {
    authorized(&app, &actor)?
        .write(Action::Delete {
            collection: Collection::Zone,
            key: id,
            expected: None,
        })
        .await?;
    Ok(Json(json!("OK")))
}

type Rules = Vec<crate::media::alias::Rule>;

async fn alias_current(cluster: &Cluster) -> Result<(u64, Rules), ApiError> {
    let state = cluster.committed().await?;
    match state.get(Collection::Alias, "rules") {
        Some(record) => Ok((
            record.revision,
            serde_json::from_value(record.value.clone())?,
        )),
        None => Ok((0, Vec::new())),
    }
}

async fn aliases(State(app): StateArg, Extension(actor): Actor) -> Reply {
    let (_, rules) = alias_current(authorized(&app, &actor)?).await?;
    Ok(Json(json!(rules)))
}

async fn alias_save(cluster: &Cluster, revision: u64, rules: Rules) -> Reply {
    crate::media::alias::Aliases::validate_rules(rules.clone()).map_err(|_| Failure::Invalid)?;
    cluster
        .write(Action::Put {
            collection: Collection::Alias,
            key: "rules".into(),
            value: json!(rules),
            expected: Some(revision),
        })
        .await?;
    Ok(Json(json!("OK")))
}

async fn aliases_replace(
    State(app): StateArg,
    Extension(actor): Actor,
    data: Result<Json<Rules>, JsonRejection>,
) -> Reply {
    let cluster = authorized(&app, &actor)?;
    let (revision, _) = alias_current(cluster).await?;
    alias_save(cluster, revision, body(data)?).await
}

async fn aliases_add(
    State(app): StateArg,
    Extension(actor): Actor,
    data: Result<Json<crate::media::alias::Rule>, JsonRejection>,
) -> Reply {
    let cluster = authorized(&app, &actor)?;
    let (revision, mut rules) = alias_current(cluster).await?;
    rules.push(body(data)?);
    alias_save(cluster, revision, rules).await
}

#[derive(Deserialize)]
struct AliasQuery {
    pattern: String,
}

async fn alias_get(
    State(app): StateArg,
    Extension(actor): Actor,
    Query(query): Query<AliasQuery>,
) -> Reply {
    let (_, rules) = alias_current(authorized(&app, &actor)?).await?;
    let rule = rules
        .into_iter()
        .find(|rule| rule.pattern == query.pattern)
        .ok_or(Failure::Missing)?;
    Ok(Json(json!(rule)))
}

async fn alias_update(
    State(app): StateArg,
    Extension(actor): Actor,
    Query(query): Query<AliasQuery>,
    data: Result<Json<crate::media::alias::Rule>, JsonRejection>,
) -> Reply {
    let cluster = authorized(&app, &actor)?;
    let (revision, mut rules) = alias_current(cluster).await?;
    let rule = rules
        .iter_mut()
        .find(|rule| rule.pattern == query.pattern)
        .ok_or(Failure::Missing)?;
    *rule = body(data)?;
    alias_save(cluster, revision, rules).await
}

async fn alias_delete(
    State(app): StateArg,
    Extension(actor): Actor,
    Query(query): Query<AliasQuery>,
) -> Reply {
    let cluster = authorized(&app, &actor)?;
    let (revision, mut rules) = alias_current(cluster).await?;
    let index = rules
        .iter()
        .position(|rule| rule.pattern == query.pattern)
        .ok_or(Failure::Missing)?;
    rules.remove(index);
    alias_save(cluster, revision, rules).await
}

use crate::{
    cluster::model::ClusterAccount,
    iam::{Account, Rule},
};

#[derive(Default, Deserialize)]
#[serde(default)]
struct UserWrite {
    name: String,
    alias: Option<String>,
    active: Option<bool>,
    superuser: Option<bool>,
    auth: UserAuth,
    policies: Option<Vec<Rule>>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct UserAuth {
    api: UserPassword,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct UserPassword {
    userpass: String,
}

fn public_identity(identity: ClusterAccount) -> Value {
    super::identity::scrubbed(
        &identity.account.name,
        Some(&identity.account),
        identity.rules,
        false,
    )
}

fn policies(rules: &mut [Rule]) -> Result<(), ApiError> {
    if rules.len() > 64 {
        return Err(Failure::Invalid.into());
    }
    for rule in rules {
        rule.normalize().map_err(ApiError::bad_request)?;
    }
    Ok(())
}

async fn users(State(app): StateArg, Extension(actor): Actor) -> Reply {
    let state = authorized(&app, &actor)?.committed().await?;
    let users: Vec<_> = state
        .records(Collection::Identity)
        .map(|(_, record)| serde_json::from_value(record.value.clone()).map(public_identity))
        .collect::<Result<_, _>>()?;
    Ok(Json(json!(users)))
}

async fn find_identity(cluster: &Cluster, name: &str) -> Result<(u64, ClusterAccount), ApiError> {
    let state = cluster.committed().await?;
    let record = state
        .get(Collection::Identity, name)
        .ok_or(Failure::Missing)?;
    Ok((
        record.revision,
        serde_json::from_value(record.value.clone())?,
    ))
}

async fn user_get(
    State(app): StateArg,
    Extension(actor): Actor,
    Path(name): Path<String>,
) -> Reply {
    let (_, identity) = find_identity(authorized(&app, &actor)?, &name).await?;
    Ok(Json(public_identity(identity)))
}

async fn policy_list(State(app): StateArg, Extension(actor): Actor) -> Reply {
    let state = authorized(&app, &actor)?.committed().await?;
    let entries = state
        .records(Collection::Identity)
        .map(|(name, record)| (name, &record.value["rules"]));
    Ok(Json(json!(
        entries.collect::<std::collections::BTreeMap<_, _>>()
    )))
}

async fn user_add(
    State(app): StateArg,
    Extension(actor): Actor,
    data: Result<Json<UserWrite>, JsonRejection>,
) -> Reply {
    let input = body(data)?;
    let name = input.name.clone();
    write_identity(&app, authorized(&app, &actor)?, name, input, None).await
}

async fn user_update(
    State(app): StateArg,
    Extension(actor): Actor,
    Path(name): Path<String>,
    data: Result<Json<UserWrite>, JsonRejection>,
) -> Reply {
    let cluster = authorized(&app, &actor)?;
    let old = find_identity(cluster, &name).await?;
    write_identity(&app, cluster, name, body(data)?, Some(old)).await
}

async fn write_identity(
    app: &App,
    cluster: &Cluster,
    name: String,
    mut input: UserWrite,
    old: Option<(u64, ClusterAccount)>,
) -> Reply {
    let _guard = app.writes.lock().await;
    let (_, settings) = super::read_settings(app).await?;
    if !crate::iam::valid_name(&name)
        || name == settings.api.auth.username
        || (!input.name.is_empty() && input.name != name)
        || input.alias.as_ref().is_some_and(|alias| alias.len() > 256)
    {
        return Err(Failure::Invalid.into());
    }
    let now = crate::engine::unix_now();
    let (revision, mut identity) = old.unwrap_or_else(|| {
        (
            0,
            ClusterAccount {
                account: Account {
                    name: name.clone(),
                    alias: String::new(),
                    password_hash: String::new(),
                    administrator: false,
                    active: true,
                    generation: String::new(),
                    created_at: now,
                    updated_at: now,
                },
                rules: Vec::new(),
            },
        )
    });
    let password = zeroize::Zeroizing::new(std::mem::take(&mut input.auth.api.userpass));
    if revision == 0 || !password.is_empty() {
        crate::state::validate_password(&password).map_err(|_| Failure::Invalid)?;
        identity.account.password_hash = crate::auth::hash_password(password)
            .await
            .map_err(|_| Failure::Unavailable)?;
    }
    if let Some(value) = input.alias {
        identity.account.alias = value;
    }
    if let Some(value) = input.active {
        identity.account.active = value;
    }
    if let Some(value) = input.superuser {
        identity.account.administrator = value;
    }
    if let Some(mut rules) = input.policies {
        policies(&mut rules)?;
        identity.rules = rules;
    }
    identity.account.updated_at = now;
    identity.account.generation = uuid::Uuid::new_v4().to_string();
    save_identity(cluster, name, revision, identity).await
}

async fn save_identity(
    cluster: &Cluster,
    name: String,
    revision: u64,
    identity: ClusterAccount,
) -> Reply {
    cluster
        .write(Action::Put {
            collection: Collection::Identity,
            key: name,
            value: serde_json::to_value(&identity)?,
            expected: Some(revision),
        })
        .await?;
    Ok(Json(public_identity(identity)))
}

async fn user_policies(
    State(app): StateArg,
    Extension(actor): Actor,
    Path(name): Path<String>,
    data: Result<Json<Vec<Rule>>, JsonRejection>,
) -> Reply {
    let cluster = authorized(&app, &actor)?;
    let (revision, mut identity) = find_identity(cluster, &name).await?;
    identity.rules = body(data)?;
    policies(&mut identity.rules)?;
    let response = json!(identity.rules);
    let _ = save_identity(cluster, name, revision, identity).await?;
    Ok(Json(response))
}

async fn user_remove(
    State(app): StateArg,
    Extension(actor): Actor,
    Path(name): Path<String>,
) -> Reply {
    authorized(&app, &actor)?
        .write(Action::Delete {
            collection: Collection::Identity,
            key: name,
            expected: None,
        })
        .await?;
    Ok(Json(json!("OK")))
}

async fn node_files(
    State(app): StateArg,
    Extension(actor): Actor,
    Path((id, storage)): Path<(u64, String)>,
    request: axum::extract::Request,
) -> Result<axum::response::Response, ApiError> {
    authorized(&app, &actor)?
        .remote_file(id, &storage, None, request)
        .await
}

async fn node_file(
    State(app): StateArg,
    Extension(actor): Actor,
    Path((id, storage, path)): Path<(u64, String, String)>,
    request: axum::extract::Request,
) -> Result<axum::response::Response, ApiError> {
    authorized(&app, &actor)?
        .remote_file(id, &storage, Some(&path), request)
        .await
}
