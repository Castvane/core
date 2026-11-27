use openraft::rt::WatchReceiver;

use std::{sync::Arc, time::Duration};

use axum::{
    Extension, Json, Router,
    body::{Body, to_bytes},
    extract::{DefaultBodyLimit, Path, Request, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderName, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::{get, put},
};
use serde_json::{Value, json};

use super::{Cluster, Failure, Peer, Raft, network::VERSION};
use crate::{
    api::{ApiError, media, media_http},
    media::{
        Media,
        storage::{Key, now_secs},
    },
};

type Local = State<(Raft, Arc<Media>)>;
pub(crate) const OPERATION_BODY_LIMIT: usize = 16 * 1024;
pub(super) const OPERATION_PATH: &str = "/fs";
pub(super) const OPERATION_TIMEOUT: Duration = Duration::from_secs(10);

pub fn routes(raft: Raft, media: Arc<Media>) -> Router {
    Router::new()
        .route(
            OPERATION_PATH,
            put(operation).layer(DefaultBodyLimit::max(OPERATION_BODY_LIMIT)),
        )
        .route("/fs/{storage}", get(list))
        .route(
            "/fs/{storage}/{*path}",
            get(download).head(download).put(write).delete(delete),
        )
        .with_state((raft, media))
}

async fn check(raft: &Raft, peer: u64) -> Result<(), ApiError> {
    if raft.current_leader().await != Some(peer) {
        return Err(ApiError::forbidden("the current leader is required"));
    }
    Ok(())
}

async fn operation(
    State((raft, media)): Local,
    Extension(peer): Extension<u64>,
    body: Result<Json<media::Operation>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    check(&raft, peer).await?;
    let Json(body) = body.map_err(|_| ApiError::bad_request("invalid filesystem operation"))?;
    media::execute_operation(&media, body).await?;
    Ok(Json(Value::Null))
}

async fn list(
    State((raft, media)): Local,
    Extension(peer): Extension<u64>,
    Path(storage): Path<String>,
    uri: Uri,
) -> Result<Json<serde_json::Value>, ApiError> {
    check(&raft, peer).await?;
    let mount = media.get(&storage)?;
    let files = media::select(mount, &uri, false).await?;
    Ok(Json(json!(files.into_iter().take(4096).map(|file|json!({
        "name":format!("/{}",file.key.as_str()),"size_bytes":file.size,"last_modified":now_secs(file.modified)
    })).collect::<Vec<_>>())))
}

async fn download(
    State((raft, media)): Local,
    Extension(peer): Extension<u64>,
    Path((storage, path)): Path<(String, String)>,
    request: Request,
) -> Result<Response, ApiError> {
    check(&raft, peer).await?;
    media_http::serve(
        &media,
        media.get(&storage)?,
        Key::parse(&path)?,
        request.headers(),
        request.method(),
        media_http::parameters(request.uri().query()),
        None,
    )
    .await
}

async fn write(
    State((raft, media)): Local,
    Extension(peer): Extension<u64>,
    Path((storage, path)): Path<(String, String)>,
    request: Request,
) -> Result<Response, ApiError> {
    check(&raft, peer).await?;
    let bytes = to_bytes(request.into_body(), 1_048_576)
        .await
        .map_err(|_| {
            ApiError::Http(
                StatusCode::PAYLOAD_TOO_LARGE,
                "cluster file uploads are limited to 1 MiB",
            )
        })?;
    media_http::write(
        &media,
        media.get(&storage)?,
        &Key::parse(&path)?,
        Body::from(bytes),
    )
    .await
}

async fn delete(
    State((raft, media)): Local,
    Extension(peer): Extension<u64>,
    Path((storage, path)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    check(&raft, peer).await?;
    let key = Key::parse(&path)?;
    media.get(&storage)?.backend.delete(&key).await?;
    Ok(format!("Deleted: /{}", key.as_str()).into_response())
}

impl Cluster {
    async fn file_peer(&self, id: u64) -> Result<&Peer, Failure> {
        self.quorum().await?;
        let peer = self.peers.get(&id).ok_or(Failure::Missing)?;
        if !self
            .raft
            .metrics()
            .borrow_watched()
            .membership_config
            .nodes()
            .any(|(node, _)| *node == id)
        {
            return Err(Failure::Missing);
        }
        Ok(peer)
    }

    pub(crate) async fn remote_file_operation(
        &self,
        id: u64,
        operation: media::Operation,
    ) -> Result<Response, ApiError> {
        let peer = self.file_peer(id).await?;
        let upstream = self
            .network
            .client
            .put(format!("{}{VERSION}{OPERATION_PATH}", peer.address))
            .header("x-castvane-cluster", self.name.as_str())
            .timeout(OPERATION_TIMEOUT + Duration::from_secs(1))
            .json(&operation)
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    ApiError::Http(
                        StatusCode::REQUEST_TIMEOUT,
                        "filesystem operation timed out",
                    )
                } else {
                    Failure::Unavailable.into()
                }
            })?;
        if upstream.status() == StatusCode::REQUEST_TIMEOUT {
            return Err(ApiError::Http(
                StatusCode::REQUEST_TIMEOUT,
                "filesystem operation timed out",
            ));
        }
        Ok(file_response(upstream))
    }

    pub async fn remote_file(
        &self,
        id: u64,
        storage: &str,
        path: Option<&str>,
        request: Request,
    ) -> Result<Response, ApiError> {
        let peer = self.file_peer(id).await?;
        let mut target = url::Url::parse(&peer.address).map_err(|_| Failure::Invalid)?;
        {
            let mut parts = target.path_segments_mut().map_err(|_| Failure::Invalid)?;
            parts
                .pop_if_empty()
                .extend(["internal", "cluster", "v1", "fs", storage]);
            if let Some(path) = path {
                let key = Key::parse(path)?;
                parts.extend(key.as_str().split('/'));
            }
        }
        target.set_query(request.uri().query());
        let mut outbound = self
            .network
            .client
            .request(request.method().clone(), target)
            .header("x-castvane-cluster", self.name.as_str())
            .timeout(Duration::from_secs(12));
        for name in [
            header::RANGE,
            header::IF_RANGE,
            header::IF_MODIFIED_SINCE,
            header::IF_NONE_MATCH,
        ] {
            if let Some(value) = request.headers().get(&name) {
                outbound = outbound.header(name, value);
            }
        }
        if request.method() == Method::PUT {
            let bytes = to_bytes(request.into_body(), 1_048_576)
                .await
                .map_err(|_| {
                    ApiError::Http(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "cluster file uploads are limited to 1 MiB",
                    )
                })?;
            outbound = outbound.body(bytes);
        }
        let upstream = outbound.send().await.map_err(|_| Failure::Unavailable)?;
        Ok(file_response(upstream))
    }
}

fn file_response(upstream: reqwest::Response) -> Response {
    let status = upstream.status();
    let mut headers = HeaderMap::new();
    for name in [
        header::CONTENT_TYPE,
        header::CONTENT_LENGTH,
        header::CONTENT_RANGE,
        header::ACCEPT_RANGES,
        header::ETAG,
        header::LAST_MODIFIED,
        header::CONTENT_DISPOSITION,
    ] {
        copy_header(upstream.headers(), &mut headers, name);
    }
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("private, no-store"),
    );
    (status, headers, Body::from_stream(upstream.bytes_stream())).into_response()
}

fn copy_header(source: &HeaderMap, target: &mut HeaderMap, name: HeaderName) {
    if let Some(value) = source.get(&name) {
        target.insert(name, value.clone());
    }
}
