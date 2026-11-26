use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{error::ApiError, process_dto as dto};
use crate::{
    App,
    auth::Identity,
    engine::{self, Control, Entry, Error, Key, Run},
};

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/skills", get(skills))
        .route("/skills/reload", get(reload_skills))
        .route("/process", get(list).post(create))
        .route("/process/validate", post(validate))
        .route("/process/probe", post(probe_config))
        .route("/process/{id}", get(get_job).put(update).delete(delete))
        .route("/process/{id}/config", get(get_config))
        .route("/process/{id}/state", get(get_state))
        .route("/process/{id}/command", axum::routing::put(command))
        .route("/process/{id}/report", get(get_report).put(set_report))
        .route("/process/{id}/probe", get(probe_job))
        .route("/process/{id}/metadata", get(process_metadata))
        .route(
            "/process/{id}/metadata/{key}",
            get(process_metadata_key).put(put_process_metadata),
        )
        .route("/metadata", get(core_metadata))
        .route(
            "/metadata/{key}",
            get(core_metadata_key).put(put_core_metadata),
        )
        .route("/report/process", get(search))
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        let message = error.public();
        match error {
            Error::Invalid(_) => Self::bad_request(message),
            Error::Forbidden(_) => Self::forbidden(message),
            Error::Missing(_) => Self::Http(StatusCode::NOT_FOUND, message),
            Error::Conflict(_) => Self::Http(StatusCode::CONFLICT, message),
            Error::Unavailable(_) => Self::Http(StatusCode::SERVICE_UNAVAILABLE, message),
            Error::Internal => Self::Internal(message),
        }
    }
}

#[derive(Clone)]
pub struct Caller(pub String);
type Response = Result<Json<Value>, ApiError>;
type QueryArgs = Result<Query<Params>, QueryRejection>;

#[derive(Default, Deserialize)]
#[serde(default)]
struct Params {
    domain: String,
    filter: String,
    id: String,
    reference: String,
    idpattern: String,
    refpattern: String,
    ownerpattern: String,
    domainpattern: String,
    zonepattern: String,
    created_at: String,
    exited_at: String,
    from: String,
    to: String,
    state: String,
    force: String,
    purge: Option<String>,
    offset: usize,
    limit: Option<usize>,
}

impl Params {
    fn key(&self, id: String) -> Key {
        (self.domain.clone(), id)
    }

    fn includes(&self, field: &str) -> bool {
        let mut fields = self
            .filter
            .split(',')
            .filter(|part| !part.is_empty())
            .peekable();
        fields.peek().is_none() || fields.any(|part| part == field)
    }

    fn matches(&self, config: &engine::Config) -> bool {
        let ids: Vec<_> = self.id.split(',').filter(|id| !id.is_empty()).collect();
        let exact = if ids.is_empty() {
            self.reference.is_empty() || self.reference == config.reference
        } else {
            ids.contains(&config.id.as_str())
        };
        exact
            && [
                (&self.idpattern, &config.id),
                (&self.refpattern, &config.reference),
                (&self.ownerpattern, &config.owner),
                (&self.domainpattern, &config.domain),
                (&self.zonepattern, &config.zone),
            ]
            .into_iter()
            .all(|(pattern, value)| {
                pattern.is_empty()
                    || glob::Pattern::new(pattern).is_ok_and(|pattern| pattern.matches(value))
            })
    }
}

fn query(args: QueryArgs) -> Result<Params, ApiError> {
    args.map(|Query(params)| params)
        .map_err(|_| ApiError::bad_request("invalid process query"))
}

fn body<T>(data: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    data.map(|Json(body)| body)
        .map_err(|_| ApiError::bad_request("invalid JSON"))
}

fn serialize(value: impl serde::Serialize) -> Response {
    Ok(Json(serde_json::to_value(value)?))
}

fn ok() -> Response {
    Ok(Json(json!("OK")))
}

async fn skills(State(app): State<Arc<App>>) -> Response {
    inventory(&app, false).await
}

async fn reload_skills(State(app): State<Arc<App>>) -> Response {
    inventory(&app, true).await
}

async fn inventory(app: &App, reload: bool) -> Response {
    let inventory = app.processes.skills(reload).await?;
    let (max, delegated) = app.processes.diagnostics();
    let mut response = serde_json::to_value(inventory.as_ref())?;
    response["resource_monitor"] = json!({
        "cpu_memory_sampling": true, "max_concurrent_processes": max,
        "cgroup_root_configured": delegated, "gpu_limits_enforced": false
    });
    Ok(Json(response))
}

async fn create(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    Extension(actor): Extension<Identity>,
    data: Result<Json<Value>, JsonRejection>,
) -> Response {
    let data = body(data)?;
    let config = app
        .processes
        .edit(None, false, move |previous| {
            write_config(previous, data, &caller, &actor)
        })
        .await?;
    serialize(dto::Configuration::from_config(&config))
}

async fn update(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    args: QueryArgs,
    Extension(caller): Extension<Caller>,
    Extension(actor): Extension<Identity>,
    data: Result<Json<Value>, JsonRejection>,
) -> Response {
    let params = query(args)?;
    let data = body(data)?;
    let config = app
        .processes
        .edit(
            Some(params.key(id)),
            params.force == "restart",
            move |previous| write_config(previous, data, &caller, &actor),
        )
        .await?;
    serialize(dto::Configuration::from_config(&config))
}

fn write_config(
    prior: Option<&engine::Config>,
    value: Value,
    caller: &Caller,
    actor: &Identity,
) -> Result<(engine::Config, engine::Metadata), Error> {
    let (mut config, metadata) =
        dto::Configuration::from_patch(prior, value, &caller.0, true)?.into_config();
    if !actor.administrator() {
        if !actor.allows(&config.domain, "process", &config.id, "write")
            || (crate::zone::canonical(&config.zone) != crate::zone::DEFAULT
                && !actor.allows(&config.domain, "zone", &config.zone, "write"))
        {
            return Err(Error::Forbidden("access denied"));
        }
        config.owner = prior.map_or_else(|| caller.0.clone(), |config| config.owner.clone());
    }
    config.scoped = !actor.administrator() || prior.is_some_and(|config| config.scoped);
    Ok((config, metadata))
}

async fn delete(State(app): State<Arc<App>>, Path(id): Path<String>, args: QueryArgs) -> Response {
    let params = query(args)?;
    let purge = params
        .purge
        .as_deref()
        .is_none_or(|value| matches!(value, "true" | "True" | "TRUE" | "t" | "T" | "1"));
    app.processes.delete(params.key(id), purge).await?;
    ok()
}

#[derive(Deserialize)]
struct Command {
    command: String,
    #[serde(default)]
    reason: String,
}

async fn command(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    args: QueryArgs,
    data: Result<Json<Command>, JsonRejection>,
) -> Response {
    let request = body(data)?;
    let command = match request.command.as_str() {
        "start" => Control::Start,
        "stop" => Control::Stop,
        "restart" => Control::Restart,
        "reload" => Control::Reload,
        _ => {
            return Err(ApiError::bad_request(
                "unknown command provided: known commands are: start, stop, reload, restart",
            ));
        }
    };
    app.processes
        .command(query(args)?.key(id), command, request.reason)
        .await?;
    ok()
}

async fn get_config(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    args: QueryArgs,
) -> Response {
    let job = app.processes.get(&query(args)?.key(id)).await?.job();
    serialize(dto::Configuration::from_config(&job.config))
}

async fn get_state(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    args: QueryArgs,
) -> Response {
    let entry = app.processes.get(&query(args)?.key(id)).await?;
    let job = entry.job();
    Ok(Json(dto::state(
        &job.config,
        job.desired,
        &entry.snapshot(),
    )))
}

async fn get_job(State(app): State<Arc<App>>, Path(id): Path<String>, args: QueryArgs) -> Response {
    let params = query(args)?;
    let entry = app.processes.get(&params.key(id)).await?;
    let (_, settings) = super::read_settings(&app).await?;
    Ok(Json(
        process_view(&app, &entry, &params, &settings.id).await?,
    ))
}

async fn list(
    State(app): State<Arc<App>>,
    Extension(actor): Extension<Identity>,
    args: QueryArgs,
) -> Response {
    let params = query(args)?;
    let (_, settings) = super::read_settings(&app).await?;
    let mut items = Vec::new();
    let mut used = 0;
    let entries = app.processes.list().await;
    for entry in entries
        .into_iter()
        .filter(|entry| {
            let job = entry.job();
            params.matches(&job.config)
                && actor.allows(&job.config.domain, "process", &job.config.id, "read")
        })
        .skip(params.offset)
        .take(params.limit.unwrap_or(engine::MAX_JOBS))
    {
        let item = process_view(&app, &entry, &params, &settings.id).await?;
        used += item.to_string().len();
        if used > 16 * 1024 * 1024 {
            return Err(ApiError::Http(
                StatusCode::PAYLOAD_TOO_LARGE,
                "use a process filter or pagination for this response",
            ));
        }
        items.push(item);
    }
    Ok(Json(json!(items)))
}

async fn process_view(
    app: &App,
    entry: &Entry,
    params: &Params,
    core_id: &str,
) -> Result<Value, ApiError> {
    let job = entry.job();
    let config = &job.config;
    let mut process = json!({
        "id":config.id,"owner":config.owner,"domain":config.domain,"type":"ffmpeg","reference":config.reference,
        "core_id":core_id,"created_at":job.created,"updated_at":job.updated,
    });
    if params.includes("config") {
        process["config"] = serde_json::to_value(dto::Configuration::from_config(config))?;
    }
    if params.includes("state") || params.includes("report") {
        let runtime = entry.snapshot();
        if params.includes("state") {
            process["state"] = dto::state(config, job.desired, &runtime);
        }
        if params.includes("report") {
            let history = app.processes.history(&job.uid).await?;
            process["report"] = dto::report(&runtime.active, &history, &config.limits);
        }
    }
    if params.includes("metadata") {
        let metadata = app.processes.metadata(Some(&job.uid)).await?;
        if !metadata.is_empty() {
            process["metadata"] = json!(metadata);
        }
    }
    Ok(process)
}

fn timestamp(value: &str) -> Result<Option<i64>, ApiError> {
    if value.is_empty() {
        Ok(None)
    } else {
        value
            .parse()
            .map(Some)
            .map_err(|_| ApiError::bad_request("invalid unix timestamp"))
    }
}

async fn get_report(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    args: QueryArgs,
) -> Response {
    let params = query(args)?;
    let from = timestamp(&params.created_at)?;
    let to = timestamp(&params.exited_at)?;
    let entry = app.processes.get(&params.key(id)).await?;
    let job = entry.job();
    let runtime = entry.snapshot();
    let mut history = app.processes.history(&job.uid).await?;
    if from.is_none() && to.is_none() {
        return Ok(Json(dto::report(
            &runtime.active,
            &history,
            &job.config.limits,
        )));
    }
    let matches = |run: &Run| match (from, to) {
        (Some(from), Some(to)) => run.created >= from && run.exited <= to,
        (Some(from), None) => run.created == from,
        (None, Some(to)) => run.exited == to,
        _ => true,
    };
    history.retain(matches);
    let active = if matches(&runtime.active) && dto::is_current(&runtime) {
        Some(runtime.active)
    } else {
        None
    };
    if active.is_none() && history.is_empty() {
        return Err(ApiError::Http(
            StatusCode::NOT_FOUND,
            "No matching reports found",
        ));
    }
    history.sort_by_key(|run| std::cmp::Reverse(run.created));
    let blank = Run {
        created: 0,
        ..Run::default()
    };
    Ok(Json(dto::report(
        active.as_ref().unwrap_or(&blank),
        &history,
        &job.config.limits,
    )))
}

async fn set_report(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    args: QueryArgs,
    data: Result<Json<dto::ImportReport>, JsonRejection>,
) -> Response {
    let current = app.processes.get(&query(args)?.key(id)).await?;
    let history = body(data)?.history()?;
    app.processes
        .set_history(&current.job().uid, &history)
        .await?;
    ok()
}

async fn search(
    State(app): State<Arc<App>>,
    Extension(actor): Extension<Identity>,
    args: QueryArgs,
) -> Response {
    let params = query(args)?;
    let from = timestamp(&params.from)?;
    let to = timestamp(&params.to)?;
    let mut items = Vec::new();
    for entry in app.processes.list().await {
        let job = entry.job();
        if !params.matches(&job.config)
            || !actor.allows(&job.config.domain, "process", &job.config.id, "read")
        {
            continue;
        }
        let history = app.processes.history(&job.uid).await?;
        for run in history {
            if (params.state.is_empty() || params.state == run.state)
                && from.is_none_or(|from| run.exited >= from)
                && to.is_none_or(|to| run.exited <= to)
            {
                items.push((run.exited,json!({
                    "id":job.config.id,"domain":job.config.domain,"reference":job.config.reference,
                    "exit_state":run.state,"created_at":run.created,"exited_at":run.exited,
                })));
            }
        }
    }
    items.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    let items: Vec<_> = items
        .into_iter()
        .skip(params.offset)
        .take(params.limit.unwrap_or(usize::MAX))
        .map(|(_, value)| value)
        .collect();
    Ok(Json(json!(items)))
}

async fn validate(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    data: Result<Json<Value>, JsonRejection>,
) -> Response {
    let config = dto::Configuration::from_patch(None, body(data)?, &caller.0, false)?;
    let copy = serde_json::to_value(&config)?;
    app.processes.validate(&config.into_config().0).await?;
    Ok(Json(copy))
}

async fn probe_job(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    args: QueryArgs,
) -> Response {
    let entry = app.processes.get(&query(args)?.key(id)).await?;
    Ok(Json(dto::probe(
        app.processes.probe(&entry.job().config).await?,
    )))
}

async fn probe_config(
    State(app): State<Arc<App>>,
    Extension(caller): Extension<Caller>,
    data: Result<Json<Value>, JsonRejection>,
) -> Response {
    let (config, _) =
        dto::Configuration::from_patch(None, body(data)?, &caller.0, false)?.into_config();
    Ok(Json(dto::probe(app.processes.probe(&config).await?)))
}

async fn process_metadata(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    args: QueryArgs,
) -> Response {
    let job = app.processes.get(&query(args)?.key(id)).await?.job();
    metadata(&app, Some(&job.uid), None).await
}

async fn process_metadata_key(
    State(app): State<Arc<App>>,
    Path((id, key)): Path<(String, String)>,
    args: QueryArgs,
) -> Response {
    let job = app.processes.get(&query(args)?.key(id)).await?.job();
    metadata(&app, Some(&job.uid), Some(&key)).await
}

async fn core_metadata(State(app): State<Arc<App>>) -> Response {
    metadata(&app, None, None).await
}

async fn core_metadata_key(State(app): State<Arc<App>>, Path(key): Path<String>) -> Response {
    metadata(&app, None, Some(&key)).await
}

async fn metadata(app: &App, uid: Option<&str>, key: Option<&str>) -> Response {
    let mut metadata = app.processes.metadata(uid).await?;
    match key {
        Some(key) => metadata.remove(key).map(Json).ok_or(ApiError::Http(
            StatusCode::NOT_FOUND,
            "unknown metadata key",
        )),
        None if metadata.is_empty() => Ok(Json(Value::Null)),
        None => serialize(metadata),
    }
}

async fn put_process_metadata(
    State(app): State<Arc<App>>,
    Path((id, key)): Path<(String, String)>,
    args: QueryArgs,
    data: Result<Json<Value>, JsonRejection>,
) -> Response {
    let job = app.processes.get(&query(args)?.key(id)).await?.job();
    let value = body(data)?;
    app.processes
        .set_metadata(Some(&job.uid), &key, &value)
        .await?;
    Ok(Json(value))
}

async fn put_core_metadata(
    State(app): State<Arc<App>>,
    Path(key): Path<String>,
    data: Result<Json<Value>, JsonRejection>,
) -> Response {
    let value = body(data)?;
    app.processes.set_metadata(None, &key, &value).await?;
    Ok(Json(value))
}
