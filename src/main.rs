#![forbid(unsafe_code)]

use std::{net::SocketAddr, sync::Arc};

use axum::serve::{Listener, ListenerExt};
use restreamer_core::{App, StartupError, config::Environment};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<(), StartupError> {
    let mut app = App::open(Environment::read()?).await?;
    let configured_level = app.log_level().await?;
    let explicit_filter = EnvFilter::try_from_default_env().ok();
    let reloadable = explicit_filter.is_none();
    let (filter, handle) = tracing_subscriber::reload::Layer::new(
        explicit_filter.unwrap_or_else(|| log_filter(&configured_level)),
    );
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_ansi(false))
        .init();
    if reloadable {
        app.set_reload_hook(move |level| {
            if handle.reload(log_filter(level)).is_err() {
                tracing::error!("failed to apply log level");
            }
        });
    }
    let listener = app.bind().await?;
    let app = Arc::new(app);
    tracing::info!(address = %listener.local_addr()?, "restreamer core listening");

    #[cfg(unix)]
    let terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    #[cfg(unix)]
    let shutdown = shutdown(terminate);
    #[cfg(not(unix))]
    let shutdown = shutdown();

    let result = axum::serve(
        listener.tap_io(|stream| {
            if stream.set_nodelay(true).is_err() {
                tracing::debug!("could not set TCP_NODELAY");
            }
        }),
        restreamer_core::router(app.clone()).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await;
    app.close().await;
    Ok(result?)
}

#[cfg(unix)]
async fn shutdown(mut terminate: tokio::signal::unix::Signal) {
    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            if let Err(error) = result {
                tracing::error!(%error, "failed to listen for interrupt");
            }
        }
        _ = terminate.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to listen for shutdown");
    }
}

fn log_filter(level: &str) -> EnvFilter {
    EnvFilter::new(if level == "silent" { "off" } else { level })
}
