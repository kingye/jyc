//! Builtin tool: `ask_user` — ask the user questions and end the turn.
//!
//! The server picks the shape per channel: a channel that can show a question
//! box (the websocket channel, whose box is the TUI) gets one `question` frame
//! per question; every other channel gets the set as a plain-text message on
//! the ordinary reply path. Neither waits. The turn ends the moment the
//! questions are handed over, and the user's answer arrives as their next
//! message — which is also how they skip one question or walk away from the
//! whole set, none of which needs a protocol of its own.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use jyc_types::channel::QuestionRequest;

use crate::tools::{Tool, ToolContext, ToolOutput};

/// How many questions one call may ask at once. Past this the user is filling
/// a form rather than making a decision, and the model should split its work.
const MAX_QUESTIONS: usize = 5;

/// What every settled call reports: the questions are with the user, this turn
/// is over, and the answer is a future message. Read back on the next turn, so
/// it names the questions it answers.
const HANDED_OVER: &str = "The questions below are on their way to the user and this \
                           turn is over. Their reply arrives as the user's next message \
                           — do not ask the same questions again.";

/// One question of a possibly multi-question call.
#[derive(Debug)]
struct Ask {
    question: String,
    options: Vec<String>,
    allow_multiple: bool,
}

/// Render a question set as the plain text a channel without a question box
/// can show: the options numbered the way the user has to answer them.
fn plain_text_block(asks: &[Ask]) -> String {
    let mut out = String::new();
    for (number, ask) in asks.iter().enumerate() {
        out.push_str(&format!("Q{}: {}\n", number + 1, ask.question));
        if ask.allow_multiple {
            out.push_str("   (multi-select)\n");
        }
        for (option, text) in ask.options.iter().enumerate() {
            out.push_str(&format!("   {}) {}\n", option + 1, text));
        }
    }
    out.push_str(
        "\nReply one line per question with the option number(s), e.g. \"Q1: 2\" or \"Q2: 1,3\".",
    );
    out
}

/// Ask a set no question box can show: queue the rendered questions for
/// delivery as an ordinary message and end the turn, so the user reads them
/// where they can answer them.
///
/// The model's reply text is not a delivery mechanism — an error handed to the
/// model is exactly what used to reach the user in place of the question. So
/// the block goes on `ToolContext::pending_texts` and the agent loop sends it
/// through the ordinary reply path, which logs when a send fails just as it
/// does for any other reply. Without a live delivery target (unit tests,
/// sub-agents) there is no message to send: the model's reply is the only
/// surface left, so the block goes to the model and the turn continues long
/// enough for it to be delivered.
fn ask_in_plain_text(ctx: &ToolContext<'_>, asks: &[Ask]) -> ToolOutput {
    let block = plain_text_block(asks);
    if ctx.live_delivery().is_none() {
        tracing::debug!("ask_user: no live delivery target, the model must ask");
        return ToolOutput::success(format!(
            "This channel has no question box and no live delivery target. Put these \
             questions in your reply and stop; the answer arrives as the user's next \
             message.\n\n{block}"
        ));
    }
    tracing::debug!(
        questions = asks.len(),
        "ask_user: no question box, asking in plain text"
    );
    let output = ToolOutput::success(format!("{HANDED_OVER}\n\n{block}"));
    ctx.pending_texts
        .lock()
        .expect("pending_texts poisoned")
        .push(block);
    output.ending_turn()
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

/// Tool for asking the user a question.
pub struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Ask the user one or more questions with selectable options. The questions \
         are delivered to the user and this call returns at once: the turn ends \
         then, and the answer arrives as the user's next message. Use at genuine \
         decision points where the user's input changes what you do next. To settle \
         several decisions at once, pass a `questions` array instead of calling this \
         tool repeatedly - the user answers them in one flow. Anything you write \
         alongside the call reaches the user before the questions, so keep it to \
         context they need to decide; do not repeat the questions themselves."
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
                "allow_multiple": {
                    "type": "boolean",
                    "description": "Whether the user may pick more than one option. \
                                    Default: false."
                },
                "questions": {
                    "type": "array",
                    "maxItems": MAX_QUESTIONS as i32,
                    "description": "Several questions asked at once, answered as one set \
                                    and arriving together as Q1/A1, Q2/A2... \
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

        // A question belongs to the channel the user wrote on, not to
        // whichever channel owns this topic: a turn piped in from feishu runs
        // on the `agents` hub, and a box pushed there is invisible to the user
        // while the plain text they can answer stays on the hub. The origin
        // comes from the inbound message's metadata, stamped by the pipe
        // retarget path just before it overwrites `InboundMessage::channel`, so
        // a tool can still tell who it is really talking to. `None` (or this
        // channel) means the message came from here directly.
        let piped_from = ctx
            .reply_target
            .as_ref()
            .and_then(|target| {
                target
                    .original
                    .metadata
                    .get(jyc_types::ORIGIN_CHANNEL_METADATA_KEY)
                    .and_then(|value| value.as_str())
            })
            .filter(|origin| Some(*origin) != ctx.current_channel.as_deref());
        let outbound = match piped_from {
            Some(name) => match &ctx.outbounds {
                Some(map) => map.lock().await.get(name).cloned(),
                None => None,
            },
            None => ctx.outbound.clone(),
        };
        let Some(outbound) = outbound else {
            if let Some(name) = piped_from {
                // Which pipe channel is missing an adapter is the one thing a
                // misconfiguration would need; the fallback text has no name.
                tracing::debug!(
                    channel = name,
                    "ask_user: no adapter for the origin channel"
                );
            }
            return Ok(ask_in_plain_text(ctx, &asks));
        };

        // Render the set on the channel's own surface and hand it over. One
        // frame per question, oldest first: the user steps through the set in
        // that order and answers the whole of it in one message.
        let topic = ctx.current_topic.clone().unwrap_or_default();
        for ask in &asks {
            let request = QuestionRequest {
                topic: topic.clone(),
                question: ask.question.clone(),
                options: ask.options.clone(),
                allow_multiple: ask.allow_multiple,
            };
            if let Err(e) = outbound.send_question(&request).await {
                // ponytail: a push failing mid-batch leaves the earlier
                // questions on screen while the plain text repeats the set. The
                // upgrade is a cancel frame per pushed request here; no
                // channel's `send_question` has ever failed after a success.
                tracing::debug!(error = %e, "ask_user: the channel refused the question");
                return Ok(ask_in_plain_text(ctx, &asks));
            }
        }
        let block = plain_text_block(&asks);
        tracing::debug!(
            questions = asks.len(),
            "ask_user: questions rendered on the channel's surface"
        );
        Ok(ToolOutput::success(format!("{HANDED_OVER}\n\n{block}")).ending_turn())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;

    /// Minimal outbound adapter with a question box: captures every frame the
    /// tool renders.
    #[derive(Default)]
    struct BoxOutbound {
        sent: tokio::sync::Mutex<Vec<QuestionRequest>>,
    }

    /// Outbound adapter using the trait default: no question box.
    struct NoBoxOutbound;

    #[async_trait]
    impl jyc_types::channel::OutboundAdapter for BoxOutbound {
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
            self.sent.lock().await.push(request.clone());
            Ok(())
        }
    }

    #[async_trait]
    impl jyc_types::channel::OutboundAdapter for NoBoxOutbound {
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

    fn message(
        channel: &str,
        metadata: serde_json::Map<String, Value>,
    ) -> jyc_types::InboundMessage {
        jyc_types::InboundMessage {
            id: "test".to_string(),
            channel: channel.to_string(),
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
            metadata: metadata.into_iter().collect(),
            matched_pattern: None,
        }
    }

    /// A turn that can put a message in front of the user right now: the hub
    /// channel's adapter, plus the message it is answering.
    fn live_ctx<'a>(
        working: &'a Path,
        outbound: Arc<dyn jyc_types::channel::OutboundAdapter>,
        origin: Option<&str>,
    ) -> ToolContext<'a> {
        let mut metadata = serde_json::Map::new();
        if let Some(origin) = origin {
            metadata.insert(
                jyc_types::ORIGIN_CHANNEL_METADATA_KEY.to_string(),
                Value::String(origin.to_string()),
            );
        }
        let mut ctx = ToolContext::new(working);
        ctx.outbound = Some(outbound);
        ctx.current_channel = Some("ws".to_string());
        ctx.current_topic = Some("topic-a".to_string());
        ctx.outbounds = Some(Arc::new(tokio::sync::Mutex::new(
            std::collections::HashMap::new(),
        )));
        ctx.reply_target = Some(crate::tools::ReplyTarget {
            original: message("ws", metadata),
            message_dir: "2026-09-29_00-00-00".to_string(),
        });
        ctx
    }

    fn input(question: &str, options: &[&str]) -> Value {
        json!({ "question": question, "options": options })
    }

    /// A channel with a question box gets one frame per question, carrying its
    /// place in the set and its own multi flag — and the call returns at once
    /// instead of waiting for the pick.
    #[tokio::test]
    async fn box_channel_renders_every_question_and_ends_the_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let outbound = Arc::new(BoxOutbound::default());
        let ctx = live_ctx(tmp.path(), outbound.clone(), None);

        let started = std::time::Instant::now();
        let out = AskUserTool
            .execute(
                json!({
                    "questions": [
                        { "question": "Which sections?", "options": ["Added", "Changed"] },
                        { "question": "Branch name?", "options": ["feat/x", "fix/x"], "allow_multiple": true },
                    ],
                }),
                &ctx,
            )
            .await
            .unwrap();

        let sent = outbound.sent.lock().await;
        assert_eq!(sent.len(), 2, "one frame per question");
        assert!(!sent[0].allow_multiple);
        assert!(sent[1].allow_multiple, "the multi flag is per question");
        assert_eq!(
            sent[1].options,
            vec!["feat/x".to_string(), "fix/x".to_string()]
        );
        assert_eq!(sent[1].topic, "topic-a");
        drop(sent);
        assert!(out.ends_turn, "the turn ends with the questions");
        assert!(
            out.content.contains("Q2: Branch name?"),
            "the result must name what was asked: {}",
            out.content
        );
        assert!(
            ctx.take_pending_texts().is_empty(),
            "a rendered set is not also sent as plain text"
        );
        assert!(started.elapsed().as_secs() < 5, "must not wait for a pick");
    }

    /// The bug this covers: a turn piped in from a channel that cannot show a
    /// question box pushed its questions to whichever channel owns the *topic*,
    /// where nobody could see them. A question belongs to the channel the
    /// message came from, so the set goes out there as a plain-text message and
    /// the turn ends at once.
    #[tokio::test]
    async fn piped_turn_sends_questions_as_plain_text_and_ends_the_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let hub_channel = Arc::new(BoxOutbound::default());
        let ctx = live_ctx(tmp.path(), hub_channel.clone(), Some("feishu_work"));

        let started = std::time::Instant::now();
        let out = AskUserTool
            .execute(input("Pick?", &["a", "b"]), &ctx)
            .await
            .unwrap();

        assert!(!out.is_error, "{}", out.content);
        assert!(out.ends_turn, "the turn ends with the questions");
        let asked = ctx.take_pending_texts();
        assert_eq!(asked.len(), 1, "the set goes out as one message");
        assert!(
            asked[0].contains("Q1: Pick?")
                && asked[0].contains("1) a")
                && asked[0].contains("2) b"),
            "the user must be able to answer by number: {}",
            asked[0]
        );
        assert!(
            hub_channel.sent.lock().await.is_empty(),
            "the topic's own channel never sees a question it was not asked on"
        );
        assert!(
            started.elapsed().as_secs() < 5,
            "must not wait for an answer nobody can give"
        );
    }

    /// A channel whose adapter has no box at all (the trait default fails) asks
    /// in plain text on the same path — no error, no waiting, turn over.
    #[tokio::test]
    async fn channel_without_a_box_asks_in_plain_text() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = live_ctx(tmp.path(), Arc::new(NoBoxOutbound), None);

        let out = AskUserTool
            .execute(input("Pick?", &["a"]), &ctx)
            .await
            .unwrap();

        assert!(!out.is_error, "{}", out.content);
        assert!(out.ends_turn);
        let asked = ctx.take_pending_texts();
        assert_eq!(asked.len(), 1);
        assert!(asked[0].contains("Q1: Pick?"), "{}", asked[0]);
    }

    /// With no live delivery target the model's reply is the only surface left,
    /// so the questions go to the model and the turn keeps running long enough
    /// for it to send them.
    #[tokio::test]
    async fn no_delivery_target_lets_the_model_ask_and_keeps_the_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(tmp.path());

        let out = AskUserTool
            .execute(input("Pick?", &["a", "b"]), &ctx)
            .await
            .unwrap();

        assert!(!out.is_error, "{}", out.content);
        assert!(!out.ends_turn, "the model still has to send them");
        assert!(
            out.content.contains("Q1: Pick?") && out.content.contains("2) b"),
            "the questions must be there to be copied: {}",
            out.content
        );
        assert!(ctx.take_pending_texts().is_empty());
    }

    #[tokio::test]
    async fn missing_options_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(tmp.path());
        let out = AskUserTool
            .execute(json!({"question": "q", "options": []}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
    }

    /// The plain text a channel without a question box shows: every question
    /// numbered, its options numbered the way the user has to answer them, and
    /// multi-select marked so a comma answer is not mistaken for a mistake.
    #[test]
    fn plain_text_block_numbers_questions_and_options() {
        let asks = vec![
            Ask {
                question: "晚饭吃什么?".to_string(),
                options: vec!["米饭".to_string(), "面条".to_string()],
                allow_multiple: false,
            },
            Ask {
                question: "周末做什么?".to_string(),
                options: vec!["徒步".to_string(), "电影".to_string(), "看书".to_string()],
                allow_multiple: true,
            },
        ];
        let block = plain_text_block(&asks);
        assert!(block.contains("Q1: 晚饭吃什么?"), "{block}");
        assert!(block.contains("   1) 米饭"), "{block}");
        assert!(block.contains("   2) 面条"), "{block}");
        assert!(block.contains("Q2: 周末做什么?"), "{block}");
        assert!(block.contains("(multi-select)"), "{block}");
        assert!(block.contains("   3) 看书"), "{block}");
        assert!(
            block.contains("\"Q2: 1,3\""),
            "how to answer must be stated: {block}"
        );
    }

    #[test]
    fn parse_questions_reads_the_single_question_shape() {
        let asks = parse_questions(&input("Which?", &["a", "b"])).unwrap();
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
}
