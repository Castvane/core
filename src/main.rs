#![forbid(unsafe_code)]

use std::{ffi::OsString, os::unix::process::CommandExt, sync::Arc};

use axum::serve::{Listener, ListenerExt};
use restreamer_core::{App, StartupError, config::Environment};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

fn main() -> Result<(), StartupError> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() == Some(std::ffi::OsStr::new("__media_child")) {
        if media_child(args).is_err() {
            eprintln!("configured media executable could not start");
            std::process::exit(126);
        }
        return Ok(());
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve())
}

fn media_child(mut args: impl Iterator<Item = OsString>) -> Result<(), StartupError> {
    use nix::{
        sys::{prctl, signal::Signal},
        unistd::getppid,
    };

    let parent = args
        .next()
        .and_then(|arg| arg.to_str()?.parse::<i32>().ok())
        .ok_or("missing media supervisor PID")?;
    let executable = args.next().ok_or("missing media executable")?;
    prctl::set_pdeathsig(Signal::SIGKILL)?;
    if getppid().as_raw() != parent {
        return Err("media supervisor exited before launch".into());
    }
    prctl::set_no_new_privs()?;
    Err(std::process::Command::new(executable)
        .args(args)
        .exec()
        .into())
}

async fn serve() -> Result<(), StartupError> {
    let environment = Environment::read()?.with_media_launcher(std::env::current_exe()?);
    let mut app = App::open(environment).await?;
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
    let tls = app.bind_tls().await?;
    let app = Arc::new(app);
    tracing::info!(address = %listener.local_addr()?, "restreamer core listening");

    #[cfg(unix)]
    let terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    #[cfg(unix)]
    let shutdown = shutdown(terminate);
    #[cfg(not(unix))]
    let shutdown = shutdown();

    let signals = app.clone();
    let drain = tokio_util::sync::CancellationToken::new();
    let trigger = drain.clone();
    let signals_task = tokio::spawn(async move {
        shutdown.await;
        signals.stop_media_delivery();
        trigger.cancel();
    });
    let tls_task = if let Some(tls) = tls {
        tracing::info!(address=%tls.local_addr()?.0,"restreamer core HTTPS listening");
        let tls_drain = drain.clone();
        let routes = restreamer_core::router(app.clone());
        Some(tokio::spawn(restreamer_core::serve_http(
            tls, routes, tls_drain,
        )))
    } else {
        None
    };
    restreamer_core::serve_http(
        listener.tap_io(|stream| {
            if stream.set_nodelay(true).is_err() {
                tracing::debug!("could not set TCP_NODELAY");
            }
        }),
        restreamer_core::router(app.clone()),
        drain.clone(),
    )
    .await;
    drain.cancel();
    signals_task.abort();
    if let Some(task) = tls_task {
        task.await?;
    }
    app.close().await;
    Ok(())
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
