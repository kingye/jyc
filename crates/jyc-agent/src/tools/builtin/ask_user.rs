//! Builtin tool: `ask_user` — ask the user an interactive question and wait
//! for their answer.
//!
//! The question is pushed to the outbound adapter of the channel the user's
//! message arrived on (the websocket channel renders it as a question box).
//! The tool blocks on [`QuestionHub`] oneshots until every answer (or decline)
//! of the set arrives, the user discards the set — which cancels this run —
//! the timeout expires, or the agent turn is cancelled. A channel without
//! interactive support never gets registered: the call fails at once and the
//! tool tells the model to ask in plain text within its reply instead.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use jyc_types::channel::{QuestionAnswer, QuestionReply, QuestionRequest};

use crate::tools::{Tool, ToolContext, ToolOutput};

/// Default question lifetime when the model omits `timeout_seconds`. A real
/// decision takes reading and thinking, so the window is deliberately wide;
/// a batch gets one window for the whole set, not one per question.
const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// How many questions one call may ask at once. Past this the user is filling
/// a form rather than making a decision, and the model should split its work.
const MAX_QUESTIONS: usize = 5;

/// One question of a possibly multi-question call.
#[derive(Debug)]
struct Ask {
    question: String,
    options: Vec<String>,
    allow_multiple: bool,
}

/// What came back for one question.
enum Outcome {
    /// The user's answer, already rendered as the tool reports it.
    Answered(String),
    /// The user declined to answer this one question (`d`); the rest of the
    /// set is unaffected.
    Declined,
    /// The user gave up on the whole set (Esc). Not an answer: it stops the
    /// run. See [`stop_run`].
    Discarded,
    /// The asker vanished mid-flight (the agent turn was cancelled).
    Gone,
    /// The deadline passed with no answer.
    Unanswered,
}

impl Outcome {
    /// How the answer appears inside a multi-question result.
    fn render(&self) -> &str {
        match self {
            Outcome::Answered(text) => text,
            Outcome::Declined => "The user declined this question.",
            Outcome::Discarded => "(discarded)",
            Outcome::Gone => "(cancelled)",
            Outcome::Unanswered => "(no answer)",
        }
    }
}

/// Reported when nothing came back before the deadline.
fn timeout_notice(timeout_secs: u64) -> String {
    format!("Timed out after {timeout_secs}s without an answer.")
}

/// Cancel this topic's run — what a discarded question set means: the user is
/// not going to answer, so there is nothing to wait for. Fires the same
/// per-topic token `/cancel` fires, so the loop stops at the same point.
///
/// A no-op when the context has no live topic to cancel (unit tests,
/// sub-agents).
async fn stop_run(ctx: &ToolContext<'_>) {
    let (Some(channel), Some(topic)) =
        (ctx.current_channel.as_deref(), ctx.current_topic.as_deref())
    else {
        return;
    };
    let Some(managers) = &ctx.topic_managers else {
        return;
    };
    if let Some(manager) = managers.lock().await.get(channel) {
        manager.cancel_topic(topic).await;
    }
}

/// Read the strings out of a JSON array field.
fn str_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(|o| o.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Read the questions out of the tool input: the `questions` array when the
/// model asks several at once, otherwise the top-level
/// `question`/`options`/`allow_multiple` — the shape every existing caller
/// uses, including the XML fallback for weak tool-calling models.
fn parse_questions(input: &Value) -> Result<Vec<Ask>, String> {
    let mut asks = Vec::new();
    if let Some(list) = input
        .get("questions")
        .and_then(|q| q.as_array())
        .filter(|list| !list.is_empty())
    {
        for item in list {
            let Some(question) = item.get("question").and_then(|q| q.as_str()) else {
                return Err("Each question needs a 'question' string".to_string());
            };
            let options = str_list(item.get("options"));
            if options.is_empty() {
                return Err(format!("Question '{question}' needs at least one option"));
            }
            asks.push(Ask {
                question: question.to_string(),
                options,
                allow_multiple: item
                    .get("allow_multiple")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            });
        }
    } else {
        let Some(question) = input.get("question").and_then(|q| q.as_str()) else {
            return Err("Missing 'question' (or a 'questions' array)".to_string());
        };
        let options = str_list(input.get("options"));
        if options.is_empty() {
            return Err("Missing or empty 'options' parameter".to_string());
        }
        asks.push(Ask {
            question: question.to_string(),
            options,
            allow_multiple: input
                .get("allow_multiple")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        });
    }
    if asks.len() > MAX_QUESTIONS {
        return Err(format!(
            "Too many questions ({}); ask at most {MAX_QUESTIONS} at once",
            asks.len()
        ));
    }
    Ok(asks)
}

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
         input changes what you do next. To settle several decisions at once, \
         pass a `questions` array instead of calling this tool repeatedly - the \
         user answers them in one flow and the answers come back together. On \
         channels without interactive support the tool reports an error - ask \
         the question as plain text in your reply instead and proceed with the \
         next user message."
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
                                    Default: 600."
                },
                "allow_multiple": {
                    "type": "boolean",
                    "description": "Whether the user may pick more than one option. \
                                    Default: false."
                },
                "questions": {
                    "type": "array",
                    "maxItems": MAX_QUESTIONS as i32,
                    "description": "Several questions asked at once, answered as one set \
                                    and returned together as Q1/A1, Q2/A2... \
                                    When present, `question`/`options`/`allow_multiple` \
                                    below are ignored. Prefer this over calling ask_user \
                                    repeatedly in the same turn.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": { "type": "string" },
                            "options": {
                                "type": "array",
                                "items": { "type": "string" },
                                "minItems": 1
                            },
                            "allow_multiple": { "type": "boolean" }
                        },
                        "required": ["question", "options"]
                    }
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let asks = match parse_questions(&input) {
            Ok(asks) => asks,
            Err(msg) => return Ok(ToolOutput::error(msg)),
        };
        let timeout_secs = input
            .get("timeout_seconds")
            .and_then(|t| t.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);

        let Some(hub) = &ctx.question_hub else {
            return Ok(ToolOutput::error(
                "ask_user is not available in this context (no question hub)",
            ));
        };

        // The question belongs to the channel the user wrote on, not to
        // whichever channel owns this topic: a turn piped in from feishu runs
        // on the `agents` hub, and a box pushed there is invisible to the user
        // while the whole turn waits for an answer only a TUI could give. A
        // channel with no question support (or no adapter registered here at
        // all) settles the call immediately, so the model asks in its reply
        // text and the turn ends normally.
        let piped_from = ctx
            .origin_channel()
            .filter(|origin| Some(*origin) != ctx.current_channel.as_deref());
        let outbound = match piped_from {
            Some(name) => match &ctx.outbounds {
                Some(map) => map.lock().await.get(name).cloned(),
                None => None,
            },
            None => ctx.outbound.clone(),
        };
        let Some(outbound) = outbound else {
            return Ok(ToolOutput::error(match piped_from {
                Some(name) => format!(
                    "channel '{name}' this message came from does not support interactive \
                     questions. Ask the question as plain text in your reply instead."
                ),
                None => {
                    "ask_user is not available in this context (no outbound adapter)".to_string()
                }
            }));
        };

        let topic = ctx.current_topic.clone().unwrap_or_default();
        let total = asks.len() as u32;
        // Register every question before pushing any, so an answer that
        // arrives while a later question is still on its way out cannot miss
        // its entry. The guards hold the entries until this call returns.
        let mut requests = Vec::with_capacity(asks.len());
        let mut receivers = Vec::with_capacity(asks.len());
        let mut _guards = Vec::with_capacity(asks.len());
        for ask in &asks {
            let id = uuid::Uuid::new_v4().to_string();
            let (tx, rx) = tokio::sync::oneshot::channel();
            _guards.push(hub.register(&id, &topic, tx));
            receivers.push(rx);
            requests.push(QuestionRequest {
                id,
                channel: ctx.current_channel.clone().unwrap_or_default(),
                topic: topic.clone(),
                question: ask.question.clone(),
                options: ask.options.clone(),
                allow_multiple: ask.allow_multiple,
                timeout_seconds: Some(timeout_secs),
            });
        }
        // Push oldest first: the user steps through the set in that order.
        // A channel with no interactive support fails the very first push, so
        // the call returns at once instead of waiting.
        for request in &requests {
            if let Err(e) = outbound.send_question(request).await {
                // ponytail: a push failing mid-batch leaves the questions
                // already on screen with no hub entry behind them (the guards
                // drop on return), so answering one is dropped silently. The
                // upgrade is a cancel frame per pushed request here; no
                // channel's `send_question` has ever failed after a success.
                return Ok(ToolOutput::error(format!(
                    "channel does not support interactive questions: {e:#}. \
                     Ask the question as plain text in your reply instead."
                )));
            }
        }

        // One await for the whole set, under one deadline. The answers are a
        // set — the client picks them all and sends them at once — so waiting
        // on them one at a time would hold the first answer inside a call that
        // has not reached the last question yet. Each question settles on its
        // own answer, dismissal or the deadline; the batch settles with the
        // last of them.
        let budget = Duration::from_secs(timeout_secs);
        let outcomes: Vec<Outcome> = futures::future::join_all(
            receivers
                .into_iter()
                .map(|rx| tokio::time::timeout(budget, rx)),
        )
        .await
        .into_iter()
        .map(|waited| match waited {
            Ok(Ok(QuestionReply::Answer(QuestionAnswer::Choice(choices)))) => {
                Outcome::Answered(match choices.len() {
                    0 => String::new(),
                    // One pick reads exactly as it did before multi-select existed.
                    1 => choices.into_iter().next().unwrap_or_default(),
                    _ => format!("Selected: {}", choices.join(", ")),
                })
            }
            Ok(Ok(QuestionReply::Answer(QuestionAnswer::Declined))) => Outcome::Declined,
            Ok(Ok(QuestionReply::Discarded)) => Outcome::Discarded,
            // The asker is gone (agent turn cancelled): nothing to report to.
            Ok(Err(_)) => Outcome::Gone,
            Err(_) => Outcome::Unanswered,
        })
        .collect();

        // Esc on the box is not an answer to any question: the user gave up on
        // the set, so there is nothing left to wait for — and no point working
        // on with the picks made before it. Stop this run; the loop watches the
        // very token `/cancel` fires and ends the turn at the same point.
        if outcomes.iter().any(|o| matches!(o, Outcome::Discarded)) {
            stop_run(ctx).await;
            return Ok(ToolOutput::error(
                "the user discarded the question set; this run is cancelled",
            ));
        }

        // A single question answers exactly as it did before batches existed.
        if total == 1 {
            return Ok(match outcomes.into_iter().next().unwrap_or(Outcome::Gone) {
                Outcome::Answered(text) => ToolOutput::success(text),
                Outcome::Declined => ToolOutput::success("The user declined the question."),
                // Settled by the discard check above.
                Outcome::Discarded => unreachable!("a discarded set returns early"),
                Outcome::Gone => ToolOutput::error("question cancelled"),
                Outcome::Unanswered => ToolOutput::success(timeout_notice(timeout_secs)),
            });
        }

        if outcomes.iter().all(|o| matches!(o, Outcome::Unanswered)) {
            return Ok(ToolOutput::success(timeout_notice(timeout_secs)));
        }
        let mut out = String::new();
        for (i, (ask, outcome)) in asks.iter().zip(outcomes).enumerate() {
            out.push_str(&format!(
                "Q{}: {}\nA: {}\n\n",
                i + 1,
                ask.question,
                outcome.render()
            ));
        }
        Ok(ToolOutput::success(out.trim_end().to_string()))
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
    async fn decline_reports_decline() {
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
                assert!(hub.respond(&id, QuestionAnswer::Declined));
                out.await.unwrap()
            }
        };

        assert!(!out.is_error);
        assert!(out.content.contains("declined"), "{}", out.content);
    }

    /// Esc on the question box discards the whole set. That is not an answer
    /// to anything: the call settles with an error, so a turn piped in from a
    /// channel that cannot show questions (or with no topic manager to cancel)
    /// never pretends the user replied.
    #[tokio::test]
    async fn discarded_batch_settles_as_a_cancellation() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = Arc::new(jyc_core::question::QuestionHub::new());
        let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let outbound = Arc::new(MockOutbound {
            broadcast: sent.clone(),
        });
        let ctx = ctx_with(tmp.path(), hub.clone(), outbound);

        let input = json!({
            "questions": [
                { "question": "a?", "options": ["x"] },
                { "question": "b?", "options": ["y"] },
            ],
            // Long on purpose: the discard, not the deadline, must end this.
            "timeout_seconds": 60,
        });
        let answerer = async {
            loop {
                let reqs = sent.lock().await.clone();
                if reqs.len() == 2 {
                    return;
                }
                tokio::task::yield_now().await;
            }
        };

        let tool = AskUserTool;
        let started = std::time::Instant::now();
        tokio::pin!(let out = tool.execute(input, &ctx););
        let out = tokio::select! {
            finished = &mut out => panic!("tool finished before the batch was discarded: {finished:?}"),
            () = answerer => {
                assert_eq!(hub.abort_topic("topic-a"), 2, "both questions settle");
                out.await.unwrap()
            }
        };

        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("discarded"), "{}", out.content);
        assert!(
            started.elapsed().as_secs() < 5,
            "a discarded set must not wait out its deadline"
        );
        assert_eq!(
            hub.abort_topic("topic-a"),
            0,
            "a discarded batch leaves nothing pending"
        );
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
        assert_eq!(hub.abort_topic("topic-a"), 0);
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
        assert_eq!(hub.abort_topic("topic-a"), 0);
    }

    /// The bug this covers: a turn piped in from a channel that cannot show a
    /// question box pushed its questions to whichever channel owns the *topic*.
    /// The user never saw them, the reply they were waiting for stayed locked
    /// behind the block, and the timeout ran out on a question that had no
    /// surface to be answered on. A question belongs to the channel the message
    /// came from, so the call fails at once and the model asks in its reply.
    #[tokio::test]
    async fn piped_turn_fails_fast_instead_of_blocking() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = Arc::new(jyc_core::question::QuestionHub::new());
        let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let outbound = Arc::new(MockOutbound {
            broadcast: sent.clone(),
        });
        let mut ctx = ctx_with(tmp.path(), hub.clone(), outbound);
        // The hub channel's outbound map: a pipe-only source channel has no
        // adapter in it at all.
        ctx.outbounds = Some(Arc::new(tokio::sync::Mutex::new(
            std::collections::HashMap::new(),
        )));
        let mut metadata = std::collections::HashMap::new();
        metadata.insert(
            jyc_types::ORIGIN_CHANNEL_METADATA_KEY.to_string(),
            serde_json::Value::String("feishu_work".to_string()),
        );
        ctx.reply_target = Some(crate::tools::ReplyTarget {
            original: jyc_types::InboundMessage {
                id: "test".to_string(),
                channel: "agents".to_string(),
                channel_uid: "1".to_string(),
                sender: "user".to_string(),
                sender_address: "user@test".to_string(),
                recipients: vec![],
                topic: "topic-a".to_string(),
                content: Default::default(),
                timestamp: chrono::Utc::now(),
                references: None,
                reply_to_id: None,
                external_id: None,
                attachments: vec![],
                metadata,
                matched_pattern: None,
            },
            message_dir: "2026-09-28_00-00-00".to_string(),
        });

        let started = std::time::Instant::now();
        let out = AskUserTool
            .execute(input("Pick?", &["a", "b"], Some(60)), &ctx)
            .await
            .unwrap();

        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains("feishu_work") && out.content.contains("plain text"),
            "the model needs to know which channel failed and what to do: {}",
            out.content
        );
        assert!(
            sent.lock().await.is_empty(),
            "the topic's own channel never sees a question it was not asked on"
        );
        assert!(
            started.elapsed().as_secs() < 5,
            "must not wait for an answer nobody can give"
        );
        assert_eq!(hub.abort_topic("topic-a"), 0, "nothing stays pending");
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

    #[test]
    fn parse_questions_reads_the_single_question_shape() {
        let asks = parse_questions(&input("Which?", &["a", "b"], None)).unwrap();
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].question, "Which?");
        assert_eq!(asks[0].options, vec!["a".to_string(), "b".to_string()]);
        assert!(!asks[0].allow_multiple);
    }

    #[test]
    fn parse_questions_rejects_a_batch_that_is_too_large() {
        let list: Vec<Value> = (0..=MAX_QUESTIONS)
            .map(|i| json!({ "question": format!("q{i}?"), "options": ["a"] }))
            .collect();
        let err = parse_questions(&json!({ "questions": list })).unwrap_err();
        assert!(err.contains("Too many questions"), "{err}");
    }

    /// The batch shape is the whole point of asking at once: every question is
    /// pushed up front carrying its place in the batch, each keeps its own
    /// multi flag, and the answers come back paired with what they answered.
    #[tokio::test]
    async fn batch_returns_answers_paired_with_their_questions() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = Arc::new(jyc_core::question::QuestionHub::new());
        let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let outbound = Arc::new(MockOutbound {
            broadcast: sent.clone(),
        });
        let ctx = ctx_with(tmp.path(), hub.clone(), outbound);

        let input = json!({
            "questions": [
                { "question": "Which sections?", "options": ["Added", "Changed"] },
                { "question": "Branch name?", "options": ["feat/x", "fix/x"], "allow_multiple": true },
            ],
            "timeout_seconds": 5,
        });
        let answerer = async {
            loop {
                let reqs = sent.lock().await.clone();
                if reqs.len() == 2 {
                    return reqs;
                }
                tokio::task::yield_now().await;
            }
        };

        let tool = AskUserTool;
        tokio::pin!(let out = tool.execute(input, &ctx););
        let out = tokio::select! {
            finished = &mut out => panic!("tool finished before the answers: {finished:?}"),
            reqs = answerer => {
                assert!(!reqs[0].allow_multiple);
                assert!(reqs[1].allow_multiple, "the multi flag is per question");
                assert!(hub.respond(
                    &reqs[0].id,
                    QuestionAnswer::Choice(vec!["Added".to_string()])
                ));
                assert!(hub.respond(
                    &reqs[1].id,
                    QuestionAnswer::Choice(vec!["feat/x".to_string(), "fix/x".to_string()])
                ));
                out.await.unwrap()
            }
        };

        assert!(!out.is_error);
        assert_eq!(
            out.content,
            "Q1: Which sections?\nA: Added\n\nQ2: Branch name?\nA: Selected: feat/x, fix/x"
        );
    }

    /// A channel that cannot show questions fails the first push, so a batch
    /// must return at once instead of waiting on answers nobody can give.
    #[tokio::test]
    async fn batch_fails_fast_without_interactive_support() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = Arc::new(jyc_core::question::QuestionHub::new());
        let ctx = ctx_with(tmp.path(), hub.clone(), Arc::new(NoQuestionOutbound));

        let started = std::time::Instant::now();
        let out = AskUserTool
            .execute(
                json!({
                    "questions": [
                        { "question": "a?", "options": ["x"] },
                        { "question": "b?", "options": ["y"] },
                    ],
                    "timeout_seconds": 60,
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(out.is_error);
        assert!(
            out.content
                .contains("does not support interactive questions"),
            "{}",
            out.content
        );
        assert!(started.elapsed().as_secs() < 5, "must not wait for answers");
        assert_eq!(
            hub.abort_topic("topic-a"),
            0,
            "a failed batch leaves nothing pending"
        );
    }

    /// The batch reports what it got even when the user only settles part of
    /// it: one question declined and one left to the deadline must still come
    /// back paired with its question, or the model cannot tell which decision
    /// is still open.
    #[tokio::test]
    async fn batch_reports_declined_and_unanswered_questions() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = Arc::new(jyc_core::question::QuestionHub::new());
        let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let outbound = Arc::new(MockOutbound {
            broadcast: sent.clone(),
        });
        let ctx = ctx_with(tmp.path(), hub.clone(), outbound);

        let input = json!({
            "questions": [
                { "question": "Which sections?", "options": ["Added"] },
                { "question": "Branch name?", "options": ["feat/x"] },
            ],
            "timeout_seconds": 1,
        });
        let answerer = async {
            loop {
                let reqs = sent.lock().await.clone();
                if reqs.len() == 2 {
                    return reqs;
                }
                tokio::task::yield_now().await;
            }
        };

        let tool = AskUserTool;
        tokio::pin!(let out = tool.execute(input, &ctx););
        let out = tokio::select! {
            finished = &mut out => panic!("tool finished before the batch could be answered: {finished:?}"),
            reqs = answerer => {
                assert!(hub.respond(&reqs[0].id, QuestionAnswer::Declined));
                out.await.unwrap()
            }
        };

        assert!(!out.is_error);
        assert_eq!(
            out.content,
            "Q1: Which sections?\nA: The user declined this question.\n\n\
             Q2: Branch name?\nA: (no answer)"
        );
    }

    /// The batch is one await, not a queue of them: the questions settle in any
    /// order — the client answers them as a form, so the last question can be
    /// settled before the first — and a set that is answered (or dismissed)
    /// whole returns at once instead of waiting out its deadline.
    #[tokio::test]
    async fn batch_settles_in_any_order_without_waiting_for_the_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = Arc::new(jyc_core::question::QuestionHub::new());
        let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let outbound = Arc::new(MockOutbound {
            broadcast: sent.clone(),
        });
        let ctx = ctx_with(tmp.path(), hub.clone(), outbound);

        let input = json!({
            "questions": [
                { "question": "Which sections?", "options": ["Added", "Changed"] },
                { "question": "Branch name?", "options": ["feat/x", "fix/x"] },
            ],
            // Long on purpose: the call must not need the deadline to return.
            "timeout_seconds": 60,
        });
        let answerer = async {
            loop {
                let reqs = sent.lock().await.clone();
                if reqs.len() == 2 {
                    return reqs;
                }
                tokio::task::yield_now().await;
            }
        };

        let tool = AskUserTool;
        let started = std::time::Instant::now();
        tokio::pin!(let out = tool.execute(input, &ctx););
        let out = tokio::select! {
            finished = &mut out => panic!("tool finished before the batch was settled: {finished:?}"),
            reqs = answerer => {
                // The second question settles first, the first is declined.
                assert!(hub.respond(
                    &reqs[1].id,
                    QuestionAnswer::Choice(vec!["fix/x".to_string()])
                ));
                assert!(hub.respond(&reqs[0].id, QuestionAnswer::Declined));
                out.await.unwrap()
            }
        };

        assert!(
            started.elapsed().as_secs() < 5,
            "must settle on the answers, not the deadline"
        );
        assert_eq!(
            out.content,
            "Q1: Which sections?\nA: The user declined this question.\n\n\
             Q2: Branch name?\nA: fix/x"
        );
    }
}
