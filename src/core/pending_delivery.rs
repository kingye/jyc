//! Background delivery watcher for MCP tools that need to send messages
//! during an active SSE stream (e.g., the question tool).
//!
//! Channel-agnostic: uses the OutboundAdapter trait for delivery.
//! Watches for `reply-sent.flag` + `reply.md` files and delivers immediately.

use std::path::Path;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::channels::types::{InboundMessage, OutboundAdapter};

const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Watch for pending message deliveries during SSE processing.
///
/// MCP tools (like the question tool) write `reply.md` + `reply-sent.flag`
/// during the SSE stream. This watcher detects them and delivers immediately
/// via the outbound adapter, without waiting for the SSE stream to complete.
///
/// The watcher runs until cancelled (when the agent finishes processing).
pub async fn watch_pending_deliveries(
    thread_path: &Path,
    message_dir: &str,
    message: &InboundMessage,
    outbound: &dyn OutboundAdapter,
    cancel: CancellationToken,
) {
    let jyc_dir = thread_path.join(".jyc");
    let signal_path = jyc_dir.join("reply-sent.flag");
    let reply_path = thread_path.join("messages").join(message_dir).join("reply.md");

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }

        // Check if a pending delivery exists
        if !signal_path.exists() || !reply_path.exists() {
            continue;
        }

        // Read the reply text
        let reply_text = match tokio::fs::read_to_string(&reply_path).await {
            Ok(text) if !text.trim().is_empty() => text,
            _ => continue,
        };

        tracing::info!(
            text_len = reply_text.len(),
            "Delivering pending message from MCP tool (background watcher)"
        );

        // Deliver via outbound adapter (channel-agnostic)
        if let Err(e) = outbound
            .send_reply(
                message,
                &reply_text,
                thread_path,
                message_dir,
                None,
            )
            .await
        {
            tracing::error!(error = %e, "Failed to deliver pending message");
        } else {
            tracing::info!("Pending message delivered successfully");
        }

        // Clean up signal file (reply.md stays for chat log)
        tokio::fs::remove_file(&signal_path).await.ok();
        // Remove reply.md to prevent re-delivery
        tokio::fs::remove_file(&reply_path).await.ok();
    }
}
