//! WeCom KF (Customer Service) outbound adapter implementation.
//!
//! Unlike the regular WeCom outbound adapter (which uses the external contact
//! message API `cgi-bin/externalcontact/message/send`), the KF outbound
//! adapter uses `kf/send_msg` API to send replies to individual customers.
//!
//! Reference: https://developer.work.weixin.qq.com/document/path/94677

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;

use jyc_core::message_storage::MessageStorage;
use jyc_types::{
    InboundMessage, OutboundAdapter, OutboundAttachment, SendResult,
    config::OutboundAttachmentConfig,
};

use crate::wecom::kf_client::KfApiClient;

/// WeCom KF outbound adapter — sends replies via the KF send_msg API.
///
/// Messages are sent to specific customers identified by `external_userid`,
/// through a specific KF account identified by `open_kfid`. Both values
/// are extracted from the original message's metadata.
pub struct WecomKfOutboundAdapter {
    kf_client: Arc<KfApiClient>,
    storage: Arc<MessageStorage>,
    #[allow(dead_code)]
    footer_enabled: bool,
}

impl WecomKfOutboundAdapter {
    /// Create a new KF outbound adapter.
    pub fn new(
        kf_client: Arc<KfApiClient>,
        storage: Arc<MessageStorage>,
        _attachment_config: Option<OutboundAttachmentConfig>,
        footer_enabled: bool,
    ) -> Self {
        Self {
            kf_client,
            storage,
            footer_enabled,
        }
    }

    /// Build the send message payload for a reply.
    fn build_payload(_reply_text: &str, original: &InboundMessage) -> (String, String, String) {
        let open_kfid = original
            .metadata
            .get("open_kfid")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let touser = original
            .metadata
            .get("external_userid")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        // WeCom KF send_msg API supports: text, image, voice, video, file, news, msgmenu, miniprogram.
        // Markdown is NOT supported. Always send as text.
        let msgtype = "text".to_string();

        (open_kfid, touser, msgtype)
    }
}

#[async_trait]
impl OutboundAdapter for WecomKfOutboundAdapter {
    fn channel_type(&self) -> &str {
        "wecomkf"
    }

    async fn connect(&self) -> Result<()> {
        // Verify connectivity by fetching an access token.
        // The KfApiClient delegates to AccessTokenCache which handles
        // the actual API call and caching.
        self.kf_client.verify_connectivity().await?;
        tracing::debug!("WeCom KF outbound: connected (access_token obtained)");
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        // No-op for stateless HTTP
        Ok(())
    }

    fn clean_body(&self, raw_body: &str) -> String {
        // WeCom KF is a simple channel with no quoting conventions to strip
        raw_body.to_string()
    }

    async fn send_reply(
        &self,
        original: &InboundMessage,
        reply_text: &str,
        thread_path: &Path,
        message_dir: &str,
        _attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult> {
        let (open_kfid, touser, msgtype) = Self::build_payload(reply_text, original);

        if open_kfid.is_empty() {
            anyhow::bail!("WeCom KF outbound: missing open_kfid in message metadata");
        }
        if touser.is_empty() {
            anyhow::bail!("WeCom KF outbound: missing external_userid in message metadata");
        }

        // Send via KF send_msg API with retry on rate limit (95001)
        let mut last_error = None;
        for attempt in 1..=3 {
            match self
                .kf_client
                .send_message(&open_kfid, &touser, &msgtype, reply_text)
                .await
            {
                Ok(_) => {
                    last_error = None;
                    break;
                }
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

        if let Some(e) = last_error {
            return Err(e).with_context(|| {
                format!(
                    "failed to send KF message to {} via {} after 3 attempts",
                    touser, open_kfid
                )
            });
        }

        let message_id = format!("wecomkf_{}", crate::wecom::crypto::generate_nonce());
        let result = SendResult {
            message_id: message_id.clone(),
        };

        // Store the reply
        self.storage
            .store_reply(thread_path, reply_text, message_dir)
            .await
            .context("failed to store WeCom KF reply")?;

        Ok(result)
    }

    async fn send_alert(&self, recipient: &str, subject: &str, body: &str) -> Result<SendResult> {
        // The recipient format is "wecomkf:{open_kfid}:{external_userid}"
        let parts: Vec<&str> = recipient
            .strip_prefix("wecomkf:")
            .unwrap_or(recipient)
            .splitn(2, ':')
            .collect();

        if parts.len() < 2 {
            anyhow::bail!(
                "WeCom KF outbound: invalid alert recipient format '{}'. Expected 'wecomkf:open_kfid:external_userid'",
                recipient
            );
        }

        let open_kfid = parts[0];
        let touser = parts[1];

        let content = format!("## {}\n\n{}", subject, body);

        // Note: KF send_msg API does not support "markdown". The "## " subject
        // prefix formatting is sent as plain text, which is acceptable.
        self.kf_client
            .send_message(open_kfid, touser, "text", &content)
            .await
            .with_context(|| format!("failed to send KF alert to {} via {}", touser, open_kfid))?;

        let message_id = format!("wecomkf_{}", crate::wecom::crypto::generate_nonce());
        Ok(SendResult { message_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use jyc_types::MessageContent;

    fn make_kf_message(open_kfid: &str, external_userid: &str, text: &str) -> InboundMessage {
        let mut metadata = HashMap::new();
        metadata.insert(
            "open_kfid".to_string(),
            serde_json::Value::String(open_kfid.to_string()),
        );
        metadata.insert(
            "external_userid".to_string(),
            serde_json::Value::String(external_userid.to_string()),
        );
        InboundMessage {
            id: "test-id".to_string(),
            channel: "wecomkf".to_string(),
            channel_uid: "test-uid".to_string(),
            sender: external_userid.to_string(),
            sender_address: format!("wecomkf:{}", external_userid),
            recipients: vec![],
            topic: "Test".to_string(),
            content: MessageContent {
                text: Some(text.to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata,
            matched_pattern: None,
        }
    }

    #[test]
    fn test_build_payload_text() {
        let msg = make_kf_message("kf001", "user123", "Hello");
        let (open_kfid, touser, msgtype) =
            WecomKfOutboundAdapter::build_payload("Hello World", &msg);
        assert_eq!(open_kfid, "kf001");
        assert_eq!(touser, "user123");
        assert_eq!(msgtype, "text");
    }

    #[test]
    fn test_build_payload_markdown() {
        let msg = make_kf_message("kf001", "user123", "Hello");
        let (open_kfid, touser, msgtype) =
            WecomKfOutboundAdapter::build_payload("## Title\n\n**bold** text", &msg);
        assert_eq!(open_kfid, "kf001");
        assert_eq!(touser, "user123");
        // KF send_msg API does not support "markdown" — always falls back to "text"
        assert_eq!(msgtype, "text");
    }

    #[test]
    fn test_build_payload_markdown_with_code_block() {
        let msg = make_kf_message("kf001", "user123", "Hello");
        let (_, _, msgtype) =
            WecomKfOutboundAdapter::build_payload("```rust\nfn main() {}\n```", &msg);
        // KF send_msg API does not support "markdown" — always falls back to "text"
        assert_eq!(msgtype, "text");
    }

    #[test]
    fn test_build_payload_missing_fields() {
        let msg = InboundMessage {
            id: "test-id".to_string(),
            channel: "wecomkf".to_string(),
            channel_uid: "test-uid".to_string(),
            sender: "user".to_string(),
            sender_address: "wecomkf:user".to_string(),
            recipients: vec![],
            topic: "Test".to_string(),
            content: MessageContent {
                text: Some("Hello".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: HashMap::new(),
            matched_pattern: None,
        };
        let (open_kfid, touser, msgtype) = WecomKfOutboundAdapter::build_payload("Hello", &msg);
        assert_eq!(open_kfid, "");
        assert_eq!(touser, "");
        assert_eq!(msgtype, "text");
    }

    #[test]
    fn test_channel_type() {
        let api_client = Arc::new(KfApiClient::new(Arc::new(
            crate::wecom::token_cache::AccessTokenCache::new(
                "corp_id".to_string(),
                "corp_secret".to_string(),
            ),
        )));
        let storage = Arc::new(MessageStorage::new(&std::env::temp_dir()));
        let adapter = WecomKfOutboundAdapter::new(api_client, storage, None, true);
        assert_eq!(adapter.channel_type(), "wecomkf");
    }

    #[test]
    fn test_clean_body() {
        let api_client = Arc::new(KfApiClient::new(Arc::new(
            crate::wecom::token_cache::AccessTokenCache::new(
                "corp_id".to_string(),
                "corp_secret".to_string(),
            ),
        )));
        let storage = Arc::new(MessageStorage::new(&std::env::temp_dir()));
        let adapter = WecomKfOutboundAdapter::new(api_client, storage, None, true);
        let cleaned = adapter.clean_body("Hello **world**");
        assert_eq!(cleaned, "Hello **world**");
    }
}
