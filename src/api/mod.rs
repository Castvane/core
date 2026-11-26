mod dto;
mod error;
mod media;
mod media_http;
mod openapi;
mod process;
mod process_dto;

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use axum::{
    Extension, Json, Router,
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
use error::ApiError;

pub fn router(app: Arc<App>) -> Router {
    let management = Router::new()
        .route("/config", get(get_config).put(set_config))
        .route("/config/reload", get(reload_config))
        .merge(process::routes())
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

async fn correlate(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let id = Uuid::new_v4().to_string();
    let upload = matches!(*request.method(), Method::PUT | Method::POST)
        && (request.uri().path().starts_with("/api/v3/fs")
            || !request.uri().path().starts_with("/api"));

    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("<unmatched>", MatchedPath::as_str);
    let span = tracing::info_span!("http", request_id = %id, method = %request.method(), route);
    async move {
        let mut response = if upload {
            next.run(request).await
        } else if let Ok(_limit) = app.requests.try_acquire() {
            match tokio::time::timeout(Duration::from_secs(15), next.run(request)).await {
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
        tracing::debug!(status = response.status().as_u16(), "request completed");
        response
    }
    .instrument(span)
    .await
}

fn peer(request: &Request) -> Option<IpAddr> {
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
    let network = if request
        .extensions()
        .get::<ConnectInfo<crate::TlsPeer>>()
        .is_some()
    {
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

fn identify(
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
        return app
            .auth
            .validate(
                token,
                &settings.api.auth.username,
                record.generation.as_deref(),
                purpose,
            )
            .map_err(|error| match error {
                TokenError::Invalid => ApiError::Http(StatusCode::UNAUTHORIZED, "invalid token"),
                TokenError::WrongUse => ApiError::forbidden("invalid token"),
            });
    }
    if purpose == TokenUse::Access {
        let loopback = peer.is_some_and(|ip| ip.is_loopback());
        if (!settings.api.auth.enable && (loopback || app.environment.allow_insecure))
            || (loopback && settings.api.auth.disable_localhost)
        {
            return Ok(Identity::TrustedLocalClient);
        }
    }
    Ok(Identity::Anonymous)
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
    let write = matches!(
        *request.method(),
        Method::PUT | Method::POST | Method::DELETE
    ) || reload;
    // Serialize policy decisions and writes so a request cannot write with a stale policy.
    let _guard = if write && path.starts_with("/api/v3/config") {
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
    )?;
    let permission = match (process_route, write) {
        (true, true) => Permission::WriteProcess,
        (true, false) => Permission::ReadProcess,
        (false, true) => Permission::WriteConfig,
        (false, false) => Permission::ReadConfig,
    };
    if !identity.permits(permission) {
        return Err(ApiError::forbidden("access denied"));
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
    request.extensions_mut().insert(identity);
    if process_route {
        let owner = settings.api.auth.username;
        request
            .extensions_mut()
            .insert(process::Caller(if owner.is_empty() {
                "$root".into()
            } else {
                owner
            }));
    }
    Ok(next.run(request).await)
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
    )?
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
            resources: dto::Resources {
                ncpu: std::thread::available_parallelism().map_or(0, usize::from),
                ..Default::default()
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
                repository_branch: "rust",
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
            clustering: false,
        },
    }))
}

async fn login(
    State(app): State<Arc<App>>,
    connection: Extension<ConnectInfo<SocketAddr>>,
    body: Result<Json<dto::Login>, JsonRejection>,
) -> Result<Json<dto::Tokens>, ApiError> {
    if !app.auth.allow_attempt(connection.0.0.ip().to_canonical()) {
        return Err(ApiError::Throttled);
    }
    let Json(credentials) = body.map_err(|_| ApiError::forbidden("access denied"))?;
    let (username, password) = credentials.into_credentials();
    let invalid = || ApiError::forbidden("invalid username or password");
    if username.len() > 128 || password.len() > 1024 {
        return Err(invalid());
    }
    let (record, settings) = read_settings(&app).await?;
    let matches = username == settings.api.auth.username;
    if !app
        .auth
        .verify(
            password,
            record.password_hash.as_deref().filter(|_| matches),
        )
        .await
        .map_err(|_| ApiError::Throttled)?
        || !matches
    {
        return Err(invalid());
    }
    let generation = record.generation.as_deref().ok_or_else(invalid)?;
    Ok(Json(dto::Tokens {
        access_token: issue(&app, &username, generation, TokenUse::Access)?,
        refresh_token: issue(&app, &username, generation, TokenUse::Refresh)?,
    }))
}

async fn refresh(
    State(app): State<Arc<App>>,
    request: Request,
) -> Result<Json<dto::Refreshed>, ApiError> {
    let (record, settings) = read_settings(&app).await?;
    let identity = identify(
        &app,
        &record,
        &settings,
        request.headers(),
        peer(&request),
        TokenUse::Refresh,
    )?;
    let denied = || ApiError::forbidden("access denied");
    if !matches!(identity, Identity::Administrator) {
        return Err(denied());
    }
    let generation = record.generation.as_deref().ok_or_else(denied)?;
    Ok(Json(dto::Refreshed {
        access_token: issue(
            &app,
            &settings.api.auth.username,
            generation,
            TokenUse::Access,
        )?,
    }))
}

fn issue(
    app: &App,
    username: &str,
    generation: &str,
    purpose: TokenUse,
) -> Result<String, ApiError> {
    app.auth
        .issue(username, generation, purpose)
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
    crate::engine::Options::from_document(&effective, &app.environment.engine)
        .map_err(|error| ApiError::validation("ffmpeg", error.public()))?;
    crate::media::validate_settings(&effective)
        .map_err(|_| ApiError::validation("storage", "invalid media settings"))?;
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
    tx.commit().await?;
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
        .options(engine.with_media(app.media.endpoints(app.media_address().await)));
    (app.on_reload)(&settings.log.level);
    *app.loaded_at.write().await = timestamp()?;
    Ok(Json("OK"))
}

fn timestamp() -> Result<String, ApiError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| ApiError::Internal("timestamp"))
}
