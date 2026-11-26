mod cluster;
mod dto;
mod error;
mod identity;
pub(crate) mod media;
pub(crate) mod media_http;
mod openapi;
mod operations;
mod process;
pub(crate) mod process_dto;
mod zones;

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use axum::{
    Json, Router,
    extract::{
        ConnectInfo, DefaultBodyLimit, MatchedPath, Request, State, rejection::JsonRejection,
    },
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tracing::Instrument;
use uuid::Uuid;

use crate::{
    App, LEGACY_API_PREFIX,
    auth::{Identity, Permission, TokenError, TokenUse, hash_password},
    config::{self, SCHEMA, Settings},
    persistence::{Record, Store},
    state::{take_secret, validate, validate_password},
};
pub(crate) use error::ApiError;

pub fn router(app: Arc<App>) -> Router {
    let management = Router::new()
        .route("/config", get(get_config).put(set_config))
        .route("/config/reload", get(reload_config))
        .merge(process::routes())
        .merge(cluster::routes())
        .merge(identity::routes())
        .merge(zones::routes())
        .merge(operations::routes())
        .merge(media::routes())
        .route_layer(middleware::from_fn_with_state(app.clone(), authorize));
    let documentation = Router::new()
        .route("/api/swagger", get(openapi::redirect))
        .route("/api/swagger/", get(openapi::redirect))
        .route("/api/swagger/index.html", get(openapi::index))
        .route("/api/swagger/doc.json", get(openapi::document))
        .route_layer(middleware::from_fn_with_state(app.clone(), authorize));
    Router::new()
        .route(
            "/ping",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/plain; charset=UTF-8")],
                    "pong",
                )
            }),
        )
        .route("/api", get(discovery))
        .route("/api/login", post(login))
        .route("/api/login/refresh", get(refresh))
        .merge(documentation)
        .merge(operations::public())
        .route("/api/v3/widget/process/{id}", get(media::widget))
        .nest(LEGACY_API_PREFIX, management)
        .fallback(media_http::public)
        .method_not_allowed_fallback(|| async {
            ApiError::Http(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed")
        })
        .layer(DefaultBodyLimit::max(128 * 1024))
        .layer(middleware::from_fn_with_state(app.clone(), correlate))
        .with_state(app)
}

async fn correlate(State(app): State<Arc<App>>, mut request: Request, next: Next) -> Response {
    let id = Uuid::new_v4().to_string();
    let socket = socket_peer(&request);
    let address = forwarded_peer(socket, request.headers(), &app.trusted_proxies);
    let tls = request
        .extensions()
        .get::<ConnectInfo<crate::TlsPeer>>()
        .is_some()
        || (socket.is_some_and(|peer| {
            app.trusted_proxies
                .iter()
                .any(|range| range.contains(&peer))
        }) && address.is_some()
            && request
                .headers()
                .get_all("x-forwarded-proto")
                .iter()
                .eq(std::iter::once(&HeaderValue::from_static("https"))));
    request.extensions_mut().insert(ClientPeer { address, tls });
    let storage_request = request.uri().path().starts_with("/api/v3/fs");
    let upload = matches!(*request.method(), Method::PUT | Method::POST)
        && (request.uri().path().starts_with("/api/v3/fs")
            || !request.uri().path().starts_with("/api"));

    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("<unmatched>", MatchedPath::as_str);
    let span = tracing::info_span!("http", request_id = %id, method = %request.method(), route);
    async move {
        let counter = app.ops.request(storage_request);
        let mut response = if upload {
            next.run(request).await
        } else if let Ok(_limit) = app.requests.try_acquire() {
            match tokio::time::timeout(app.request_timeout, next.run(request)).await {
                Ok(response) => response,
                Err(_) => {
                    ApiError::Http(StatusCode::REQUEST_TIMEOUT, "request timed out").into_response()
                }
            }
        } else {
            ApiError::Http(StatusCode::SERVICE_UNAVAILABLE, "server busy").into_response()
        };
        if let Ok(value) = HeaderValue::from_str(&id) {
            response.headers_mut().insert("x-request-id", value);
        }
        response
            .headers_mut()
            .entry(header::CACHE_CONTROL)
            .or_insert(HeaderValue::from_static("no-store"));
        response.headers_mut().insert(
            "x-content-type-options",
            HeaderValue::from_static("nosniff"),
        );
        counter.status(response.status().as_u16());
        if storage_request && response.status().is_server_error() {
            app.ops.emit("storage", "failure", "", "");
        }
        tracing::debug!(status = response.status().as_u16(), "request completed");
        response
    }
    .instrument(span)
    .await
}

#[derive(Clone, Copy)]
struct ClientPeer {
    address: Option<IpAddr>,
    tls: bool,
}

fn forwarded_peer(
    socket: Option<IpAddr>,
    headers: &HeaderMap,
    trusted: &[ipnet::IpNet],
) -> Option<IpAddr> {
    let socket = socket?;
    let known = |ip: &IpAddr| trusted.iter().any(|range| range.contains(ip));
    if !known(&socket) {
        return Some(socket);
    }
    let lines: Vec<_> = headers.get_all("x-forwarded-for").iter().take(5).collect();
    if lines.len() > 4 || lines.iter().map(|header| header.len()).sum::<usize>() > 1024 {
        return None;
    }
    let mut hops = 0;
    for line in lines.into_iter().rev() {
        for value in line.to_str().ok()?.rsplit(',') {
            hops += 1;
            if hops > 16 {
                return None;
            }
            let candidate = value.trim().parse::<IpAddr>().ok()?.to_canonical();
            if !known(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn peer(request: &Request) -> Option<IpAddr> {
    request
        .extensions()
        .get::<ClientPeer>()
        .map_or_else(|| socket_peer(request), |peer| peer.address)
}

fn secure_transport(request: &Request) -> bool {
    request.extensions().get::<ClientPeer>().map_or_else(
        || {
            request
                .extensions()
                .get::<ConnectInfo<crate::TlsPeer>>()
                .is_some()
        },
        |peer| peer.tls,
    )
}

fn socket_peer(request: &Request) -> Option<IpAddr> {
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip().to_canonical())
        .or_else(|| {
            request
                .extensions()
                .get::<ConnectInfo<crate::TlsPeer>>()
                .map(|info| info.0.0.ip().to_canonical())
        })
}

async fn read_settings(app: &App) -> Result<(Record, Settings), ApiError> {
    let record = app.store.read().await?;
    let settings = serde_json::from_value(app.effective(&record)?)?;
    Ok((record, settings))
}

fn check_network(
    settings: &Settings,
    request: &Request,
    peer: Option<IpAddr>,
) -> Result<(), ApiError> {
    let network = if secure_transport(request) {
        &settings.api.access.https
    } else {
        &settings.api.access.http
    };
    let matches = |ranges: &[ipnet::IpNet]| {
        peer.is_some_and(|peer| ranges.iter().any(|range| range.contains(&peer)))
    };
    if network.block.as_deref().is_some_and(matches)
        || network
            .allow
            .as_deref()
            .is_some_and(|allow| !allow.is_empty() && !matches(allow))
    {
        return Err(ApiError::forbidden("access denied"));
    }
    Ok(())
}

async fn identify(
    app: &App,
    record: &Record,
    settings: &Settings,
    headers: &HeaderMap,
    peer: Option<IpAddr>,
    purpose: TokenUse,
) -> Result<Identity, ApiError> {
    let bearer = headers
        .get_all(header::AUTHORIZATION)
        .iter()
        .filter_map(|header| header.to_str().ok()?.strip_prefix("Bearer "))
        .find(|token| !token.is_empty());
    if let Some(token) = bearer {
        let (subject, generation) =
            app.auth
                .authenticate(token, purpose)
                .map_err(|error| match error {
                    TokenError::Invalid => invalid_token(),
                    TokenError::WrongUse => ApiError::forbidden("invalid token"),
                })?;
        if subject == settings.api.auth.username {
            return if record.generation.as_deref() == Some(generation.as_str()) {
                Ok(Identity::Administrator)
            } else {
                Err(invalid_token())
            };
        }
        let account = if let Some(cluster) = &app.cluster {
            match cluster.identity(&subject).await? {
                Some(identity) => {
                    crate::iam::Principal::from_account(identity.account, identity.rules)?
                }
                None => None,
            }
        } else {
            app.iam.principal(&subject).await?
        }
        .ok_or_else(invalid_token)?;
        if account.generation != generation {
            return Err(invalid_token());
        }
        return Ok(Identity::Managed(account));
    }
    if purpose == TokenUse::Access {
        let loopback = peer.is_some_and(|ip| ip.is_loopback());
        if app.cluster.is_none()
            && ((!settings.api.auth.enable && (loopback || app.environment.allow_insecure))
                || (loopback && settings.api.auth.disable_localhost))
        {
            return Ok(Identity::TrustedLocalClient);
        }
    }
    Ok(Identity::Anonymous)
}

fn invalid_token() -> ApiError {
    ApiError::Http(StatusCode::UNAUTHORIZED, "invalid token")
}

async fn authorize(
    State(app): State<Arc<App>>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let path = request
        .extensions()
        .get::<axum::extract::OriginalUri>()
        .map_or(request.uri().path(), |uri| uri.0.path());
    let process_route = path.starts_with("/api/v3/process")
        || path.starts_with("/api/v3/metadata")
        || path.starts_with("/api/v3/report")
        || path.starts_with("/api/v3/skills");
    let reload = path.ends_with("/config/reload");
    let read_post = path == "/api/v3/metrics" || path.starts_with("/api/v3/events");
    let write = (matches!(
        *request.method(),
        Method::PUT | Method::POST | Method::DELETE
    ) && !read_post)
        || reload;
    // Serialize policy decisions and writes so a request cannot write with a stale policy.
    let _guard = if write
        && ["/api/v3/config", "/api/v3/iam", "/api/v3/zone"]
            .iter()
            .any(|prefix| path.starts_with(prefix))
    {
        Some(app.writes.lock().await)
    } else {
        None
    };
    let (record, settings) = read_settings(&app).await?;
    let source = peer(&request);
    check_network(&settings, &request, source)?;
    let identity = identify(
        &app,
        &record,
        &settings,
        request.headers(),
        source,
        TokenUse::Access,
    )
    .await?;
    let permission = match (process_route, write) {
        (true, true) => Permission::WriteProcess,
        (true, false) => Permission::ReadProcess,
        (false, true) => Permission::WriteConfig,
        (false, false) => Permission::ReadConfig,
    };
    if !identity.permits(permission)
        && !managed_route(&identity, path, request.uri(), request.method(), write)
    {
        return Err(ApiError::forbidden("access denied"));
    }
    if app.cluster.is_some()
        && write
        && (path == "/api/v3/process" || path.starts_with("/api/v3/process/"))
        && path != "/api/v3/process/probe"
        && path != "/api/v3/process/validate"
    {
        return Err(ApiError::Http(
            StatusCode::CONFLICT,
            "use the cluster process API to change desired state",
        ));
    }
    if app.cluster.is_some() && write && path.starts_with("/api/v3/iam/user") {
        return Err(ApiError::Http(
            StatusCode::CONFLICT,
            "use the cluster IAM API for managed identities",
        ));
    }
    if write && settings.api.read_only {
        return Err(if reload || process_route {
            ApiError::not_found()
        } else {
            ApiError::Http(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed")
        });
    }
    if reload && request.method() == Method::HEAD {
        return Err(ApiError::Http(
            StatusCode::METHOD_NOT_ALLOWED,
            "Method Not Allowed",
        ));
    }
    if process_route {
        let owner = identity.name().map(str::to_owned).unwrap_or_else(|| {
            if settings.api.auth.username.is_empty() {
                "$root".into()
            } else {
                settings.api.auth.username
            }
        });
        request.extensions_mut().insert(process::Caller(owner));
    }
    request.extensions_mut().insert(identity);
    Ok(next.run(request).await)
}

fn managed_route(
    identity: &Identity,
    path: &str,
    uri: &axum::http::Uri,
    method: &Method,
    write: bool,
) -> bool {
    if identity.name().is_none() {
        return false;
    }
    let Some(path) = path.strip_prefix("/api/v3/") else {
        return path.starts_with("/api/swagger");
    };
    let domain = media::query_uri(uri, "domain").unwrap_or_default();
    let action = if write { "write" } else { "read" };
    let (group, rest) = path.split_once('/').unwrap_or((path, ""));
    let component = rest.split('/').next().unwrap_or_default();
    let resource = percent_encoding::percent_decode_str(component).decode_utf8();
    let Ok(resource) = resource else {
        return false;
    };
    match (group, rest) {
        ("process", "") if !write => true,
        ("report", "process") if !write => true,
        ("process", "") if *method == Method::POST => true,
        ("process", "probe" | "validate") if write => false,
        ("process", _) if write => {
            (matches!(*method, Method::PUT | Method::DELETE) && !rest.contains('/')
                || rest.strip_suffix("/command").is_some())
                && identity.allows(&domain, "process", &resource, action)
        }
        ("process", _) => identity.allows(&domain, "process", &resource, action),
        ("iam", "user") if !write => true,
        ("iam", _) if !write => true,
        ("zone", "") if !write => true,
        ("zone", _) if !write => {
            &*resource == crate::zone::DEFAULT
                || identity.allows(&domain, "zone", &resource, action)
        }
        ("fs", _) if !rest.is_empty() && rest.contains('/') => {
            if write && rest.ends_with('/') {
                return false;
            }
            let path = percent_encoding::percent_decode_str(rest).decode_utf8();
            path.is_ok_and(|path| identity.allows(&domain, "fs", &format!("/{path}"), action))
        }
        (
            "metrics" | "log" | "alias" | "rtmp" | "srt" | "session" | "config" | "metadata"
            | "skills",
            _,
        ) if !write => identity.allows(&domain, "api", &format!("/api/v3/{path}"), action),
        _ => false,
    }
}

async fn discovery(
    State(app): State<Arc<App>>,
    request: Request,
) -> Result<Json<dto::Discovery<'static>>, ApiError> {
    let (record, settings) = read_settings(&app).await?;
    let source = peer(&request);
    check_network(&settings, &request, source)?;
    let full = identify(
        &app,
        &record,
        &settings,
        request.headers(),
        source,
        TokenUse::Access,
    )
    .await?
    .permits(Permission::ReadDiscovery);
    let auths: &[_] = if record.password_hash.is_some() {
        &["localjwt"]
    } else {
        &[]
    };
    let node = if full {
        Some(dto::Node {
            id: settings.id,
            name: settings.name,
            created_at: app
                .started_at
                .format(&Rfc3339)
                .map_err(|_| ApiError::Internal("timestamp"))?,
            uptime_seconds: (OffsetDateTime::now_utc() - app.started_at)
                .whole_seconds()
                .max(0),
            resources: {
                let host = app.ops.host();
                dto::Resources {
                    ncpu: host.logical_cpus.max(1),
                    cpu_limit: host
                        .effective_cpus
                        .map_or(0, |limit| (limit * 100.0) as u64),
                    cpu_used: host.host_cpu_percent.map_or(0, |usage| usage as u64),
                    memory_total_bytes: host.total_memory.unwrap_or_default(),
                    memory_limit_bytes: host.cgroup_memory_limit.unwrap_or_default(),
                    memory_used_bytes: host.cgroup_memory_used.unwrap_or_default(),
                    ..Default::default()
                }
            },
        })
    } else {
        None
    };
    Ok(Json(dto::Discovery {
        app: "datarhei-core",
        auths,
        version: dto::Version {
            number: if full {
                env!("CARGO_PKG_VERSION")
            } else {
                concat!(env!("CARGO_PKG_VERSION_MAJOR"), ".0.0")
            },
            build: full.then_some(dto::Build {
                repository_commit: option_env!("GITHUB_SHA").unwrap_or(""),
                repository_branch: option_env!("GITHUB_REF_NAME").unwrap_or("development"),
                build_date: "",
                arch: match std::env::consts::ARCH {
                    "x86_64" => "linux/amd64",
                    "aarch64" => "linux/arm64",
                    other => other,
                },
                compiler: "rustc",
            }),
        },
        node,
        capabilities: dto::Capabilities {
            api_version: 3,
            configuration: true,
            configuration_write: !settings.api.read_only,
            process_engine: true,
            local_login: !auths.is_empty(),
            media: true,
            clustering: app.cluster.is_some(),
        },
    }))
}

async fn login(
    State(app): State<Arc<App>>,
    request: Request,
) -> Result<Json<dto::Tokens>, ApiError> {
    use axum::RequestExt;
    let peer = peer(&request).ok_or_else(|| ApiError::forbidden("access denied"))?;
    let (record, settings) = read_settings(&app).await?;
    check_network(&settings, &request, Some(peer))?;
    if !app.auth.allow_attempt(peer) {
        return Err(ApiError::Throttled);
    }
    let Json(credentials): Json<dto::Login> = request
        .extract()
        .await
        .map_err(|_| ApiError::forbidden("access denied"))?;
    let (username, password) = credentials.into_credentials();
    let invalid = || ApiError::forbidden("invalid username or password");
    if username.len() > 128 || password.len() > 1024 {
        return Err(invalid());
    }
    let account = if username == settings.api.auth.username {
        record.password_hash.zip(record.generation)
    } else if let Some(cluster) = &app.cluster {
        cluster
            .identity(&username)
            .await?
            .map(|identity| identity.account)
            .filter(|account| account.active)
            .map(|account| (account.password_hash, account.generation))
    } else {
        app.iam
            .get(&username)
            .await?
            .filter(|account| account.active)
            .map(|account| (account.password_hash, account.generation))
    };
    if !app
        .auth
        .verify(password, account.as_ref().map(|(hash, _)| hash.as_str()))
        .await
        .map_err(|_| ApiError::Throttled)?
    {
        return Err(invalid());
    }
    let (_, generation) = account.ok_or_else(invalid)?;
    let generation = generation.as_str();
    Ok(Json(dto::Tokens {
        access_token: issue(
            &app,
            &username,
            generation,
            TokenUse::Access,
            settings.api.auth.jwt.access_ttl_sec,
        )?,
        refresh_token: issue(
            &app,
            &username,
            generation,
            TokenUse::Refresh,
            settings.api.auth.jwt.refresh_ttl_sec,
        )?,
    }))
}

async fn refresh(
    State(app): State<Arc<App>>,
    request: Request,
) -> Result<Json<dto::Refreshed>, ApiError> {
    let (record, settings) = read_settings(&app).await?;
    check_network(&settings, &request, peer(&request))?;
    let identity = identify(
        &app,
        &record,
        &settings,
        request.headers(),
        peer(&request),
        TokenUse::Refresh,
    )
    .await?;
    let (name, generation) = match &identity {
        Identity::Administrator => (
            settings.api.auth.username.as_str(),
            record.generation.as_deref().ok_or_else(invalid_token)?,
        ),
        Identity::Managed(identity) => (identity.name.as_str(), identity.generation.as_str()),
        _ => return Err(ApiError::forbidden("access denied")),
    };
    Ok(Json(dto::Refreshed {
        access_token: issue(
            &app,
            name,
            generation,
            TokenUse::Access,
            settings.api.auth.jwt.access_ttl_sec,
        )?,
    }))
}

fn issue(
    app: &App,
    username: &str,
    generation: &str,
    purpose: TokenUse,
    ttl: i64,
) -> Result<String, ApiError> {
    app.auth
        .issue_with_ttl(username, generation, purpose, ttl)
        .map_err(|_| ApiError::Internal("token signing"))
}

async fn get_config(State(app): State<Arc<App>>) -> Result<Response, ApiError> {
    let record = app.store.read().await?;
    let mut config = app.effective(&record)?;
    config::redact(&mut config);
    config["created_at"] = Value::String(record.created_at.clone());
    let loaded_at = app.loaded_at.read().await;
    Ok(Json(dto::Configuration {
        created_at: &record.created_at,
        updated_at: &record.updated_at,
        loaded_at: &loaded_at,
        config,
        overrides: app.environment.override_names(),
    })
    .into_response())
}

async fn set_config(
    State(app): State<Arc<App>>,
    axum::Extension(actor): axum::Extension<Identity>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<&'static str>, ApiError> {
    let Json(patch) = body.map_err(|_| ApiError::bad_request("invalid JSON"))?;
    let patch = config::migrate_patch(patch)
        .ok_or_else(|| ApiError::bad_request("invalid config version"))?;
    let mut tx = app.store.write().await?;
    let record = Store::read_for_write(&mut tx)
        .await?
        .ok_or(ApiError::Internal("missing configuration"))?;
    let mut document = serde_json::from_str(&record.document)?;
    config::merge(&mut document, patch)
        .map_err(|_| ApiError::bad_request("invalid JSON field type"))?;
    let password = take_secret(&mut document, "/api/auth/password");
    if take_secret(&mut document, "/api/auth/jwt/secret").is_some() {
        return Err(ApiError::validation(
            "api.auth.jwt.secret",
            "configure the signing key via CORE_API_AUTH_JWT_SECRET",
        ));
    }
    let effective = app.environment.apply(document.clone());
    let settings = validate(
        &effective,
        password.is_some() || record.password_hash.is_some(),
    )
    .map_err(|field| ApiError::validation(field, "invalid value"))?;
    let collides: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM iam_identity WHERE name = ?)")
            .bind(&settings.api.auth.username)
            .fetch_one(&mut *tx)
            .await?;
    if collides {
        return Err(ApiError::validation(
            "api.auth.username",
            "name belongs to a managed identity",
        ));
    }
    crate::engine::Options::from_document(&effective, &app.environment.engine)
        .map_err(|error| ApiError::validation("ffmpeg", error.public()))?;
    crate::media::validate_settings(&effective)
        .map_err(|_| ApiError::validation("storage", "invalid media settings"))?;
    crate::certificates::Acme::parse(&effective, &app.environment.db)
        .map_err(|field| ApiError::validation(field, "invalid ACME configuration"))?;
    let new_address = config::address(&settings.address)
        .map_err(|_| ApiError::validation("address", "invalid value"))?;
    if (!app.listening_address().await.ip().is_loopback() || !new_address.ip().is_loopback())
        && !settings.api.auth.enable
        && !app.environment.allow_insecure
    {
        return Err(ApiError::validation(
            "api.auth.enable",
            "authentication required for external listeners",
        ));
    }
    if let Some(password) = password {
        if app.environment.password_is_managed() {
            return Err(ApiError::validation(
                "api.auth.password",
                "managed by environment",
            ));
        }
        validate_password(&password)
            .map_err(|_| ApiError::validation("api.auth.password", "requires 12–1024 bytes"))?;
        let hash = hash_password(password).await.map_err(ApiError::Internal)?;
        Store::save_password(&mut tx, &hash, &Uuid::new_v4().to_string()).await?;
    } else if let Some(hash) = &record.password_hash {
        let previous: Settings = serde_json::from_value(app.effective(&record)?)?;
        if previous.api.auth.username != settings.api.auth.username {
            Store::save_password(&mut tx, hash, &Uuid::new_v4().to_string()).await?;
        }
    }
    if document.to_string().len() > 128 * 1024 {
        return Err(ApiError::validation(
            "config",
            "stored configuration exceeds 128 KiB",
        ));
    }
    let now = timestamp()?;
    Store::save(&mut tx, SCHEMA, &document, &record.created_at, &now).await?;
    operations::record(
        &mut tx,
        "configuration",
        "update",
        Some(actor.name().unwrap_or("$root")),
        None,
    )
    .await?;
    tx.commit().await?;
    app.ops.emit("configuration", "update", "", "");
    Ok(Json("OK"))
}

async fn reload_config(State(app): State<Arc<App>>) -> Result<Json<&'static str>, ApiError> {
    let (record, settings) = read_settings(&app).await?;
    let effective = app.effective(&record)?;
    let engine = crate::engine::Options::from_document(&effective, &app.environment.engine)
        .map_err(|error| ApiError::validation("ffmpeg", error.public()))?;
    validate(&effective, record.password_hash.is_some())
        .map_err(|field| ApiError::validation(field, "invalid value"))?;
    let address = config::address(&settings.address)
        .map_err(|_| ApiError::validation("address", "invalid value"))?;
    app.reload_address(address)
        .await
        .map_err(|detail| ApiError::validation("address", detail))?;
    app.processes
        .options(engine.with_media(app.media.endpoints(app.media_address().await, true)));
    (app.on_reload)(&settings.log.level);
    *app.loaded_at.write().await = timestamp()?;
    app.ops.emit("configuration", "reload", "", "");
    Ok(Json("OK"))
}

fn timestamp() -> Result<String, ApiError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| ApiError::Internal("timestamp"))
}
