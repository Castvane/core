use std::time::Duration;

use axum::{Router, body::Body, extract::ConnectInfo, serve::Listener};
use hyper::{Request, body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// Serve HTTP with bounded connections and allow complete uploads from senders
/// that close their write side without waiting for an HTTP response.
pub async fn serve_http<L>(mut listener: L, router: Router, stop: CancellationToken)
where
    L: Listener,
    L::Addr: Clone + Send + Sync + 'static,
{
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            _ = connections.join_next(), if !connections.is_empty() => {},
            (io, peer) = listener.accept(), if connections.len() < 1024 => {
                let stop = stop.clone();
                let router = router.clone();
                connections.spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let router = router.clone();
                        let mut request = request.map(Body::new);
                        request.extensions_mut().insert(ConnectInfo(peer.clone()));
                        router.oneshot(request)
                    });
                    let mut builder = http1::Builder::new();
                    builder.half_close(true).timer(TokioTimer::new())
                        .header_read_timeout(Duration::from_secs(10));
                    let connection = builder.serve_connection(TokioIo::new(io), service);
                    tokio::pin!(connection);
                    tokio::select! {
                        _ = &mut connection => {},
                        _ = stop.cancelled() => {
                            connection.as_mut().graceful_shutdown();
                            let _ = tokio::time::timeout(Duration::from_secs(5), &mut connection).await;
                        }
                    }
                });
            }
        }
    }
    while connections.join_next().await.is_some() {}
}
