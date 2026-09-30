//! Bardic server entry point.

use bardic_server::{app, clock::SystemClock, config::Config};
use clap::Parser;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let config = Config::parse();
    if !config.bind.ip().is_loopback() {
        tracing::warn!(
            bind = %config.bind,
            "listening on the network. Bardic has no passwords: use it only on a network you trust"
        );
    }
    match app::spawn(config, Arc::new(SystemClock)).await {
        Ok(running) => {
            tracing::info!(addr = %running.addr, "bardic-server listening");
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
            running.stop().await;
        }
        Err(e) => {
            eprintln!("bardic-server: {e}");
            std::process::exit(1);
        }
    }
}
