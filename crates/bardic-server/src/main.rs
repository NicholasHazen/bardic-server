//! Bardic server entry point.

use bardic_server::{app, clock::SystemClock, config::Config};
use clap::Parser;
use std::sync::Arc;

#[cfg(unix)]
fn shutdown_signal() -> std::io::Result<impl std::future::Future<Output = std::io::Result<()>>> {
    use tokio::signal::unix::{signal, SignalKind};

    // Install both handlers before startup, including the period before the
    // listening socket opens. Both signals use the same graceful drain below.
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = interrupt.recv() => Ok(()),
            _ = terminate.recv() => Ok(()),
        }
    })
}

#[cfg(not(unix))]
fn shutdown_signal() -> std::io::Result<impl std::future::Future<Output = std::io::Result<()>>> {
    Ok(tokio::signal::ctrl_c())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let config = Config::parse();
    let shutdown = match shutdown_signal() {
        Ok(signal) => signal,
        Err(e) => {
            eprintln!("bardic-server: cannot listen for shutdown signals: {e}");
            std::process::exit(1);
        }
    };
    if !config.bind.ip().is_loopback() {
        tracing::warn!(
            bind = %config.bind,
            "listening on the network. Bardic has no passwords: use it only on a network you trust"
        );
    }
    match app::spawn(config, Arc::new(SystemClock)).await {
        Ok(running) => {
            tracing::info!(addr = %running.addr, "bardic-server listening");
            if let Err(e) = shutdown.await {
                tracing::error!(error = %e, "shutdown signal listener failed");
            }
            tracing::info!("shutting down");
            running.stop().await;
        }
        Err(e) => {
            eprintln!("bardic-server: {e}");
            std::process::exit(1);
        }
    }
}
