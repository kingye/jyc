//! WeCom KF (Customer Service) outbound wire-format helper.
//!
//! Pipe-only architecture (see docs/core-hub-adapters.md): the hub
//! channel owns the reply lifecycle; this module only knows how to put a
//! text message on the wire via the KF `send_msg` API.
//!
//! Note: KF `send_msg` supports text/image/voice/video/file/news/msgmenu/
//! miniprogram — markdown is NOT supported, so replies go out as text.
//! Attachments are not relayed (same as the pre-migration behavior, where
//! the outbound adapter ignored them).

use anyhow::{Context, Result};

use crate::wecom::kf_client::KfApiClient;

/// Send a text message via the KF `send_msg` API, retrying on rate limit
/// (errcode 95001) up to 3 attempts with a 5s backoff.
pub async fn send_kf_text(
    kf_client: &KfApiClient,
    open_kfid: &str,
    touser: &str,
    text: &str,
) -> Result<()> {
    let mut last_error = None;
    for attempt in 1..=3 {
        match kf_client
            .send_message(open_kfid, touser, "text", text)
            .await
        {
            Ok(_) => return Ok(()),
            Err(e) => {
                let err_msg = format!("{e:?}");
                if err_msg.contains("95001") && attempt < 3 {
                    tracing::warn!(
                        attempt,
                        max_attempts = 3,
                        delay_sec = 5,
                        "KF send_msg rate limited (95001), retrying..."
                    );
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    last_error = Some(e);
                } else {
                    return Err(e).with_context(|| {
                        format!(
                            "failed to send KF message to {} via {} (attempt {})",
                            touser, open_kfid, attempt
                        )
                    });
                }
            }
        }
    }

    Err(last_error.expect("retry loop runs at least once")).with_context(|| {
        format!(
            "failed to send KF message to {} via {} after 3 attempts",
            touser, open_kfid
        )
    })
}
