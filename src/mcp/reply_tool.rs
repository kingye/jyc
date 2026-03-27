use anyhow::Result;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_router, tool_handler,
};
use std::path::{Path, PathBuf};

use super::context::deserialize_context;
use crate::channels::email::outbound::EmailOutboundAdapter;
use crate::channels::types::OutboundAttachment;
use crate::config;
use crate::core::email_parser;
use crate::core::message_storage::MessageStorage;

const EXCLUDED_DIRS: &[&str] = &[".opencode", ".jyc"];

/// File-based logger for the MCP tool (stdout is used for MCP protocol).
struct McpLogger {
    path: PathBuf,
}

impl McpLogger {
    fn new(cwd: &Path) -> Self {
        let jyc_dir = cwd.join(".jyc");
        std::fs::create_dir_all(&jyc_dir).ok();
        Self {
            path: jyc_dir.join("reply-tool.log"),
        }
    }

    fn log(&self, level: &str, msg: &str) {
        let line = format!(
            "[{}] [{}] {}\n",
            chrono::Utc::now().to_rfc3339(),
            level,
            msg
        );
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// Parameters for the reply_message tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReplyMessageParams {
    #[schemars(description = "The reply text to send")]
    pub message: String,
    #[schemars(description = "The opaque token value from the REPLY_TOKEN= line. Pass it exactly as-is.")]
    pub token: String,
    #[schemars(description = "Optional list of filenames within the thread directory to attach")]
    pub attachments: Option<Vec<String>>,
}

/// The MCP reply tool handler.
#[derive(Debug, Clone)]
pub struct ReplyToolHandler {
    tool_router: ToolRouter<Self>,
}

impl ReplyToolHandler {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl ReplyToolHandler {
    #[tool(description = "Send a reply message back through the originating channel. Handles quoting, threading, and reply storage.")]
    async fn reply_message(
        &self,
        Parameters(params): Parameters<ReplyMessageParams>,
    ) -> Result<CallToolResult, rmcp::Error> {
        let cwd = std::env::current_dir().unwrap_or_default();
        let logger = McpLogger::new(&cwd);

        logger.log("INFO", &format!(
            "reply_message called: message_len={}, has_token={}, attachments={:?}, cwd={}",
            params.message.len(),
            !params.token.is_empty(),
            params.attachments,
            cwd.display()
        ));

        match handle_reply(
            &logger,
            &cwd,
            &params.message,
            &params.token,
            params.attachments.as_deref(),
        )
        .await
        {
            Ok(text) => {
                logger.log("INFO", &format!("reply_message completed: {text}"));
                Ok(CallToolResult::success(vec![Content::text(text)]))
            }
            Err(e) => {
                let err_msg = format!("Error: {e}");
                logger.log("ERROR", &format!("reply_message FAILED: {e}"));
                Ok(CallToolResult::error(vec![Content::text(err_msg)]))
            }
        }
    }
}

#[tool_handler]
impl ServerHandler for ReplyToolHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "jiny_reply",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions("MCP reply tool for JYC — sends replies through the originating channel")
    }
}

/// Core reply logic.
async fn handle_reply(
    logger: &McpLogger,
    cwd: &Path,
    message: &str,
    token: &str,
    attachments: Option<&[String]>,
) -> Result<String> {
    // 1. Decode and validate context
    let ctx = deserialize_context(token)?;
    logger.log("INFO", &format!(
        "Context: channel={}, thread={}, messageDir={}",
        ctx.channel, ctx.thread_name, ctx.incoming_message_dir
    ));

    // 2. Validate message
    if message.trim().is_empty() {
        anyhow::bail!("Message cannot be empty");
    }

    // 3. Load config from JYC_ROOT
    let root_dir = std::env::var("JYC_ROOT")
        .map_err(|_| anyhow::anyhow!("JYC_ROOT environment variable is not set"))?;
    let config_path = Path::new(&root_dir).join("config.toml");
    let app_config = config::load_config(&config_path)?;
    logger.log("INFO", &format!("Config loaded: {}", config_path.display()));

    // 4. Thread path = cwd
    let thread_path = cwd;

    // 5. Validate attachments
    let validated_attachments = if let Some(filenames) = attachments {
        validate_attachments(thread_path, filenames, logger)?
    } else {
        vec![]
    };

    // 6. Load message metadata from stored received.md (authoritative source — NOT from token)
    let received_path = thread_path
        .join("messages")
        .join(&ctx.incoming_message_dir)
        .join("received.md");
    let received_content = tokio::fs::read_to_string(&received_path)
        .await
        .map_err(|e| anyhow::anyhow!("failed to read {}: {}", received_path.display(), e))?;
    let parsed = email_parser::parse_stored_message(&received_content);

    let sender = parsed.sender.unwrap_or_else(|| "Unknown".to_string());
    let sender_address = parsed.sender_address
        .ok_or_else(|| anyhow::anyhow!("sender_address not found in received.md frontmatter"))?;
    let topic = parsed.topic.unwrap_or_default();
    let timestamp = parsed.timestamp.unwrap_or_default();
    let external_id = parsed.external_id;
    let thread_refs = parsed.thread_refs;

    logger.log("INFO", &format!(
        "Loaded from received.md: sender={}, recipient={}, topic={}, body={} chars",
        sender, sender_address, topic, parsed.body.len()
    ));

    // 7. Build full reply text — uses the shared build_full_reply_text()
    let full_reply = email_parser::build_full_reply_text(
        message,
        thread_path,
        &sender,
        &timestamp,
        &topic,
        &parsed.body,
        &ctx.incoming_message_dir,
    )
    .await;

    // 8. Create outbound adapter and send
    let channel_config = app_config
        .channels
        .get(&ctx.channel)
        .ok_or_else(|| anyhow::anyhow!("channel '{}' not found in config", ctx.channel))?;

    let smtp_config = channel_config
        .outbound
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no outbound config for channel '{}'", ctx.channel))?;

    let outbound = EmailOutboundAdapter::new(smtp_config);
    outbound.connect().await?;

    // Reconstruct InboundMessage from stored metadata (not from token)
    let original = crate::channels::types::InboundMessage {
        id: uuid::Uuid::new_v4().to_string(),
        channel: ctx.channel.clone(),
        channel_uid: ctx.uid.clone(),
        sender: sender.clone(),
        sender_address: sender_address.clone(),
        recipients: vec![],
        topic: topic.clone(),
        content: crate::channels::types::MessageContent::default(),
        timestamp: chrono::Utc::now(),
        thread_refs: thread_refs,
        reply_to_id: None,
        external_id: external_id,
        attachments: vec![],
        metadata: std::collections::HashMap::new(),
        matched_pattern: None,
    };

    logger.log("INFO", &format!(
        "Sending reply: channel={}, recipient={}, len={}, attachments={}",
        ctx.channel, sender_address, full_reply.len(), validated_attachments.len()
    ));

    // Build OutboundAttachment list from validated filenames
    let outbound_attachments: Vec<OutboundAttachment> = validated_attachments
        .iter()
        .map(|filename| {
            let path = thread_path.join(filename);
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let content_type = match ext.as_str() {
                "pdf" => "application/pdf",
                "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                "ppt" => "application/vnd.ms-powerpoint",
                "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "doc" => "application/msword",
                "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "xls" => "application/vnd.ms-excel",
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "txt" | "md" => "text/plain",
                "zip" => "application/zip",
                _ => "application/octet-stream",
            };
            OutboundAttachment {
                filename: filename.clone(),
                path,
                content_type: content_type.to_string(),
            }
        })
        .collect();

    let send_result = outbound
        .send_reply(
            &original,
            &full_reply,
            if outbound_attachments.is_empty() {
                None
            } else {
                Some(&outbound_attachments)
            },
        )
        .await?;
    outbound.disconnect().await?;

    logger.log("INFO", &format!("Reply sent: message_id={}", send_result.message_id));

    // 9. Store reply
    let storage = MessageStorage::new(
        thread_path.parent().unwrap_or(thread_path),
    );
    storage.store_reply(thread_path, &full_reply, &ctx.incoming_message_dir).await.ok();
    logger.log("INFO", "Reply stored");

    // 10. Write signal file
    let jyc_dir = thread_path.join(".jyc");
    tokio::fs::create_dir_all(&jyc_dir).await.ok();
    let signal = serde_json::json!({
        "sent_at": chrono::Utc::now().to_rfc3339(),
        "channel": ctx.channel,
        "recipient": sender_address,
        "message_id": send_result.message_id,
        "attachment_count": validated_attachments.len(),
    });
    tokio::fs::write(
        jyc_dir.join("reply-sent.flag"),
        serde_json::to_string(&signal).unwrap_or_default(),
    )
    .await
    .ok();
    logger.log("INFO", "Signal file written");

    // 11. Return success
    let mut result = format!(
        "Reply sent successfully via {} to {}",
        ctx.channel, sender_address
    );
    if !validated_attachments.is_empty() {
        result.push_str(&format!(
            " with {} attachment(s): {}",
            validated_attachments.len(),
            validated_attachments.join(", ")
        ));
    }

    Ok(result)
}

/// Validate attachment filenames.
fn validate_attachments(
    thread_path: &Path,
    filenames: &[String],
    logger: &McpLogger,
) -> Result<Vec<String>> {
    let mut valid = Vec::new();

    for filename in filenames {
        let path = thread_path.join(filename);
        let canonical = path
            .canonicalize()
            .map_err(|_| anyhow::anyhow!("attachment not found: {filename}"))?;

        let thread_canonical = thread_path
            .canonicalize()
            .unwrap_or_else(|_| thread_path.to_path_buf());

        if !canonical.starts_with(&thread_canonical) {
            anyhow::bail!("attachment path escapes thread directory: {filename}");
        }

        let relative = canonical
            .strip_prefix(&thread_canonical)
            .unwrap_or(canonical.as_path());
        for component in relative.components() {
            let part = component.as_os_str().to_string_lossy();
            if EXCLUDED_DIRS.contains(&part.as_ref()) {
                anyhow::bail!(
                    "cannot attach files from excluded directories ({:?}): {filename}",
                    EXCLUDED_DIRS
                );
            }
        }

        if canonical.is_dir() {
            anyhow::bail!("cannot attach a directory: {filename}");
        }

        logger.log("INFO", &format!("Attachment validated: {filename}"));
        valid.push(filename.clone());
    }

    Ok(valid)
}

/// Start the MCP reply tool server on stdio.
pub async fn run_server() -> Result<()> {
    let handler = ReplyToolHandler::new();
    let service = handler.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
