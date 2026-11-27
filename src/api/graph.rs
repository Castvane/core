use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_graphql::{
    Error, Request, Response, Value as GraphValue,
    dynamic::{
        Enum, Field, FieldFuture, FieldValue, InputObject, InputValue, Interface, InterfaceField,
        Object, ResolverContext, Scalar, Schema, TypeRef,
    },
};
use axum::{
    Extension, Json, Router,
    extract::{State, rejection::JsonRejection},
    http::{StatusCode, header},
    response::{Html, IntoResponse},
    routing::{get, post},
};
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{ApiError, operations, process, process_dto};
use crate::{App, auth::Identity};

const RESPONSE_BUDGET: usize = 4 * 1024 * 1024;

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/graph", get(index))
        .route("/api/graph/query", post(query))
        .route("/api/graph/schema.graphql", get(sdl))
}

async fn index() -> Html<&'static str> {
    Html(
        r#"<!doctype html><html lang="en"><meta charset="utf-8"><title>Castvane GraphQL</title>
<h1>Castvane GraphQL</h1><p>Send a JSON request to <code>POST /api/graph/query</code> using an access token.</p><p><a href="/api/graph/schema.graphql">GraphQL schema</a></p></html>"#,
    )
}

async fn sdl() -> Result<impl IntoResponse, ApiError> {
    Ok((
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        schema()?.sdl(),
    ))
}

struct Caller {
    app: Arc<App>,
    actor: Identity,
    remaining: AtomicUsize,
    projected: AtomicUsize,
}

fn charge(balance: &AtomicUsize, amount: usize) -> async_graphql::Result<()> {
    balance
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
            remaining.checked_sub(amount)
        })
        .map(|_| ())
        .map_err(|_| Error::new("GraphQL response limit exceeded"))
}

async fn query(
    State(app): State<Arc<App>>,
    Extension(actor): Extension<Identity>,
    payload: Result<Json<Request>, JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let request = match payload {
        Ok(Json(request)) => request,
        Err(rejection) => {
            let status = if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::BAD_REQUEST
            };
            return Ok((status, Json(graph_error("expected a GraphQL JSON request"))));
        }
    };
    static CONCURRENT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);
    let Ok(_permit) = CONCURRENT.try_acquire() else {
        return Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(graph_error("GraphQL server busy")),
        ));
    };
    let timeout = app
        .request_timeout
        .saturating_sub(Duration::from_millis(100))
        .min(Duration::from_secs(20));
    let caller = Caller {
        app,
        actor,
        remaining: AtomicUsize::new(RESPONSE_BUDGET),
        projected: AtomicUsize::new(RESPONSE_BUDGET),
    };
    let result = tokio::time::timeout(timeout, schema()?.execute(request.data(caller))).await;
    Ok(match result {
        Ok(response) => (StatusCode::OK, Json(response)),
        Err(_) => (
            StatusCode::REQUEST_TIMEOUT,
            Json(graph_error("GraphQL execution timed out")),
        ),
    })
}

fn graph_error(message: &str) -> Response {
    Response::from_errors(vec![async_graphql::ServerError::new(message, None)])
}

fn schema() -> Result<&'static Schema, ApiError> {
    static SCHEMA: OnceLock<Result<Schema, async_graphql::dynamic::SchemaError>> = OnceLock::new();
    SCHEMA
        .get_or_init(build)
        .as_ref()
        .map_err(|_| ApiError::Internal("GraphQL schema"))
}

fn build() -> Result<Schema, async_graphql::dynamic::SchemaError> {
    let process_args = [("id", "ID!"), ("domain", "String!")];
    let query = Object::new("Query")
        .field(ping())
        .field(root("about", "About", &[]))
        .field(root(
            "processes",
            "[Process!]!",
            &[
                ("idpattern", "String"),
                ("refpattern", "String"),
                ("domainpattern", "String"),
            ],
        ))
        .field(root("process", "Process", &process_args))
        .field(root("probe", "Probe!", &process_args))
        .field(root("log", "[String!]!", &[]))
        .field(root("metrics", "Metrics!", &[("query", "MetricsInput!")]))
        .field(root(
            "playoutStatus",
            "RawAVstream",
            &[("id", "ID!"), ("domain", "String!"), ("input", "ID!")],
        ));
    let mut schema = Schema::build("Query", Some("Mutation"), None)
        .limit_depth(14)
        .limit_recursive_depth(24)
        .limit_directives(8)
        .limit_complexity(192)
        .register(query)
        .register(Object::new("Mutation").field(ping()))
        .register(Enum::new("State").item("RUNNING").item("IDLE"))
        .register(
            Enum::new("Command")
                .item("START")
                .item("STOP")
                .item("RESTART")
                .item("RELOAD"),
        )
        .register(
            InputObject::new("MetricsInput")
                .field(InputValue::new("metrics", ty("[MetricInput!]!")))
                .field(InputValue::new("timerange_seconds", ty("Int")))
                .field(InputValue::new("interval_seconds", ty("Int"))),
        )
        .register(
            InputObject::new("MetricInput")
                .field(InputValue::new("name", ty("String!")))
                .field(InputValue::new("labels", ty("Map"))),
        );
    for scalar in ["Time", "Any", "Map", "Uint64", "MetricsResponseValue"] {
        schema = schema.register(Scalar::new(scalar));
    }
    let mut report = Interface::new("IProcessReportHistoryEntry");
    for (field, kind) in shape(REPORT) {
        report = report.field(InterfaceField::new(field, ty(kind)));
    }
    schema = schema.register(report);
    for (name, fields) in OBJECTS {
        let mut object = Object::new(*name);
        for (field, kind) in shape(fields) {
            object = object.field(Field::new(field, ty(kind), move |ctx| {
                FieldFuture::new(async move {
                    let value = ctx.parent_value.try_downcast_ref::<Value>()?;
                    field_value(
                        value.get(field).unwrap_or(&Value::Null),
                        kind,
                        &ctx.data::<Caller>()?.projected,
                    )
                })
            }));
        }
        if matches!(*name, "ProcessReport" | "ProcessReportHistoryEntry") {
            object = object.implement("IProcessReportHistoryEntry");
        }
        schema = schema.register(object);
    }
    schema.finish()
}

fn ping() -> Field {
    Field::new("ping", ty("String!"), |_| {
        FieldFuture::new(async { Ok(Some(FieldValue::value("pong"))) })
    })
}

fn shape(fields: &'static str) -> impl Iterator<Item = (&'static str, &'static str)> {
    fields
        .split_ascii_whitespace()
        .filter_map(|item| item.split_once(':'))
}

fn ty(name: &str) -> TypeRef {
    if let Some(inner) = name.strip_suffix('!') {
        TypeRef::NonNull(Box::new(ty(inner)))
    } else if let Some(inner) = name
        .strip_prefix('[')
        .and_then(|name| name.strip_suffix(']'))
    {
        TypeRef::List(Box::new(ty(inner)))
    } else {
        TypeRef::named(name)
    }
}

fn field_value<'a>(
    value: &'a Value,
    kind: &str,
    budget: &AtomicUsize,
) -> async_graphql::Result<Option<FieldValue<'a>>> {
    let kind = kind.trim_end_matches('!');
    if let Some(inner) = kind
        .strip_prefix('[')
        .and_then(|name| name.strip_suffix(']'))
    {
        charge(budget, 2)?;
        let mut items = Vec::new();
        if let Some(values) = value.as_array() {
            for value in values {
                items.push(field_value(value, inner, budget)?.unwrap_or(FieldValue::NULL));
            }
        }
        return Ok(Some(FieldValue::list(items)));
    }
    charge(budget, 16)?;
    if value.is_null() {
        return Ok(None);
    }
    if OBJECTS.iter().any(|(name, _)| *name == kind) {
        return Ok(Some(FieldValue::borrowed_any(value)));
    }
    Ok(Some(FieldValue::value(GraphValue::from_json(
        value.clone(),
    )?)))
}

fn root(name: &'static str, kind: &'static str, args: &[(&str, &str)]) -> Field {
    let mut field = Field::new(name, ty(kind), move |ctx| {
        FieldFuture::new(async move {
            let caller = ctx.data::<Caller>()?;
            let value = resolve(name, &ctx, caller).await?;
            let size = serde_json::to_vec(&value)?.len();
            charge(&caller.remaining, size)?;
            if !OBJECTS
                .iter()
                .any(|(name, _)| *name == kind.trim_matches(['[', ']', '!']))
            {
                charge(&caller.projected, size)?;
            }
            if let Value::Array(items) = value {
                let item_kind = kind.trim_matches(['[', ']', '!']);
                if OBJECTS.iter().any(|(name, _)| *name == item_kind) {
                    return Ok(Some(FieldValue::list(
                        items.into_iter().map(FieldValue::owned_any),
                    )));
                }
                let values = items
                    .into_iter()
                    .map(GraphValue::from_json)
                    .collect::<Result<Vec<_>, _>>()?;
                return Ok(Some(FieldValue::list(
                    values.into_iter().map(FieldValue::value),
                )));
            }
            Ok((!value.is_null()).then(|| FieldValue::owned_any(value)))
        })
    });
    for (name, kind) in args {
        field = field.argument(InputValue::new(*name, ty(kind)));
    }
    field
}

fn require(condition: bool) -> async_graphql::Result<()> {
    if condition {
        Ok(())
    } else {
        Err(Error::new("forbidden"))
    }
}

fn api_error(error: ApiError) -> Error {
    match error {
        ApiError::Http(_, message) => Error::new(message),
        _ => Error::new("internal error"),
    }
}

async fn resolve(
    name: &str,
    ctx: &ResolverContext<'_>,
    caller: &Caller,
) -> async_graphql::Result<Value> {
    let app = &caller.app;
    let arg = |name| -> async_graphql::Result<String> {
        Ok(ctx
            .args
            .get(name)
            .filter(|arg| !arg.is_null())
            .map(|arg| arg.string().map(str::to_owned))
            .transpose()?
            .unwrap_or_default())
    };
    match name {
        "about" => {
            let (_, settings) = super::read_settings(app).await.map_err(api_error)?;
            Ok(json!({
                "app":"datarhei-core","id":settings.id,"name":settings.name,
                "created_at":app.started_at.format(&Rfc3339).map_err(|_| Error::new("could not format application start"))?,
                "uptime_seconds":(OffsetDateTime::now_utc()-app.started_at).whole_seconds().max(0),
                "version":{"number":env!("CARGO_PKG_VERSION"),"repository_commit":env!("CASTVANE_BUILD_COMMIT"),
                    "repository_branch":option_env!("GITHUB_REF_NAME").unwrap_or("development"),
                    "build_date":"","arch":super::build_arch(),"compiler":"rustc"}
            }))
        }
        "processes" => {
            let params = process::Params::graph(
                graph_fields(ctx),
                arg("idpattern")?,
                arg("refpattern")?,
                arg("domainpattern")?,
            );
            let Json(mut result) =
                process::process_list(app, &caller.actor, params, RESPONSE_BUDGET)
                    .await
                    .map_err(api_error)?;
            if let Some(items) = result.as_array_mut() {
                for item in items {
                    normalize_process(item);
                }
            }
            Ok(result)
        }
        "process" | "probe" | "playoutStatus" => {
            let key = (arg("domain")?, arg("id")?);
            require(caller.actor.allows(
                &key.0,
                "process",
                &key.1,
                if name == "probe" { "write" } else { "read" },
            ))?;
            let entry = app
                .processes
                .get(&key)
                .await
                .map_err(|err| Error::new(err.public()))?;
            if name == "probe" {
                let (_, settings) = super::read_settings(app).await.map_err(api_error)?;
                require(
                    !settings.api.read_only
                        && (caller.actor.administrator() || entry.job().config.scoped),
                )?;
                let data = app
                    .processes
                    .probe(&entry.job().config)
                    .await
                    .map_err(|err| Error::new(err.public()))?;
                let mut value = process_dto::probe(data);
                if let Some(streams) = value["streams"].as_array_mut() {
                    for stream in streams {
                        rename(stream, "duration_sec", "duration_seconds");
                        rename(stream, "sampling_hz", "sampling");
                    }
                }
                return Ok(value);
            }
            if name == "playoutStatus" {
                let input = arg("input")?;
                if !entry
                    .job()
                    .config
                    .inputs
                    .iter()
                    .any(|item| item.id == input)
                {
                    return Err(Error::new("unknown process or input"));
                }
                return Err(Error::new(
                    "playout requires the Restreamer FFmpeg control extension",
                ));
            }
            let params = process::Params::graph(
                graph_fields(ctx),
                String::new(),
                String::new(),
                String::new(),
            );
            let mut result = process::process_view(&app.processes, &entry, &params, "")
                .await
                .map_err(api_error)?;
            normalize_process(&mut result);
            Ok(result)
        }
        "log" => {
            require(caller.actor.allows("", "api", "/api/v3/log", "read"))?;
            Ok(Value::Array(operations::log_lines(app, false, None)))
        }
        "metrics" => {
            require(caller.actor.allows("", "api", "/api/v3/metrics", "read"))?;
            let mut query: Value = ctx.args.try_get("query")?.deserialize()?;
            for (from, to) in [
                ("timerange_seconds", "timerange_sec"),
                ("interval_seconds", "interval_sec"),
            ] {
                let value = query[from].as_i64().unwrap_or_default();
                if value < 0 {
                    return Err(Error::new("metrics duration must be nonnegative"));
                }
                query[to] = json!(value);
            }
            if let Some(metrics) = query["metrics"].as_array_mut() {
                for metric in metrics {
                    let labels = metric["labels"]
                        .as_object()
                        .map(|labels| {
                            labels
                                .iter()
                                .filter(|(_, value)| value.is_string())
                                .map(|(name, value)| (name.clone(), value.clone()))
                                .collect::<serde_json::Map<_, _>>()
                        })
                        .unwrap_or_default();
                    metric["labels"] = Value::Object(labels);
                }
            }
            let Json(mut result) = operations::metrics_query(
                app,
                serde_json::from_value(query).map_err(|_| Error::new("invalid metrics query"))?,
            )
            .await
            .map_err(api_error)?;
            rename(&mut result, "timerange_sec", "timerange_seconds");
            rename(&mut result, "interval_sec", "interval_seconds");
            if let Some(series) = result["metrics"].as_array_mut() {
                for metric in series {
                    if let Some(points) = metric["values"].as_array_mut() {
                        for point in points {
                            let timestamp = point["ts"]
                                .as_str()
                                .and_then(|ts| OffsetDateTime::parse(ts, &Rfc3339).ok())
                                .map(|ts| ts.unix_timestamp());
                            *point = json!([timestamp, point["value"]]);
                        }
                    }
                }
            }
            Ok(result)
        }
        _ => Err(Error::new("unknown GraphQL field")),
    }
}

fn graph_fields(ctx: &ResolverContext<'_>) -> String {
    let requested = ctx.look_ahead();
    ["identity", "config", "state", "report", "metadata"]
        .into_iter()
        .filter(|field| *field == "identity" || requested.field(field).exists())
        .collect::<Vec<_>>()
        .join(",")
}

fn normalize_process(process: &mut Value) {
    date(process, "created_at");
    if let Some(config) = process.get_mut("config") {
        config["limits"]["memory_bytes"] = json!(
            config["limits"]["memory_mbytes"]
                .as_u64()
                .unwrap_or_default()
                .saturating_mul(1024 * 1024)
        );
    }
    if let Some(state) = process.get_mut("state") {
        rename(state, "exec", "state");
        let progress = &mut state["progress"];
        rename(progress, "inputs", "input");
        rename(progress, "outputs", "output");
        for direction in ["input", "output"] {
            if let Some(items) = progress[direction].as_array_mut() {
                for item in items {
                    for field in ["q", "width", "height", "sampling", "channels"] {
                        item[field] = json!(0);
                    }
                    item["pixfmt"] = json!("");
                    item["layout"] = json!("");
                }
            }
        }
    }
    if let Some(report) = process.get_mut("report") {
        normalize_report(report);
        if let Some(history) = report["history"].as_array_mut() {
            for entry in history {
                normalize_report(entry);
            }
        }
    }
}

fn normalize_report(report: &mut Value) {
    date(report, "created_at");
    if let Some(lines) = report["log"].as_array_mut() {
        for entry in lines {
            *entry = json!({"timestamp":entry[0],"data":entry[1]});
        }
    }
}

fn date(item: &mut Value, field: &str) {
    item[field] = json!(
        item[field]
            .as_i64()
            .and_then(|ts| OffsetDateTime::from_unix_timestamp(ts).ok())
            .and_then(|ts| ts.format(&Rfc3339).ok())
            .unwrap_or_else(|| "0001-01-01T00:00:00Z".to_owned())
    );
}

fn rename(object: &mut Value, from: &str, to: &str) {
    if let Some(map) = object.as_object_mut()
        && let Some(value) = map.remove(from)
    {
        map.insert(to.to_owned(), value);
    }
}

const REPORT: &str = "created_at:Time! prelude:[String!]! log:[ProcessReportLogEntry!]!";
const OBJECTS: &[(&str, &str)] = &[
    (
        "About",
        "id:ID! app:String! name:String! version:AboutVersion! uptime_seconds:Uint64! created_at:Time!",
    ),
    (
        "AboutVersion",
        "number:String! arch:String! compiler:String! build_date:String! repository_branch:String! repository_commit:String!",
    ),
    (
        "Process",
        "id:String! owner:String! domain:String! reference:String! type:String! created_at:Time! metadata:Map state:ProcessState! config:ProcessConfig! report:ProcessReport!",
    ),
    (
        "ProcessConfig",
        "id:String! owner:String! domain:String! reference:String! type:String! reconnect:Boolean! reconnect_delay_seconds:Uint64! autostart:Boolean! stale_timeout_seconds:Uint64! limits:ProcessConfigLimits! input:[ProcessConfigIO!]! output:[ProcessConfigIO!]! options:[String!]!",
    ),
    (
        "ProcessConfigIO",
        "id:String! address:String! options:[String!]!",
    ),
    (
        "ProcessConfigLimits",
        "cpu_usage:Float! memory_bytes:Uint64! waitfor_seconds:Uint64!",
    ),
    (
        "ProcessState",
        "state:String! order:String! runtime_seconds:Uint64! reconnect_seconds:Int! cpu_usage:Float! memory_bytes:Uint64! last_logline:String! command:[String!]! progress:Progress!",
    ),
    (
        "ProcessReport",
        "history:[ProcessReportHistoryEntry!]! created_at:Time! prelude:[String!]! log:[ProcessReportLogEntry!]!",
    ),
    ("ProcessReportHistoryEntry", REPORT),
    ("ProcessReportLogEntry", "timestamp:Time! data:String!"),
    (
        "Progress",
        "input:[ProgressIO!]! output:[ProgressIO!]! frame:Uint64! packet:Uint64! fps:Float! q:Float! size_kb:Uint64! time:Float! bitrate_kbit:Float! speed:Float! drop:Uint64! dup:Uint64!",
    ),
    (
        "ProgressIO",
        "id:String! address:String! index:Uint64! stream:Uint64! format:String! codec:String! coder:String! type:String! fps:Float! frame:Uint64! packet:Uint64! pps:Float! size_kb:Uint64! bitrate_kbit:Float! pixfmt:String! q:Float! width:Uint64! height:Uint64! sampling:Uint64! layout:String! channels:Uint64! avstream:AVStream",
    ),
    (
        "AVStreamIO",
        "state:String! packet:Uint64! time:Uint64! size_kb:Uint64!",
    ),
    (
        "AVStream",
        "input:AVStreamIO! output:AVStreamIO! aqueue:Uint64! queue:Uint64! dup:Uint64! drop:Uint64! enc:Uint64! looping:Boolean! duplicating:Boolean! gop:String!",
    ),
    ("Probe", "streams:[ProbeIO!]! log:[String!]!"),
    (
        "ProbeIO",
        "url:String! index:Uint64! stream:Uint64! language:String! type:String! codec:String! coder:String! bitrate_kbps:Float! duration_seconds:Float! fps:Float! pix_fmt:String! width:Uint64! height:Uint64! sampling:Uint64! layout:String! channels:Uint64!",
    ),
    (
        "Metrics",
        "metrics:[Metric!]! interval_seconds:Int timerange_seconds:Int",
    ),
    (
        "Metric",
        "name:String! labels:Map values:[MetricsResponseValue!]!",
    ),
    (
        "RawAVstream",
        "id:String! url:String! stream:Uint64! queue:Uint64! aqueue:Uint64! dup:Uint64! drop:Uint64! enc:Uint64! looping:Boolean! duplicating:Boolean! gop:String! debug:Any input:RawAVstreamIO! output:RawAVstreamIO! swap:RawAVstreamSwap!",
    ),
    (
        "RawAVstreamIO",
        "state:State! packet:Uint64! time:Uint64! size_kb:Uint64!",
    ),
    (
        "RawAVstreamSwap",
        "url:String! status:String! lasturl:String! lasterror:String!",
    ),
];
