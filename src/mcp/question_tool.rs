//! MCP Question Tool — sends questions to users via the messaging channel and waits for answers.
//!
//! Channel-agnostic: uses the same signal file mechanism as the reply tool.
//! The monitor process handles channel-specific delivery and answer routing.
//!
//! Flow:
//! 1. AI calls ask_user with a question
//! 2. Tool writes question to .jyc/question-sent.flag
//! 3. Monitor delivers question to user via outbound adapter
//! 4. User responds via their messaging channel
//! 5. Monitor detects waiting state, writes response to .jyc/question-answer.json
//! 6. Tool polls for answer file, reads it, returns to AI

use anyhow::{Context, Result};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_router, tool_handler,
};
use std::path::{Path, PathBuf};
use std::time::Duration;

const ANSWER_POLL_INTERVAL: Duration = Duration::from_secs(2);
const ANSWER_TIMEOUT: Duration = Duration::from_secs(5 * 60); // 5 minutes

/// File-based logger for the MCP tool (stdout is used for MCP protocol).
struct McpLogger {
    path: PathBuf,
}

impl McpLogger {
    fn new(cwd: &Path) -> Self {
        let jyc_dir = cwd.join(".jyc");
        std::fs::create_dir_all(&jyc_dir).ok();
        Self {
            path: jyc_dir.join("question-tool.log"),
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

/// Parameters for the ask_user tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct AskUserParams {
    #[schemars(description = "The question to ask the user. Be clear and specific.")]
    pub question: String,
}

/// The MCP question tool handler.
#[derive(Debug, Clone)]
pub struct QuestionToolHandler {
    tool_router: ToolRouter<Self>,
}

impl QuestionToolHandler {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl QuestionToolHandler {
    #[tool(description = "Ask the user a question and wait for their response. The question is delivered to the user automatically and the tool blocks until the user replies (up to 5 minutes). Use this when you need clarification or a decision from the user before proceeding.")]
    async fn ask_user(
        &self,
        Parameters(params): Parameters<AskUserParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let cwd = std::env::current_dir().unwrap_or_default();
        let logger = McpLogger::new(&cwd);

        logger.log("INFO", &format!(
            "ask_user called: question_len={}, cwd={}",
            params.question.len(),
            cwd.display()
        ));

        match handle_ask_user(&logger, &cwd, &params.question).await {
            Ok(answer) => {
                logger.log("INFO", &format!("ask_user completed: answer_len={}", answer.len()));
                Ok(CallToolResult::success(vec![Content::text(answer)]))
            }
            Err(e) => {
                let err_msg = format!("Error: {e}");
                logger.log("ERROR", &format!("ask_user FAILED: {e}"));
                Ok(CallToolResult::error(vec![Content::text(err_msg)]))
            }
        }
    }
}

#[tool_handler]
impl ServerHandler for QuestionToolHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "jyc_question",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions("MCP question tool for JYC — sends questions to users via messaging channels and waits for their response")
    }
}

/// Core question logic.
///
/// 1. Deliver the question to the user via the reply signal file mechanism
/// 2. Write question flag (.jyc/question-sent.flag) for answer routing
/// 3. Poll for answer file (.jyc/question-answer.json)
/// 4. Return the answer text
async fn handle_ask_user(
    logger: &McpLogger,
    cwd: &Path,
    question: &str,
) -> Result<String> {
    // Validate question
    if question.trim().is_empty() {
        anyhow::bail!("Question cannot be empty");
    }

    let jyc_dir = cwd.join(".jyc");
    tokio::fs::create_dir_all(&jyc_dir).await.ok();

    let question_flag = jyc_dir.join("question-sent.flag");
    let answer_file = jyc_dir.join("question-answer.json");

    // Clean up any stale files from previous questions
    if answer_file.exists() {
        tokio::fs::remove_file(&answer_file).await.ok();
    }

    // Step 1: Deliver the question to the user via the reply signal file.
    // Uses the same mechanism as the reply tool — the monitor detects
    // reply-sent.flag and delivers the message via the outbound adapter.
    let ctx = crate::mcp::context::load_reply_context(cwd).await
        .context("Failed to load reply context — cannot deliver question")?;

    let question_text = format!("❓ **Question:** {}", question);

    // Write the question text to reply.md (monitor reads this for delivery)
    let message_dir = cwd.join("messages").join(&ctx.incoming_message_dir);
    tokio::fs::create_dir_all(&message_dir).await.ok();
    tokio::fs::write(message_dir.join("reply.md"), &question_text).await.ok();

    // Write the reply signal file (triggers monitor to deliver)
    let signal = serde_json::json!({
        "sent_at": chrono::Utc::now().to_rfc3339(),
        "channel": ctx.channel,
        "message_dir": ctx.incoming_message_dir,
        "message_len": question_text.len(),
        "attachment_count": 0,
        "attachments": [],
    });
    tokio::fs::write(
        jyc_dir.join("reply-sent.flag"),
        serde_json::to_string(&signal).unwrap_or_default(),
    )
    .await
    .ok();

    logger.log("INFO", "Question delivered to user via reply signal");

    // Step 2: Write question flag for answer routing.
    // When the next message arrives, the thread manager detects this flag
    // and routes the message body as the answer instead of a new prompt.
    let flag = serde_json::json!({
        "question": question,
        "asked_at": chrono::Utc::now().to_rfc3339(),
    });
    tokio::fs::write(
        &question_flag,
        serde_json::to_string_pretty(&flag).unwrap_or_default(),
    )
    .await
    .context("Failed to write question flag")?;

    logger.log("INFO", "Question flag written, waiting for answer...");

    // Step 3: Poll for answer
    let start = tokio::time::Instant::now();
    loop {
        if start.elapsed() > ANSWER_TIMEOUT {
            tokio::fs::remove_file(&question_flag).await.ok();
            anyhow::bail!(
                "Timed out waiting for user response after {} seconds",
                ANSWER_TIMEOUT.as_secs()
            );
        }

        if answer_file.exists() {
            let content = tokio::fs::read_to_string(&answer_file)
                .await
                .context("Failed to read answer file")?;

            let answer: serde_json::Value = serde_json::from_str(&content)
                .context("Failed to parse answer JSON")?;

            let answer_text = answer["answer"]
                .as_str()
                .unwrap_or("")
                .to_string();

            // Clean up
            tokio::fs::remove_file(&answer_file).await.ok();
            tokio::fs::remove_file(&question_flag).await.ok();

            logger.log("INFO", &format!("Answer received: {} chars", answer_text.len()));

            if answer_text.is_empty() {
                anyhow::bail!("User response was empty");
            }

            return Ok(answer_text);
        }

        tokio::time::sleep(ANSWER_POLL_INTERVAL).await;
    }
}

/// Start the MCP question tool server on stdio.
pub async fn run_server() -> Result<()> {
    let handler = QuestionToolHandler::new();
    let service = handler.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
