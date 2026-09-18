//! Feishu live progress indicator ("process indicator").
//!
//! When a piped Feishu message enters an agent run, a watcher subscribes
//! to the topic's event bus and maintains a plain-text status message in
//! the originating Feishu chat:
//!
//! - Two-phase: the card is only sent once the first fresh
//!   `ProcessingStarted` arrives — messages that never reach the agent
//!   (slash commands, empty-body drops) produce no card at all. In `attach`
//!   mode the run is already in flight, so the card is sent immediately
//!   instead (see `spawn_progress_watcher`).
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

/// Max chars of accumulated thinking embedded in the final card's collapsed
/// panel. Card JSON is limited to ~30 KB (bytes); CJK chars take 3 UTF-8
/// bytes each, so 8 000 chars stays safely under the cap.
const FINAL_THINKING_PANEL_CHARS: usize = 8_000;

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

/// Record a cumulative thinking snapshot. Snapshots within one LLM request
/// grow by extension — replace the current block. A snapshot that does not
/// extend the current block starts a new request's block, so the final card
/// can embed every round's thinking, not just the last one.
fn push_thinking_block(blocks: &mut Vec<String>, text: String) {
    match blocks.last_mut() {
        Some(last) if text.starts_with(last.as_str()) => *last = text,
        _ => blocks.push(text),
    }
}

/// Build the finalized status card JSON: the status markdown followed by a
/// collapsed panel (Feishu `collapsible_panel`, client ≥ V7.9) holding all
/// accumulated thinking blocks, tail-capped to stay under the card size
/// limit. Without thinking the card is a single markdown element.
fn final_card_json(status_text: &str, thinking_blocks: &[String]) -> serde_json::Value {
    let mut elements = vec![serde_json::json!({
        "tag": "markdown",
        "content": status_text
    })];
    let thinking = thinking_blocks.join("\n\n---\n\n");
    let thinking = thinking.trim();
    if !thinking.is_empty() {
        let truncated = thinking.chars().count() > FINAL_THINKING_PANEL_CHARS;
        let body = tail_chars(thinking, FINAL_THINKING_PANEL_CHARS);
        let content = if truncated {
            format!("…（仅显示尾部 {FINAL_THINKING_PANEL_CHARS} 字符）\n\n{body}")
        } else {
            body.to_string()
        };
        elements.push(serde_json::json!({
            "tag": "collapsible_panel",
            "expanded": false,
            "background_color": "grey",
            "header": {
                "title": {"tag": "plain_text", "content": "💭 Thinking"}
            },
            "elements": [{"tag": "markdown", "content": content}]
        }));
    }
    // Full card-JSON 2.0 envelope: Feishu's update-card API requires
    // `config.update_multi` in BOTH the initial card and every PATCH
    // payload — a bare `{"elements": …}` body is rejected.
    serde_json::json!({
        "schema": "2.0",
        "config": {"update_multi": true},
        "body": {"elements": elements}
    })
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
    lines.join("\n")
}

/// Build the in-progress status card: status markdown plus, when thinking
/// is present, a collapsed panel holding the live thinking tail — keeps
/// the noisy stream folded behind a header tap, matching the final card's
/// presentation. (NB: each PATCH re-sends `expanded: false`, so manually
/// expanding mid-run folds back on the next update.)
fn progress_card_json(status_text: &str, thinking: Option<&str>) -> serde_json::Value {
    let mut elements = vec![serde_json::json!({
        "tag": "markdown",
        "content": status_text
    })];
    let thinking = thinking.map(str::trim).filter(|t| !t.is_empty());
    if let Some(t) = thinking {
        elements.push(serde_json::json!({
            "tag": "collapsible_panel",
            "expanded": false,
            "background_color": "grey",
            "header": {
                "title": {"tag": "plain_text", "content": "💭 Thinking"}
            },
            "elements": [{
                "tag": "markdown",
                "content": tail_chars(t, THINKING_PREVIEW_CHARS)
            }]
        }));
    }
    serde_json::json!({
        "schema": "2.0",
        "config": {"update_multi": true},
        "body": {"elements": elements}
    })
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
/// `attach: true` skips phase 1 and arms immediately, for runs that are
/// already in flight when the watcher spawns (the caller just answered
/// such a run's question in-chat — see `try_answer_pending_question`).
/// The run id is then unknown (`None`): any fresh `ProcessingCompleted`
/// finalizes the card and any new `ProcessingStarted` supersedes it.
/// The card itself — builders, PATCH payloads, dedup — is identical in
/// both modes.
///
/// Exits on `ProcessingCompleted` (final card), when a *different* run
/// starts on the topic (this run was cancelled — a cancelled run publishes
/// no event, so the next start is the only signal), when the bus is
/// dropped, or after `MAX_LIFETIME` (safety net). A PATCH failure only
/// skips the current tick — the card itself was already posted, so
/// transient API errors must not kill the watcher. The dedup entry is
/// released on every exit path.
///
/// Concurrent watchers of the same topic share one status message via the
/// `cards` registry: the first watcher to arm posts it and records the
/// message id; later watchers armed by the same run reuse that id instead
/// of posting a duplicate. An entry left by a *previous* run is never
/// reused: while waiting, a fresh `ProcessingCompleted` marks the previous
/// run as done, and any surviving entry is then stale (its owner's release
/// lags behind its final PATCH) — the new watcher posts a fresh message.
/// The entry is removed when the run finalizes (or the watcher exits), so
/// the next run posts a fresh message.
async fn release_topic_card(
    cards: &Arc<tokio::sync::Mutex<std::collections::HashMap<String, String>>>,
    topic: &str,
    message_id: &str,
) {
    // Only release our own entry: a concurrent watcher may have replaced
    // it (its final card would otherwise be orphaned for the next run).
    let mut g = cards.lock().await;
    if g.get(topic).is_some_and(|id| id == message_id) {
        g.remove(topic);
    }
}

/// Whether an arming watcher should reuse an existing dedup entry.
///
/// `None` → post a fresh status message. An entry left behind by the
/// previous run is only safe to reuse while that run is still live
/// (`prior_completed == false`): a fresh `ProcessingCompleted` already
/// arrived, so the previous run is over and its owner's release may not
/// have landed yet — a reused entry would resurrect that run's buried card
/// instead of posting a new one.
fn reuse_decision(existing: Option<&String>, prior_completed: bool) -> Option<String> {
    if prior_completed {
        return None;
    }
    existing.cloned()
}

// Args are built inline at the two pipe call sites, which already sit next
// to several local clones (same pattern as `spawn_feishu_adapter`).
#[allow(clippy::too_many_arguments)]
pub fn spawn_progress_watcher(
    feishu_client: Arc<FeishuClient>,
    topic_manager: Arc<TopicManager>,
    topic: String,
    chat_id: String,
    start: std::time::Instant,
    seen_after: chrono::DateTime<chrono::Utc>,
    cards: Arc<tokio::sync::Mutex<std::collections::HashMap<String, String>>>,
    attach: bool,
) {
    tokio::spawn(async move {
        const MAX_LIFETIME: std::time::Duration = std::time::Duration::from_secs(2 * 60 * 60);

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
        // Set once a fresh ProcessingCompleted arrives while waiting — the
        // previous run is over, so a dedup entry still in the registry is
        // stale (its owner's release lags behind its final PATCH) and must
        // not be reused.
        let mut prior_completed = false;
        // True only when this watcher posted the status message itself; a
        // watcher that reused another live watcher's card must not release
        // that watcher's dedup entry.
        let mut owns_entry = false;
        // Set by the arming ProcessingStarted (`None` in attach mode, where
        // the run is already in flight and its id is unknown); a *different*
        // run's start in Phase 2 means ours is over (see the superseded-exit
        // arm there).
        let run_message_id;
        let (status_message_id, run_message_id, mut last_text) = loop {
            if !attach {
                // ── Phase 1: wait for the first fresh ProcessingStarted ──
                //
                // `seen_after` is captured by the caller *before routing*, so
                // every event of this run is strictly newer and previous runs'
                // replayed events stay filtered. A fresh ProcessingCompleted
                // while waiting belongs to a *previous* run — ignored: this
                // message may still be queued behind it.
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
                if ev.timestamp() <= seen_after {
                    continue;
                }
                match ev {
                    TopicEvent::ProcessingCompleted { .. } => {
                        // A previous run's completion (this message may be
                        // queued behind it) — remember it for the dedup check.
                        prior_completed = true;
                        continue;
                    }
                    TopicEvent::ProcessingStarted { message_id, .. } => {
                        run_message_id = Some(message_id);
                    }
                    _ => continue,
                }
            } else {
                // Attach mode: the run is already in flight — no fresh
                // ProcessingStarted will come. Arm immediately; Phase 2's
                // completion and supersede arms (unfiltered by run id) take
                // it from here.
                run_message_id = None;
            }
            // Processing actually starts now — exclude the queue wait
            // from the elapsed time shown on the card.
            start = std::time::Instant::now();
            let display = topic_manager.topic_display_state(&topic).await;
            let text = progress_card("⏳ 处理中", start.elapsed().as_secs(), 0, None, &display);
            // Dedup: another watcher of this topic may already have posted
            // this run's status message (a dormant watcher armed by the
            // same ProcessingStarted). Reuse its message id instead of
            // posting a duplicate — but only while the previous run is
            // still live; an entry left by a completed run would resurrect
            // its buried card instead of showing a fresh one in the chat.
            // The lock is held across the send so two watchers arming
            // concurrently can't both create.
            let mut cards = cards.lock().await;
            if let Some(existing) = reuse_decision(cards.get(&topic), prior_completed) {
                break (
                    existing,
                    run_message_id,
                    progress_card_json(&text, None).to_string(),
                );
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
                    owns_entry = true;
                    break (
                        r.message_id,
                        run_message_id,
                        progress_card_json(&text, None).to_string(),
                    );
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
        let mut thinking_blocks: Vec<String> = Vec::new();
        let mut done: Option<(bool, u64)> = None;
        let mut last_patch = std::time::Instant::now();
        let mut patch_warned = false;
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
                            // (throttled at the agent loop): extend the
                            // current block, or start a new one per request,
                            // so the final card can embed the full history.
                            TopicEvent::Thinking { text, .. } => {
                                push_thinking_block(&mut thinking_blocks, text);
                            }
                            // A new run started on this topic: ours is over —
                            // a cancelled run publishes no event, so the next
                            // start is the only signal. Release now (owner
                            // only) so the new watcher posts a fresh card
                            // instead of reusing ours; the post-loop release
                            // is then an idempotent no-op.
                            TopicEvent::ProcessingStarted { message_id, .. }
                                if run_message_id.as_deref() != Some(message_id.as_str()) =>
                            {
                                if owns_entry {
                                    release_topic_card(&cards, &topic, &status_message_id).await;
                                }
                                break;
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
            let preview = thinking_blocks.last().map(String::as_str);
            let text = match done {
                Some((success, duration_secs)) => progress_card(
                    if success { "✅ 完成" } else { "❌ 失败" },
                    duration_secs,
                    tool_count,
                    last_activity.as_deref(),
                    &display,
                ),
                None => progress_card(
                    "⏳ 处理中",
                    start.elapsed().as_secs(),
                    tool_count,
                    last_activity.as_deref(),
                    &display,
                ),
            };
            let card = if terminal {
                final_card_json(&text, &thinking_blocks)
            } else {
                progress_card_json(&text, preview)
            };
            // Dedup on the serialized card: thinking lives in the panel
            // body now, so thinking-only updates must still trigger a PATCH.
            let card_str = card.to_string();
            if !terminal && card_str == last_text {
                continue;
            }
            // Bounded update: the event bus is shared with the agent loop,
            // so a hung Feishu PATCH must never stall this watcher.
            let upd = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                feishu_client.update_card_message(&status_message_id, &card),
            )
            .await;
            match upd {
                Ok(Ok(())) => {
                    patch_warned = false;
                    last_text = card_str;
                    last_patch = std::time::Instant::now();
                }
                Ok(Err(e)) => {
                    // Transient (throttle/network): skip this tick and keep
                    // watching. First failure is warn-level so a systematic
                    // problem (e.g. a rejected card schema) is visible
                    // without spamming one line per tick.
                    if patch_warned {
                        tracing::debug!(error = %e, topic = %topic,
                            "feishu progress watcher: status card update failed");
                    } else {
                        tracing::warn!(error = %e, topic = %topic,
                            "feishu progress watcher: status card update failed; \
                             further failures logged at debug");
                        patch_warned = true;
                    }
                }
                Err(_elapsed) => {
                    if patch_warned {
                        tracing::debug!(topic = %topic,
                            "feishu progress watcher: status card update timed out");
                    } else {
                        tracing::warn!(topic = %topic,
                            "feishu progress watcher: status card update timed out; \
                             further failures logged at debug");
                        patch_warned = true;
                    }
                }
            }
            if terminal {
                break;
            }
        }
        // Every exit path (final card, superseded by a new run, bus drop,
        // lifetime cap) releases the dedup entry so the next run posts a
        // fresh status message — a leaked entry would make later watchers
        // PATCH a stale, buried card. The superseded path also releases
        // inline above, before the new watcher's arm check; this call is
        // then an idempotent no-op. Only the owner releases: a watcher that
        // reused another live watcher's card shares its message id and must
        // keep the entry.
        if owns_entry {
            release_topic_card(&cards, &topic, &status_message_id).await;
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
            progress_card("⏳ 处理中", 12, 3, None, &none),
            "⏳ 处理中 · 12s · 工具 3"
        );
        assert_eq!(
            progress_card("⏳ 处理中", 12, 3, Some("edit — tools.rs"), &none),
            "⏳ 处理中 · 12s · 工具 3\n最近：edit — tools.rs"
        );
        assert_eq!(
            progress_card("✅ 完成", 52, 8, None, &none),
            "✅ 完成 · 52s · 工具 8"
        );
    }

    #[test]
    fn progress_card_json_folds_thinking_into_collapsed_panel() {
        let none = TopicDisplayState::default();
        let status = progress_card("⏳ 处理中", 12, 3, Some("read — a.rs"), &none);
        let card = progress_card_json(&status, Some("计划步骤一\n然后调用工具读取文件"));
        let elements = card["body"]["elements"].as_array().expect("elements");
        assert_eq!(elements.len(), 2, "card: {card}");
        assert_eq!(elements[0]["tag"], "markdown");
        assert_eq!(elements[0]["content"], status);
        let panel = &elements[1];
        assert_eq!(panel["tag"], "collapsible_panel");
        assert_eq!(panel["expanded"], false);
        assert_eq!(panel["header"]["title"]["content"], "💭 Thinking");
        assert_eq!(
            panel["elements"][0]["content"],
            "计划步骤一\n然后调用工具读取文件"
        );
    }

    #[test]
    fn progress_card_json_omits_panel_without_thinking() {
        let card = progress_card_json("⏳ 处理中 · 1s · 工具 0", None);
        assert_eq!(
            card["body"]["elements"].as_array().expect("elements").len(),
            1
        );
        // Whitespace-only thinking → no panel either.
        let card = progress_card_json("⏳ 处理中 · 1s · 工具 0", Some("  "));
        assert_eq!(
            card["body"]["elements"].as_array().expect("elements").len(),
            1
        );
    }

    #[test]
    fn progress_card_json_truncates_thinking_to_tail_chars() {
        let thinking = "a".repeat(THINKING_PREVIEW_CHARS + 500);
        let card = progress_card_json("⏳ 处理中 · 1s · 工具 0", Some(&thinking));
        let body = card["body"]["elements"][1]["elements"][0]["content"]
            .as_str()
            .expect("panel body");
        assert_eq!(body.chars().count(), THINKING_PREVIEW_CHARS);
    }

    #[test]
    fn progress_card_json_declares_update_multi_config() {
        // Regression: Feishu's update-card API rejects PATCH bodies that do
        // not declare `config.update_multi`, so the card JSON sent via
        // `update_card_message` must carry the full 2.0 envelope — same as
        // the initial card posted by `FeishuClient::build_card_content`.
        let card = progress_card_json("⏳ 处理中 · 1s · 工具 0", None);
        assert_eq!(card["schema"], "2.0");
        assert_eq!(card["config"]["update_multi"], true);
        let card = final_card_json("✅ 完成", &[]);
        assert_eq!(card["schema"], "2.0");
        assert_eq!(card["config"]["update_multi"], true);
    }

    #[test]
    fn reuse_decision_never_reuses_after_a_fresh_completion() {
        // Same-run concurrent watcher: entry exists, previous run still
        // live → share the card.
        assert_eq!(
            reuse_decision(Some(&"card".to_string()), false),
            Some("card".to_string())
        );
        // Previous run completed (cancel bursts: its release lags behind
        // its final PATCH) → post a fresh card even though the entry is
        // still present.
        assert_eq!(reuse_decision(Some(&"stale".to_string()), true), None);
        // No entry → post fresh.
        assert_eq!(reuse_decision(None, false), None);
        assert_eq!(reuse_decision(None, true), None);
    }

    #[tokio::test]
    async fn release_topic_card_only_removes_own_entry() {
        let cards: Arc<tokio::sync::Mutex<std::collections::HashMap<String, String>>> =
            Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
        cards.lock().await.insert("t".into(), "mine".into());
        // Another watcher's entry (different id) is untouched.
        release_topic_card(&cards, "t", "theirs").await;
        assert_eq!(
            cards.lock().await.get("t").map(String::as_str),
            Some("mine")
        );
        // Our own entry is removed.
        release_topic_card(&cards, "t", "mine").await;
        assert!(!cards.lock().await.contains_key("t"));
        // Releasing on an empty registry is a no-op.
        release_topic_card(&cards, "t", "mine").await;
    }

    #[test]
    fn tail_chars_keeps_last_chars_on_char_boundaries() {
        assert_eq!(tail_chars("hello", 3), "llo");
        assert_eq!(tail_chars("héllo", 3), "llo");
        assert_eq!(tail_chars("短", THINKING_PREVIEW_CHARS), "短");
        assert_eq!(tail_chars("hi", 0), "");
    }

    #[test]
    fn push_thinking_block_extends_or_appends() {
        let mut blocks = Vec::new();
        push_thinking_block(&mut blocks, "abc".to_string());
        // Same request: snapshot grows by extension → replace in place.
        push_thinking_block(&mut blocks, "abcdef".to_string());
        assert_eq!(blocks, vec!["abcdef".to_string()]);
        // New request: snapshot is not an extension → new block.
        push_thinking_block(&mut blocks, "new request".to_string());
        assert_eq!(blocks.len(), 2);
        // Identical re-publish (final flush) replaces, no new block.
        push_thinking_block(&mut blocks, "new request".to_string());
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn final_card_embeds_collapsed_thinking_panel() {
        let blocks = vec!["block one".to_string(), "block two".to_string()];
        let card = final_card_json("✅ 完成 · 52s · 工具 8", &blocks);
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(elements[0]["tag"].as_str().unwrap(), "markdown");
        assert_eq!(
            elements[0]["content"].as_str().unwrap(),
            "✅ 完成 · 52s · 工具 8"
        );
        let panel = &elements[1];
        assert_eq!(panel["tag"].as_str().unwrap(), "collapsible_panel");
        // Collapsed by default: expand reveals all rounds, TUI-style.
        assert!(!panel["expanded"].as_bool().unwrap());
        let content = panel["elements"][0]["content"].as_str().unwrap();
        assert!(content.contains("block one\n\n---\n\nblock two"));
    }

    #[test]
    fn final_card_without_thinking_has_no_panel() {
        let card = final_card_json("✅ 完成", &[]);
        assert_eq!(card["body"]["elements"].as_array().unwrap().len(), 1);
        // Whitespace-only thinking collapses to nothing.
        let card = final_card_json("✅ 完成", &["  ".to_string()]);
        assert_eq!(card["body"]["elements"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn final_card_truncates_long_thinking_tail() {
        let long = "x".repeat(FINAL_THINKING_PANEL_CHARS + 500);
        let card = final_card_json("✅ 完成", &[long]);
        let content = card["body"]["elements"][1]["elements"][0]["content"]
            .as_str()
            .unwrap();
        assert!(content.starts_with('…'));
        // Truncation marker plus the capped tail.
        assert!(content.len() < FINAL_THINKING_PANEL_CHARS + 100);
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
            progress_card("⏳ 处理中", 23, 2, None, &full),
            "⏳ 处理中 · 23s · 工具 2 · plan · kimi/k3-256k · 41%"
        );
        assert_eq!(
            progress_card("✅ 完成", 757, 24, None, &full),
            "✅ 完成 · 12m37s · 工具 24 · plan · kimi/k3-256k · 41%"
        );

        // Activity line still comes second when present.
        let with_activity = progress_card("⏳ 处理中", 23, 2, Some("bash — cargo check"), &full);
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
            progress_card("⏳ 处理中", 3, 0, None, &no_tokens),
            "⏳ 处理中 · 3s · 工具 0 · build · kimi/k3-256k"
        );

        // Zero max_tokens → pct hidden (division guard).
        let zero_max = TopicDisplayState {
            input_tokens: Some(100),
            max_tokens: Some(0),
            ..TopicDisplayState::default()
        };
        assert_eq!(
            progress_card("⏳ 处理中", 3, 0, None, &zero_max),
            "⏳ 处理中 · 3s · 工具 0"
        );
    }
}
