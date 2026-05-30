//! WeCom KF (Customer Service) inbound adapter implementation.
//!
//! Unlike the regular WeCom inbound adapter (which receives direct XML push
//! messages via webhook), the KF adapter:
//!
//! 1. Receives `kf_msg_or_event` event notifications via the shared webhook server
//! 2. On notification, pulls actual message content via `kf/sync_msg` API
//! 3. Deduplicates messages by `msgid`
//! 4. Routes messages through the standard pattern matching
//!
//! Thread name follows the pattern `{channel_name}_{sanitized_open_kfid}_{sanitized_external_userid}`.
//!
//! Reference: https://developer.work.weixin.qq.com/document/path/94677

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use jyc_types::{
    ChannelMatcher, ChannelPattern, InboundAdapter, InboundAdapterOptions, InboundMessage,
    MessageContent, PatternMatch,
};
use jyc_utils::helpers::sanitize_for_filesystem;

use crate::wecom::inbound::wecom_match_message;
use crate::wecom::kf_client::{KfApiClient, KfMessage};
use crate::wecom::kf_cursor::KfCursorStore;
use crate::wecom::kf_dedup::KfDedupStore;
use crate::wecom::server::{ChannelWebhookConfig, ParsedWecomMessage, WecomWebhookServer};
use jyc_types::WecomKfConfig;

/// WeCom KF inbound adapter.
///
/// Registers a webhook handler with the shared `WecomWebhookServer` and
/// translates incoming KF event notifications into `InboundMessage`s
/// by pulling messages from the KF `sync_msg` API.
pub struct WecomKfInboundAdapter {
    channel_name: String,
    config: WecomKfConfig,
    server: Arc<WecomWebhookServer>,
    kf_client: Arc<KfApiClient>,
    cursor_store: Arc<KfCursorStore>,
    dedup_store: Arc<KfDedupStore>,
}

impl WecomKfInboundAdapter {
    /// Create a new KF inbound adapter.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &WecomKfConfig,
        channel_name: &str,
        server: Arc<WecomWebhookServer>,
        kf_client: Arc<KfApiClient>,
        cursor_store: Arc<KfCursorStore>,
        dedup_store: Arc<KfDedupStore>,
    ) -> Self {
        Self {
            channel_name: channel_name.to_string(),
            config: config.clone(),
            server,
            kf_client,
            cursor_store,
            dedup_store,
        }
    }
}

/// Convert a synced KF message into an `InboundMessage`.
fn kf_message_to_inbound(msg: &KfMessage, channel_name: &str, token: &str) -> InboundMessage {
    let mut metadata = HashMap::new();
    metadata.insert(
        "open_kfid".to_string(),
        serde_json::Value::String(msg.open_kfid.clone()),
    );
    metadata.insert(
        "external_userid".to_string(),
        serde_json::Value::String(msg.external_userid.clone()),
    );
    metadata.insert(
        "msgid".to_string(),
        serde_json::Value::String(msg.msgid.clone()),
    );
    metadata.insert(
        "token".to_string(),
        serde_json::Value::String(token.to_string()),
    );
    metadata.insert(
        "channel_name".to_string(),
        serde_json::Value::String(channel_name.to_string()),
    );
    metadata.insert(
        "msg_type".to_string(),
        serde_json::Value::String(msg.msgtype.clone()),
    );

    let text_content = msg
        .text
        .as_ref()
        .map(|t| t.content.clone())
        .unwrap_or_default();

    InboundMessage {
        id: uuid::Uuid::new_v4().to_string(),
        channel: "wecomkf".to_string(),
        channel_uid: format!(
            "wecomkf_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ),
        sender: msg.external_userid.clone(),
        sender_address: format!("wecomkf:{}", msg.external_userid),
        recipients: vec![],
        topic: "WeCom KF Message".to_string(),
        content: MessageContent {
            text: if text_content.is_empty() {
                None
            } else {
                Some(text_content)
            },
            html: None,
            markdown: None,
        },
        timestamp: chrono::Utc::now(),
        thread_refs: None,
        reply_to_id: None,
        external_id: Some(msg.msgid.clone()),
        attachments: vec![],
        metadata,
        matched_pattern: None,
    }
}

/// Handle a KF event notification from the webhook.
///
/// This is called when the shared webhook server receives a `kf_msg_or_event`
/// event. It pulls messages from the KF API, deduplicates them, and routes them.
fn handle_kf_event(
    parsed: ParsedWecomMessage,
    kf_client: Arc<KfApiClient>,
    cursor_store: Arc<KfCursorStore>,
    dedup_store: Arc<KfDedupStore>,
    channel_name: String,
    on_message: Arc<dyn Fn(InboundMessage) -> Result<()> + Send + Sync>,
    cancel: CancellationToken,
) -> Result<()> {
    let token = parsed.token.clone();
    let open_kfid = parsed.open_kfid.clone();
    let limit = 100;

    // Spawn an async task for the actual API call.
    // Cursor is read inside the spawned task to avoid a race condition:
    // if two notifications for the same open_kfid arrive rapidly,
    // each task reads the latest cursor independently.
    tokio::spawn(async move {
        // Get the current cursor for this open_kfid
        let mut current_cursor = cursor_store.get_cursor(&open_kfid).unwrap_or_default();
        loop {
            // Check for cancellation before each sync request
            if cancel.is_cancelled() {
                tracing::debug!(
                    open_kfid = %open_kfid,
                    "WeCom KF inbound: sync task cancelled"
                );
                break;
            }

            match kf_client
                .sync_messages(&token, &current_cursor, &open_kfid, limit)
                .await
            {
                Ok(response) => {
                    for msg in &response.msg_list {
                        tracing::debug!(
                            msgid = %msg.msgid,
                            msgtype = %msg.msgtype,
                            external_userid = %msg.external_userid,
                            open_kfid = %msg.open_kfid,
                            "WeCom KF inbound: received message from sync_msg"
                        );

                        // Skip messages without external_userid (system messages, etc.)
                        if msg.external_userid.is_empty() {
                            tracing::debug!(
                                msgid = %msg.msgid,
                                msgtype = %msg.msgtype,
                                "WeCom KF inbound: skipping message with empty external_userid"
                            );
                            continue;
                        }

                        // Dedup check
                        if dedup_store.is_duplicate(&msg.msgid) {
                            tracing::debug!(
                                msgid = %msg.msgid,
                                "KfDedupStore: skipping duplicate message"
                            );
                            continue;
                        }

                        dedup_store.mark_seen(&msg.msgid);

                        // Convert to InboundMessage
                        let inbound = kf_message_to_inbound(msg, &channel_name, &token);

                        // Call the on_message callback
                        if let Err(e) = (on_message)(inbound) {
                            tracing::error!(
                                error = %e,
                                msgid = %msg.msgid,
                                "WeCom KF inbound: on_message callback error"
                            );
                        }
                    }

                    // Save cursor for next sync
                    if !response.next_cursor.is_empty() {
                        current_cursor = response.next_cursor.clone();
                        cursor_store.set_cursor(&open_kfid, &response.next_cursor);
                    }

                    // Check if there are more messages to sync
                    let has_more = response.has_more.unwrap_or(0) != 0;
                    if !has_more || response.next_cursor.is_empty() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        open_kfid = %open_kfid,
                        "WeCom KF inbound: sync_messages error"
                    );
                    break;
                }
            }
        }

        // Flush cursors to disk after sync completes
        if let Err(e) = cursor_store.flush_to_disk().await {
            tracing::warn!(
                open_kfid = %open_kfid,
                error = %e,
                "WeCom KF inbound: failed to flush cursors"
            );
        }
    });

    Ok(())
}

/// Shared helper to derive a KF thread name from message metadata.
///
/// Format: `{sanitized_external_userid_prefix}`
/// One thread per customer (regardless of which KF account they contact).
///
/// WeCom's `external_userid` may contain a variable suffix (session-specific),
/// so we use only the first 12 chars as the stable customer identifier.
fn wecomkf_derive_thread_name(message: &InboundMessage, _default_channel: &str) -> String {
    let external_userid = message
        .metadata
        .get("external_userid")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown_user");

    // Use first 10 chars of external_userid as stable customer identifier.
    // WeCom's external_userid has a stable prefix (~10 chars) + variable suffix.
    let stable_user_id = if external_userid.len() > 10 {
        &external_userid[..10]
    } else {
        external_userid
    };

    sanitize_for_filesystem(stable_user_id)
}

impl ChannelMatcher for WecomKfInboundAdapter {
    fn channel_type(&self) -> &str {
        "wecomkf"
    }

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        _patterns: &[ChannelPattern],
        _pattern_match: Option<&PatternMatch>,
    ) -> String {
        wecomkf_derive_thread_name(message, &self.channel_name)
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        wecom_match_message(message, patterns)
    }
}

#[async_trait]
impl InboundAdapter for WecomKfInboundAdapter {
    async fn start(&self, options: InboundAdapterOptions, cancel: CancellationToken) -> Result<()> {
        let channel_name = self.channel_name.clone();

        // Build the webhook config with KF event handler
        let webhook_config = ChannelWebhookConfig {
            token: self.config.token.clone(),
            encoding_aes_key: self.config.encoding_aes_key.clone(),
            corp_id: self.config.corp_id.clone(),
            on_message: {
                let kf_client = self.kf_client.clone();
                let cursor_store = self.cursor_store.clone();
                let dedup_store = self.dedup_store.clone();
                let channel_name = channel_name.clone();
                let on_message: Arc<dyn Fn(InboundMessage) -> Result<()> + Send + Sync> =
                    Arc::from(options.on_message);
                let cancel_for_handler = cancel.clone();

                Arc::new(move |parsed: ParsedWecomMessage| {
                    // Only process event type messages (kf_msg_or_event notifications)
                    if parsed.msg_type != "event" {
                        tracing::debug!(
                            msg_type = %parsed.msg_type,
                            "WeCom KF inbound: skipping non-event message"
                        );
                        return Ok(());
                    }

                    handle_kf_event(
                        parsed,
                        kf_client.clone(),
                        cursor_store.clone(),
                        dedup_store.clone(),
                        channel_name.clone(),
                        on_message.clone(),
                        cancel_for_handler.clone(),
                    )
                })
            },
        };

        self.server
            .register_channel(&channel_name, webhook_config)
            .await;

        tracing::info!(
            channel = %channel_name,
            "WeCom KF inbound adapter registered webhook handler"
        );

        // KF inbound does not need to run a separate task — the webhook
        // server handles incoming requests. We wait on cancellation.
        // The actual message processing happens in the webhook callbacks.
        cancel.cancelled().await;

        Ok(())
    }
}

/// WeCom KF channel matcher — stateless pattern matching.
///
/// Delegates to `wecom_match_message` for the actual matching logic.
/// The `channel_type` returns `"wecomkf"` so patterns can be configured
/// with `channel = "wecomkf"` for KF-specific rules.
pub struct WecomKfMatcher;

impl ChannelMatcher for WecomKfMatcher {
    fn channel_type(&self) -> &str {
        "wecomkf"
    }

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        _patterns: &[ChannelPattern],
        _pattern_match: Option<&PatternMatch>,
    ) -> String {
        wecomkf_derive_thread_name(message, "wecomkf")
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        wecom_match_message(message, patterns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wecom::kf_client::KfTextContent;
    use crate::wecom::token_cache::AccessTokenCache;

    #[test]
    fn test_derive_thread_name() {
        let config = WecomKfConfig {
            token: "test_token".to_string(),
            encoding_aes_key: "abc123abc123abc123abc123abc123abc123abc123abc123abc12".to_string(),
            corp_id: "ww12345".to_string(),
            corp_secret: "secret".to_string(),
            open_kf_ids: vec![],
            cursor_store_path: None,
            metadata: HashMap::new(),
        };
        let server = Arc::new(WecomWebhookServer::new("127.0.0.1:1"));
        let access_token_cache = Arc::new(AccessTokenCache::new(
            "corp_id".to_string(),
            "corp_secret".to_string(),
        ));
        let kf_client = Arc::new(KfApiClient::new(access_token_cache));
        let cursor_store = Arc::new(KfCursorStore::new(None));
        let dedup_store = Arc::new(KfDedupStore::new());

        let adapter = WecomKfInboundAdapter::new(
            &config,
            "my_kf_bot",
            server,
            kf_client,
            cursor_store,
            dedup_store,
        );

        let mut metadata = HashMap::new();
        metadata.insert(
            "open_kfid".to_string(),
            serde_json::Value::String("kf001".to_string()),
        );
        metadata.insert(
            "external_userid".to_string(),
            serde_json::Value::String("user123".to_string()),
        );
        metadata.insert(
            "channel_name".to_string(),
            serde_json::Value::String("my_kf_bot".to_string()),
        );

        let message = InboundMessage {
            id: "test".to_string(),
            channel: "wecomkf".to_string(),
            channel_uid: "test_uid".to_string(),
            sender: "user123".to_string(),
            sender_address: "wecomkf:user123".to_string(),
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
            external_id: Some("msg_001".to_string()),
            attachments: vec![],
            metadata,
            matched_pattern: None,
        };

        let name = adapter.derive_thread_name(&message, &[], None);
        // external_userid "user123" is < 12 chars, so full value is used
        assert_eq!(name, "user123");
    }

    #[test]
    fn test_derive_thread_name_stable_prefix() {
        // WeCom external_userid has stable prefix + variable suffix
        let mut metadata = HashMap::new();
        metadata.insert(
            "external_userid".to_string(),
            serde_json::Value::String("wmE8OcHAAA358dWFTX0hH4C_bjM15KSQ".to_string()),
        );

        let message = InboundMessage {
            id: "test".to_string(),
            channel: "wecomkf".to_string(),
            channel_uid: "test_uid".to_string(),
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
            external_id: Some("msg_001".to_string()),
            attachments: vec![],
            metadata,
            matched_pattern: None,
        };

        let name = wecomkf_derive_thread_name(&message, "my_kf_bot");
        // First 10 chars of "wmE8OcHAAA358dWFTX0hH4C_bjM15KSQ"
        assert_eq!(name, "wmE8OcHAAA");
    }

    #[test]
    fn test_derive_thread_name_missing_fields() {
        let config = WecomKfConfig {
            token: "test_token".to_string(),
            encoding_aes_key: "abc123abc123abc123abc123abc123abc123abc123abc123abc12".to_string(),
            corp_id: "ww12345".to_string(),
            corp_secret: "secret".to_string(),
            open_kf_ids: vec![],
            cursor_store_path: None,
            metadata: HashMap::new(),
        };
        let server = Arc::new(WecomWebhookServer::new("127.0.0.1:1"));
        let access_token_cache = Arc::new(AccessTokenCache::new(
            "corp_id".to_string(),
            "corp_secret".to_string(),
        ));
        let kf_client = Arc::new(KfApiClient::new(access_token_cache));
        let cursor_store = Arc::new(KfCursorStore::new(None));
        let dedup_store = Arc::new(KfDedupStore::new());

        let adapter = WecomKfInboundAdapter::new(
            &config,
            "my_kf_bot",
            server,
            kf_client,
            cursor_store,
            dedup_store,
        );

        // Empty metadata — should fall back to defaults
        let message = InboundMessage {
            id: "test".to_string(),
            channel: "wecomkf".to_string(),
            channel_uid: "test_uid".to_string(),
            sender: "user123".to_string(),
            sender_address: "wecomkf:user123".to_string(),
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
            external_id: Some("msg_001".to_string()),
            attachments: vec![],
            metadata: HashMap::new(),
            matched_pattern: None,
        };

        let name = adapter.derive_thread_name(&message, &[], None);
        // Falls back to "unknown_user" when external_userid is missing,
        // truncated to first 10 chars
        assert_eq!(name, "unknown_us");
    }

    #[test]
    fn test_channel_type() {
        let config = WecomKfConfig {
            token: "test_token".to_string(),
            encoding_aes_key: "abc123abc123abc123abc123abc123abc123abc123abc123abc12".to_string(),
            corp_id: "ww12345".to_string(),
            corp_secret: "secret".to_string(),
            open_kf_ids: vec![],
            cursor_store_path: None,
            metadata: HashMap::new(),
        };
        let server = Arc::new(WecomWebhookServer::new("127.0.0.1:1"));
        let access_token_cache = Arc::new(AccessTokenCache::new(
            "corp_id".to_string(),
            "corp_secret".to_string(),
        ));
        let kf_client = Arc::new(KfApiClient::new(access_token_cache));
        let cursor_store = Arc::new(KfCursorStore::new(None));
        let dedup_store = Arc::new(KfDedupStore::new());

        let adapter = WecomKfInboundAdapter::new(
            &config,
            "my_kf_bot",
            server,
            kf_client,
            cursor_store,
            dedup_store,
        );

        assert_eq!(adapter.channel_type(), "wecomkf");
    }

    #[test]
    fn test_kf_message_to_inbound() {
        let msg = KfMessage {
            msgid: "msg_001".to_string(),
            open_kfid: "kf001".to_string(),
            external_userid: "user123".to_string(),
            send_time: 1700000000,
            msgtype: "text".to_string(),
            text: Some(KfTextContent {
                content: "Hello, support!".to_string(),
            }),
        };

        let inbound = kf_message_to_inbound(&msg, "my_kf_bot", "token_xxx");
        assert_eq!(inbound.channel, "wecomkf");
        assert_eq!(inbound.sender, "user123");
        assert_eq!(inbound.sender_address, "wecomkf:user123");
        assert_eq!(inbound.content.text.as_deref(), Some("Hello, support!"));
        assert_eq!(inbound.external_id, Some("msg_001".to_string()));
        assert_eq!(
            inbound.metadata.get("open_kfid").and_then(|v| v.as_str()),
            Some("kf001")
        );
        assert_eq!(
            inbound
                .metadata
                .get("external_userid")
                .and_then(|v| v.as_str()),
            Some("user123")
        );
        assert_eq!(
            inbound.metadata.get("token").and_then(|v| v.as_str()),
            Some("token_xxx")
        );
    }

    #[test]
    fn test_kf_message_to_inbound_no_text() {
        let msg = KfMessage {
            msgid: "msg_002".to_string(),
            open_kfid: "kf001".to_string(),
            external_userid: "user456".to_string(),
            send_time: 1700000001,
            msgtype: "image".to_string(),
            text: None,
        };

        let inbound = kf_message_to_inbound(&msg, "my_kf_bot", "token_xxx");
        assert_eq!(inbound.content.text, None);
        assert_eq!(
            inbound.metadata.get("msg_type").and_then(|v| v.as_str()),
            Some("image")
        );
    }
}
