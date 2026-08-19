//! WeCom Smart Robot (wecom_bot) outbound adapter implementation.
//!
//! Handles sending replies via WebSocket using `aibot_respond_msg` and
//! proactive messages using `aibot_send_msg`.
//!
//! Supports streaming replies via `msgtype: "stream"`.
//!
//! Reference: doc 101031 - Passive Reply Messages

use std::path::Path;

use anyhow::{Context, Result, anyhow};

use super::client::{WecomBotConnectionHandle, generate_req_id};
use super::types::{
    CMD_AIBOT_RESPOND_MSG, CMD_AIBOT_UPLOAD_MEDIA_CHUNK, CMD_AIBOT_UPLOAD_MEDIA_FINISH,
    CMD_AIBOT_UPLOAD_MEDIA_INIT, UploadMediaChunkBody, UploadMediaFinishBody, UploadMediaInitBody,
};

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

// ─── Public helpers exposed for the pipe adapter ───────────────────
//
// The pipe-only `spawn_wecom_bot_adapter` (jyc-cli) reuses these
// directly instead of going through `WecomBotOutboundAdapter` (which
// carries the full reply-lifecycle surface: footer, reply context,
// session tokens, chat-log storage). Keeping them as free functions
// leaves one canonical wire-format definition instead of duplicating
// the JSON shape in two places.

/// Send a streaming reply chunk (`aibot_respond_msg` + `msgtype: stream`).
///
/// `finish=false` opens a streaming window for the user-visible
/// "thinking" indicator; `finish=true` completes the stream with the
/// final reply content. Both share the same `stream_id` so the client's
/// message is updated in-place rather than posted twice.
///
/// This is the same wire format that `WecomBotOutboundAdapter::send_reply`
/// uses; it is exposed here so the pipe reply forwarder does not have
/// to construct the JSON inline.
pub async fn send_stream_reply(
    handle: &WecomBotConnectionHandle,
    req_id: &str,
    stream_id: &str,
    content: &str,
    finish: bool,
) -> Result<()> {
    let json = serde_json::json!({
        "cmd": "aibot_respond_msg",
        "headers": {"req_id": req_id},
        "body": {
            "msgtype": "stream",
            "stream": {
                "id": stream_id,
                "content": content,
                "finish": finish,
            }
        }
    })
    .to_string();
    handle
        .sender
        .send(json)
        .map_err(|e| anyhow::anyhow!("Failed to send WeCom Bot stream reply: {e}"))
}

/// Send a streaming reply chunk and wait for the server ack.
///
/// Returns `Ok(())` on `errcode == 0`; `Err` on any other ack (`errcode`
/// is surfaced, including `846604` for an expired passive-reply window)
/// or transport failure (timeout, channel closed). The pipe reply
/// forwarder uses this for the `finish=true` final reply and falls back
/// to proactive `aibot_send_msg` on error, so the user still receives
/// the text when the streaming window has closed (common for long
/// agent runs that exceed the WeCom passive-reply window).
pub async fn send_stream_reply_and_wait(
    handle: &WecomBotConnectionHandle,
    req_id: &str,
    stream_id: &str,
    content: &str,
    finish: bool,
) -> Result<()> {
    let body = serde_json::json!({
        "msgtype": "stream",
        "stream": {
            "id": stream_id,
            "content": content,
            "finish": finish,
        }
    });
    match handle
        .send_and_wait(
            CMD_AIBOT_RESPOND_MSG,
            req_id,
            body,
            std::time::Duration::from_secs(10),
        )
        .await
    {
        Ok(ack) => {
            let errcode = ack["errcode"].as_i64().unwrap_or(-1);
            if errcode == 0 {
                Ok(())
            } else {
                let errmsg = ack["errmsg"].as_str().unwrap_or("unknown");
                Err(anyhow!(
                    "wecom_bot stream reply rejected: errcode={errcode}, errmsg={errmsg}"
                ))
            }
        }
        Err(e) => {
            // Timeout or transport error. The frame was (probably) sent
            // before the failure surfaced; we can't tell whether the
            // server actually delivered it. Treat as best-effort
            // streamed (return Ok) so the caller does not fall back to a
            // proactive send — a duplicate would be worse than a rare
            // missing reply, and if the connection is dead the fallback
            // would fail anyway.
            tracing::warn!(
                error = format!("{e:#}"),
                "wecom_bot stream reply ack not received; assuming best-effort delivery"
            );
            Ok(())
        }
    }
}

/// Build the body JSON for a proactive `aibot_send_msg` text/markdown
/// message. Used by `WecomBotOutboundAdapter::send_message` and the pipe
/// reply forwarder's fallback path so the wire format is defined in one
/// place.
///
/// Markdown detection is a simple heuristic (presence of common markdown
/// sigils). The caller is responsible for the surrounding `cmd` and
/// `headers` frame.
pub fn build_proactive_text_body(recipient: &str, text: &str) -> serde_json::Value {
    let use_markdown = text.contains("**")
        || text.contains("*")
        || text.contains("`")
        || text.contains("#")
        || text.contains("[")
        || text.contains("- ");
    if use_markdown {
        serde_json::json!({
            "msgtype": "markdown",
            "chatid": recipient,
            "markdown": {"content": text},
        })
    } else {
        serde_json::json!({
            "msgtype": "text",
            "chatid": recipient,
            "text": {"content": text},
        })
    }
}

/// Map a filename/extension to WeCom media type.
///
/// WeCom supports:
/// - image: png, jpg/jpeg, gif (max 10MB)
/// - voice: amr (max 2MB)
/// - video: mp4 (max 10MB)
/// - file: everything else (max 20MB)
pub fn wecom_media_type(_content_type: &str, filename: &str) -> &'static str {
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
pub fn validate_wecom_media_size(bytes: &[u8], media_type: &str) -> Result<()> {
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
///
/// Used by the pipe reply forwarder to relay outbound attachments.
pub async fn upload_attachment(
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
pub fn build_media_message_body(media_type: &str, media_id: &str) -> serde_json::Value {
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
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{Mutex, mpsc, oneshot};
    use tokio::time::timeout;

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
}
