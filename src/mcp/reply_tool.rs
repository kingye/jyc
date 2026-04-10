use anyhow::Result;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_router, tool_handler,
};
use std::path::{Path, PathBuf};

use super::context::load_reply_context;

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
    #[tool(description = "Send a reply message back through the originating channel. The reply will be delivered by the monitor process.")]
    async fn reply_message(
        &self,
        Parameters(params): Parameters<ReplyMessageParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let cwd = std::env::current_dir().unwrap_or_default();
        let logger = McpLogger::new(&cwd);

        logger.log("INFO", &format!(
            "reply_message called: message_len={}, attachments={:?}, cwd={}",
            params.message.len(),
            params.attachments,
            cwd.display()
        ));

        match handle_reply(
            &logger,
            &cwd,
            &params.message,
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
                "jyc_reply",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions("MCP reply tool for JYC — stores replies for delivery by the monitor process")
    }
}

/// Core reply logic.
///
/// This tool no longer sends messages directly. It only:
/// 1. Validates the reply text and attachments
/// 2. Writes the reply-sent.flag signal file with reply metadata
///
/// The actual message delivery and chat log storage is handled by the
/// monitor process's outbound adapter, which has pre-warmed connections and
/// cached tokens — eliminating cold-start timeouts.
async fn handle_reply(
    logger: &McpLogger,
    cwd: &Path,
    message: &str,
    attachments: Option<&[String]>,
) -> Result<String> {
    // 1. Load reply context from disk (.jyc/reply-context.json)
    let ctx = load_reply_context(cwd).await?;

    logger.log("INFO", &format!(
        "Context loaded: channel={}, thread={}, messageDir={}, model={:?}, mode={:?}",
        ctx.channel, ctx.thread_name, ctx.incoming_message_dir, ctx.model, ctx.mode
    ));

    // 2. Validate message
    if message.trim().is_empty() {
        anyhow::bail!("Message cannot be empty");
    }

    // 3. Thread path = cwd
    let thread_path = cwd;

    // 4. Validate attachments
    let validated_attachments = if let Some(filenames) = attachments {
        validate_attachments(thread_path, filenames, logger)?
    } else {
        vec![]
    };

    // 5. Write signal file with reply metadata
    //    The monitor process reads this to know a reply is ready for delivery.
    //    Note: The reply is NOT stored to the chat log here — that is done by
    //    the outbound adapter after building the full reply with quoted history.
    //    Storing here would cause the reply to appear in the quoted history
    //    and be double-stored.
    let jyc_dir = thread_path.join(".jyc");
    tokio::fs::create_dir_all(&jyc_dir).await.ok();
    let signal = serde_json::json!({
        "sent_at": chrono::Utc::now().to_rfc3339(),
        "channel": ctx.channel,
        "message_dir": ctx.incoming_message_dir,
        "message_len": message.len(),
        "attachment_count": validated_attachments.len(),
        "attachments": validated_attachments,
    });
    tokio::fs::write(
        jyc_dir.join("reply-sent.flag"),
        serde_json::to_string(&signal).unwrap_or_default(),
    )
    .await
    .ok();
    logger.log("INFO", "Signal file written");

    // 6. Return success
    let mut result = format!(
        "Reply stored for delivery via {} ({} chars)",
        ctx.channel,
        message.len(),
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
