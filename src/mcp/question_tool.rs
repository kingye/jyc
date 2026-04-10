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
    #[tool(description = "Ask the user a question and wait for their response. The question is sent via the messaging channel (email, Feishu, etc.) and the tool blocks until the user replies or timeout (5 minutes). Use this when you need clarification or a decision from the user before proceeding.")]
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
/// 1. Write question signal file (.jyc/question-sent.flag)
/// 2. Poll for answer file (.jyc/question-answer.json)
/// 3. Return the answer text
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

    // Clean up any stale answer file from previous questions
    if answer_file.exists() {
        tokio::fs::remove_file(&answer_file).await.ok();
    }

    // Write question signal file
    // The monitor reads this to know a question needs to be sent,
    // and routes the next user message as an answer instead of a new prompt.
    let signal = serde_json::json!({
        "question": question,
        "asked_at": chrono::Utc::now().to_rfc3339(),
    });
    tokio::fs::write(
        &question_flag,
        serde_json::to_string_pretty(&signal).unwrap_or_default(),
    )
    .await
    .context("Failed to write question signal file")?;

    logger.log("INFO", "Question signal file written, waiting for answer...");

    // Poll for answer
    let start = tokio::time::Instant::now();
    loop {
        if start.elapsed() > ANSWER_TIMEOUT {
            // Clean up signal file on timeout
            tokio::fs::remove_file(&question_flag).await.ok();
            anyhow::bail!(
                "Timed out waiting for user response after {} seconds",
                ANSWER_TIMEOUT.as_secs()
            );
        }

        if answer_file.exists() {
            // Read the answer
            let content = tokio::fs::read_to_string(&answer_file)
                .await
                .context("Failed to read answer file")?;

            let answer: serde_json::Value = serde_json::from_str(&content)
                .context("Failed to parse answer JSON")?;

            let answer_text = answer["answer"]
                .as_str()
                .unwrap_or("")
                .to_string();

            // Clean up both files
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
