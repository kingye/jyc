//! WeCom Smart Robot (wecom_bot) outbound adapter implementation.
//!
//! Handles sending replies via WebSocket using `aibot_respond_msg` and
//! proactive messages using `aibot_send_msg`.
//!
//! Supports streaming replies via `msgtype: "stream"`.
//!
//! Reference: doc 101031 - Passive Reply Messages

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::Mutex;

use jyc_core::email_parser;
use jyc_core::message_storage::MessageStorage;
use jyc_types::{
    InboundMessage, OutboundAdapter, OutboundAttachment, OutboundAttachmentConfig, SendResult,
};

use jyc_utils::attachment_validator;

use super::client::{WecomBotConnectionHandle, generate_req_id};
use super::types::{
    CMD_AIBOT_UPLOAD_MEDIA_CHUNK, CMD_AIBOT_UPLOAD_MEDIA_FINISH, CMD_AIBOT_UPLOAD_MEDIA_INIT,
    UploadMediaChunkBody, UploadMediaFinishBody, UploadMediaInitBody,
};

/// Tracks an active streaming message so the final reply can reuse the same
/// `stream.id` and update the message in-place instead of posting a second one.
#[derive(Debug, Clone)]
struct ActiveStream {
    req_id: String,
    stream_id: String,
}

/// WeCom Bot outbound adapter for sending messages via WebSocket.
///
/// Uses a shared `WecomBotConnectionHandle` to push messages into the shared
/// WebSocket connection established by the inbound adapter and to await ack
/// responses (needed for media upload).
pub struct WecomBotOutboundAdapter {
    /// Shared connection handle for sending frames and awaiting responses.
    handle: Arc<Mutex<Option<WecomBotConnectionHandle>>>,
    /// Message storage for logging replies
    storage: Arc<MessageStorage>,
    /// Attachment configuration
    attachment_config: Option<OutboundAttachmentConfig>,
    /// Whether footer is enabled
    footer_enabled: bool,
    /// Currently active stream message (if any). Used to correlate the final
    /// `finish=true` reply with an earlier `finish=false` processing indicator.
    active_stream: Arc<Mutex<Option<ActiveStream>>>,
}

impl WecomBotOutboundAdapter {
    /// Create a new WeCom Bot outbound adapter.
    pub fn new(storage: Arc<MessageStorage>) -> Self {
        Self::new_with_attachments(storage, None, true)
    }

    /// Create a new WeCom Bot outbound adapter with attachment config.
    pub fn new_with_attachments(
        storage: Arc<MessageStorage>,
        attachment_config: Option<OutboundAttachmentConfig>,
        footer_enabled: bool,
    ) -> Self {
        Self {
            handle: Arc::new(Mutex::new(None)),
            storage,
            attachment_config,
            footer_enabled,
            active_stream: Arc::new(Mutex::new(None)),
        }
    }

    /// Get the shared connection handle Arc so the monitor can set it after
    /// WebSocket creation.
    pub fn handle_arc(&self) -> Arc<Mutex<Option<WecomBotConnectionHandle>>> {
        self.handle.clone()
    }

    /// Set the WebSocket connection handle after the WebSocket connection is established.
    #[allow(dead_code)]
    pub async fn set_handle(&self, handle: WecomBotConnectionHandle) {
        let mut guard = self.handle.lock().await;
        *guard = Some(handle);
    }

    /// Send a JSON-formatted message through the WebSocket.
    async fn send_internal(&self, json_msg: &str) -> Result<()> {
        let guard = self.handle.lock().await;
        match guard.as_ref() {
            Some(handle) => handle
                .sender
                .send(json_msg.to_string())
                .map_err(|e| anyhow::anyhow!("Failed to send WeCom Bot outbound message: {}", e)),
            None => Err(anyhow::anyhow!(
                "WeCom Bot outbound handle not set (WebSocket not initialized)"
            )),
        }
    }

    /// Send a text/markdown reply through the WebSocket.
    ///
    /// When `chatid` is `Some`, uses `aibot_send_msg` (proactive mode).
    /// When `chatid` is `None`, uses `aibot_respond_msg` (passive reply mode).
    /// NOTE: aibot_respond_msg only supports msgtype="stream" (and "template_card").
    /// Text/markdown must be sent as a stream with finish=true.
    async fn send_text_reply(
        &self,
        req_id: &str,
        content: &str,
        stream_id: &str,
        finish: bool,
        chatid: Option<&str>,
    ) -> Result<()> {
        let cmd = if chatid.is_some() {
            "aibot_send_msg"
        } else {
            "aibot_respond_msg"
        };

        let mut body = serde_json::json!({
            "msgtype": "stream",
            "stream": {
                "id": stream_id,
                "content": content,
                "finish": finish
            }
        });

        if let Some(chatid) = chatid {
            body["chatid"] = serde_json::Value::String(chatid.to_string());
        }

        let json = serde_json::json!({
            "cmd": cmd,
            "headers": {"req_id": req_id},
            "body": body,
        })
        .to_string();

        self.send_internal(&json).await
    }

    /// Update an existing processing indicator with new content.
    ///
    /// Unlike `send_reply`, this does NOT clear the `active_stream` state,
    /// allowing subsequent updates to reuse the same `stream_id`.
    pub async fn update_processing_indicator(
        &self,
        req_id: &str,
        stream_id: &str,
        content: &str,
    ) -> Result<()> {
        self.send_text_reply(req_id, content, stream_id, false, None)
            .await
            .context("Failed to update WeCom Bot processing indicator")
    }

    /// Upload and send media attachments through the WebSocket.
    ///
    /// For each attachment: upload via `upload_attachment`, build a media message
    /// body via `build_media_message_body`, and send via the WebSocket.
    /// `cmd` is the command to use (e.g. `"aibot_send_msg"` or `"aibot_respond_msg"`).
    /// When `chat_id` is `Some`, it is included in the message body.
    async fn send_media_attachments(
        &self,
        handle: &WecomBotConnectionHandle,
        attachments: &[OutboundAttachment],
        cmd: &str,
        req_id: &str,
        chat_id: Option<&str>,
    ) {
        for att in attachments {
            let media_type = wecom_media_type(&att.content_type, &att.filename);

            match upload_attachment(handle, &att.path, &att.filename, &att.content_type).await {
                Ok(media_id) => {
                    let mut body = build_media_message_body(media_type, &media_id);
                    if let Some(cid) = chat_id {
                        body["chatid"] = serde_json::Value::String(cid.to_string());
                    }
                    let json = serde_json::json!({
                        "cmd": cmd,
                        "headers": {"req_id": req_id},
                        "body": body,
                    })
                    .to_string();

                    if let Err(e) = handle.sender.send(json) {
                        tracing::warn!(
                            filename = %att.filename,
                            error = %e,
                            "Failed to send WeCom Bot attachment message"
                        );
                    } else {
                        tracing::info!(
                            filename = %att.filename,
                            media_type = %media_type,
                            "WeCom Bot attachment message sent"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        filename = %att.filename,
                        error = %e,
                        "Failed to upload WeCom Bot attachment"
                    );
                }
            }
        }
    }
}

#[async_trait]
impl OutboundAdapter for WecomBotOutboundAdapter {
    fn channel_type(&self) -> &str {
        "wecom_bot"
    }

    async fn connect(&self) -> Result<()> {
        let guard = self.handle.lock().await;
        if guard.is_some() {
            tracing::info!("WeCom Bot outbound adapter connected (handle available)");
        } else {
            tracing::warn!(
                "WeCom Bot outbound adapter: no handle set yet (WebSocket may not be connected)"
            );
        }
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        tracing::info!("WeCom Bot outbound adapter disconnected");
        Ok(())
    }

    fn clean_body(&self, raw_body: &str) -> String {
        raw_body.trim().to_string()
    }

    async fn send_reply(
        &self,
        original: &InboundMessage,
        reply_text: &str,
        thread_path: &Path,
        message_dir: &str,
        attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult> {
        // 1. Read model/mode from reply context file
        let reply_ctx = jyc_mcp::context::load_reply_context(thread_path).await.ok();
        let model = reply_ctx.as_ref().and_then(|c| c.model.as_deref());
        let mode = reply_ctx.as_ref().and_then(|c| c.mode.as_deref());

        // Read current input tokens from session state
        let (input_tokens, max_tokens) =
            jyc_core::session_state::read_input_tokens(thread_path).await;

        // 2. Build footer
        let footer =
            email_parser::build_footer(model, mode, input_tokens, max_tokens, self.footer_enabled);

        // 3. Clean reply text
        let clean_reply = email_parser::strip_trailing_separators(reply_text);

        // 4. Combine with footer
        let full_reply = if footer.is_empty() {
            clean_reply
        } else {
            format!("{}\n\n{}", clean_reply, footer)
        };

        // 5. Get req_id from original message metadata.
        //    Proactive messages (scheduled tasks) have no req_id — derive chatid
        //    from thread_path and use aibot_send_msg instead.
        let original_req_id = original
            .metadata
            .get("req_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let (req_id, proactive_chatid) = if original_req_id.is_empty() {
            // Derive chatid from thread_path: strip "bot-" prefix from last component
            let thread_name = thread_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let chatid = thread_name.strip_prefix("bot-").unwrap_or(thread_name);
            let new_req_id = generate_req_id("aibot_send_msg");
            tracing::warn!(
                thread_name = %thread_name,
                chatid = %chatid,
                "Original message missing req_id, using proactive send"
            );
            (new_req_id, Some(chatid.to_string()))
        } else {
            (original_req_id.to_string(), None)
        };

        // 6. Validate attachments if configuration is present
        if let Some(attachments) = attachments
            && let Some(ref config) = self.attachment_config
        {
            attachment_validator::validate_outbound_attachments(attachments, config)
                .await
                .context("Failed to validate outbound attachments")?;
            tracing::debug!("Outbound attachments validated successfully for WeCom Bot");
        }

        // 7. Check if there's an active stream for this req_id
        let active = self.active_stream.lock().await.take();
        let stream_id = if let Some(ref stream) = active
            && stream.req_id == req_id
        {
            tracing::debug!(
                req_id = %req_id,
                stream_id = %stream.stream_id,
                "Reusing active stream for final reply"
            );
            stream.stream_id.clone()
        } else {
            if active.is_some() {
                tracing::debug!(
                    "Active stream req_id mismatch (expected a different message), creating new stream"
                );
            }
            uuid::Uuid::new_v4().to_string()
        };

        // 8. Send reply via WebSocket with finish=true
        self.send_text_reply(
            &req_id,
            &full_reply,
            &stream_id,
            true,
            proactive_chatid.as_deref(),
        )
        .await
        .context("Failed to send WeCom Bot reply")?;

        let message_id = uuid::Uuid::new_v4().to_string();

        tracing::info!(
            text_len = full_reply.len(),
            req_id = %req_id,
            stream_id = %stream_id,
            message_id = %message_id,
            "WeCom Bot reply sent"
        );

        // 9. Upload and send attachments as separate media messages.
        if let Some(attachments) = attachments
            && !attachments.is_empty()
        {
            let handle = {
                let guard = self.handle.lock().await;
                guard
                    .clone()
                    .context("WeCom Bot outbound handle not set; cannot upload attachments")?
            };
            let att_cmd = if proactive_chatid.is_some() {
                "aibot_send_msg"
            } else {
                "aibot_respond_msg"
            };
            self.send_media_attachments(
                &handle,
                attachments,
                att_cmd,
                &req_id,
                proactive_chatid.as_deref(),
            )
            .await;
        }

        // 10. Store reply to chat log
        self.storage
            .store_reply(thread_path, &full_reply, message_dir)
            .await
            .context("Failed to store WeCom Bot reply")?;

        Ok(SendResult { message_id })
    }

    async fn send_message(
        &self,
        recipient: &str,
        _subject: &str,
        body: &str,
    ) -> Result<SendResult> {
        // Proactive message: use aibot_send_msg with nested format
        let use_markdown = body.contains("**")
            || body.contains("*")
            || body.contains("`")
            || body.contains("#")
            || body.contains("[")
            || body.contains("- ");

        let body_json = if use_markdown {
            serde_json::json!({
                "msgtype": "markdown",
                "chatid": recipient,
                "markdown": {"content": body}
            })
        } else {
            serde_json::json!({
                "msgtype": "text",
                "chatid": recipient,
                "text": {"content": body}
            })
        };

        let json = serde_json::json!({
            "cmd": "aibot_send_msg",
            "headers": {"req_id": format!("aibot_send_msg_{}_{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
                &uuid::Uuid::new_v4().to_string().replace('-', "")[..8]
            )},
            "body": body_json
        })
        .to_string();

        self.send_internal(&json)
            .await
            .context("Failed to send WeCom Bot proactive message")?;

        let message_id = uuid::Uuid::new_v4().to_string();

        tracing::info!(
            recipient = %recipient,
            text_len = body.len(),
            message_id = %message_id,
            "WeCom Bot proactive message sent"
        );

        Ok(SendResult { message_id })
    }

    /// Send a message with file attachments to a WeCom Bot conversation.
    ///
    /// Sends text first via `aibot_send_msg`, then uploads each attachment
    /// and sends as a separate media message. Reuses the same module-level
    /// functions (`upload_attachment`, `build_media_message_body`,
    /// `wecom_media_type`) as `send_reply`'s attachment handling.
    async fn send_message_with_attachments(
        &self,
        recipient: &str,
        subject: &str,
        body: &str,
        attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult> {
        // Validate attachments if configuration is present
        if let Some(attachments) = attachments
            && let Some(ref config) = self.attachment_config
        {
            attachment_validator::validate_outbound_attachments(attachments, config)
                .await
                .context("Failed to validate outbound attachments")?;
            tracing::debug!("Outbound attachments validated successfully for WeCom Bot");
        }

        // Send text message first (reuse send_message logic)
        let text_result = self.send_message(recipient, subject, body).await?;

        // Send attachments as separate media messages
        if let Some(attachments) = attachments
            && !attachments.is_empty()
        {
            let handle = {
                let guard = self.handle.lock().await;
                guard
                    .clone()
                    .context("WeCom Bot outbound handle not set; cannot upload attachments")?
            };
            let req_id = format!(
                "aibot_send_msg_{}_{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
                &uuid::Uuid::new_v4().to_string().replace('-', "")[..8]
            );
            self.send_media_attachments(
                &handle,
                attachments,
                "aibot_send_msg",
                &req_id,
                Some(recipient),
            )
            .await;
        }

        Ok(SendResult {
            message_id: text_result.message_id,
        })
    }

    /// Send a processing indicator (`finish=false`) so the user sees
    /// "正在思考中..." while AI is working.
    ///
    /// The returned `stream_id` is also stored internally so that a
    /// subsequent `send_reply` can reuse it and set `finish=true`.
    async fn send_processing_indicator(&self, original: &InboundMessage) -> Result<Option<String>> {
        let req_id = original
            .metadata
            .get("req_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if req_id.is_empty() {
            tracing::warn!("Cannot send processing indicator: original message missing req_id");
            return Ok(None);
        }

        let stream_id = uuid::Uuid::new_v4().to_string();
        let content = "正在思考中...";

        let json = serde_json::json!({
            "cmd": "aibot_respond_msg",
            "headers": {"req_id": req_id},
            "body": {
                "msgtype": "stream",
                "stream": {
                    "id": &stream_id,
                    "content": content,
                    "finish": false
                }
            }
        })
        .to_string();

        self.send_internal(&json)
            .await
            .context("Failed to send WeCom Bot processing indicator")?;

        // Store the active stream so send_reply can reuse the stream_id
        let mut guard = self.active_stream.lock().await;
        *guard = Some(ActiveStream {
            req_id: req_id.to_string(),
            stream_id: stream_id.clone(),
        });

        tracing::info!(
            req_id = %req_id,
            stream_id = %stream_id,
            "WeCom Bot processing indicator sent"
        );

        Ok(Some(stream_id))
    }

    /// Clear a previously sent processing indicator.
    ///
    /// Called when AI processing fails or produces no reply. Sends a
    /// `finish=true` message using the same stream_id so the indicator
    /// does not remain stuck in an intermediate state.
    async fn clear_processing_indicator(&self, _handle: Option<String>) -> Result<()> {
        let active = self.active_stream.lock().await.take();

        let Some(stream) = active else {
            tracing::debug!("No active stream to clear");
            return Ok(());
        };

        let content = "处理失败，请稍后重试";

        let json = serde_json::json!({
            "cmd": "aibot_respond_msg",
            "headers": {"req_id": &stream.req_id},
            "body": {
                "msgtype": "stream",
                "stream": {
                    "id": &stream.stream_id,
                    "content": content,
                    "finish": true
                }
            }
        })
        .to_string();

        self.send_internal(&json)
            .await
            .context("Failed to clear WeCom Bot processing indicator")?;

        tracing::info!(
            req_id = %stream.req_id,
            stream_id = %stream.stream_id,
            "WeCom Bot processing indicator cleared"
        );

        Ok(())
    }

    /// Update an existing processing indicator with new content.
    ///
    /// Sends `finish=false` with the same `stream_id` so the message
    /// is updated in-place rather than creating a new one.
    async fn update_processing_indicator(
        &self,
        original: &InboundMessage,
        handle: &str,
        content: &str,
    ) -> Result<()> {
        let req_id = original
            .metadata
            .get("req_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if req_id.is_empty() {
            tracing::warn!("Cannot update processing indicator: original message missing req_id");
            return Ok(());
        }

        self.send_text_reply(req_id, content, handle, false, None)
            .await
            .context("Failed to update WeCom Bot processing indicator")?;

        tracing::debug!(
            req_id = %req_id,
            stream_id = %handle,
            content = %content,
            "WeCom Bot processing indicator updated"
        );

        Ok(())
    }
}

// ─── Attachment Upload Helpers ────────────────────────────────────

/// Maximum chunk size before base64 encoding (512 KiB).
const UPLOAD_CHUNK_SIZE: usize = 512 * 1024;

/// Maximum number of chunks allowed by WeCom.
const MAX_UPLOAD_CHUNKS: usize = 100;

/// Timeout for a single upload command ack.
const UPLOAD_COMMAND_TIMEOUT_SECS: u64 = 60;

/// WeCom media type limits (in bytes).
const IMAGE_MAX_SIZE: usize = 10 * 1024 * 1024;
const VOICE_MAX_SIZE: usize = 2 * 1024 * 1024;
const VIDEO_MAX_SIZE: usize = 10 * 1024 * 1024;
const FILE_MAX_SIZE: usize = 20 * 1024 * 1024;

/// Map a filename/extension to WeCom media type.
///
/// WeCom supports:
/// - image: png, jpg/jpeg, gif (max 10MB)
/// - voice: amr (max 2MB)
/// - video: mp4 (max 10MB)
/// - file: everything else (max 20MB)
fn wecom_media_type(_content_type: &str, filename: &str) -> &'static str {
    let ext = std::path::Path::new(filename)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" => "image",
        "amr" => "voice",
        "mp4" => "video",
        _ => "file",
    }
}

/// Validate that a payload does not exceed WeCom media type limits.
fn validate_wecom_media_size(bytes: &[u8], media_type: &str) -> Result<()> {
    let max = match media_type {
        "image" => IMAGE_MAX_SIZE,
        "voice" => VOICE_MAX_SIZE,
        "video" => VIDEO_MAX_SIZE,
        _ => FILE_MAX_SIZE,
    };

    if bytes.len() > max {
        anyhow::bail!(
            "WeCom {media_type} attachment exceeds {max} bytes (got {} bytes)",
            bytes.len()
        );
    }

    Ok(())
}

/// Upload a file through the WeCom Bot WebSocket and return the `media_id`.
async fn upload_attachment(
    handle: &WecomBotConnectionHandle,
    path: &Path,
    filename: &str,
    content_type: &str,
) -> Result<String> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("Failed to read WeCom Bot attachment: {}", path.display()))?;

    let media_type = wecom_media_type(content_type, filename);
    validate_wecom_media_size(&bytes, media_type)
        .with_context(|| format!("WeCom Bot attachment validation failed: {filename}"))?;

    let total_chunks = bytes.chunks(UPLOAD_CHUNK_SIZE).count();
    if total_chunks > MAX_UPLOAD_CHUNKS {
        anyhow::bail!(
            "WeCom Bot attachment requires {total_chunks} chunks, max is {MAX_UPLOAD_CHUNKS}"
        );
    }

    let md5_digest = format!("{:x}", md5::compute(&bytes));
    let timeout = std::time::Duration::from_secs(UPLOAD_COMMAND_TIMEOUT_SECS);

    // 1. Initialize upload session.
    let init_req_id = generate_req_id("aibot_upload_media_init");
    let init_body = serde_json::to_value(UploadMediaInitBody {
        media_type: media_type.to_string(),
        filename: filename.to_string(),
        total_size: bytes.len(),
        total_chunks,
        md5: md5_digest,
    })
    .context("Failed to serialize WeCom Bot upload init body")?;

    let init_resp = handle
        .send_and_wait(
            CMD_AIBOT_UPLOAD_MEDIA_INIT,
            &init_req_id,
            init_body,
            timeout,
        )
        .await
        .context("Failed to initialize WeCom Bot media upload")?;

    let errcode = init_resp["errcode"].as_i64().unwrap_or(-1);
    if errcode != 0 {
        let errmsg = init_resp["errmsg"].as_str().unwrap_or("unknown");
        anyhow::bail!("WeCom Bot upload init failed: errcode={errcode}, errmsg={errmsg}");
    }

    let upload_id = init_resp["body"]["upload_id"]
        .as_str()
        .context("WeCom Bot upload init response missing upload_id")?;

    // 2. Upload chunks.
    for (index, chunk) in bytes.chunks(UPLOAD_CHUNK_SIZE).enumerate() {
        let chunk_req_id = generate_req_id("aibot_upload_media_chunk");
        let chunk_body = serde_json::to_value(UploadMediaChunkBody {
            upload_id: upload_id.to_string(),
            chunk_index: index,
            base64_data: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, chunk),
        })
        .context("Failed to serialize WeCom Bot upload chunk body")?;

        let chunk_resp = handle
            .send_and_wait(
                CMD_AIBOT_UPLOAD_MEDIA_CHUNK,
                &chunk_req_id,
                chunk_body,
                timeout,
            )
            .await
            .with_context(|| format!("Failed to upload WeCom Bot chunk {index}"))?;

        let errcode = chunk_resp["errcode"].as_i64().unwrap_or(-1);
        if errcode != 0 {
            let errmsg = chunk_resp["errmsg"].as_str().unwrap_or("unknown");
            anyhow::bail!(
                "WeCom Bot upload chunk {index} failed: errcode={errcode}, errmsg={errmsg}"
            );
        }
    }

    // 3. Finish upload and obtain media_id.
    let finish_req_id = generate_req_id("aibot_upload_media_finish");
    let finish_body = serde_json::to_value(UploadMediaFinishBody {
        upload_id: upload_id.to_string(),
    })
    .context("Failed to serialize WeCom Bot upload finish body")?;

    let finish_resp = handle
        .send_and_wait(
            CMD_AIBOT_UPLOAD_MEDIA_FINISH,
            &finish_req_id,
            finish_body,
            timeout,
        )
        .await
        .context("Failed to finish WeCom Bot media upload")?;

    let errcode = finish_resp["errcode"].as_i64().unwrap_or(-1);
    if errcode != 0 {
        let errmsg = finish_resp["errmsg"].as_str().unwrap_or("unknown");
        anyhow::bail!("WeCom Bot upload finish failed: errcode={errcode}, errmsg={errmsg}");
    }

    let media_id = finish_resp["body"]["media_id"]
        .as_str()
        .context("WeCom Bot upload finish response missing media_id")?;

    tracing::info!(
        filename = %filename,
        media_type = %media_type,
        size = bytes.len(),
        chunks = total_chunks,
        "WeCom Bot attachment uploaded"
    );

    Ok(media_id.to_string())
}

/// Build an `aibot_respond_msg` body for a media attachment.
fn build_media_message_body(media_type: &str, media_id: &str) -> serde_json::Value {
    match media_type {
        "image" => serde_json::json!({
            "msgtype": "image",
            "image": {"media_id": media_id}
        }),
        "voice" => serde_json::json!({
            "msgtype": "voice",
            "voice": {"media_id": media_id}
        }),
        "video" => serde_json::json!({
            "msgtype": "video",
            "video": {"media_id": media_id}
        }),
        _ => serde_json::json!({
            "msgtype": "file",
            "file": {"media_id": media_id}
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Write;
    use std::time::Duration;
    use tokio::sync::{Mutex, mpsc, oneshot};
    use tokio::time::timeout;

    #[test]
    fn test_clean_body() {
        let adapter = WecomBotOutboundAdapter::new(Arc::new(MessageStorage::new(
            std::path::Path::new("/tmp"),
        )));
        assert_eq!(adapter.clean_body("  hello  "), "hello");
        assert_eq!(adapter.clean_body("hello\n\n"), "hello");
    }

    #[test]
    fn test_markdown_detection() {
        assert!("**bold**".contains("**"));
        assert!("*italic*".contains("*"));
        assert!("`code`".contains("`"));
        assert!("# heading".contains("#"));
        assert!("[link](url)".contains("["));
        assert!("- list".contains("- "));
    }

    /// Documents the wire format for a processing indicator.
    #[test]
    fn test_processing_indicator_wire_format() {
        let json = serde_json::json!({
            "cmd": "aibot_respond_msg",
            "headers": {"req_id": "req_123"},
            "body": {
                "msgtype": "stream",
                "stream": {
                    "id": "stream_abc",
                    "content": "正在思考中...",
                    "finish": false
                }
            }
        });

        assert_eq!(json["cmd"], "aibot_respond_msg");
        assert_eq!(json["headers"]["req_id"], "req_123");
        assert_eq!(json["body"]["msgtype"], "stream");
        assert_eq!(json["body"]["stream"]["id"], "stream_abc");
        assert_eq!(json["body"]["stream"]["content"], "正在思考中...");
        assert_eq!(json["body"]["stream"]["finish"], false);
    }

    /// Documents the wire format for clearing a processing indicator.
    #[test]
    fn test_clear_indicator_wire_format() {
        let json = serde_json::json!({
            "cmd": "aibot_respond_msg",
            "headers": {"req_id": "req_123"},
            "body": {
                "msgtype": "stream",
                "stream": {
                    "id": "stream_abc",
                    "content": "处理失败，请稍后重试",
                    "finish": true
                }
            }
        });

        assert_eq!(json["body"]["stream"]["finish"], true);
        assert_eq!(json["body"]["stream"]["content"], "处理失败，请稍后重试");
    }

    /// When send_reply is called after send_processing_indicator, it must
    /// reuse the same stream_id and set finish=true.
    #[tokio::test]
    async fn test_send_reply_reuses_active_stream() {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let storage = Arc::new(MessageStorage::new(std::path::Path::new("/tmp")));
        let adapter = WecomBotOutboundAdapter::new_with_attachments(storage, None, false);

        // Set the handle so messages can be captured
        let handle = WecomBotConnectionHandle::new(tx, Arc::new(Mutex::new(HashMap::new())));
        adapter.set_handle(handle).await;

        // Build a message with req_id
        let message = InboundMessage {
            id: "test".to_string(),
            channel: "wecom_bot".to_string(),
            channel_uid: "msg_1".to_string(),
            sender: "user".to_string(),
            sender_address: "user".to_string(),
            recipients: vec![],
            topic: String::new(),
            content: jyc_types::MessageContent {
                text: Some("hello".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: {
                let mut m = HashMap::new();
                m.insert(
                    "req_id".to_string(),
                    serde_json::Value::String("req_123".to_string()),
                );
                m
            },
            matched_pattern: None,
        };

        // Send processing indicator
        let handle = adapter
            .send_processing_indicator(&message)
            .await
            .expect("indicator should send");
        assert!(handle.is_some(), "should return stream_id");

        // Capture the indicator frame
        let indicator_json = rx.recv().await.expect("indicator frame should be sent");
        let indicator: serde_json::Value = serde_json::from_str(&indicator_json).unwrap();
        let stream_id = indicator["body"]["stream"]["id"]
            .as_str()
            .expect("stream id should be present")
            .to_string();
        assert_eq!(indicator["body"]["stream"]["finish"], false);
        assert_eq!(indicator["body"]["stream"]["content"], "正在思考中...");

        // Now send reply — it should reuse the same stream_id
        let thread_path = std::path::PathBuf::from("/tmp/test_thread");
        tokio::fs::create_dir_all(&thread_path).await.ok();
        let result = adapter
            .send_reply(&message, "AI reply", &thread_path, "msg_001", None)
            .await
            .expect("reply should send");
        assert!(!result.message_id.is_empty());

        // Capture the reply frame
        let reply_json = rx.recv().await.expect("reply frame should be sent");
        let reply: serde_json::Value = serde_json::from_str(&reply_json).unwrap();
        assert_eq!(reply["body"]["stream"]["finish"], true);
        assert_eq!(
            reply["body"]["stream"]["id"], stream_id,
            "reply must reuse the same stream_id"
        );
        assert_eq!(reply["body"]["stream"]["content"], "AI reply");

        // Active stream should be cleared after send_reply
        let guard = adapter.active_stream.lock().await;
        assert!(
            guard.is_none(),
            "active stream should be cleared after reply"
        );
    }

    /// When send_reply is called without a prior processing indicator,
    /// it should create a new stream with finish=true.
    #[tokio::test]
    async fn test_send_reply_without_indicator() {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let storage = Arc::new(MessageStorage::new(std::path::Path::new("/tmp")));
        let adapter = WecomBotOutboundAdapter::new_with_attachments(storage, None, false);

        let handle = WecomBotConnectionHandle::new(tx, Arc::new(Mutex::new(HashMap::new())));
        adapter.set_handle(handle).await;

        let message = InboundMessage {
            id: "test".to_string(),
            channel: "wecom_bot".to_string(),
            channel_uid: "msg_1".to_string(),
            sender: "user".to_string(),
            sender_address: "user".to_string(),
            recipients: vec![],
            topic: String::new(),
            content: jyc_types::MessageContent {
                text: Some("hello".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: {
                let mut m = HashMap::new();
                m.insert(
                    "req_id".to_string(),
                    serde_json::Value::String("req_456".to_string()),
                );
                m
            },
            matched_pattern: None,
        };

        let thread_path = std::path::PathBuf::from("/tmp/test_thread2");
        tokio::fs::create_dir_all(&thread_path).await.ok();
        adapter
            .send_reply(&message, "Direct reply", &thread_path, "msg_002", None)
            .await
            .expect("reply should send");

        let reply_json = rx.recv().await.expect("reply frame should be sent");
        let reply: serde_json::Value = serde_json::from_str(&reply_json).unwrap();
        assert_eq!(reply["body"]["stream"]["finish"], true);
        assert!(
            !reply["body"]["stream"]["id"].as_str().unwrap().is_empty(),
            "should have a new stream_id"
        );
    }

    /// clear_processing_indicator should send finish=true with an error message
    /// and clear the active stream.
    #[tokio::test]
    async fn test_clear_processing_indicator() {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let storage = Arc::new(MessageStorage::new(std::path::Path::new("/tmp")));
        let adapter = WecomBotOutboundAdapter::new_with_attachments(storage, None, false);

        let handle = WecomBotConnectionHandle::new(tx, Arc::new(Mutex::new(HashMap::new())));
        adapter.set_handle(handle).await;

        let message = InboundMessage {
            id: "test".to_string(),
            channel: "wecom_bot".to_string(),
            channel_uid: "msg_1".to_string(),
            sender: "user".to_string(),
            sender_address: "user".to_string(),
            recipients: vec![],
            topic: String::new(),
            content: jyc_types::MessageContent {
                text: Some("hello".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: {
                let mut m = HashMap::new();
                m.insert(
                    "req_id".to_string(),
                    serde_json::Value::String("req_789".to_string()),
                );
                m
            },
            matched_pattern: None,
        };

        // Send indicator first
        adapter
            .send_processing_indicator(&message)
            .await
            .expect("indicator should send");

        // Consume the indicator frame
        let indicator_json = rx.recv().await.expect("indicator frame");
        let indicator: serde_json::Value = serde_json::from_str(&indicator_json).unwrap();
        let stream_id = indicator["body"]["stream"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        // Clear it
        adapter
            .clear_processing_indicator(None)
            .await
            .expect("clear should succeed");

        let clear_json = rx.recv().await.expect("clear frame");
        let clear: serde_json::Value = serde_json::from_str(&clear_json).unwrap();
        assert_eq!(clear["body"]["stream"]["finish"], true);
        assert_eq!(
            clear["body"]["stream"]["id"], stream_id,
            "clear must use the same stream_id"
        );
        assert_eq!(clear["body"]["stream"]["content"], "处理失败，请稍后重试");

        // Active stream should be cleared
        let guard = adapter.active_stream.lock().await;
        assert!(guard.is_none());
    }

    #[test]
    fn test_wecom_media_type() {
        assert_eq!(wecom_media_type("image/png", "photo.png"), "image");
        assert_eq!(wecom_media_type("image/jpeg", "photo.jpg"), "image");
        assert_eq!(wecom_media_type("image/jpeg", "photo.jpeg"), "image");
        assert_eq!(wecom_media_type("image/gif", "photo.gif"), "image");
        assert_eq!(wecom_media_type("audio/amr", "voice.amr"), "voice");
        assert_eq!(wecom_media_type("video/mp4", "clip.mp4"), "video");
        assert_eq!(wecom_media_type("application/pdf", "report.pdf"), "file");
        assert_eq!(
            wecom_media_type(
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "data.xlsx"
            ),
            "file"
        );
        assert_eq!(wecom_media_type("text/csv", "data.csv"), "file");
        assert_eq!(
            wecom_media_type("application/octet-stream", "data.bin"),
            "file"
        );
    }

    #[test]
    fn test_validate_wecom_media_size() {
        assert!(validate_wecom_media_size(&[0u8; 1], "file").is_ok());
        assert!(validate_wecom_media_size(&[0u8; FILE_MAX_SIZE], "file").is_ok());
        assert!(validate_wecom_media_size(&[0u8; FILE_MAX_SIZE + 1], "file").is_err());
        assert!(validate_wecom_media_size(&[0u8; IMAGE_MAX_SIZE + 1], "image").is_err());
        assert!(validate_wecom_media_size(&[0u8; VOICE_MAX_SIZE + 1], "voice").is_err());
        assert!(validate_wecom_media_size(&[0u8; VIDEO_MAX_SIZE + 1], "video").is_err());
    }

    /// Helper: run `upload_attachment` against a mock handle that injects the
    /// given ack responses in order.
    async fn run_upload_with_responses(
        path: std::path::PathBuf,
        filename: String,
        content_type: String,
        responses: Vec<serde_json::Value>,
    ) -> Result<String> {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let pending = Arc::new(Mutex::new(HashMap::<
            String,
            oneshot::Sender<serde_json::Value>,
        >::new()));
        let handle = WecomBotConnectionHandle::new(tx, pending.clone());

        let upload_task = tokio::spawn(async move {
            upload_attachment(&handle, &path, &filename, &content_type).await
        });

        for resp in responses {
            let cmd_json = timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("receive command")
                .expect("channel open");
            let cmd: serde_json::Value = serde_json::from_str(&cmd_json).unwrap();
            let req_id = cmd["headers"]["req_id"]
                .as_str()
                .expect("req_id present")
                .to_string();
            let mut guard = pending.lock().await;
            let sender = guard.remove(&req_id).expect("pending response registered");
            sender.send(resp).expect("receiver alive");
        }

        upload_task.await.expect("upload task completed")
    }

    #[tokio::test]
    async fn test_upload_attachment_success() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("report.pdf");
        let content = b"hello pdf";
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(content).unwrap();

        let md5_hex = format!("{:x}", md5::compute(content));

        let responses = [
            serde_json::json!({
                "headers": {"req_id": "ignored"},
                "errcode": 0,
                "errmsg": "ok",
                "body": {"upload_id": "upload_123"}
            }),
            serde_json::json!({
                "headers": {"req_id": "ignored"},
                "errcode": 0,
                "errmsg": "ok"
            }),
            serde_json::json!({
                "headers": {"req_id": "ignored"},
                "errcode": 0,
                "errmsg": "ok",
                "body": {
                    "type": "file",
                    "media_id": "media_abc",
                    "created_at": "1700000000"
                }
            }),
        ];

        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let pending = Arc::new(Mutex::new(HashMap::<
            String,
            oneshot::Sender<serde_json::Value>,
        >::new()));
        let handle = WecomBotConnectionHandle::new(tx, pending.clone());

        let upload_task = tokio::spawn(async move {
            upload_attachment(&handle, &path, "report.pdf", "application/pdf").await
        });

        // Capture and verify init command.
        let init_json = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("init recv")
            .expect("init channel open");
        let init: serde_json::Value = serde_json::from_str(&init_json).unwrap();
        assert_eq!(init["cmd"], "aibot_upload_media_init");
        assert_eq!(init["body"]["type"], "file");
        assert_eq!(init["body"]["filename"], "report.pdf");
        assert_eq!(init["body"]["total_size"], content.len());
        assert_eq!(init["body"]["total_chunks"], 1);
        assert_eq!(init["body"]["md5"], md5_hex);

        let req_id = init["headers"]["req_id"].as_str().unwrap().to_string();
        {
            let mut guard = pending.lock().await;
            let sender = guard.remove(&req_id).unwrap();
            sender.send(responses[0].clone()).unwrap();
        }

        // Capture and verify chunk command.
        let chunk_json = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("chunk recv")
            .expect("chunk channel open");
        let chunk: serde_json::Value = serde_json::from_str(&chunk_json).unwrap();
        assert_eq!(chunk["cmd"], "aibot_upload_media_chunk");
        assert_eq!(chunk["body"]["upload_id"], "upload_123");
        assert_eq!(chunk["body"]["chunk_index"], 0);
        assert_eq!(
            chunk["body"]["base64_data"],
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, content)
        );

        let req_id = chunk["headers"]["req_id"].as_str().unwrap().to_string();
        {
            let mut guard = pending.lock().await;
            let sender = guard.remove(&req_id).unwrap();
            sender.send(responses[1].clone()).unwrap();
        }

        // Capture and verify finish command.
        let finish_json = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("finish recv")
            .expect("finish channel open");
        let finish: serde_json::Value = serde_json::from_str(&finish_json).unwrap();
        assert_eq!(finish["cmd"], "aibot_upload_media_finish");
        assert_eq!(finish["body"]["upload_id"], "upload_123");

        let req_id = finish["headers"]["req_id"].as_str().unwrap().to_string();
        {
            let mut guard = pending.lock().await;
            let sender = guard.remove(&req_id).unwrap();
            sender.send(responses[2].clone()).unwrap();
        }

        let media_id = upload_task.await.unwrap().expect("upload succeeded");
        assert_eq!(media_id, "media_abc");
    }

    #[tokio::test]
    async fn test_upload_attachment_init_failure() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("report.pdf");
        std::fs::write(&path, b"x").unwrap();

        let responses = vec![serde_json::json!({
            "headers": {"req_id": "ignored"},
            "errcode": 40001,
            "errmsg": "invalid credential"
        })];

        let result = run_upload_with_responses(
            path,
            "report.pdf".to_string(),
            "application/pdf".to_string(),
            responses,
        )
        .await;
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("init failed"), "error: {msg}");
    }

    #[tokio::test]
    async fn test_send_reply_with_attachment() {
        let dir = tempfile::TempDir::new().unwrap();
        let attachment_path = dir.path().join("report.pdf");
        std::fs::write(&attachment_path, b"pdf content").unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let pending = Arc::new(Mutex::new(HashMap::<
            String,
            oneshot::Sender<serde_json::Value>,
        >::new()));
        let handle = WecomBotConnectionHandle::new(tx, pending.clone());

        let storage = Arc::new(MessageStorage::new(dir.path()));
        let adapter = WecomBotOutboundAdapter::new_with_attachments(storage, None, false);
        adapter.set_handle(handle).await;

        let message = InboundMessage {
            id: "test".to_string(),
            channel: "wecom_bot".to_string(),
            channel_uid: "msg_1".to_string(),
            sender: "user".to_string(),
            sender_address: "user".to_string(),
            recipients: vec![],
            topic: String::new(),
            content: jyc_types::MessageContent {
                text: Some("hello".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: {
                let mut m = HashMap::new();
                m.insert(
                    "req_id".to_string(),
                    serde_json::Value::String("req_123".to_string()),
                );
                m
            },
            matched_pattern: None,
        };

        let attachments = vec![OutboundAttachment {
            filename: "report.pdf".to_string(),
            path: attachment_path,
            content_type: "application/pdf".to_string(),
        }];

        let reply_task = tokio::spawn(async move {
            let thread_path = dir.path().join("thread");
            tokio::fs::create_dir_all(&thread_path).await.unwrap();
            adapter
                .send_reply(
                    &message,
                    "AI reply",
                    &thread_path,
                    "msg_001",
                    Some(&attachments),
                )
                .await
        });

        // 1. Text reply frame.
        let text_json = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("text recv")
            .expect("text channel open");
        let text: serde_json::Value = serde_json::from_str(&text_json).unwrap();
        assert_eq!(text["cmd"], "aibot_respond_msg");
        assert_eq!(text["headers"]["req_id"], "req_123");
        assert_eq!(text["body"]["msgtype"], "stream");
        assert_eq!(text["body"]["stream"]["finish"], true);
        assert_eq!(text["body"]["stream"]["content"], "AI reply");

        // 2. Upload init frame.
        let init_json = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("init recv")
            .expect("init channel open");
        let init: serde_json::Value = serde_json::from_str(&init_json).unwrap();
        assert_eq!(init["cmd"], "aibot_upload_media_init");
        let init_req_id = init["headers"]["req_id"].as_str().unwrap().to_string();
        {
            let mut guard = pending.lock().await;
            let sender = guard.remove(&init_req_id).unwrap();
            sender
                .send(serde_json::json!({
                    "headers": {"req_id": init_req_id},
                    "errcode": 0,
                    "errmsg": "ok",
                    "body": {"upload_id": "upload_123"}
                }))
                .unwrap();
        }

        // 3. Upload chunk frame.
        let chunk_json = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("chunk recv")
            .expect("chunk channel open");
        let chunk: serde_json::Value = serde_json::from_str(&chunk_json).unwrap();
        assert_eq!(chunk["cmd"], "aibot_upload_media_chunk");
        let chunk_req_id = chunk["headers"]["req_id"].as_str().unwrap().to_string();
        {
            let mut guard = pending.lock().await;
            let sender = guard.remove(&chunk_req_id).unwrap();
            sender
                .send(serde_json::json!({
                    "headers": {"req_id": chunk_req_id},
                    "errcode": 0,
                    "errmsg": "ok"
                }))
                .unwrap();
        }

        // 4. Upload finish frame.
        let finish_json = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("finish recv")
            .expect("finish channel open");
        let finish: serde_json::Value = serde_json::from_str(&finish_json).unwrap();
        assert_eq!(finish["cmd"], "aibot_upload_media_finish");
        let finish_req_id = finish["headers"]["req_id"].as_str().unwrap().to_string();
        {
            let mut guard = pending.lock().await;
            let sender = guard.remove(&finish_req_id).unwrap();
            sender
                .send(serde_json::json!({
                    "headers": {"req_id": finish_req_id},
                    "errcode": 0,
                    "errmsg": "ok",
                    "body": {"type": "file", "media_id": "media_abc", "created_at": "1700000000"}
                }))
                .unwrap();
        }

        // 5. Attachment message frame.
        let att_json = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("attachment recv")
            .expect("attachment channel open");
        let att: serde_json::Value = serde_json::from_str(&att_json).unwrap();
        assert_eq!(att["cmd"], "aibot_respond_msg");
        assert_eq!(att["headers"]["req_id"], "req_123");
        assert_eq!(att["body"]["msgtype"], "file");
        assert_eq!(att["body"]["file"]["media_id"], "media_abc");

        reply_task.await.unwrap().expect("send_reply succeeded");
    }
}
