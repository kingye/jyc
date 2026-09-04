//! Feishu live progress indicator ("process indicator").
//!
//! When a piped Feishu message enters an agent run, a watcher subscribes
//! to the topic's event bus and maintains a plain-text status message in
//! the originating Feishu chat:
//!
//! - Two-phase: the card is only sent once the first fresh
//!   `ProcessingStarted` arrives — messages that never reach the agent
//!   (slash commands, empty-body drops) produce no card at all.
//! - While live, the card is PATCHed as tools fire (throttled, never
//!   blocking the event bus).
//! - On `ProcessingCompleted` the card is finalized with the outcome.

use std::sync::Arc;

use jyc_core::duration::{DurationStyle, format_duration_secs};
use jyc_core::topic_event::TopicEvent;
use jyc_core::topic_manager::{TopicDisplayState, TopicManager};
// Shared tool-call formatter (field extraction + basename + collapse +
// truncate) — also used by the TUI chat progress tail at render time.
use jyc_types::inspect::tool_activity_summary as tool_activity;

use super::client::FeishuClient;

/// Minimum seconds between Feishu status-card PATCHes (rate-limit guard).
const PROGRESS_PATCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(4);

/// Compact "what is happening now" summary for a tool call, extracted from
/// Max chars of the thinking tail shown on the progress card.
const THINKING_PREVIEW_CHARS: usize = 1500;

/// Return the trailing `max_chars` characters of `text`, cut on a char
/// boundary. Returns the whole string when it fits.
fn tail_chars(text: &str, max_chars: usize) -> &str {
    if max_chars == 0 {
        return "";
    }
    match text.char_indices().rev().nth(max_chars - 1) {
        Some((idx, _)) => &text[idx..],
        None => text,
    }
}

/// Build the status-card markdown for the Feishu progress watcher.
///
/// `state` is `"⏳ 处理中"` while running, `"✅ 完成"` on success,
/// `"❌ 失败"` on failure. Display segments (mode · model · context %)
/// are omitted when the topic has no recorded state yet — `pct` needs
/// both token bounds, i.e. at least one LLM call.
fn progress_card(
    state: &str,
    elapsed_secs: u64,
    tool_count: usize,
    activity: Option<&str>,
    display: &TopicDisplayState,
    thinking: Option<&str>,
) -> String {
    let mut line = format!(
        "{state} · {} · 工具 {tool_count}",
        format_duration_secs(elapsed_secs, DurationStyle::Precise)
    );
    if let Some(mode) = &display.mode {
        line.push_str(&format!(" · {mode}"));
    }
    if let Some(model) = &display.model {
        line.push_str(&format!(" · {model}"));
    }
    if let Some(pct) = display.context_pct() {
        line.push_str(&format!(" · {pct}%"));
    }
    let mut lines = vec![line];
    if let Some(a) = activity {
        lines.push(format!("最近：{a}"));
    }
    let thinking = thinking.map(str::trim).filter(|t| !t.is_empty());
    if let Some(t) = thinking {
        lines.push(format!("💭 {}", tail_chars(t, THINKING_PREVIEW_CHARS)));
    }
    lines.join("\n")
}

/// Watch one topic's event bus and maintain the Feishu status card.
///
/// Two phases:
/// 1. **Waiting** — no card is sent until the first fresh
///    `ProcessingStarted`. Messages that never reach the agent loop
///    (slash commands, empty-body drops) publish no such event, so they
///    produce no card at all. Waiting costs one sleeping task, zero
///    API calls.
/// 2. **Live** — PATCH the card as tools fire; finalize on
///    `ProcessingCompleted`.
///
/// Exits on `ProcessingCompleted` (final card), when the bus is dropped,
/// after `MAX_LIFETIME` (safety net), or silently after 3 consecutive
/// Feishu API failures — the reply footer works independently.
///
/// Concurrent watchers of the same topic share one status message via the
/// `cards` registry: the first watcher to arm posts it and records the
/// message id; later watchers armed by the same run reuse that id instead
/// of posting a duplicate. The entry is removed when the run finalizes,
/// so the next run posts a fresh message.
pub fn spawn_progress_watcher(
    feishu_client: Arc<FeishuClient>,
    topic_manager: Arc<TopicManager>,
    topic: String,
    chat_id: String,
    start: std::time::Instant,
    seen_after: chrono::DateTime<chrono::Utc>,
    cards: Arc<tokio::sync::Mutex<std::collections::HashMap<String, String>>>,
) {
    tokio::spawn(async move {
        const MAX_LIFETIME: std::time::Duration = std::time::Duration::from_secs(2 * 60 * 60);
        const MAX_PATCH_FAILURES: u32 = 3;

        // No bus (events disabled) → no status card at all.
        let Some(bus) = topic_manager.get_or_create_event_bus(&topic).await else {
            return;
        };
        let mut rx = match bus.subscribe().await {
            Ok(rx) => rx,
            Err(_) => return,
        };

        // ── Phase 1: wait for the first fresh ProcessingStarted ─────────
        //
        // `seen_after` is captured by the caller *before routing*, so
        // every event of this run is strictly newer and previous runs'
        // replayed events stay filtered. A fresh ProcessingCompleted
        // while waiting belongs to a *previous* run — ignored: this
        // message may still be queued behind it.
        // `start` is captured by the caller at message *arrival*. A
        // message queued behind a busy topic would otherwise count the
        // queue wait as processing time, so the card clock is reset when
        // this run actually starts (until then the watcher-lifetime bound
        // still uses the original `start`).
        let mut start = start;
        let (status_message_id, mut last_text) = loop {
            // Bounded wait: even on a completely silent topic (no events
            // at all) the watcher exits once MAX_LIFETIME is exceeded.
            let remaining = MAX_LIFETIME.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return;
            }
            let ev = match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(ev)) => ev,
                Ok(None) => return, // bus dropped
                Err(_) => return,   // lifetime exceeded
            };
            let is_start =
                ev.timestamp() > seen_after && matches!(ev, TopicEvent::ProcessingStarted { .. });
            if !is_start {
                continue;
            }
            // Processing actually starts now — exclude the queue wait
            // from the elapsed time shown on the card.
            start = std::time::Instant::now();
            let display = topic_manager.topic_display_state(&topic).await;
            let text = progress_card(
                "⏳ 处理中",
                start.elapsed().as_secs(),
                0,
                None,
                &display,
                None,
            );
            // Dedup: another watcher of this topic may already have posted
            // this run's status message (a dormant watcher armed by the
            // same ProcessingStarted). Reuse its message id instead of
            // posting a duplicate. The lock is held across the send so two
            // watchers arming concurrently can't both create.
            let mut cards = cards.lock().await;
            if let Some(existing) = cards.get(&topic) {
                break (existing.clone(), text);
            }
            // Bounded send: the event bus is shared with the agent loop,
            // so a hung Feishu call must never stall this watcher.
            match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                feishu_client.send_text_message(&chat_id, &text),
            )
            .await
            {
                Ok(Ok(r)) => {
                    cards.insert(topic.clone(), r.message_id.clone());
                    break (r.message_id, text);
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        error = %e, topic = %topic,
                        "feishu progress watcher: failed to send status card"
                    );
                    return;
                }
                Err(_) => {
                    tracing::warn!(
                        topic = %topic,
                        "feishu progress watcher: status card send timed out"
                    );
                    return;
                }
            }
        };

        // ── Phase 2: live updates until ProcessingCompleted ─────────────
        let mut tool_count = 0usize;
        let mut last_activity: Option<String> = None;
        let mut last_thinking: Option<String> = None;
        let mut done: Option<(bool, u64)> = None;
        let mut last_patch = std::time::Instant::now();
        let mut fails = 0u32;
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));

        loop {
            if start.elapsed() > MAX_LIFETIME {
                break;
            }
            let ev = if done.is_some() {
                None
            } else {
                tokio::select! {
                    ev = rx.recv() => Some(ev),
                    _ = ticker.tick() => None,
                }
            };
            match ev {
                Some(Some(ev)) => {
                    let fresh = ev.timestamp() > seen_after;
                    if fresh {
                        match ev {
                            TopicEvent::ToolStarted {
                                tool_name, input, ..
                            } => {
                                tool_count += 1;
                                // Keep the previous activity when the tool has
                                // no summary (reply/MCP tools) — otherwise the
                                // "最近：" line flickers away mid-run.
                                if let Some(a) = tool_activity(&tool_name, input.as_deref()) {
                                    last_activity = Some(format!("{tool_name} — {a}"));
                                }
                            }
                            TopicEvent::ProcessingCompleted {
                                success,
                                duration_secs,
                                ..
                            } => {
                                done = Some((success, duration_secs));
                            }
                            // Cumulative snapshot of the current LLM request
                            // (throttled at the agent loop): keep the latest —
                            // its tail is the current thinking position.
                            TopicEvent::Thinking { text, .. } => {
                                last_thinking = Some(text);
                            }
                            _ => {}
                        }
                    }
                }
                Some(None) => break, // bus dropped
                None => {}           // tick or terminal
            }

            let terminal = done.is_some();
            if !terminal && last_patch.elapsed() < PROGRESS_PATCH_INTERVAL {
                continue;
            }
            // Fetch display state (mode · model · context %) at render
            // time — small state-file reads, cheap enough per tick, so
            // mid-run /plan or /model switches show up on the next PATCH.
            let display = topic_manager.topic_display_state(&topic).await;
            let text = match done {
                Some((success, duration_secs)) => progress_card(
                    if success { "✅ 完成" } else { "❌ 失败" },
                    duration_secs,
                    tool_count,
                    last_activity.as_deref(),
                    &display,
                    last_thinking.as_deref(),
                ),
                None => progress_card(
                    "⏳ 处理中",
                    start.elapsed().as_secs(),
                    tool_count,
                    last_activity.as_deref(),
                    &display,
                    last_thinking.as_deref(),
                ),
            };
            if !terminal && text == last_text {
                continue;
            }
            // Bounded update: the event bus is shared with the agent loop,
            // so a hung Feishu PATCH must never stall this watcher.
            let upd = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                feishu_client.update_text_message(&status_message_id, &text),
            )
            .await;
            match upd {
                Ok(Ok(())) => {
                    fails = 0;
                    last_text = text;
                    last_patch = std::time::Instant::now();
                }
                Ok(Err(e)) => {
                    fails += 1;
                    tracing::debug!(
                        error = %e, topic = %topic, fails,
                        "feishu progress watcher: status card update failed"
                    );
                    if fails >= MAX_PATCH_FAILURES {
                        break;
                    }
                }
                Err(_elapsed) => {
                    fails += 1;
                    tracing::debug!(
                        topic = %topic, fails,
                        "feishu progress watcher: status card update timed out"
                    );
                    if fails >= MAX_PATCH_FAILURES {
                        break;
                    }
                }
            }
            if terminal {
                // Run finalized — release the dedup entry so the next run
                // posts a fresh status message.
                cards.lock().await.remove(&topic);
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_card_renders_state_elapsed_and_activity() {
        let none = TopicDisplayState::default();
        assert_eq!(
            progress_card("⏳ 处理中", 12, 3, None, &none, None),
            "⏳ 处理中 · 12s · 工具 3"
        );
        assert_eq!(
            progress_card("⏳ 处理中", 12, 3, Some("edit — tools.rs"), &none, None),
            "⏳ 处理中 · 12s · 工具 3\n最近：edit — tools.rs"
        );
        assert_eq!(
            progress_card("✅ 完成", 52, 8, None, &none, None),
            "✅ 完成 · 52s · 工具 8"
        );
    }

    #[test]
    fn progress_card_shows_thinking_tail_and_retains_it_on_finalize() {
        let none = TopicDisplayState::default();
        let thinking = "计划步骤一\n然后调用工具读取文件";
        let card = progress_card(
            "⏳ 处理中",
            12,
            3,
            Some("read — a.rs"),
            &none,
            Some(thinking),
        );
        assert!(
            card.contains("💭 计划步骤一\n然后调用工具读取文件"),
            "card:\n{card}"
        );
        // Thinking line renders after the activity line.
        let activity_pos = card.find("最近：read — a.rs").expect("activity line");
        let thinking_pos = card.find('💭').expect("thinking line");
        assert!(thinking_pos > activity_pos);
        // Finalized card keeps the last thinking preview.
        let done = progress_card("✅ 完成", 52, 8, None, &none, Some(thinking));
        assert!(done.contains("💭 计划步骤一"), "card:\n{done}");
        // No thinking (or whitespace-only) → no 💭 line.
        assert!(!progress_card("⏳ 处理中", 12, 3, None, &none, None).contains('💭'));
        assert!(!progress_card("⏳ 处理中", 12, 3, None, &none, Some("  ")).contains('💭'));
    }

    #[test]
    fn progress_card_truncates_thinking_to_tail_chars() {
        let none = TopicDisplayState::default();
        let thinking = "a".repeat(THINKING_PREVIEW_CHARS + 500);
        let card = progress_card("⏳ 处理中", 1, 0, None, &none, Some(&thinking));
        let line = card
            .lines()
            .find(|l| l.starts_with('💭'))
            .expect("thinking line");
        assert_eq!(line.chars().count(), 2 + THINKING_PREVIEW_CHARS);
    }

    #[test]
    fn tail_chars_keeps_last_chars_on_char_boundaries() {
        assert_eq!(tail_chars("hello", 3), "llo");
        assert_eq!(tail_chars("héllo", 3), "llo");
        assert_eq!(tail_chars("短", THINKING_PREVIEW_CHARS), "短");
        assert_eq!(tail_chars("hi", 0), "");
    }

    #[test]
    fn progress_card_display_segments() {
        // Full display state: mode · model · context % all appended.
        let full = TopicDisplayState {
            mode: Some("plan".to_string()),
            model: Some("kimi/k3-256k".to_string()),
            input_tokens: Some(108_134),
            max_tokens: Some(262_144),
        };
        assert_eq!(
            progress_card("⏳ 处理中", 23, 2, None, &full, None),
            "⏳ 处理中 · 23s · 工具 2 · plan · kimi/k3-256k · 41%"
        );
        assert_eq!(
            progress_card("✅ 完成", 757, 24, None, &full, None),
            "✅ 完成 · 12m37s · 工具 24 · plan · kimi/k3-256k · 41%"
        );

        // Activity line still comes second when present.
        let with_activity =
            progress_card("⏳ 处理中", 23, 2, Some("bash — cargo check"), &full, None);
        assert_eq!(
            with_activity,
            "⏳ 处理中 · 23s · 工具 2 · plan · kimi/k3-256k · 41%\n最近：bash — cargo check"
        );

        // pct hidden until both token bounds are known (pre-first-LLM-call).
        let no_tokens = TopicDisplayState {
            mode: Some("build".to_string()),
            model: Some("kimi/k3-256k".to_string()),
            ..TopicDisplayState::default()
        };
        assert_eq!(
            progress_card("⏳ 处理中", 3, 0, None, &no_tokens, None),
            "⏳ 处理中 · 3s · 工具 0 · build · kimi/k3-256k"
        );

        // Zero max_tokens → pct hidden (division guard).
        let zero_max = TopicDisplayState {
            input_tokens: Some(100),
            max_tokens: Some(0),
            ..TopicDisplayState::default()
        };
        assert_eq!(
            progress_card("⏳ 处理中", 3, 0, None, &zero_max, None),
            "⏳ 处理中 · 3s · 工具 0"
        );
    }
}
