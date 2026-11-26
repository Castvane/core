use std::{collections::BTreeMap, convert::Infallible, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    body::{Body, Bytes, to_bytes},
    extract::{Path, Request, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use super::{ApiError, check_network, identify, peer, read_settings};
use crate::{
    App,
    auth::TokenUse,
    ops::{Event, METRICS, PROCESS_STATES},
};

type Reply = Result<Json<Value>, ApiError>;

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/metrics", get(describe).post(metrics))
        .route("/resources", get(resources))
        .route("/log", get(log))
        .route("/events", post(log_events))
        .route("/events/log", post(log_events))
        .route("/events/process", post(process_events))
        .route("/events/media/{kind}", post(media_events))
        .route("/audit", get(audit))
}

pub fn public() -> Router<Arc<App>> {
    Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(readiness))
        .route("/metrics", get(scrape))
        .route("/.well-known/acme-challenge/{token}", get(acme_challenge))
}

pub(crate) async fn record(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    category: &str,
    action: &str,
    actor: Option<&str>,
    resource: Option<&str>,
) -> Result<(), ApiError> {
    sqlx::query("INSERT INTO operation_event(timestamp, category, action, actor, resource) VALUES (?,?,?,?,?)")
        .bind(crate::engine::unix_now()).bind(category).bind(action)
        .bind(actor).bind(resource).execute(&mut **tx).await?;
    sqlx::query(
        "DELETE FROM operation_event WHERE id <= (SELECT max(id) - 4096 FROM operation_event)",
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn acme_challenge(
    State(app): State<Arc<App>>,
    Path(token): Path<String>,
) -> Result<String, ApiError> {
    app.certificates
        .challenge(&token)
        .ok_or_else(ApiError::not_found)
}

async fn health(State(app): State<Arc<App>>) -> impl IntoResponse {
    let live = !app.ops.stop.is_cancelled();
    (
        if live {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(json!({"status":if live {"ok"} else {"stopping"}})),
    )
}

async fn readiness(State(app): State<Arc<App>>) -> impl IntoResponse {
    let document = tokio::time::timeout(Duration::from_secs(2), app.store.read())
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|record| app.effective(&record).ok());
    let db = document.is_some();
    let (ffmpeg, storage) = app.ops.components();
    let tls = document
        .and_then(|document| document["tls"]["enable"].as_bool())
        .is_none_or(|enabled| {
            !enabled
                || app
                    .certificates
                    .expiry()
                    .is_some_and(|expiry| expiry > crate::engine::unix_now())
        });
    let native = app.media.transport_requested.then(|| {
        app.media
            .transport
            .as_ref()
            .is_some_and(|transport| transport.metrics().available)
    });
    let ready = db
        && ffmpeg
        && storage
        && tls
        && (!app.media.transport_required || native == Some(true))
        && !app.ops.stop.is_cancelled();
    (
        if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(
            json!({"status":if ready {"ok"} else {"unavailable"},"database":db,"ffmpeg":ffmpeg,"required_storage":storage,"tls":tls,"native_media":native}),
        ),
    )
}

async fn resources(State(app): State<Arc<App>>) -> Reply {
    let mut result = serde_json::to_value(app.ops.host().as_ref())?;
    result["node_reservations"] = app.processes.reservations();
    result["tls_expires_at"] = json!(app.certificates.expiry());
    Ok(Json(result))
}

async fn enabled(app: &App, field: &str) -> Result<(), ApiError> {
    if app.effective(&app.store.read().await?)?["metrics"][field].as_bool() == Some(true) {
        Ok(())
    } else {
        Err(ApiError::not_found())
    }
}

async fn scrape(State(app): State<Arc<App>>, request: Request) -> Result<Response, ApiError> {
    enabled(&app, "enable_prometheus").await?;
    let (record, settings) = read_settings(&app).await?;
    let source = peer(&request);
    check_network(&settings, &request, source)?;
    let user = identify(
        &app,
        &record,
        &settings,
        request.headers(),
        source,
        TokenUse::Access,
    )
    .await?;
    if !user.allows("", "api", "/metrics", "read") {
        return Err(ApiError::forbidden("access denied"));
    }
    let mut output = app.ops.prometheus();
    if let Some(cluster) = &app.cluster {
        output.push_str(&cluster.prometheus());
    }
    Ok((
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        output,
    )
        .into_response())
}

async fn describe(State(app): State<Arc<App>>) -> Reply {
    enabled(&app, "enable").await?;
    let mut descriptions: Vec<_> = METRICS
        .iter()
        .map(|&(name, doc)| (name, doc, &[][..]))
        .collect();
    descriptions.push((
        "restream_state",
        "Summarized current process states",
        &["state"],
    ));
    descriptions.sort_by_key(|(name, _, _)| *name);
    Ok(Json(json!(
        descriptions
            .into_iter()
            .map(|(name, doc, labels)| json!({"name":name,"description":doc,"labels":labels}))
            .collect::<Vec<_>>()
    )))
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Query {
    timerange_sec: u64,
    interval_sec: u64,
    metrics: Vec<QueryMetric>,
}

#[derive(Deserialize)]
struct QueryMetric {
    name: String,
    #[serde(default)]
    labels: std::collections::BTreeMap<String, String>,
}

async fn metrics(State(app): State<Arc<App>>, query: Result<Json<Query>, JsonRejection>) -> Reply {
    enabled(&app, "enable").await?;
    let Json(query) = query.map_err(|_| ApiError::bad_request("invalid metrics query"))?;
    if query.metrics.len() > 128
        || query
            .metrics
            .iter()
            .any(|query| query.name.len() > 256 || query.labels.len() > 16)
    {
        return Err(ApiError::bad_request("metrics query exceeds limits"));
    }
    let interval = app.ops.interval.as_secs().max(query.interval_sec);
    let range = app.ops.range.min(query.timerange_sec);
    let matchers = query
        .metrics
        .into_iter()
        .map(|metric| {
            let parse = |value: &str| {
                glob::Pattern::new(value)
                    .map_err(|_| ApiError::bad_request("invalid metric pattern"))
            };
            let name = parse(&metric.name)?;
            let labels = metric
                .labels
                .into_iter()
                .map(|(key, value)| {
                    if key.len() > 64 || value.len() > 256 {
                        return Err(ApiError::bad_request("metrics query exceeds limits"));
                    }
                    Ok((key, parse(&value)?))
                })
                .collect::<Result<BTreeMap<_, _>, ApiError>>()?;
            Ok((name, labels))
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    let matches = |name: &str, state: Option<&str>| {
        matchers.iter().any(|(metric, labels)| {
            metric.matches(name)
                && labels.iter().all(|(key, value)| {
                    key == "state" && state.is_some_and(|state| value.matches(state))
                })
        })
    };
    let values = app
        .ops
        .measure(|measurements| {
            let last = measurements.back().map_or(0, |sample| sample.ts);
            let mut result = Vec::new();
            let series = METRICS.iter().map(|&(name, _)| (name, None)).chain(
                PROCESS_STATES
                    .iter()
                    .enumerate()
                    .map(|(index, state)| ("restream_state", Some((index, state.wire())))),
            );
            for (name, state) in series {
                if !matches(name, state.map(|(_, state)| state)) {
                    continue;
                }
                let mut points = Vec::new();
                let mut earliest = last;
                for sample in measurements.iter().rev() {
                    if sample.ts < last.saturating_sub(range as i64) {
                        break;
                    }
                    if sample.ts > earliest {
                        continue;
                    }
                    let value = match state {
                        Some((index, _)) => Some(f64::from(sample.process_states[index])),
                        None => sample.values.get(name).copied(),
                    };
                    if let Some(value) = value {
                        let ts = time::OffsetDateTime::from_unix_timestamp(sample.ts)
                            .ok()
                            .and_then(|timestamp| {
                                timestamp
                                    .format(&time::format_description::well_known::Rfc3339)
                                    .ok()
                            });
                        points.push(json!({"ts":ts,"value":value}));
                        earliest = sample
                            .ts
                            .saturating_sub(interval.min(i64::MAX as u64) as i64);
                    }
                }
                if !points.is_empty() {
                    points.reverse();
                    let labels =
                        state.map_or_else(|| json!({}), |(_, state)| json!({"state":state}));
                    result.push(json!({"name":name,"labels":labels,"values":points}));
                }
            }
            result
        })
        .unwrap_or_default();
    Ok(Json(
        json!({"timerange_sec":app.ops.range,"interval_sec":app.ops.interval.as_secs(),"metrics":values}),
    ))
}

async fn log(State(app): State<Arc<App>>, request: Request) -> Reply {
    let raw = super::media::query(&request, "format").as_deref() == Some("raw");
    let component = super::media::query(&request, "component");
    let records = app.ops.records().into_iter().filter(|event| {
        component
            .as_deref()
            .is_none_or(|comp| comp == event.category)
    });
    let result: Vec<_> = if raw {
        records.map(|event| {
            let timestamp = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(event.ts) * 1_000_000)
                .ok().and_then(|timestamp| timestamp.format(&time::format_description::well_known::Rfc3339).ok());
            json!({"ts":timestamp,"component":event.category,"level":event.level,"message":event_text(&event)})
        }).collect()
    } else {
        records
            .map(|event| {
                json!(format!(
                    "{} {} {} {}",
                    event.ts,
                    event.level,
                    event.category,
                    event_text(&event)
                ))
            })
            .collect()
    };
    Ok(Json(json!(result)))
}

fn event_text(event: &Event) -> &str {
    event.message.as_deref().unwrap_or(event.action)
}

fn log_entry(event: &Event) -> Value {
    json!({"ts":event.ts / 1000,"event":event.category,
        "level":event.level,"message":event_text(event),"caller":"","data":{}})
}

fn process_entry(event: &Event) -> Value {
    let mut value =
        json!({"pid":event.name,"domain":event.domain,"type":event.action,"ts":event.ts});
    if let Some(progress) = &event.progress {
        value["progress"] =
            json!({"input":null,"output":null,"time":progress.seconds,"speed":progress.speed});
    }
    value
}

#[derive(sqlx::FromRow, serde::Serialize)]
struct Audit {
    id: i64,
    timestamp: i64,
    category: String,
    action: String,
    actor: Option<String>,
    resource: Option<String>,
}

async fn audit(State(app): State<Arc<App>>, request: Request) -> Reply {
    let after = super::media::query(&request, "after")
        .map(|value| value.parse::<i64>())
        .transpose()
        .map_err(|_| ApiError::bad_request("invalid event cursor"))?
        .unwrap_or_default();
    let events: Vec<Audit> = sqlx::query_as(
        "SELECT id,timestamp,category,action,actor,resource
        FROM operation_event WHERE id > ? ORDER BY id DESC LIMIT 256",
    )
    .bind(after)
    .fetch_all(&app.store.0)
    .await?;
    Ok(Json(json!(events)))
}

#[derive(Clone, Copy)]
enum StreamKind {
    Log,
    Process,
    Media,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct FilterRequest {
    filters: Vec<Filter>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Filter {
    event: String,
    message: String,
    level: String,
    caller: String,
    core_id: String,
    data: BTreeMap<String, String>,
    pid: String,
    domain: String,
    #[serde(rename = "type")]
    kind: String,
}

enum Filters {
    Log(BTreeMap<String, LogFilter>),
    Process(Vec<[Option<Regex>; 4]>),
    Media,
}

struct LogFilter {
    message: Option<Regex>,
    level: Option<Regex>,
    requires_data: bool,
}

impl Filters {
    fn matches(&self, event: &Event) -> bool {
        match self {
            Self::Log(filters) => {
                event.progress.is_none()
                    && (filters.is_empty()
                        || filters.get(event.category).is_some_and(|filter| {
                            !filter.requires_data
                                && filter
                                    .message
                                    .as_ref()
                                    .is_none_or(|re| re.is_match(event_text(event)))
                                && filter
                                    .level
                                    .as_ref()
                                    .is_none_or(|re| re.is_match(event.level))
                        }))
            }
            Self::Process(filters) => {
                event.category == "process"
                    && (filters.is_empty()
                        || filters.iter().any(|filter| {
                            [&event.name, &event.domain, event.action, ""]
                                .into_iter()
                                .zip(filter)
                                .all(|(value, re)| re.as_ref().is_none_or(|re| re.is_match(value)))
                        }))
            }
            Self::Media => event.category == "media",
        }
    }
}

fn event_regex(value: &str) -> Result<Option<Regex>, ApiError> {
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > 512 {
        return Err(ApiError::bad_request("invalid event filter"));
    }
    RegexBuilder::new(value)
        .case_insensitive(true)
        .size_limit(64 * 1024)
        .build()
        .map(Some)
        .map_err(|_| ApiError::bad_request("invalid event filter"))
}

async fn get_filters(body: Body) -> Result<Vec<Filter>, ApiError> {
    let body = to_bytes(body, 64 * 1024)
        .await
        .map_err(|_| ApiError::bad_request("invalid event filters"))?;
    let filters: FilterRequest = if body.is_empty() {
        FilterRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|_| ApiError::bad_request("invalid event filters"))?
    };
    if filters.filters.len() > 32 {
        return Err(ApiError::bad_request("too many filters"));
    }
    Ok(filters.filters)
}

struct Audience {
    app: Arc<App>,
    headers: HeaderMap,
    peer: Option<std::net::IpAddr>,
}

impl Audience {
    fn from_request(app: Arc<App>, request: &Request) -> Self {
        let mut headers = HeaderMap::new();
        if let Some(token) = request.headers().get(header::AUTHORIZATION) {
            headers.insert(header::AUTHORIZATION, token.clone());
        }
        Self {
            app,
            headers,
            peer: peer(request),
        }
    }

    async fn authorized(&self) -> Result<bool, ApiError> {
        let (record, settings) = read_settings(&self.app).await?;
        Ok(identify(
            &self.app,
            &record,
            &settings,
            &self.headers,
            self.peer,
            TokenUse::Access,
        )
        .await?
        .administrator())
    }
}

async fn log_events(State(app): State<Arc<App>>, request: Request) -> Result<Response, ApiError> {
    let sse = !request
        .headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| accept.contains("application/x-json-stream"));
    let audience = Audience::from_request(app, &request);
    let mut filters = BTreeMap::new();
    for filter in get_filters(request.into_body()).await? {
        if filter.event.len() > 128 || filter.data.len() > 16 {
            return Err(ApiError::bad_request("invalid event filter"));
        }
        // Local records have no caller, cluster ID or arbitrary tracing fields.
        event_regex(&filter.caller)?;
        event_regex(&filter.core_id)?;
        for pattern in filter.data.values() {
            event_regex(pattern)?;
        }
        filters.insert(
            filter.event.to_ascii_lowercase(),
            LogFilter {
                message: event_regex(&filter.message)?,
                level: event_regex(&filter.level)?,
                requires_data: !filter.data.is_empty(),
            },
        );
    }
    events(
        audience,
        StreamKind::Log,
        None,
        None,
        Filters::Log(filters),
        sse,
    )
}

async fn process_events(
    State(app): State<Arc<App>>,
    request: Request,
) -> Result<Response, ApiError> {
    let audience = Audience::from_request(app, &request);
    let filters = get_filters(request.into_body())
        .await?
        .into_iter()
        .map(|filter| {
            Ok([
                event_regex(&filter.pid)?,
                event_regex(&filter.domain)?,
                event_regex(&filter.kind)?,
                event_regex(&filter.core_id)?,
            ])
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    events(
        audience,
        StreamKind::Process,
        None,
        None,
        Filters::Process(filters),
        false,
    )
}

async fn media_events(
    State(app): State<Arc<App>>,
    Path(kind): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    let glob = super::media::query(&request, "glob").unwrap_or_default();
    if glob.len() > 512 {
        return Err(ApiError::bad_request("invalid media pattern"));
    }
    let pattern = glob::Pattern::new(if glob.is_empty() { "**" } else { &glob })
        .map_err(|_| ApiError::bad_request("invalid media pattern"))?;
    let names: Vec<_> = if let Ok(mount) = app.media.get(&kind) {
        mount
            .backend
            .list()
            .await?
            .iter()
            .map(|meta| format!("/{}", meta.key.as_str()))
            .filter(|name| pattern.matches(name))
            .take(4096)
            .collect()
    } else if let Some(transport) = &app.media.transport
        && matches!(kind.as_str(), "rtmp" | "srt")
        && transport.enabled(&kind)
    {
        transport
            .metrics()
            .publisher_names
            .into_iter()
            .filter_map(|(name, path)| (name == kind && pattern.matches(&path)).then_some(path))
            .take(4096)
            .collect()
    } else {
        return Err(ApiError::not_found());
    };
    events(
        Audience::from_request(app, &request),
        StreamKind::Media,
        Some((kind, pattern)),
        Some(names),
        Filters::Media,
        false,
    )
}

fn events(
    audience: Audience,
    kind: StreamKind,
    media: Option<(String, glob::Pattern)>,
    names: Option<Vec<String>>,
    filters: Filters,
    sse: bool,
) -> Result<Response, ApiError> {
    let app = &audience.app;
    let permit = app.ops.streams.clone().try_acquire_owned().map_err(|_| {
        ApiError::Http(
            StatusCode::SERVICE_UNAVAILABLE,
            "event stream capacity reached",
        )
    })?;
    let mut sub = app.ops.bus.subscribe();
    let stop = app.ops.stop.clone();
    let body = Body::from_stream(async_stream::stream! {
        let _permit = permit;
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        let timeout = tokio::time::sleep(Duration::from_secs(300));
        tokio::pin!(timeout);
        if let Some(names) = names {
            yield Ok::<_, Infallible>(Bytes::from(format!("{}\n", json!({"action":"list","names":names,"ts":crate::engine::unix_now()*1000}))));
        }
        loop {
            tokio::select! {
                _ = stop.cancelled() => break,
                _ = &mut timeout => break,
                _= interval.tick() => {
                    if !tokio::time::timeout(Duration::from_secs(2), audience.authorized()).await.is_ok_and(|result| result.unwrap_or(false)) {
                        break;
                    }
                    let heartbeat = match kind {
                        StreamKind::Log if sse => ":keepalive\n\n",
                        StreamKind::Log => "{\"event\":\"keepalive\"}\n",
                        StreamKind::Media => "{\"action\":\"keepalive\"}\n",
                        StreamKind::Process => "{\"type\":\"keepalive\"}\n",
                    };
                    yield Ok(Bytes::from_static(heartbeat.as_bytes()));
                }
                item = sub.recv() => {
                    let event = match item {
                        Ok(event)=>event,
                        Err(broadcast::error::RecvError::Lagged(_))=>break,
                        Err(broadcast::error::RecvError::Closed)=>break,
                    };
                    if !filters.matches(&event) { continue; }
                    let value = match kind {
                        StreamKind::Process => process_entry(&event),
                        StreamKind::Media if event.category=="media" && media.as_ref().is_some_and(|(mount, pattern)| &event.domain==mount && pattern.matches(&event.name)) => json!({"action":event.action,"name":event.name,"ts":event.ts}),
                        StreamKind::Log => log_entry(&event),
                        _ => continue,
                    };
                    let wire = if sse { format!("event: {}\ndata: {value}\n\n", event.category) } else {format!("{value}\n")};
                    yield Ok(Bytes::from(wire));
                }
            }
        }
    });
    let mime = if sse {
        "text/event-stream; charset=utf-8"
    } else {
        "application/x-json-stream; charset=utf-8"
    };
    Ok((
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "no-store"),
            (header::CONNECTION, "close"),
        ],
        body,
    )
        .into_response())
}
