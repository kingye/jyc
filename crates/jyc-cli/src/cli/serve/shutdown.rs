//! Shutdown helpers + CLI args for `jyc serve`.
//!
//! Extracted from the monolithic `serve.rs`.

use clap::Args;
use std::path::PathBuf;

pub(crate) struct PidFileGuard {
    path: PathBuf,
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl PidFileGuard {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Config file path (default: <config_home>/config.toml, e.g.
    /// ~/.config/jyc/config.toml; or config.toml in --workdir when given)
    #[arg(short, long)]
    pub config: Option<String>,

    /// Use polling instead of IMAP IDLE
    #[arg(long)]
    pub no_idle: bool,

    /// Reset monitoring state before starting
    #[arg(long)]
    pub reset: bool,
}

/// Wait for a shutdown signal (Ctrl+C on all platforms, plus SIGTERM on Unix).
pub(crate) async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to create SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Received Ctrl+C, shutting down...");
            }
            _ = sigterm.recv() => {
                tracing::info!("Received SIGTERM, shutting down...");
            }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("Received Ctrl+C, shutting down...");
    }
}
