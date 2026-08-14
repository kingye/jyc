//! Feishu channel bridge for JYC.
//!
//! Connects to jyc over WebSocket (`/ws/<channel>`) and relays feishu events
//! per the route table in the bridge config. See `docs/plugin-architecture.md`.

mod config;
mod feishu;
mod ws;

use anyhow::Result;

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());
    let cfg = config::BridgeConfig::load(&path)?;
    tracing::info!(
        name = %cfg.name,
        channels = ?cfg.channels(),
        "feishu bridge config loaded"
    );
    Ok(())
}
