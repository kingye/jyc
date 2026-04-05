//! Configuration validation for Feishu channel.

use anyhow::Result;

use super::config::FeishuConfig;

/// Validate Feishu configuration.
pub fn validate_config(config: &FeishuConfig) -> Result<()> {
    // Validate app_id
    if config.app_id.is_empty() {
        anyhow::bail!("Feishu app_id is required");
    }

    if config.app_id.len() < 5 {
        anyhow::bail!("Feishu app_id appears to be too short");
    }

    // Validate app_secret
    if config.app_secret.is_empty() {
        anyhow::bail!("Feishu app_secret is required");
    }

    if config.app_secret.len() < 10 {
        anyhow::bail!("Feishu app_secret appears to be too short");
    }

    // Validate base_url
    if config.base_url.is_empty() {
        anyhow::bail!("Feishu base_url is required");
    }

    if !config.base_url.starts_with("https://") {
        anyhow::bail!("Feishu base_url must start with https://");
    }

    // Validate WebSocket configuration
    if config.websocket.enabled {
        if config.websocket.reconnect_delay_secs == 0 {
            anyhow::bail!("WebSocket reconnect_delay_secs must be greater than 0");
        }

        if config.websocket.heartbeat_interval_secs < 10 {
            anyhow::bail!("WebSocket heartbeat_interval_secs must be at least 10");
        }
    }

    tracing::info!("Feishu configuration validated successfully");

    Ok(())
}
