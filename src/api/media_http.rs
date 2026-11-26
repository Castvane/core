use std::{
    ops::Range,
    sync::Arc,
    time::{Duration, SystemTime},
};

use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::Engine as _;
use futures_util::{StreamExt, TryStreamExt};
use tokio_util::io::ReaderStream;

use super::{ApiError, identify, peer, read_settings};
use crate::{
    App,
    auth::{Permission, TokenUse},
    engine::executable::launch,
    media::{
        Media, Mount, content_type, playlist,
        sessions::Transfer,
        storage::{Error, Key, Meta, Storage, Stream, now_secs},
    },
};

const MAX_MANIFEST: u64 = 1024 * 1024;
const IDLE: Duration = Duration::from_secs(15);

#[derive(Default)]
pub struct Parameters {
    pub filename: Option<String>,
    pub session: Option<String>,
    pub token: Option<String>,
    dash_session: Option<String>,
    secure: bool,
}

pub fn parameters(query: Option<&str>) -> Parameters {
    let mut parameters = Parameters::default();
    for (name, value) in url::form_urlencoded::parse(query.unwrap_or_default().as_bytes()).take(40)
    {
        match name.as_ref() {
            "filename" if value.len() <= 255 => parameters.filename = Some(value.into_owned()),
            "session" if value.len() <= 64 => parameters.session = Some(value.into_owned()),
            "token" if value.len() <= 4096 => parameters.token = Some(value.into_owned()),
            _ => {}
        }
    }
    parameters
}

pub async fn public(State(app): State<Arc<App>>, request: Request) -> Result<Response, ApiError> {
    let raw = request.uri().path();
    if raw == "/api" || raw.starts_with("/api/") {
        return Err(ApiError::not_found());
    }
    let raw = if raw.ends_with('/') {
        format!("{raw}index.html")
    } else {
        raw.to_owned()
    };
    let path = percent_encoding::percent_decode_str(&raw)
        .decode_utf8()
        .map_err(|_| Error::Invalid)?
        .into_owned();
    if path == "/api" || path.starts_with("/api/") || path == "/ping" {
        return Err(ApiError::not_found());
    }
    let media = &app.media;
    let source_peer = peer(&request);
    let secure = request
        .extensions()
        .get::<axum::extract::ConnectInfo<crate::tls::TlsPeer>>()
        .is_some();
    let mut parameters = parameters(request.uri().query());
    parameters.secure = secure;
    let cookies = request
        .headers()
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let cookie = |name: &str| {
        cookies
            .split(';')
            .filter_map(|pair| pair.trim().split_once('='))
            .find_map(|(key, value)| (key == name && value.len() <= 4096).then_some(value))
    };
    let query_token = parameters.token.clone();
    if parameters.token.is_none() {
        parameters.token = cookie("core_media").map(str::to_owned);
    }
    parameters.dash_session = cookie("core_dash")
        .filter(|value| value.len() == 32)
        .map(str::to_owned);
    let origin = request.headers().get(header::ORIGIN).cloned();
    let allowed_origin = origin.filter(|origin| {
        origin
            .to_str()
            .ok()
            .is_some_and(|o| media.allowed_origins.iter().any(|v| v == "*" || v == o))
    });
    let method = request.method().clone();
    if method == Method::OPTIONS {
        let mut response = StatusCode::NO_CONTENT.into_response();
        if let Some(origin) = allowed_origin {
            cors(response.headers_mut(), origin);
            response.headers_mut().insert(
                header::ACCESS_CONTROL_ALLOW_METHODS,
                HeaderValue::from_static("GET, HEAD, PUT, POST, DELETE, OPTIONS"),
            );
            response.headers_mut().insert(
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                HeaderValue::from_static("Authorization, Content-Type, Range"),
            );
        }
        return Ok(response);
    }
    let resolved = if method == Method::GET || method == Method::HEAD {
        media.aliases.resolve(&path).await?
    } else {
        path.clone()
    };
    let mut response = if let Some(transport) = &media.transport
        && let Some(prefix) = transport.flv_mount()
        && let Some(channel) = resolved
            .strip_prefix(prefix)
            .filter(|tail| tail.starts_with('/'))
    {
        if method != Method::GET {
            return Err(ApiError::Http(
                StatusCode::METHOD_NOT_ALLOWED,
                "Method Not Allowed",
            ));
        }
        if !transport.allow_rtmp(query_token.as_deref()) {
            return Err(Error::Forbidden.into());
        }
        authorize_download(
            &app,
            request.headers(),
            peer(&request),
            &path,
            &resolved,
            &parameters,
        )
        .await?;
        flv(media, channel).await?
    } else {
        let (mount, key) = media.mounted(&resolved)?;
        match method {
            Method::GET | Method::HEAD => {
                authorize_download(
                    &app,
                    request.headers(),
                    peer(&request),
                    &path,
                    &resolved,
                    &parameters,
                )
                .await?;
                serve(
                    media,
                    mount,
                    key,
                    request.headers(),
                    &method,
                    parameters,
                    Some(&path),
                )
                .await?
            }
            Method::PUT | Method::POST | Method::DELETE => {
                let (record, settings) = read_settings(&app).await?;
                if settings.api.read_only {
                    return Err(ApiError::not_found());
                }
                let trusted = (mount.kind == "mem" && !media.private && !media.memory_require_auth)
                    || ((mount.kind == "mem" || mount.kind == "s3")
                        && basic(media, request.headers()));

                if !trusted {
                    let identity = identify(
                        &app,
                        &record,
                        &settings,
                        request.headers(),
                        peer(&request),
                        TokenUse::Access,
                    )?;
                    if !identity.permits(Permission::WriteConfig) {
                        if mount.kind == "mem" || mount.kind == "s3" {
                            return Ok((
                                StatusCode::UNAUTHORIZED,
                                [(header::WWW_AUTHENTICATE, r#"Basic realm="datarhei-core""#)],
                            )
                                .into_response());
                        }
                        return Err(Error::Forbidden.into());
                    }
                }
                if method == Method::DELETE {
                    mount.backend.delete(&key).await?;
                    format!("Deleted: /{}", key.as_str()).into_response()
                } else {
                    write(media, mount, &key, request.into_body()).await?
                }
            }
            _ => return Err(ApiError::not_found()),
        }
    };
    if let Some(origin) = allowed_origin {
        cors(response.headers_mut(), origin);
    }
    if media.private
        && let Some(token) = query_token
        && method == Method::GET
        && response.status().is_success()
        && app
            .media
            .sessions
            .validate_token(&token, &path, source_peer)
        && let Ok(cookie) = HeaderValue::from_str(&format!(
            "core_media={token}; Path=/; HttpOnly; SameSite=Strict{}",
            if secure { "; Secure" } else { "" }
        ))
    {
        response.headers_mut().append(header::SET_COOKIE, cookie);
    }
    Ok(response)
}

fn cors(headers: &mut HeaderMap, origin: HeaderValue) {
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    headers.append(header::VARY, HeaderValue::from_static("Origin"));
    headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("Content-Length, Content-Range, ETag, Last-Modified"),
    );
}

fn basic(media: &Media, headers: &HeaderMap) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .filter(|v| v.len() <= 8192)
        .and_then(|v| base64::engine::general_purpose::STANDARD.decode(v).ok())
        .and_then(|v| String::from_utf8(v).ok())
        .is_some_and(|v| {
            v.split_once(':')
                .is_some_and(|(user, password)| media.basic(user, password))
        })
}

async fn authorize_download(
    app: &App,
    headers: &HeaderMap,
    source: Option<std::net::IpAddr>,
    path: &str,
    resolved: &str,
    parameters: &Parameters,
) -> Result<(), ApiError> {
    if !app.media.private {
        return Ok(());
    }
    if parameters.token.as_deref().is_some_and(|token| {
        app.media.sessions.validate_token(token, path, source)
            && app.media.sessions.validate_token(token, resolved, source)
    }) || (basic(&app.media, headers)
        && app
            .media
            .mounted(resolved)
            .is_ok_and(|(mount, _)| matches!(mount.kind, "mem" | "s3")))
    {
        return Ok(());
    }
    let (record, settings) = read_settings(app).await?;
    let identity = identify(app, &record, &settings, headers, source, TokenUse::Access)?;
    if !identity.permits(Permission::ReadConfig) {
        return Err(Error::Forbidden.into());
    }
    Ok(())
}

pub async fn write(
    media: &Media,
    mount: &Mount,
    key: &Key,
    body: Body,
) -> Result<Response, ApiError> {
    let _slot = media.uploads.try_acquire().map_err(|_| Error::Full)?;
    let input = Box::pin(body.into_data_stream().map_err(|_| Error::Interrupted));
    let stream = transfer_stream(input, media.stop.clone(), None, media.upload_rate);
    let new = tokio::time::timeout(Duration::from_secs(600), mount.backend.put(key, stream))
        .await
        .map_err(|_| Error::Interrupted)??;
    Ok(if new {
        StatusCode::CREATED
    } else {
        StatusCode::NO_CONTENT
    }
    .into_response())
}

pub async fn serve(
    media: &Media,
    mount: &Mount,
    key: Key,
    headers: &HeaderMap,
    method: &Method,
    parameters: Parameters,
    path: Option<&str>,
) -> Result<Response, ApiError> {
    let public = path.is_some();
    let path = path.unwrap_or_default();
    let slot = media
        .transfers
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error::Full)?;
    let meta = mount.backend.stat(&key).await?;
    let mime = content_type(&media.mime, key.as_str());
    let playlist = public && key.as_str().to_ascii_lowercase().ends_with(".m3u8");
    let dash_manifest = key.as_str().to_ascii_lowercase().ends_with(".mpd");
    let is_manifest = playlist || dash_manifest;
    let personalised = playlist || media.private || !public;
    let mut response_headers = HeaderMap::new();
    add(&mut response_headers, header::CONTENT_TYPE, mime);
    add(&mut response_headers, header::ETAG, &meta.tag);
    if (0..253_402_300_800).contains(&now_secs(meta.modified))
        && meta.modified >= std::time::UNIX_EPOCH
    {
        add(
            &mut response_headers,
            header::LAST_MODIFIED,
            &httpdate::fmt_http_date(meta.modified),
        );
    }
    add(&mut response_headers, header::ACCEPT_RANGES, "bytes");
    if let Some(name) = parameters.filename.as_deref() {
        let quoted = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>();
        add(
            &mut response_headers,
            header::CONTENT_DISPOSITION,
            &format!("attachment; filename=\"{quoted}\""),
        );
    }
    if personalised || is_manifest || parameters.filename.is_some() {
        add(
            &mut response_headers,
            header::CACHE_CONTROL,
            "private, no-store",
        );
    } else {
        add(
            &mut response_headers,
            header::CACHE_CONTROL,
            "public, max-age=30, must-revalidate",
        );
    }
    if !playlist && not_modified(headers, &meta) {
        return Ok((StatusCode::NOT_MODIFIED, response_headers, Body::empty()).into_response());
    }
    let wanted = headers
        .get(header::RANGE)
        .filter(|_| if_range(headers, &meta));
    let (ranges, status) = match wanted {
        Some(range) if !playlist => match parse_ranges(range, meta.size) {
            Ok(ranges) => (ranges, StatusCode::PARTIAL_CONTENT),
            Err(status) => {
                if status == StatusCode::RANGE_NOT_SATISFIABLE {
                    add(
                        &mut response_headers,
                        header::CONTENT_RANGE,
                        &format!("bytes */{}", meta.size),
                    );
                }
                let mut response =
                    ApiError::Http(status, "unsupported or invalid byte range").into_response();
                if let Some(value) = response_headers.remove(header::CONTENT_RANGE) {
                    response.headers_mut().insert(header::CONTENT_RANGE, value);
                }
                return Ok(response);
            }
        },
        _ => (std::iter::once(0..meta.size).collect(), StatusCode::OK),
    };
    let range = ranges.first().cloned().ok_or(Error::Invalid)?;
    let multipart = if ranges.len() > 1 {
        let boundary = uuid::Uuid::new_v4().simple().to_string();
        add(
            &mut response_headers,
            header::CONTENT_TYPE,
            &format!("multipart/byteranges; boundary={boundary}"),
        );
        Some(MultipartRanges::new(boundary, mime, meta.size, ranges))
    } else {
        if status == StatusCode::PARTIAL_CONTENT {
            add(
                &mut response_headers,
                header::CONTENT_RANGE,
                &content_range(&range, meta.size),
            );
        }
        None
    };
    if *method == Method::HEAD {
        if !playlist {
            add(
                &mut response_headers,
                header::CONTENT_LENGTH,
                &multipart
                    .as_ref()
                    .map_or(range.end - range.start, |ranges| ranges.len)
                    .to_string(),
            );
        }
        return Ok((status, response_headers, Body::empty()).into_response());
    }
    let labelled_hls = playlist || parameters.session.is_some();
    let dash_segment = !labelled_hls
        && parameters.dash_session.is_some()
        && matches!(
            std::path::Path::new(key.as_str())
                .extension()
                .and_then(|part| part.to_str()),
            Some("m4s" | "mp4" | "webm")
        );
    let dash = dash_manifest || dash_segment;
    let directory = path.rsplit_once('/').map_or("", |(prefix, _)| prefix);
    let reference = if labelled_hls || dash {
        directory
    } else {
        path
    }
    .trim_start_matches('/');
    let transfer = if public {
        let (id, protocol) = if labelled_hls {
            (parameters.session.as_deref(), "hls")
        } else if dash {
            (parameters.dash_session.as_deref(), "dash")
        } else {
            (None, "http")
        };
        Some(media.sessions.open(id, protocol, reference)?)
    } else {
        None
    };
    if dash_manifest
        && let Some(viewer) = &transfer
        && !directory.contains([';', ',', '\\', ' '])
        && let Ok(cookie) = HeaderValue::from_str(&format!(
            "core_dash={}; Path={directory}/; HttpOnly; SameSite=Strict{}",
            viewer.id,
            if parameters.secure { "; Secure" } else { "" }
        ))
    {
        response_headers.append(header::SET_COOKIE, cookie);
    }
    let (source, len) = if playlist {
        if meta.size > MAX_MANIFEST {
            return Err(Error::Full.into());
        }
        let source = mount.backend.read(&meta, range).await?;
        let data = collect(source, MAX_MANIFEST).await?;
        let text = std::str::from_utf8(&data).map_err(|_| Error::Invalid)?;
        if !text.trim_start_matches('\u{feff}').starts_with("#EXTM3U") {
            return Err(Error::Invalid.into());
        }
        let session = transfer.as_ref().ok_or(Error::Unavailable)?;
        let value = playlist::rewrite(
            text,
            path,
            headers.get(header::HOST).and_then(|v| v.to_str().ok()),
            &session.id,
            parameters.token.as_deref(),
        );
        response_headers.remove(header::ETAG);
        response_headers.remove(header::LAST_MODIFIED);
        response_headers.remove(header::ACCEPT_RANGES);
        let bytes = Bytes::from(value);
        let len = bytes.len() as u64;
        (
            Box::pin(futures_util::stream::once(async move { Ok(bytes) })) as Stream,
            len,
        )
    } else if let Some(ranges) = multipart {
        let len = ranges.len;
        (ranges.stream(mount.backend.clone(), meta.clone()), len)
    } else {
        let len = range.end - range.start;
        (
            read(
                media,
                mount,
                &meta,
                range,
                !personalised && !is_manifest && status == StatusCode::OK,
            )
            .await?,
            len,
        )
    };
    add(
        &mut response_headers,
        header::CONTENT_LENGTH,
        &len.to_string(),
    );
    let mut source = transfer_stream(source, media.stop.clone(), transfer, media.download_rate);
    let stream: Stream = Box::pin(async_stream::try_stream! {
        let _slot=slot;
        while let Some(chunk)=source.next().await {yield chunk?;}
    });
    let body = Body::from_stream(stream);
    Ok((status, response_headers, body).into_response())
}

async fn read(
    media: &Media,
    mount: &Mount,
    meta: &Meta,
    range: Range<u64>,
    cacheable: bool,
) -> Result<Stream, Error> {
    if let Some(cache) = &media.cache
        && cacheable
        && mount.kind == "disk"
        && meta.size <= cache.size
    {
        let ext = std::path::Path::new(meta.key.as_str())
            .extension()
            .and_then(|v| v.to_str())
            .map_or_else(String::new, |v| format!(".{v}"));
        if !cache.block.contains(&ext) && (cache.allow.is_empty() || cache.allow.contains(&ext)) {
            let tag = meta.tag.trim_matches('"');
            if let Ok(key) = Key::parse(tag) {
                if let Ok(hit) = cache.data.stat(&key).await {
                    if hit.modified.elapsed().is_ok_and(|age| age < cache.ttl)
                        && let Ok(source) = cache.data.read(&hit, 0..hit.size).await
                    {
                        return Ok(source);
                    }
                    let _ = cache.data.delete(&key).await;
                }
                let Ok(_fill) = cache.fills.try_acquire() else {
                    return mount.backend.read(meta, range).await;
                };
                let bytes =
                    collect(mount.backend.read(meta, range.clone()).await?, cache.size).await?;
                let copy = bytes.clone();
                let source = Box::pin(futures_util::stream::once(async move { Ok(copy) }));
                let _ = cache.data.put(&key, source).await;
                return Ok(Box::pin(futures_util::stream::once(
                    async move { Ok(bytes) },
                )));
            }
        }
    }
    mount.backend.read(meta, range).await
}

async fn collect(mut source: Stream, max: u64) -> Result<Bytes, Error> {
    let mut output = bytes::BytesMut::new();
    while let Some(chunk) = tokio::time::timeout(IDLE, source.next())
        .await
        .map_err(|_| Error::Interrupted)?
    {
        let chunk = chunk?;
        if output.len() as u64 + chunk.len() as u64 > max {
            return Err(Error::Full);
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output.freeze())
}

pub fn transfer_stream(
    mut stream: Stream,
    stop: tokio_util::sync::CancellationToken,
    transfer: Option<Transfer>,
    rate: u64,
) -> Stream {
    Box::pin(async_stream::try_stream! {
        let mut deadline=tokio::time::Instant::now();
        loop {
            let next=tokio::select! {
                _=stop.cancelled()=>Err(Error::Interrupted),
                next=tokio::time::timeout(IDLE,stream.next())=>next.map_err(|_|Error::Interrupted),
            }?;
            let Some(chunk)=next else {break};
            let mut chunk=chunk?;
            while !chunk.is_empty() {
                let bytes=chunk.split_to(chunk.len().min(if rate>0 {rate.clamp(1,64*1024) as usize}else{64*1024}));
                if rate>0 {
                    deadline=deadline.max(tokio::time::Instant::now())+Duration::from_secs_f64(bytes.len() as f64/rate as f64);
                    tokio::time::sleep_until(deadline).await;
                }
                if let Some(transfer)=&transfer {transfer.account(bytes.len()).await?;}
                yield bytes;
            }
        }
    })
}

pub fn add(headers: &mut HeaderMap, name: axum::http::HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

fn not_modified(headers: &HeaderMap, meta: &Meta) -> bool {
    if let Some(etag) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        return etag
            .split(',')
            .map(str::trim)
            .any(|v| v == "*" || v.trim_start_matches("W/") == meta.tag);
    }
    since(headers, header::IF_MODIFIED_SINCE)
        .is_some_and(|date| now_secs(meta.modified) <= now_secs(date))
}

fn if_range(headers: &HeaderMap, meta: &Meta) -> bool {
    headers
        .get(header::IF_RANGE)
        .and_then(|v| v.to_str().ok())
        .is_none_or(|v| {
            v == meta.tag
                || httpdate::parse_http_date(v)
                    .is_ok_and(|date| now_secs(meta.modified) <= now_secs(date))
        })
}

fn since(headers: &HeaderMap, name: axum::http::HeaderName) -> Option<SystemTime> {
    httpdate::parse_http_date(headers.get(name)?.to_str().ok()?).ok()
}

fn content_range(range: &Range<u64>, size: u64) -> String {
    format!(
        "bytes {}-{}/{}",
        range.start,
        range.end.saturating_sub(1),
        size
    )
}

struct MultipartRanges {
    parts: Vec<(Bytes, Range<u64>)>,
    end: Bytes,
    len: u64,
}

impl MultipartRanges {
    fn new(boundary: String, mime: &str, size: u64, ranges: Vec<Range<u64>>) -> Self {
        let end = Bytes::from(format!("--{boundary}--\r\n"));
        let parts: Vec<_> = ranges
            .into_iter()
            .map(|range| {
                let header = Bytes::from(format!(
                    "--{boundary}\r\nContent-Type: {mime}\r\nContent-Range: {}\r\n\r\n",
                    content_range(&range, size)
                ));
                (header, range)
            })
            .collect();
        let len = end.len() as u64
            + parts
                .iter()
                .map(|(header, range)| header.len() as u64 + range.end - range.start + 2)
                .sum::<u64>();
        Self { parts, end, len }
    }

    fn stream(self, backend: std::sync::Arc<dyn Storage>, meta: Meta) -> Stream {
        Box::pin(async_stream::try_stream! {
            for (header, range) in self.parts {
                let mut source = backend.read(&meta, range).await?;
                yield header;
                while let Some(chunk) = source.next().await { yield chunk?; }
                yield Bytes::from_static(b"\r\n");
            }
            yield self.end;
        })
    }
}

fn parse_ranges(value: &HeaderValue, size: u64) -> Result<Vec<Range<u64>>, StatusCode> {
    let invalid = StatusCode::BAD_REQUEST;
    let unsat = StatusCode::RANGE_NOT_SATISFIABLE;
    let value = value
        .to_str()
        .map_err(|_| invalid)?
        .strip_prefix("bytes=")
        .ok_or(invalid)?;
    let mut ranges = Vec::new();
    for (index, item) in value.split(',').enumerate() {
        if index >= 8 {
            return Err(unsat);
        }
        match parse_range(item.trim(), size) {
            Ok(range) => ranges.push(range),
            Err(status) if status == unsat => {}
            Err(status) => return Err(status),
        }
    }
    let total = ranges.iter().fold(0u64, |total, range| {
        total.saturating_add(range.end - range.start)
    });
    if ranges.is_empty() || total > size {
        return Err(unsat);
    }
    Ok(ranges)
}

fn parse_range(value: &str, size: u64) -> Result<Range<u64>, StatusCode> {
    let invalid = StatusCode::BAD_REQUEST;
    let unsat = StatusCode::RANGE_NOT_SATISFIABLE;
    let (start, end) = value.split_once('-').ok_or(invalid)?;
    if size == 0 {
        return Err(unsat);
    }
    if start.is_empty() {
        let suffix = end.parse::<u64>().map_err(|_| invalid)?;
        if suffix == 0 {
            return Err(unsat);
        }
        return Ok(size.saturating_sub(suffix)..size);
    }
    let start = start.parse::<u64>().map_err(|_| invalid)?;
    let end = if end.is_empty() {
        size
    } else {
        end.parse::<u64>()
            .map_err(|_| invalid)?
            .saturating_add(1)
            .min(size)
    };
    if start >= end {
        return Err(unsat);
    }
    Ok(start..end)
}

async fn flv(media: &Media, channel: &str) -> Result<Response, ApiError> {
    let slot = media
        .flv
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error::Full)?;
    let transport = media.transport.as_ref().ok_or(Error::Missing)?;
    let channel = channel.strip_suffix(".flv").unwrap_or(channel);
    if !transport
        .connections("rtmp")
        .await?
        .iter()
        .any(|conn| conn.publisher && conn.path == channel.trim_start_matches('/'))
    {
        return Err(Error::Missing.into());
    }
    let url = transport.rtmp_url(channel)?;
    let transfer = media
        .sessions
        .open(None, "flv", channel.trim_start_matches('/'))?;
    let mut args = vec!["-nostdin", "-v", "error"];
    if url.starts_with("rtmps://") {
        args.extend(["-tls_verify", "1"]);
        if let Some(ca) = transport.rtmp_tls_trust() {
            args.extend(["-ca_file", ca]);
        }
    }
    args.extend(["-i", &url, "-c", "copy", "-f", "flv", "pipe:1"]);
    let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
    let (mut child, group) =
        launch(&media.ffmpeg, &args, media.launcher.as_deref()).map_err(|_| Error::Unavailable)?;
    let stderr = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        if let Some(mut stderr) = stderr {
            let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
        }
    });
    let stdout = child.stdout.take().ok_or(Error::Unavailable)?;
    let mut source = ReaderStream::with_capacity(stdout, 64 * 1024);
    let first = tokio::time::timeout(Duration::from_secs(8), source.next())
        .await
        .map_err(|_| Error::Unavailable)?
        .ok_or(Error::Unavailable)?
        .map_err(Error::from)?;
    let stream = Box::pin(async_stream::try_stream! {
        let (_child,_group,_slot,_stderr)=(child,group,slot,stderr_task);
        yield first;
        while let Some(chunk)=source.next().await {yield chunk.map_err(Error::from)?;}
    });
    let body = Body::from_stream(transfer_stream(
        stream,
        media.stop.clone(),
        Some(transfer),
        media.download_rate,
    ));
    Ok((
        [
            (header::CONTENT_TYPE, "video/x-flv"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response())
}
