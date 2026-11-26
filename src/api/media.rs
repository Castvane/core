use std::{collections::BTreeMap, sync::Arc};

use axum::{
    Extension, Json, Router,
    extract::{Path, Request, State},
    http::{StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, put},
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiError, media_http};
use crate::{
    App,
    media::{
        Mount,
        alias::Rule,
        sessions::TokenRequest,
        storage::{Error, Key, Meta, now_secs},
        transport::{Connection, Transport},
    },
};

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        match error {
            Error::Invalid => Self::bad_request("invalid media path or setting"),
            Error::Missing => Self::Http(StatusCode::NOT_FOUND, "file not found"),
            Error::Forbidden => Self::forbidden("access denied"),
            Error::Full => Self::Http(
                StatusCode::INSUFFICIENT_STORAGE,
                "storage or connection capacity exceeded",
            ),
            Error::Changed => Self::Http(
                StatusCode::PRECONDITION_FAILED,
                "object changed during request",
            ),
            Error::Unavailable => {
                Self::Http(StatusCode::SERVICE_UNAVAILABLE, "media service unavailable")
            }
            Error::Interrupted => {
                Self::Http(StatusCode::REQUEST_TIMEOUT, "media transfer interrupted")
            }
        }
    }
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/fs", get(filesystems).put(operation))
        .route("/fs/{storage}", get(files).delete(delete_many))
        .route(
            "/fs/{storage}/{*path}",
            get(get_file).put(put_file).delete(delete_file),
        )
        .route("/fs/{storage}/", get(files).delete(delete_many))
        .route("/rtmp", get(rtmp_list))
        .route("/rtmp/channels", get(rtmp_channels))
        .route("/rtmp/disconnect", put(rtmp_disconnect))
        .route("/srt", get(srt_list))
        .route("/srt/channels", get(srt_channels))
        .route("/srt/disconnect", put(srt_disconnect))
        .route("/session", get(session_summary))
        .route("/session/active", get(session_active))
        .route("/session/token/{username}", put(tokens))
        .route("/alias", get(aliases).post(alias_add).put(alias_replace))
        .route(
            "/alias/rule",
            get(alias_get).put(alias_update).delete(alias_delete),
        )
        .route("/process/{id}/playout/{input}/status", get(playout))
        .route("/process/{id}/playout/{input}/reopen", get(playout))
        .route("/process/{id}/playout/{input}/stream", put(playout))
        .route(
            "/process/{id}/playout/{input}/keyframe/{*name}",
            get(playout),
        )
        .route(
            "/process/{id}/playout/{input}/errorframe/encode",
            get(playout),
        )
        .route(
            "/process/{id}/playout/{input}/errorframe/{*name}",
            put(playout).post(playout),
        )
}

fn json_body<T>(
    value: Result<Json<T>, axum::extract::rejection::JsonRejection>,
) -> Result<T, ApiError> {
    value
        .map(|Json(v)| v)
        .map_err(|_| ApiError::bad_request("invalid JSON"))
}

pub(super) fn query(request: &Request, field: &str) -> Option<String> {
    query_uri(request.uri(), field)
}

pub(super) fn query_uri(uri: &Uri, field: &str) -> Option<String> {
    url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .take(40)
        .find(|(key, _)| key == field)
        .map(|(_, value)| value.into_owned())
}

type JsonResponse = Result<Json<Value>, ApiError>;

async fn filesystems(State(app): State<Arc<App>>, request: Request) -> JsonResponse {
    let detail = query(&request, "usage").as_deref() == Some("true");
    let mut result = Vec::new();
    for mount in app.media.mounts.values() {
        let mut entry = json!({"name":mount.name,"type":mount.kind,"mount":mount.prefix});
        if detail {
            entry["usage"] = serde_json::to_value(mount.backend.usage().await?)?;
            entry["capabilities"] = json!({"range":true,"streaming":true,"atomic_replace":true,
                "persistent":mount.kind!="mem","multipart":mount.kind=="s3"});
        }
        result.push(entry);
    }
    Ok(Json(json!(result)))
}

async fn files(
    State(app): State<Arc<App>>,
    Path(storage): Path<String>,
    Extension(actor): Extension<crate::auth::Identity>,
    request: Request,
) -> JsonResponse {
    let mount = app.media.get(&storage)?;
    let domain = query(&request, "domain").unwrap_or_default();
    let files = select(mount, request.uri(), false).await?;
    Ok(Json(json!(
        files
            .iter()
            .filter(|meta| actor.allows(
                &domain,
                "fs",
                &format!("/{storage}/{}", meta.key.as_str()),
                "read"
            ))
            .map(|meta| json!({"name":format!("/{}",meta.key.as_str()),
        "size_bytes":meta.size,"last_modified":now_secs(meta.modified)}))
            .collect::<Vec<_>>()
    )))
}

pub(crate) async fn select(
    mount: &Mount,
    uri: &Uri,
    deleting: bool,
) -> Result<Vec<Meta>, ApiError> {
    let pattern = query_uri(uri, "glob").or_else(|| query_uri(uri, "pattern"));
    if deleting && pattern.as_ref().is_none_or(|v| v.is_empty()) {
        return Err(ApiError::bad_request("a glob pattern is required"));
    }
    let pattern = pattern
        .filter(|v| !v.is_empty())
        .map(|v| glob::Pattern::new(v.trim_start_matches('/')))
        .transpose()
        .map_err(|_| Error::Invalid)?;
    let number = |name: &str| -> Result<Option<u64>, ApiError> {
        query_uri(uri, name)
            .filter(|v| !v.is_empty())
            .map(|value| {
                value
                    .parse()
                    .map_err(|_| ApiError::bad_request("invalid file filter"))
            })
            .transpose()
    };
    let min = number("size_min")?;
    let max = number("size_max")?;
    let from = number("lastmod_start")?;
    let to = number("lastmod_end")?;
    let mut items = mount.backend.list().await?;
    items.retain(|meta| {
        pattern
            .as_ref()
            .is_none_or(|p| p.matches(meta.key.as_str()))
            && min.is_none_or(|n| meta.size >= n)
            && max.is_none_or(|n| meta.size <= n)
            && from.is_none_or(|n| now_secs(meta.modified) >= n)
            && to.is_none_or(|n| now_secs(meta.modified) <= n)
    });
    match query_uri(uri, "sort").as_deref() {
        Some("size") => items.sort_by_key(|m| m.size),
        Some("lastmod") => items.sort_by_key(|m| m.modified),
        Some("name") => items.sort_by(|a, b| a.key.cmp(&b.key)),
        _ => {}
    }
    if query_uri(uri, "order").as_deref() == Some("desc") {
        items.reverse();
    }
    Ok(items)
}

async fn delete_many(
    State(app): State<Arc<App>>,
    Path(storage): Path<String>,
    request: Request,
) -> JsonResponse {
    let mount = app.media.get(&storage)?;
    let mut deleted = Vec::new();
    for item in select(mount, request.uri(), true).await? {
        mount.backend.delete(&item.key).await?;
        let name = format!("/{}", item.key.as_str());
        app.ops.emit("media", "remove", &name, &storage);
        deleted.push(name);
    }
    Ok(Json(json!(deleted)))
}

async fn get_file(
    State(app): State<Arc<App>>,
    Path((storage, path)): Path<(String, String)>,
    request: Request,
) -> Result<Response, ApiError> {
    let mount = app.media.get(&storage)?;
    let params = media_http::parameters(request.uri().query());
    media_http::serve(
        &app.media,
        mount,
        Key::parse(&path)?,
        request.headers(),
        request.method(),
        params,
        None,
    )
    .await
}

async fn put_file(
    State(app): State<Arc<App>>,
    Path((storage, path)): Path<(String, String)>,
    request: Request,
) -> Result<Response, ApiError> {
    let mount = app.media.get(&storage)?;
    let key = Key::parse(&path)?;
    let response = media_http::write(&app.media, mount, &key, request.into_body()).await?;
    app.ops.emit(
        "media",
        if response.status() == StatusCode::CREATED {
            "add"
        } else {
            "replace"
        },
        &format!("/{}", key.as_str()),
        &storage,
    );
    Ok(response)
}

async fn delete_file(
    State(app): State<Arc<App>>,
    Path((storage, path)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let key = Key::parse(&path)?;
    app.media.get(&storage)?.backend.delete(&key).await?;
    let name = format!("/{}", key.as_str());
    app.ops.emit("media", "remove", &name, &storage);
    Ok(format!("Deleted: {name}").into_response())
}

#[derive(Deserialize)]
struct Operation {
    operation: String,
    source: String,
    target: String,
    #[serde(default)]
    bandwidth_limit_kbit: u64,
}

async fn operation(
    State(app): State<Arc<App>>,
    data: Result<Json<Operation>, axum::extract::rejection::JsonRejection>,
) -> JsonResponse {
    let data = json_body(data)?;
    if !matches!(data.operation.as_str(), "copy" | "move") {
        return Err(ApiError::bad_request("invalid operation"));
    }
    let resolve = |address: &str| -> Result<(&Mount, Key), ApiError> {
        let (storage, path) = address
            .split_once(':')
            .ok_or(ApiError::bad_request("missing filesystem prefix"))?;
        Ok((app.media.get(storage)?, Key::parse(path)?))
    };
    let (from, source) = resolve(&data.source)?;
    let (to, target) = resolve(&data.target)?;
    if from.name == to.name && source == target {
        return Ok(Json(json!("OK")));
    }
    let _slot = app.media.uploads.try_acquire().map_err(|_| Error::Full)?;
    let meta = from.backend.stat(&source).await?;
    let read = from.backend.read(&meta, 0..meta.size).await?;
    let stream = media_http::transfer_stream(
        read,
        app.media.stop.clone(),
        None,
        data.bandwidth_limit_kbit.saturating_mul(125),
    );
    to.backend.put(&target, stream).await?;
    if data.operation == "move" {
        if from.backend.stat(&source).await?.tag != meta.tag {
            return Err(Error::Changed.into());
        }
        from.backend.delete(&source).await?;
    }
    Ok(Json(json!("OK")))
}

fn transport(app: &App) -> Result<&Transport, ApiError> {
    app.media.transport.as_ref().ok_or_else(ApiError::not_found)
}

async fn rtmp_list(State(app): State<Arc<App>>) -> JsonResponse {
    let connections = transport(&app)?.connections("rtmp").await?;
    Ok(Json(json!(
        connections
            .iter()
            .filter(|c| c.publisher)
            .map(|c| json!({"name":format!("/{}",c.path)}))
            .collect::<Vec<_>>()
    )))
}

async fn rtmp_channels(State(app): State<Arc<App>>) -> JsonResponse {
    channels(&app, "rtmp", false).await
}

async fn srt_channels(State(app): State<Arc<App>>) -> JsonResponse {
    channels(&app, "srt", false).await
}

async fn srt_list(State(app): State<Arc<App>>) -> JsonResponse {
    channels(&app, "srt", true).await
}

async fn channels(app: &App, protocol: &str, old_srt: bool) -> JsonResponse {
    let connections = transport(app)?.connections(protocol).await?;
    let mut grouped = BTreeMap::<&str, Vec<&Connection>>::new();
    for connection in &connections {
        grouped
            .entry(&connection.path)
            .or_default()
            .push(connection);
    }
    let mut result = Vec::new();
    for (path, group) in grouped {
        let Some(publisher) = group.iter().find(|c| c.publisher) else {
            continue;
        };
        let subscribers = group.iter().filter(|c| !c.publisher);
        if old_srt {
            let mut ids = Vec::new();
            let mut stats = serde_json::Map::new();
            let mut publisher_id = 0;
            for conn in group {
                let id = uuid::Uuid::parse_str(&conn.id)
                    .map_err(|_| Error::Unavailable)?
                    .as_u128() as u32;
                if conn.publisher {
                    publisher_id = id;
                } else {
                    ids.push(id);
                }
                stats.insert(id.to_string(), json!({"log":{},"stats":conn.stats}));
            }
            result.push(json!({"name":path,"socketid":publisher_id,"subscriber":ids,"connections":stats,"log":{}}));
        } else {
            let wire = |connection: &Connection| {
                let mut value = connection.wire();
                if protocol == "srt" {
                    value["stats"] = connection.stats.clone();
                }
                value
            };
            let subscribers: Vec<_> = subscribers.map(|c| wire(c)).collect();
            let subscribers = if subscribers.is_empty() {
                Value::Null
            } else {
                json!(subscribers)
            };
            let name = if protocol == "rtmp" {
                format!("/{path}")
            } else {
                path.to_string()
            };
            result.push(json!({"name":name,"is_proxy":false,"publisher":wire(publisher),"subscriber":subscribers}));
        }
    }
    Ok(Json(json!(result)))
}

async fn rtmp_disconnect(State(app): State<Arc<App>>, request: Request) -> JsonResponse {
    transport(&app)?
        .disconnect("rtmp", &query(&request, "path").unwrap_or_default())
        .await?;
    Ok(Json(json!("OK")))
}

async fn srt_disconnect(State(app): State<Arc<App>>) -> JsonResponse {
    transport(&app)?.disconnect("srt", "*").await?;
    Ok(Json(json!("OK")))
}

async fn session_summary(State(app): State<Arc<App>>, request: Request) -> JsonResponse {
    session_view(
        &app,
        &query(&request, "collectors").unwrap_or_default(),
        true,
    )
    .await
}

async fn session_active(State(app): State<Arc<App>>, request: Request) -> JsonResponse {
    session_view(
        &app,
        &query(&request, "collectors").unwrap_or_default(),
        false,
    )
    .await
}

async fn session_view(app: &App, collectors: &str, summary: bool) -> JsonResponse {
    let mut view = app.media.sessions.view(collectors, summary);
    if app.media.sessions.limits().enabled
        && let Some(transport) = &app.media.transport
    {
        for protocol in ["rtmp", "srt"] {
            if view.get(protocol).is_none() || !transport.enabled(protocol) {
                continue;
            }
            if let Ok(connections) = transport.connections(protocol).await {
                let list:Vec<_>=connections.iter().map(|conn| {
                    let seconds=now_secs(std::time::SystemTime::now()).saturating_sub(conn.created).max(1);
                    json!({"id":conn.id,"reference":conn.path,"created_at":conn.created,"local":"","remote":"",
                        "extra":{"publisher":conn.publisher},"bytes_rx":conn.rx,"bytes_tx":conn.tx,
                        "bandwidth_rx_kbit":conn.rx as f64 *0.008/seconds as f64,
                        "bandwidth_tx_kbit":conn.tx as f64 *0.008/seconds as f64})
                }).collect();
                if summary {
                    view[protocol]["active"]["sessions"] = json!(list.len());
                    view[protocol]["active"]["list"] = json!(list);
                } else {
                    view[protocol] = json!(list);
                }
            }
        }
    }
    Ok(Json(view))
}

async fn tokens(
    State(app): State<Arc<App>>,
    Path(user): Path<String>,
    data: Result<Json<Vec<TokenRequest>>, axum::extract::rejection::JsonRejection>,
) -> JsonResponse {
    let (_, settings) = super::read_settings(&app).await?;
    let known = if settings.api.auth.username.is_empty() {
        "$root"
    } else {
        &settings.api.auth.username
    };
    if user != known {
        return Err(ApiError::not_found());
    }
    let mut requests = json_body(data)?;
    if requests.len() > 64 {
        return Err(Error::Full.into());
    }
    for request in &mut requests {
        app.media.sessions.issue(&user, request)?;
    }
    Ok(Json(json!(requests)))
}

pub async fn widget(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    request: Request,
) -> JsonResponse {
    let domain = query(&request, "domain").unwrap_or_default();
    let entry = app.processes.get(&(domain, id)).await?;
    let job = entry.job();
    let runtime = entry.snapshot();
    let (current, total) = app.media.sessions.counts(&job.config.reference);
    Ok(Json(
        json!({"current_sessions":current,"total_sessions":total,
        "uptime":runtime.launched.map_or(0,|at|at.elapsed().as_secs())}),
    ))
}

async fn aliases(State(app): State<Arc<App>>) -> JsonResponse {
    Ok(Json(json!(app.media.aliases.list().await)))
}
async fn alias_add(
    State(app): State<Arc<App>>,
    data: Result<Json<Rule>, axum::extract::rejection::JsonRejection>,
) -> JsonResponse {
    let rule = json_body(data)?;
    let result = app
        .media
        .aliases
        .edit(|rules| {
            if rules.iter().any(|old| old.pattern == rule.pattern) {
                return Err(Error::Changed);
            }
            rules.push(rule.clone());
            Ok(rule)
        })
        .await
        .map_err(|error| {
            if matches!(error, Error::Changed) {
                ApiError::Http(StatusCode::CONFLICT, "alias already exists")
            } else {
                error.into()
            }
        })?;
    Ok(Json(json!(result)))
}
async fn alias_replace(
    State(app): State<Arc<App>>,
    data: Result<Json<Vec<Rule>>, axum::extract::rejection::JsonRejection>,
) -> JsonResponse {
    let new = json_body(data)?;
    Ok(Json(json!(
        app.media
            .aliases
            .edit(|rules| {
                rules.clone_from(&new);
                Ok(new)
            })
            .await?
    )))
}
async fn alias_get(State(app): State<Arc<App>>, request: Request) -> JsonResponse {
    let pattern = alias_pattern(&request)?;
    let result = app
        .media
        .aliases
        .list()
        .await
        .into_iter()
        .find(|rule| rule.pattern == pattern)
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(json!(result)))
}
async fn alias_update(State(app): State<Arc<App>>, request: Request) -> JsonResponse {
    let pattern = alias_pattern(&request)?;
    let rule: Rule = serde_json::from_slice(
        &axum::body::to_bytes(request.into_body(), 128 * 1024)
            .await
            .map_err(|_| Error::Invalid)?,
    )
    .map_err(|_| ApiError::bad_request("invalid JSON"))?;
    Ok(Json(json!(
        app.media
            .aliases
            .edit(|rules| {
                let original = rules
                    .iter_mut()
                    .find(|rule| rule.pattern == pattern)
                    .ok_or(Error::Missing)?;
                *original = rule.clone();
                Ok(rule)
            })
            .await?
    )))
}
async fn alias_delete(State(app): State<Arc<App>>, request: Request) -> JsonResponse {
    let pattern = alias_pattern(&request)?;
    app.media
        .aliases
        .edit(|rules| {
            let index = rules
                .iter()
                .position(|rule| rule.pattern == pattern)
                .ok_or(Error::Missing)?;
            rules.remove(index);
            Ok(())
        })
        .await?;
    Ok(Json(json!("OK")))
}

fn alias_pattern(request: &Request) -> Result<String, ApiError> {
    query(request, "pattern")
        .filter(|pattern| !pattern.is_empty())
        .ok_or(ApiError::bad_request("pattern is required"))
}

async fn playout(
    State(app): State<Arc<App>>,
    Path(vars): Path<BTreeMap<String, String>>,
    request: Request,
) -> JsonResponse {
    let record = app.store.read().await?;
    if app.effective(&record)?["playout"]["enable"].as_bool() != Some(true) {
        return Err(ApiError::not_found());
    }
    let key = (
        query(&request, "domain").unwrap_or_default(),
        vars.get("id").cloned().ok_or(Error::Missing)?,
    );
    let job = app.processes.get(&key).await?.job();
    if !job
        .config
        .inputs
        .iter()
        .any(|input| vars.get("input") == Some(&input.id))
    {
        return Err(ApiError::not_found());
    }
    Err(ApiError::Http(
        StatusCode::NOT_IMPLEMENTED,
        "playout requires the Restreamer FFmpeg control extension",
    ))
}
