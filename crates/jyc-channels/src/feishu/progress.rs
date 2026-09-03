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

use jyc_core::topic_event::TopicEvent;
use jyc_core::topic_manager::TopicManager;

use super::client::FeishuClient;

/// Minimum seconds between Feishu status-card PATCHes (rate-limit guard).
const PROGRESS_PATCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(4);

/// Compact "what is happening now" summary for a tool call, extracted from
/// the `ToolStarted` event's JSON input. Used by the Feishu progress
/// watcher's live status card: `edit — service/tools.rs`, `bash — cargo ch…`
///
/// File paths are reduced to their basename (the Feishu card stays short);
/// commands/URLs/patterns are truncated. Tools without a meaningful
/// argument (or with sensitive args like the reply text) return `None`.
fn tool_activity(tool_name: &str, input: Option<&str>) -> Option<String> {
    let get = |key: &str| -> Option<String> {
        serde_json::from_str::<serde_json::Value>(input?)
            .ok()?
            .get(key)?
            .as_str()
            .map(str::to_string)
    };
    let raw = match tool_name {
        // File tools: basename keeps the card short (read_image names its
        // argument "path", the others "file_path").
        "read" | "edit" | "write" | "read_image" => {
            let p = get("file_path").or_else(|| get("path"))?;
            p.rsplit('/').next().unwrap_or(&p).to_string()
        }
        "grep" | "glob" => get("pattern")?,
        "bash" => get("command")?,
        "webfetch" => get("url")?,
        _ => return None,
    };
    const MAX: usize = 40;
    if raw.chars().count() > MAX {
        Some(raw.chars().take(MAX - 1).collect::<String>() + "…")
    } else {
        Some(raw)
    }
}

/// Build the status-card markdown for the Feishu progress watcher.
///
/// `state` is `"⏳ 处理中"` while running, `"✅ 完成"` on success,
/// `"❌ 失败"` on failure.
fn progress_card(
    state: &str,
    elapsed_secs: u64,
    tool_count: usize,
    activity: Option<&str>,
) -> String {
    let mut lines = vec![format!("{state} · {elapsed_secs}s · 工具 {tool_count}")];
    if let Some(a) = activity {
        lines.push(format!("最近：{a}"));
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
pub fn spawn_progress_watcher(
    feishu_client: Arc<FeishuClient>,
    topic_manager: Arc<TopicManager>,
    topic: String,
    chat_id: String,
    start: std::time::Instant,
    seen_after: chrono::DateTime<chrono::Utc>,
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
        let (status_message_id, mut last_text) = loop {
            if start.elapsed() > MAX_LIFETIME {
                return;
            }
            let ev = match rx.recv().await {
                Some(ev) => ev,
                None => return, // bus dropped
            };
            let is_start =
                ev.timestamp() > seen_after && matches!(ev, TopicEvent::ProcessingStarted { .. });
            if !is_start {
                continue;
            }
            let text = progress_card("⏳ 处理中", start.elapsed().as_secs(), 0, None);
            // Bounded send: the event bus is shared with the agent loop,
            // so a hung Feishu call must never stall this watcher.
            match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                feishu_client.send_text_message(&chat_id, &text),
            )
            .await
            {
                Ok(Ok(r)) => break (r.message_id, text),
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
        let mut done: Option<bool> = None;
        let mut final_text = String::new();
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
                                done = Some(success);
                                let state = if success { "✅ 完成" } else { "❌ 失败" };
                                final_text = progress_card(
                                    state,
                                    duration_secs,
                                    tool_count,
                                    last_activity.as_deref(),
                                );
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
            let text = if terminal {
                final_text.clone()
            } else {
                progress_card(
                    "⏳ 处理中",
                    start.elapsed().as_secs(),
                    tool_count,
                    last_activity.as_deref(),
                )
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
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_activity_extracts_basename_for_file_tools() {
        let input =
            r#"{"file_path": "/home/jiny/projects/jyc/crates/jyc-agent/src/service/tools.rs"}"#;
        assert_eq!(
            tool_activity("edit", Some(input)).as_deref(),
            Some("tools.rs")
        );
        assert_eq!(
            tool_activity("read", Some(input)).as_deref(),
            Some("tools.rs")
        );
        assert_eq!(
            tool_activity("write", Some(input)).as_deref(),
            Some("tools.rs")
        );
        // read_image names its argument "path", not "file_path".
        assert_eq!(
            tool_activity("read_image", Some(r#"{"path": "/tmp/x/photo.png"}"#)).as_deref(),
            Some("photo.png")
        );
    }

    #[test]
    fn tool_activity_extracts_command_pattern_url() {
        assert_eq!(
            tool_activity("bash", Some(r#"{"command": "cargo check -p jyc-cli"}"#)).as_deref(),
            Some("cargo check -p jyc-cli")
        );
        assert_eq!(
            tool_activity(
                "grep",
                Some(r#"{"pattern": "TopicEvent", "path": "crates"}"#)
            )
            .as_deref(),
            Some("TopicEvent")
        );
        assert_eq!(
            tool_activity(
                "webfetch",
                Some(r#"{"url": "https://open.feishu.cn/document/server-docs/im"}"#)
            )
            .as_deref(),
            Some("https://open.feishu.cn/document/server-…")
        );
    }

    #[test]
    fn tool_activity_truncates_long_values() {
        let long = "a".repeat(100);
        let input = format!(r#"{{"command": "{long}"}}"#);
        let got = tool_activity("bash", Some(&input)).unwrap();
        assert_eq!(got.chars().count(), 40);
        assert!(got.ends_with('…'));
    }

    #[test]
    fn tool_activity_none_for_unknown_or_missing_input() {
        assert!(tool_activity("jyc_reply_message", Some(r#"{"message": "hi"}"#)).is_none());
        assert!(tool_activity("read", None).is_none());
        // Malformed JSON input → None (never panics).
        assert!(tool_activity("edit", Some("not json")).is_none());
        // Valid JSON but missing the expected key → None.
        assert!(tool_activity("edit", Some(r#"{"other": 1}"#)).is_none());
    }

    #[test]
    fn progress_card_renders_state_elapsed_and_activity() {
        assert_eq!(
            progress_card("⏳ 处理中", 12, 3, None),
            "⏳ 处理中 · 12s · 工具 3"
        );
        assert_eq!(
            progress_card("⏳ 处理中", 12, 3, Some("edit — tools.rs")),
            "⏳ 处理中 · 12s · 工具 3\n最近：edit — tools.rs"
        );
        assert_eq!(
            progress_card("✅ 完成", 52, 8, None),
            "✅ 完成 · 52s · 工具 8"
        );
    }
}
