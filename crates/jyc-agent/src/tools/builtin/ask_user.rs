//! Builtin tool: `ask_user` — ask the user an interactive question and wait
//! for their answer.
//!
//! The question is pushed to the current channel's outbound adapter (the
//! websocket channel renders it as a modal; feishu/wecom cards later). The
//! tool blocks on a [`QuestionHub`] oneshot until the channel's inbound
//! adapter submits the user's answer, the timeout expires, or the agent turn
//! is cancelled. Channels without interactive support fail `send_question`
//! gracefully; the tool reports that to the model so it can ask in plain
//! text within its reply instead.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use jyc_types::channel::{QuestionAnswer, QuestionRequest};

use crate::tools::{Tool, ToolContext, ToolOutput};

/// Default question lifetime when the model omits `timeout_seconds`.
const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// Tool for interactive user questions.
pub struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Ask the user an interactive question with selectable options and \
         wait for their answer. Blocks until the user answers, cancels, or \
         the timeout expires. Use at genuine decision points where the user's \
         input changes what you do next. On channels without interactive \
         support the tool reports an error — ask the question as plain text \
         in your reply instead and proceed with the next user message."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to ask the user"
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "description": "The selectable options"
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": "How long to wait for an answer before giving up. \
                                    Default: 300."
                },
                "allow_multiple": {
                    "type": "boolean",
                    "description": "Whether the user may pick more than one option. \
                                    Default: false."
                }
            },
            "required": ["question", "options"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let Some(question) = input.get("question").and_then(|q| q.as_str()) else {
            return Ok(ToolOutput::error("Missing 'question' parameter"));
        };
        let options: Vec<String> = input
            .get("options")
            .and_then(|o| o.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if options.is_empty() {
            return Ok(ToolOutput::error("Missing or empty 'options' parameter"));
        }
        let timeout_secs = input
            .get("timeout_seconds")
            .and_then(|t| t.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        let allow_multiple = input
            .get("allow_multiple")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let (Some(hub), Some(outbound)) = (&ctx.question_hub, &ctx.outbound) else {
            return Ok(ToolOutput::error(
                "ask_user is not available in this context (no question hub or outbound adapter)",
            ));
        };

        // Register before pushing so a fast answer can never miss the entry.
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let topic = ctx.current_topic.clone().unwrap_or_default();
        let _guard = hub.register(&id, &topic, tx);

        let request = QuestionRequest {
            id: id.clone(),
            channel: ctx.current_channel.clone().unwrap_or_default(),
            topic: topic.clone(),
            question: question.to_string(),
            options,
            allow_multiple,
            timeout_seconds: Some(timeout_secs),
        };
        if let Err(e) = outbound.send_question(&request).await {
            return Ok(ToolOutput::error(format!(
                "channel does not support interactive questions: {e:#}. \
                 Ask the question as plain text in your reply instead."
            )));
        }

        match tokio::time::timeout(Duration::from_secs(timeout_secs), rx).await {
            Ok(Ok(QuestionAnswer::Choice(choices))) => {
                Ok(ToolOutput::success(match choices.len() {
                    0 => String::new(),
                    // One pick reads exactly as it did before multi-select existed.
                    1 => choices.into_iter().next().unwrap_or_default(),
                    _ => format!("Selected: {}", choices.join(", ")),
                }))
            }
            Ok(Ok(QuestionAnswer::Cancelled)) => {
                Ok(ToolOutput::success("The user dismissed the question."))
            }
            // The asker is gone (agent turn cancelled): nothing to report to.
            Ok(Err(_)) => Ok(ToolOutput::error("question cancelled")),
            Err(_) => Ok(ToolOutput::success(format!(
                "Timed out after {timeout_secs}s without an answer."
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;

    /// Minimal outbound adapter capturing the pushed question.
    struct MockOutbound {
        broadcast: Arc<tokio::sync::Mutex<Vec<QuestionRequest>>>,
    }

    #[async_trait]
    impl jyc_types::channel::OutboundAdapter for MockOutbound {
        fn channel_type(&self) -> &str {
            "mock"
        }
        async fn connect(&self) -> Result<()> {
            Ok(())
        }
        async fn disconnect(&self) -> Result<()> {
            Ok(())
        }
        fn clean_body(&self, raw: &str) -> String {
            raw.to_string()
        }
        async fn send_reply(
            &self,
            _original: &jyc_types::InboundMessage,
            _reply_text: &str,
            _topic_path: &Path,
            _message_dir: &str,
            _attachments: Option<&[jyc_types::OutboundAttachment]>,
        ) -> Result<jyc_types::SendResult> {
            unimplemented!()
        }
        async fn send_message(
            &self,
            _recipient: &str,
            _subject: &str,
            _body: &str,
        ) -> Result<jyc_types::SendResult> {
            unimplemented!()
        }
        async fn send_question(&self, request: &QuestionRequest) -> Result<()> {
            self.broadcast.lock().await.push(request.clone());
            Ok(())
        }
    }

    /// Outbound adapter using the trait default: no question support.
    struct NoQuestionOutbound;

    #[async_trait]
    impl jyc_types::channel::OutboundAdapter for NoQuestionOutbound {
        fn channel_type(&self) -> &str {
            "mock"
        }
        async fn connect(&self) -> Result<()> {
            Ok(())
        }
        async fn disconnect(&self) -> Result<()> {
            Ok(())
        }
        fn clean_body(&self, raw: &str) -> String {
            raw.to_string()
        }
        async fn send_reply(
            &self,
            _original: &jyc_types::InboundMessage,
            _reply_text: &str,
            _topic_path: &Path,
            _message_dir: &str,
            _attachments: Option<&[jyc_types::OutboundAttachment]>,
        ) -> Result<jyc_types::SendResult> {
            unimplemented!()
        }
        async fn send_message(
            &self,
            _recipient: &str,
            _subject: &str,
            _body: &str,
        ) -> Result<jyc_types::SendResult> {
            unimplemented!()
        }
    }

    fn ctx_with(
        working: &Path,
        hub: Arc<jyc_core::question::QuestionHub>,
        outbound: Arc<dyn jyc_types::channel::OutboundAdapter>,
    ) -> ToolContext<'_> {
        let mut ctx = ToolContext::new(working);
        ctx.question_hub = Some(hub);
        ctx.outbound = Some(outbound);
        ctx.current_channel = Some("ws".to_string());
        ctx.current_topic = Some("topic-a".to_string());
        ctx
    }

    fn input(question: &str, options: &[&str], timeout: Option<u64>) -> Value {
        json!({
            "question": question,
            "options": options,
            "timeout_seconds": timeout,
        })
    }

    /// `allow_multiple` must reach the channel, and two picks come back as one
    /// readable line (one pick staying verbatim - see `answer_returns_choice`).
    #[tokio::test]
    async fn answer_returns_multiple_choices() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = Arc::new(jyc_core::question::QuestionHub::new());
        let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let outbound = Arc::new(MockOutbound {
            broadcast: sent.clone(),
        });
        let ctx = ctx_with(tmp.path(), hub.clone(), outbound);

        let mut input = input("Which?", &["a", "b", "c"], Some(5));
        input["allow_multiple"] = json!(true);
        let answerer = async {
            loop {
                if let Some(req) = sent.lock().await.first().cloned() {
                    return req;
                }
                tokio::task::yield_now().await;
            }
        };

        let tool = AskUserTool;
        tokio::pin!(let out = tool.execute(input, &ctx););
        let out = tokio::select! {
            finished = &mut out => panic!("tool finished before the answer: {finished:?}"),
            req = answerer => {
                assert!(req.allow_multiple, "the flag must reach the channel");
                assert!(hub.respond(
                    &req.id,
                    QuestionAnswer::Choice(vec!["a".to_string(), "c".to_string()])
                ));
                out.await.unwrap()
            }
        };

        assert!(!out.is_error);
        assert_eq!(out.content, "Selected: a, c");
    }

    #[tokio::test]
    async fn answer_returns_choice() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = Arc::new(jyc_core::question::QuestionHub::new());
        let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let outbound = Arc::new(MockOutbound {
            broadcast: sent.clone(),
        });
        let ctx = ctx_with(tmp.path(), hub.clone(), outbound);

        // Poll the outbound capture until the question lands, then answer
        // it. `select!` interleaves the two futures on one task because the
        // tool context borrows `tmp` (not 'static, so no spawn).
        let answerer = async {
            loop {
                let req = sent.lock().await.first().cloned();
                if let Some(req) = req {
                    return req.id;
                }
                tokio::task::yield_now().await;
            }
        };

        let tool = AskUserTool;
        tokio::pin!(let out = tool.execute(input("Pick?", &["a", "b"], Some(5)), &ctx););
        let out = tokio::select! {
            finished = &mut out => panic!("tool finished before the answer: {finished:?}"),
            id = answerer => {
                assert!(hub.respond(&id, QuestionAnswer::Choice(vec!["b".to_string()])));
                out.await.unwrap()
            }
        };

        assert!(!out.is_error);
        assert_eq!(out.content, "b");
    }

    #[tokio::test]
    async fn cancel_reports_dismissal() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = Arc::new(jyc_core::question::QuestionHub::new());
        let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let outbound = Arc::new(MockOutbound {
            broadcast: sent.clone(),
        });
        let ctx = ctx_with(tmp.path(), hub.clone(), outbound);

        let answerer = async {
            loop {
                let req = sent.lock().await.first().cloned();
                if let Some(req) = req {
                    return req.id;
                }
                tokio::task::yield_now().await;
            }
        };

        let tool = AskUserTool;
        tokio::pin!(let out = tool.execute(input("Pick?", &["a"], Some(5)), &ctx););
        let out = tokio::select! {
            finished = &mut out => panic!("tool finished before the answer: {finished:?}"),
            id = answerer => {
                assert!(hub.respond(&id, QuestionAnswer::Cancelled));
                out.await.unwrap()
            }
        };

        assert!(!out.is_error);
        assert!(out.content.contains("dismissed"));
    }

    #[tokio::test]
    async fn timeout_returns_notice() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = Arc::new(jyc_core::question::QuestionHub::new());
        let outbound = Arc::new(MockOutbound {
            broadcast: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        });
        let ctx = ctx_with(tmp.path(), hub.clone(), outbound);

        let tool = AskUserTool;
        let out = tool
            .execute(input("Pick?", &["a"], Some(1)), &ctx)
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(
            out.content.contains("Timed out after 1s"),
            "{}",
            out.content
        );
        // The hub entry was cleaned up by the guard.
        assert_eq!(hub.pending_for("topic-a"), None);
    }

    #[tokio::test]
    async fn unsupported_channel_reports_error() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = Arc::new(jyc_core::question::QuestionHub::new());
        let outbound = Arc::new(NoQuestionOutbound);
        let ctx = ctx_with(tmp.path(), hub.clone(), outbound);

        let tool = AskUserTool;
        let out = tool
            .execute(input("Pick?", &["a"], None), &ctx)
            .await
            .unwrap();

        assert!(out.is_error);
        assert!(
            out.content
                .contains("does not support interactive questions"),
            "{}",
            out.content
        );
        // Failed push must not leak a pending entry.
        assert_eq!(hub.pending_for("topic-a"), None);
    }

    #[tokio::test]
    async fn missing_options_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(tmp.path());
        let tool = AskUserTool;
        let out = tool
            .execute(json!({"question": "q", "options": []}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
    }
}
