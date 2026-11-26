// OpenRaft defines the error types returned by its storage and transport traits.
#![allow(clippy::result_large_err)]

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Extension, State},
    http::StatusCode,
    routing::post,
};
use hyper_util::rt::TokioIo;
use openraft::{
    BasicNode, RaftNetwork, RaftNetworkFactory,
    error::{InstallSnapshotError, RPCError, RaftError, RemoteError, Unreachable},
    network::RPCOption,
    raft::{
        AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest,
        InstallSnapshotResponse, VoteRequest, VoteResponse,
    },
};
use rustls::{
    ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};
use serde::{Serialize, de::DeserializeOwned};
use tokio::{net::TcpListener, sync::Semaphore};
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use super::{Peer, Raft, Types, model::MAX_STATE};
use crate::StartupError;

const MAX_MESSAGE: usize = 1_048_576;
const VERSION: &str = "/internal/cluster/v1";

#[derive(Clone)]
pub struct Network {
    pub client: reqwest::Client,
    pub peers: Arc<BTreeMap<u64, Peer>>,
    pub cluster: Arc<str>,
}

pub struct Connection {
    network: Network,
    target: u64,
}

pub fn tls(
    ca_pem: &[u8],
    cert: &[u8],
    key: &[u8],
    identity: &[u8],
) -> Result<(Arc<ServerConfig>, reqwest::Client), StartupError> {
    let mut roots = rustls::RootCertStore::empty();
    let mut client_roots = Vec::new();
    for certificate in CertificateDer::pem_slice_iter(ca_pem) {
        let certificate = certificate?;
        roots.add(certificate.clone())?;
        client_roots.push(reqwest::Certificate::from_der(certificate.as_ref())?);
    }
    if roots.is_empty() {
        return Err("cluster CA contains no valid certificates".into());
    }
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let auth = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots),
        provider.clone(),
    )
    .build()?;
    let server = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(auth)
        .with_single_cert(
            CertificateDer::pem_slice_iter(cert).collect::<Result<Vec<_>, _>>()?,
            PrivateKeyDer::from_pem_slice(key)?,
        )?;
    let client = reqwest::Client::builder()
        .use_rustls_tls()
        .tls_certs_only(client_roots)
        .identity(reqwest::Identity::from_pem(identity)?)
        .min_tls_version(reqwest::tls::Version::TLS_1_3)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_millis(500))
        .timeout(Duration::from_secs(3))
        .build()?;
    Ok((Arc::new(server), client))
}

impl RaftNetworkFactory<Types> for Network {
    type Network = Connection;
    async fn new_client(&mut self, target: u64, _node: &BasicNode) -> Connection {
        Connection {
            network: self.clone(),
            target,
        }
    }
}

impl Connection {
    async fn call<T: DeserializeOwned, E: DeserializeOwned + std::error::Error + 'static>(
        &self,
        path: &str,
        body: impl Serialize,
        option: RPCOption,
    ) -> Result<T, RPCError<u64, BasicNode, RaftError<u64, E>>> {
        let unavailable = |error: &str| {
            RPCError::Unreachable(Unreachable::new(&std::io::Error::other(error.to_owned())))
        };
        let peer = self
            .network
            .peers
            .get(&self.target)
            .ok_or_else(|| unavailable("peer is not provisioned"))?;
        let response = self
            .network
            .client
            .post(format!("{}{VERSION}/{path}", peer.address))
            .header("x-castvane-cluster", self.network.cluster.as_ref())
            .json(&body)
            .timeout(option.hard_ttl())
            .send()
            .await
            .map_err(|error| RPCError::Unreachable(Unreachable::new(&error)))?;
        if !response.status().is_success() {
            return Err(unavailable("cluster peer rejected the RPC"));
        }
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| RPCError::Unreachable(Unreachable::new(&error)))?
        {
            if bytes.len() + chunk.len() > MAX_MESSAGE {
                return Err(unavailable("cluster RPC response exceeds limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let result: Result<T, RaftError<u64, E>> = serde_json::from_slice(&bytes)
            .map_err(|error| RPCError::Unreachable(Unreachable::new(&error)))?;
        result.map_err(|error| RPCError::RemoteError(RemoteError::new(self.target, error)))
    }
}

impl RaftNetwork<Types> for Connection {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<Types>,
        option: RPCOption,
    ) -> Result<AppendEntriesResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        self.call("append", rpc, option).await
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<u64>,
        option: RPCOption,
    ) -> Result<VoteResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        self.call("vote", rpc, option).await
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<Types>,
        option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<u64>,
        RPCError<u64, BasicNode, RaftError<u64, InstallSnapshotError>>,
    > {
        self.call("snapshot", rpc, option).await
    }
}

fn sender(peer: u64, vote: openraft::Vote<u64>) -> Result<(), StatusCode> {
    if vote.leader_id().node_id == peer {
        Ok(())
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

pub async fn serve(
    listener: TcpListener,
    config: Arc<ServerConfig>,
    network: Network,
    raft: Raft,
    stop: CancellationToken,
    telemetry: super::work::SharedTelemetry,
    media: Arc<crate::media::Media>,
) {
    let Network { peers, cluster, .. } = network;
    let files = Router::new().nest(VERSION, super::files::routes(raft.clone(), media));
    let routes = Router::new()
        .nest(
            VERSION,
            Router::new()
                .route(
                    "/read-fence",
                    post(|State(raft): State<Raft>| async move {
                        raft.ensure_linearizable()
                            .await
                            .map(Json)
                            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)
                    }),
                )
                .route(
                    "/telemetry",
                    post(move || {
                        let state = telemetry.clone();
                        async move { Json(state.read().await.clone()) }
                    }),
                )
                .route(
                    "/append",
                    post(
                        |State(raft): State<Raft>,
                         Extension(peer): Extension<u64>,
                         Json(rpc): Json<AppendEntriesRequest<Types>>| async move {
                            sender(peer, rpc.vote)?;
                            Ok::<_, StatusCode>(Json(raft.append_entries(rpc).await))
                        },
                    ),
                )
                .route(
                    "/vote",
                    post(
                        |State(raft): State<Raft>,
                         Extension(peer): Extension<u64>,
                         Json(rpc): Json<VoteRequest<u64>>| async move {
                            sender(peer, rpc.vote)?;
                            Ok::<_, StatusCode>(Json(raft.vote(rpc).await))
                        },
                    ),
                )
                .route(
                    "/snapshot",
                    post(
                        |State(raft): State<Raft>,
                         Extension(peer): Extension<u64>,
                         Json(rpc): Json<InstallSnapshotRequest<Types>>| async move {
                            sender(peer, rpc.vote)?;
                            if rpc.offset.saturating_add(rpc.data.len() as u64)
                                > (MAX_STATE + 64 * 1024) as u64
                            {
                                return Err(StatusCode::PAYLOAD_TOO_LARGE);
                            }
                            Ok(Json(raft.install_snapshot(rpc).await))
                        },
                    ),
                ),
        )
        .layer(DefaultBodyLimit::max(MAX_MESSAGE))
        .with_state(raft)
        .merge(files);
    let slots = Arc::new(Semaphore::new(64));
    let acceptor = TlsAcceptor::from(config);
    loop {
        let accepted = tokio::select! {
            _ = stop.cancelled() => break,
            value = listener.accept() => value,
        };
        let Ok((tcp, _)) = accepted else {
            break;
        };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            continue;
        };
        let acceptor = acceptor.clone();
        let peers = peers.clone();
        let routes = routes.clone();
        let stop = stop.clone();
        let cluster = cluster.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let Ok(Ok(socket)) =
                tokio::time::timeout(Duration::from_secs(3), acceptor.accept(tcp)).await
            else {
                return;
            };
            let Some(cert) = socket
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|chain| chain.first())
            else {
                return;
            };
            let Some((&id, _)) = peers.iter().find(|(_, peer)| peer.certificate == *cert) else {
                return;
            };
            let routes = routes.layer(Extension(id));
            let service = hyper::service::service_fn(
                move |request: hyper::Request<hyper::body::Incoming>| {
                    let routes = routes.clone();
                    let cluster = cluster.clone();
                    async move {
                        if request
                            .headers()
                            .get("x-castvane-cluster")
                            .and_then(|value| value.to_str().ok())
                            != Some(cluster.as_ref())
                        {
                            return Ok(axum::response::IntoResponse::into_response(
                                StatusCode::FORBIDDEN,
                            ));
                        }
                        match tokio::time::timeout(Duration::from_secs(4), routes.oneshot(request))
                            .await
                        {
                            Ok(result) => result,
                            Err(_) => Ok(axum::response::IntoResponse::into_response(
                                StatusCode::REQUEST_TIMEOUT,
                            )),
                        }
                    }
                },
            );
            tokio::select! {
                _ = stop.cancelled() => {},
                _ = hyper::server::conn::http1::Builder::new().timer(hyper_util::rt::TokioTimer::new())
                    .header_read_timeout(Duration::from_secs(5)).serve_connection(TokioIo::new(socket),service) => {},
            }
        });
    }
}
