//! Response collection from the LLM stream.
//!
//! Extracted from the monolithic `agent_loop.rs`.

use std::ops::Range;
use std::sync::LazyLock;

use anyhow::Result;
use chrono::Utc;
use futures::StreamExt;
use regex::Regex;

use jyc_core::topic_event::TopicEvent;
use jyc_core::topic_event_bus::TopicEventBusRef;

use crate::types::{ContentBlock, Message, Role, StreamEvent};

use super::{ToolCall, publish_event};

/// Collected response from streaming.
#[derive(Debug, Default)]
pub(crate) struct CollectedResponse {
    pub(crate) text: String,
    pub(crate) reasoning_content: String,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    /// Per-call prompt-cache **read** tokens. For Anthropic, this is
    /// `cache_read_input_tokens`; for every other vendor, the single
    /// `cached_tokens` / `prompt_cache_hit_tokens` field. `0` when
    /// the provider didn't surface cache hits for this call.
    pub(crate) cache_hit_tokens: u64,
    /// Per-call prompt-cache **creation** (write) tokens. Anthropic
    /// and OpenAI (GPT-5.6+) report writes separately from reads;
    /// for every other provider this is `0`.
    pub(crate) cache_creation_tokens: u64,
    /// Per-call reasoning (thinking) tokens — the hidden chain-of-thought
    /// share of the output. Already included in `output_tokens` (and
    /// billed as such); informational only. `0` when the provider
    /// doesn't break it out.
    pub(crate) reasoning_tokens: u64,
}

impl CollectedResponse {
    /// Convert to a Message for internal logic (reply detection, text extraction).
    pub(crate) fn to_message(&self) -> Message {
        let mut content = Vec::new();

        if !self.text.is_empty() {
            content.push(ContentBlock::Text {
                text: self.text.clone(),
            });
        }

        for tc in &self.tool_calls {
            let input: serde_json::Value = serde_json::from_str(&tc.arguments)
                .unwrap_or(serde_json::Value::Object(Default::default()));
            content.push(ContentBlock::ToolUse {
                id: tc.id.clone(),
                name: tc.name.clone(),
                input,
            });
        }

        Message {
            role: Role::Assistant,
            content,
        }
    }

    /// Build the raw provider JSON for this assistant response.
    pub(crate) fn to_raw_message(
        &self,
        provider: &dyn crate::provider::Provider,
    ) -> serde_json::Value {
        let tool_calls: Vec<(String, String, String)> = self
            .tool_calls
            .iter()
            .map(|tc| (tc.id.clone(), tc.name.clone(), tc.arguments.clone()))
            .collect();
        provider.build_raw_assistant_message(&self.text, &self.reasoning_content, &tool_calls)
    }
}

const THINKING_PUBLISH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Collect a streaming response into a complete response.
pub(crate) async fn collect_response(
    stream: crate::provider::EventStream,
    sse_read_timeout: std::time::Duration,
    event_bus: Option<&TopicEventBusRef>,
    topic_name: &str,
    thinking_enabled: bool,
) -> Result<CollectedResponse> {
    let mut response = CollectedResponse::default();
    let mut current_tool_id: Option<String> = None;
    let mut current_tool_name: Option<String> = None;
    let mut current_tool_args = String::new();
    let mut saw_done = false;

    // Throttle Thinking events so we don't flood the event bus.
    let mut last_thinking_publish: Option<std::time::Instant> = None;

    tokio::pin!(stream);

    loop {
        let event = match tokio::time::timeout(sse_read_timeout, stream.next()).await {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "SSE stream timed out: no events for {}s",
                    sse_read_timeout.as_secs()
                ));
            }
        };
        match event? {
            StreamEvent::TextDelta(text) => {
                response.text.push_str(&text);
            }
            StreamEvent::ReasoningDelta(text) => {
                response.reasoning_content.push_str(&text);

                // Publish a throttled Thinking event for the dashboard chat pane.
                // Skipped entirely when the user has run `/thinking hide`.
                if thinking_enabled {
                    let now = std::time::Instant::now();
                    let should_publish = match last_thinking_publish {
                        None => true,
                        Some(t) => now.duration_since(t) >= THINKING_PUBLISH_INTERVAL,
                    };
                    if should_publish {
                        last_thinking_publish = Some(now);
                        let text = response.reasoning_content.clone();
                        publish_event(
                            event_bus,
                            TopicEvent::Thinking {
                                topic_name: topic_name.to_string(),
                                text,
                                full_length: response.reasoning_content.len(),
                                timestamp: Utc::now(),
                            },
                        )
                        .await;
                    }
                }
            }
            StreamEvent::ToolUseStart { id, name } => {
                // Flush previous tool call if one is in progress.
                // This handles providers that send multiple tool calls in a
                // single response — the next ToolUseStart arrives before the
                // previous ToolUseEnd, so we must save the previous call now.
                if let (Some(prev_id), Some(prev_name)) =
                    (current_tool_id.take(), current_tool_name.take())
                {
                    response.tool_calls.push(ToolCall {
                        id: prev_id,
                        name: prev_name,
                        arguments: std::mem::take(&mut current_tool_args),
                    });
                }
                current_tool_id = Some(id);
                current_tool_name = Some(name);
            }
            StreamEvent::ToolInputDelta(delta) => {
                current_tool_args.push_str(&delta);
            }
            StreamEvent::ToolUseEnd => {
                if let (Some(id), Some(name)) = (current_tool_id.take(), current_tool_name.take()) {
                    response.tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments: std::mem::take(&mut current_tool_args),
                    });
                }
            }
            StreamEvent::Usage {
                input_tokens,
                output_tokens,
                cache_hit_tokens,
                cache_creation_tokens,
                reasoning_tokens,
            } => {
                response.input_tokens = input_tokens;
                response.output_tokens += output_tokens;
                response.cache_hit_tokens = cache_hit_tokens;
                response.cache_creation_tokens = cache_creation_tokens;
                response.reasoning_tokens += reasoning_tokens;
            }
            StreamEvent::Done => {
                saw_done = true;
                break;
            }
            StreamEvent::Error(msg) => {
                return Err(anyhow::anyhow!("LLM error: {}", msg));
            }
        }
    }

    // A stream that reaches EOF without the provider's `Done` marker was
    // truncated mid-response (gateway cut, connection drop, proxy timeout).
    // Accepting the partial text as complete lets the text-only
    // auto-delivery fallback ship a broken fragment to the user (#786).
    // Fail instead: the message matches the transient retry patterns
    // ("stream ended"), so `complete_with_retry` re-issues the call.
    if !saw_done {
        return Err(anyhow::anyhow!(
            "SSE stream ended without Done marker (truncated mid-stream)"
        ));
    }

    // Final thinking flush: the throttled publishes inside the stream loop
    // may have skipped the tail of the reasoning stream — emit one last
    // snapshot with the full text so live consumers don't lose the ending.
    if thinking_enabled && !response.reasoning_content.is_empty() {
        publish_event(
            event_bus,
            TopicEvent::Thinking {
                topic_name: topic_name.to_string(),
                text: response.reasoning_content.clone(),
                full_length: response.reasoning_content.len(),
                timestamp: Utc::now(),
            },
        )
        .await;
    }

    // Safety net: flush any pending tool call that was started (ToolUseStart)
    // but never ended (ToolUseEnd). Providers that omit `finish_reason` on
    // the last chunk, or that stream the entire tool call in a single chunk
    // without a subsequent end marker, would otherwise drop the accumulated
    // arguments silently.
    if let (Some(id), Some(name)) = (current_tool_id.take(), current_tool_name.take()) {
        response.tool_calls.push(ToolCall {
            id,
            name,
            arguments: std::mem::take(&mut current_tool_args),
        });
    }

    Ok(response)
}

/// Tool-call syntax models leak into the text channel when their function
/// calling is weak. Covers the dialects seen in the wild: Anthropic XML
/// (`tool_use` / `invoke` / `parameter`, plus the `antml:` prefix), the
/// `<call tool=` / `<argument key=` gateway wrapper, the legacy OpenAI
/// `functions.foo(` text call, and a bare `{"name":` JSON call.
///
/// Opening and closing forms both match, the plural spellings too, and the
/// separator after a tag name is optional — the observed leaks are truncated
/// mid-tag (`<parameter name="command" type="string`), so nothing may be
/// required after the name.
static TOOL_CALL_SYNTAX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"</?\s*(?:tool_use|tool_calls?|function_calls?|invoke|parameter",
        r"|response_tools?|response\s+tools?)\b",
        r"|</?\s*(?:call\s+tool=|argument\s+key=)",
        r"|</call>",
        r"|antml:(?:invoke|parameter)\b",
        r"|functions\.\w+\s*\(",
        r#"|\{\s*"name"\s*:"#,
    ))
    .unwrap()
});

/// Byte range of `text` occupied by leaked tool-call syntax, or `None` when
/// the text carries none.
///
/// A *leak line* is a line that IS tool-call syntax: after leading whitespace it
/// begins with one of the dialects, and it sits outside a fenced code block.
/// Prose keeps a dialect mention inline, or fences it — a reply documenting
/// these very tags does both — so neither counts, which is what keeps
/// meta-discussion deliverable (#786 review). For the same reason an inline
/// closing tag in prose is *not* a signal: that check used to kill replies that
/// quoted one. The two generic alternatives (`functions.foo(`, `{"name":`) are
/// only safe because of that line-start rule.
///
/// A run of two or more leak lines, or a single leak line with nothing but
/// blanks after it, is a *block*: the range runs to the end of the text, because
/// the observed leaks are truncated mid-tag and their tail (the half-written
/// argument) is not itself syntax. A single leak line with content after it is a
/// quoted line inside a reply: only that line is returned, so the prose around
/// it survives.
pub(crate) fn leaked_syntax_range(text: &str) -> Option<Range<usize>> {
    let mut offsets = Vec::new();
    let mut offset = 0;
    let mut fences = 0;
    for raw in text.split_inclusive('\n') {
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        if line.trim_start().starts_with("```") {
            fences += 1;
        }
        offsets.push((offset, line));
        offset += raw.len();
    }
    // An unbalanced fence means a truncated reply, whose fence state is
    // unknown — scan again ignoring fences rather than treat the whole tail
    // as code.
    let fenced = fences % 2 == 0;
    let first = first_leak_line(&offsets, true).or_else(|| {
        if fenced {
            None
        } else {
            first_leak_line(&offsets, false)
        }
    })?;

    let leak_lines = offsets[first..]
        .iter()
        .filter(|(_, line)| is_leak_line(line))
        .count();
    let content_after = offsets[first + 1..]
        .iter()
        .any(|(_, line)| !line.trim().is_empty() && !is_leak_line(line));
    if leak_lines > 1 || !content_after {
        Some(offsets[first].0..text.len())
    } else {
        Some(offsets[first].0..offsets[first + 1].0)
    }
}

/// Index of the first line that IS tool-call syntax, skipping fenced code
/// blocks while `respect_fences`.
fn first_leak_line(offsets: &[(usize, &str)], respect_fences: bool) -> Option<usize> {
    let mut in_fence = false;
    for (idx, (_, line)) in offsets.iter().enumerate() {
        let trimmed = line.trim_start();
        if respect_fences && trimmed.starts_with("```") {
            in_fence = !in_fence;
        } else if !in_fence && is_leak_line(line) {
            return Some(idx);
        }
    }
    None
}

/// Whether the line starts with tool-call syntax.
fn is_leak_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    TOOL_CALL_SYNTAX
        .find(trimmed)
        .is_some_and(|m| m.start() == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    fn event_stream(events: Vec<StreamEvent>) -> crate::provider::EventStream {
        Box::pin(stream::iter(events.into_iter().map(Ok)))
    }

    #[tokio::test]
    async fn done_stream_collects_text_and_tool_calls() {
        let events = vec![
            StreamEvent::TextDelta("hello ".to_string()),
            StreamEvent::ToolUseStart {
                id: "1".to_string(),
                name: "bash".to_string(),
            },
            StreamEvent::ToolInputDelta("{\"a\":1}".to_string()),
            StreamEvent::ToolUseEnd,
            StreamEvent::Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_hit_tokens: 0,
                cache_creation_tokens: 0,
                reasoning_tokens: 0,
            },
            StreamEvent::Done,
        ];
        let r = collect_response(
            event_stream(events),
            std::time::Duration::from_secs(5),
            None,
            "topic",
            false,
        )
        .await
        .expect("stream with Done marker must collect");
        assert_eq!(r.text, "hello ");
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].name, "bash");
    }

    #[tokio::test]
    async fn eof_without_done_marker_is_rejected() {
        // A stream that ends (EOF) before `Done` was truncated
        // mid-response; the partial text must not be accepted as complete
        // (#786).
        let events = vec![StreamEvent::TextDelta("half a sentence ".to_string())];
        let err = collect_response(
            event_stream(events),
            std::time::Duration::from_secs(5),
            None,
            "topic",
            false,
        )
        .await
        .expect_err("EOF without Done must fail");
        assert!(
            format!("{err:#}").contains("stream ended"),
            "error should match the transient retry patterns, got: {err:#}"
        );
    }

    #[test]
    fn leaked_syntax_range_covers_the_observed_shapes() {
        // Truncated markup-only reply, cut mid-tag (observed 2026-09-28):
        // everything goes.
        let whole = "<tool_use>\n<invoke name=\"bash\">\n<parameter name=\"command\" type=\"string";
        assert_eq!(leaked_syntax_range(whole), Some(0..whole.len()));

        // Same leak appended after real prose: the prose is kept, and the
        // block runs to the end because its truncated tail (the half-written
        // argument) is not itself syntax.
        let prose = "顺手再查一次 CI 状态：";
        let tail = format!("{prose}\n{whole}");
        assert_eq!(
            leaked_syntax_range(&tail),
            Some(prose.len() + 1..tail.len())
        );

        // Complete block, closing tags included.
        let complete = "<response_tools>\n<invoke name=\"bash\">x\n</response_tools>";
        assert_eq!(leaked_syntax_range(complete), Some(0..complete.len()));

        // Gateway wrapper and the other dialects.
        let gateway = "<response tools=\"<call tool=\"bash\"";
        assert_eq!(leaked_syntax_range(gateway), Some(0..gateway.len()));
        let funcs = "functions.bash(\"ls\")";
        assert_eq!(leaked_syntax_range(funcs), Some(0..funcs.len()));
        let json = "{\"name\": \"bash\", \"arguments\": {}}";
        assert_eq!(leaked_syntax_range(json), Some(0..json.len()));

        // Degenerate dump: bare openers, no closing tags.
        let dump = format!("先说结论。\n{}", "<response_tools>\n".repeat(6));
        assert_eq!(
            leaked_syntax_range(&dump),
            Some("先说结论。\n".len()..dump.len())
        );

        // A LONE leak line with content after it is a quote inside a reply:
        // only that line goes, so the prose around it survives.
        assert_eq!(
            leaked_syntax_range("<response_tools>\n这是正文里引用一次标签。"),
            Some(0.."<response_tools>\n".len())
        );
        assert_eq!(
            leaked_syntax_range("先说结论。\n</call>\n后面还有正文。"),
            Some("先说结论。\n".len().."先说结论。\n</call>\n".len())
        );
    }

    #[test]
    fn leaked_syntax_range_ignores_prose_about_the_dialects() {
        assert_eq!(leaked_syntax_range("普通回复，没有工具调用语法。"), None);
        assert_eq!(leaked_syntax_range(""), None);
        // Inline quote inside prose (#786 review: meta-discussion stays
        // deliverable).
        assert_eq!(
            leaked_syntax_range("比如 `<parameter name=\"command\">` 这种写法。"),
            None
        );
        assert_eq!(
            leaked_syntax_range("报错输出里有 `<response_tools>` 标签，重试后还会出现。"),
            None
        );
        // …and inside a fenced block, the way this guard is documented.
        assert_eq!(
            leaked_syntax_range(
                "两个方言：\n```\n<tool_use>\n<invoke name=\"bash\">\n</tool_use>\n```\n就这样。"
            ),
            None
        );
        // An unbalanced fence means a truncated reply whose fence state is
        // unknown: scan again ignoring fences instead of hiding the leak.
        let truncated = "说明：\n```\n<tool_use>\n<invoke name=\"bash\">";
        assert_eq!(
            leaked_syntax_range(truncated),
            Some("说明：\n```\n".len()..truncated.len())
        );
    }
}
