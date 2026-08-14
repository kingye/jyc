//! Activity/event tracking for the inspect server.
//!
//! Extracted from the monolithic `server.rs`.

use arc_swap::ArcSwap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use jyc_core::activity_log_store::ActivityLogStore;
use jyc_core::thread_event::ThreadEvent;
use jyc_core::thread_manager::ThreadManager;
use jyc_types::*;

use super::{MAX_ACTIVITY_ENTRIES, MAX_RECENT_MESSAGES, SharedActivityMap, seed_next_id_from_disk};

pub(crate) fn filter_chat_by_since(
    mut entries: Vec<ChatMessageEntry>,
    since: Option<&str>,
) -> Vec<ChatMessageEntry> {
    if let Some(since_ts) = since {
        entries.retain(|e| {
            e.timestamp
                .as_deref()
                .map(|t| t >= since_ts)
                .unwrap_or(false)
        });
    }
    entries
}

/// Publish an activity entry to the inspect-broadcast bus.
///
/// Payload format:
///   {"type":"activity","channel":"...","thread":"...","id":N,"entry":{...}}
fn publish_activity_event(
    bus: &tokio::sync::broadcast::Sender<String>,
    channel: &str,
    thread: &str,
    entry: &ActivityEntry,
) {
    let payload = serde_json::json!({
        "type": "activity",
        "channel": channel,
        "thread": thread,
        "id": entry.id,
        "entry": entry,
    });
    let _ = bus.send(payload.to_string());
}

/// Publish a chat message to the inspect-broadcast bus.
///
/// Payload format:
///   {"type":"chat_message","channel":"...","thread":"...","id":N,"entry":{...}}
fn publish_chat_message_event(
    bus: &tokio::sync::broadcast::Sender<String>,
    channel: &str,
    thread: &str,
    msg: &ChatMessageEntry,
) {
    let payload = serde_json::json!({
        "type": "chat_message",
        "channel": channel,
        "thread": thread,
        "id": msg.id,
        "entry": msg,
    });
    let _ = bus.send(payload.to_string());
}

/// Publish a thinking event to the inspect-broadcast bus.
///
/// Payload format:
///   {"type":"thinking","channel":"...","thread":"...","text":"..."}
fn publish_thinking_event(
    bus: &tokio::sync::broadcast::Sender<String>,
    channel: &str,
    thread: &str,
    text: &str,
) {
    let payload = serde_json::json!({
        "type": "thinking",
        "channel": channel,
        "thread": thread,
        "text": text,
    });
    let _ = bus.send(payload.to_string());
}

/// Publish a processing-status event to the inspect-broadcast bus.
///
/// Payload format:
///   {"type":"processing","channel":"...","thread":"...","is_processing":bool,"has_error":bool}
fn publish_processing_event(
    bus: &tokio::sync::broadcast::Sender<String>,
    channel: &str,
    thread: &str,
    is_processing: bool,
    has_error: bool,
) {
    let payload = serde_json::json!({
        "type": "processing",
        "channel": channel,
        "thread": thread,
        "is_processing": is_processing,
        "has_error": has_error,
    });
    let _ = bus.send(payload.to_string());
}

/// Broadcast a 1 Hz wall-clock elapsed tick to dashboard WS clients so
/// the chat pane, chat-mode info pane, and dashboard Details panel can
/// show a live duration indicator. The first tick fires at t=0 (see
/// `run_ticker` in the agent loop) so sub-second loops still emit one
/// event. Not persisted to `activity.jsonl` (handled upstream via
/// `is_internal`).
///
/// Payload format:
///   {"type":"loop_tick","channel":"...","thread":"...","elapsed_ms":u64}
fn publish_loop_tick_event(
    bus: &tokio::sync::broadcast::Sender<String>,
    channel: &str,
    thread: &str,
    elapsed_ms: u64,
) {
    let payload = serde_json::json!({
        "type": "loop_tick",
        "channel": channel,
        "thread": thread,
        "elapsed_ms": elapsed_ms,
    });
    let _ = bus.send(payload.to_string());
}
/// activity entries for the inspect server.
pub struct ActivityTracker;

impl ActivityTracker {
    /// Start tracking activity for all thread managers.
    /// Periodically discovers new threads and subscribes to their event buses.
    /// Persists activity entries to `.jyc/activity.jsonl` per thread.
    /// Fans out events to the inspect-broadcast bus for dashboard WS clients.
    /// On startup, loads historical activity from disk.
    pub fn start(
        thread_managers: Arc<ArcSwap<Vec<Arc<ThreadManager>>>>,
        activity_map: SharedActivityMap,
        _workspace_dirs: Arc<ArcSwap<Vec<PathBuf>>>,
        inspect_broadcast: Arc<tokio::sync::broadcast::Sender<String>>,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let subscribed: Arc<Mutex<HashSet<(String, String)>>> =
                Arc::new(Mutex::new(HashSet::new()));
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));

            // Load historical activity from disk for all existing threads
            let tms = thread_managers.load();
            for tm in tms.iter() {
                let channel = tm.channel_name().to_string();
                let threads = tm.list_threads().await;
                for thread in &threads {
                    let thread_path = thread.thread_path.clone();
                    if let Some(ref path) = thread_path
                        && let Ok(entries) =
                            ActivityLogStore::load_recent(path, MAX_ACTIVITY_ENTRIES)
                        && !entries.is_empty()
                    {
                        let mut map = activity_map.lock().await;
                        let state = map
                            .entry((channel.clone(), thread.name.clone()))
                            .or_default();
                        state.entries = entries.into_iter().collect();
                        state.is_processing = false;
                        if let Some(last) = state.entries.back()
                            && let Some(ref ts) = last.timestamp
                            && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts)
                        {
                            state.last_active_at = Some(dt.with_timezone(&chrono::Utc));
                        }
                    }
                }
            }
            drop(tms);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        // Discover new threads and subscribe to their event buses
                        let tms = thread_managers.load();
                        for tm in tms.iter() {
                            let channel = tm.channel_name().to_string();
                            let threads = tm.list_threads().await;
                            for thread in threads {
                                let key = (channel.clone(), thread.name.clone());
                                {
                                    let sub = subscribed.lock().await;
                                    if sub.contains(&key) {
                                        continue;
                                    }
                                }
                                // Try to get an existing event bus. If none exists but
                                // the thread has an active queue (worker running or
                                // pending messages), force-create one so we don't miss
                                // events. If no active queue, the thread is idle — clear
                                // any stale `is_processing` flag and mark as subscribed
                                // to avoid retrying every 2s.
                                let bus = match tm.get_event_bus(&thread.name).await {
                                    Some(b) => Some(b),
                                    None if tm.has_active_queue(&thread.name).await => {
                                        tracing::info!(
                                            thread = %thread.name,
                                            "Event bus missing but queue active, force-creating event bus"
                                        );
                                        tm.get_or_create_event_bus(&thread.name).await
                                    }
                                    None => {
                                        // Thread is idle (no active queue, no event bus).
                                        // Clear any stale processing state so the dashboard
                                        // doesn't get stuck showing "Processing" forever.
                                        // Do NOT insert into `subscribed` — that would
                                        // permanently exclude this thread from future checks,
                                        // so if the event bus is created just after this tick
                                        // (race with create_and_enqueue), the ActivityTracker
                                        // would never subscribe.
                                        let mut map = activity_map.lock().await;
                                        if let Some(state) = map.get_mut(&key) {
                                            state.is_processing = false;
                                        }
                                        drop(map);
                                        continue;
                                    }
                                };

                                if let Some(bus) = bus
                                    && let Ok(mut rx) = bus.subscribe().await {
                                        {
                                            let mut sub = subscribed.lock().await;
                                            sub.insert(key.clone());
                                        }
                                        let map = activity_map.clone();
                                        let name = thread.name.clone();
                                        let channel_for_task = channel.clone();
                                        let thread_path = thread.thread_path.clone();
                                        let cancel_inner = cancel.clone();
                                        let subscribed_clone = subscribed.clone();
                                        let key_clone = key.clone();
                                        let inspect_broadcast_for_task = inspect_broadcast.clone();
                                        tokio::spawn(async move {
                                            use futures_util::FutureExt;
                                            use std::panic::AssertUnwindSafe;

                                            let result = AssertUnwindSafe(async {
                                                loop {
                                                    tokio::select! {
                                                        event = rx.recv() => {
                                                            match event {
                                                                Some(event) => {
                                                                    let is_processing = matches!(
                                                                        &event,
                                                                        ThreadEvent::ProcessingStarted { .. }
                                                                        | ThreadEvent::ProcessingProgress { .. }
                                                                        | ThreadEvent::ToolStarted { .. }
                                                                        | ThreadEvent::LLMRequestStarted { .. }
                                                                    );
                                                                    let is_completed = matches!(
                                                                        &event,
                                                                        ThreadEvent::ProcessingCompleted { .. }
                                                                    );

                                                                    // Capture chat messages for live dashboard display
                                                                    let chat_msg: Option<ChatMessageEntry> = match &event {
                                                                        ThreadEvent::IncomingMessage { sender, text, timestamp, .. } => {
                                                                            Some(ChatMessageEntry {
                                                                                sender: sender.clone(),
                                                                                text: text.clone(),
                                                                                timestamp: Some(timestamp.to_rfc3339()),
                                                                                id: 0, // assigned below in the fanout step
                                                                            })
                                                                        }
                                                                        ThreadEvent::ReplySent { text, timestamp, .. } => {
                                                                            Some(ChatMessageEntry {
                                                                                sender: "ai".to_string(),
                                                                                text: text.clone(),
                                                                                timestamp: Some(timestamp.to_rfc3339()),
                                                                                id: 0, // assigned below in the fanout step
                                                                            })
                                                                        }
                                                                        _ => None,
                                                                    };

                                                                    let is_thinking =
                                                                        matches!(&event, ThreadEvent::Thinking { .. });

                                                                    // Thinking events are NOT persisted to
                                                                    // activity.jsonl or the activity buffer.
                                                                    // They update `thinking_text` instead
                                                                    // (displayed in the chat pane, not the
                                                                    // activity pane).
                                                                    if !is_thinking {
                                                                        let mut entry = event_to_activity(&event);
                                                                        let is_error = entry.severity == Severity::Error;
                                                                        // Internal events (ProcessingProgress heartbeats) are
                                                                        // debug-only: skip persisting to disk and skip the
                                                                        // in-memory log + WS broadcast so they don't flood the
                                                                        // activity pane / chat progress.
                                                                        let is_internal = entry.is_internal;
                                                                        let mut map = map.lock().await;
                                                                        let state = map
                                                                            .entry((channel_for_task.clone(), name.clone()))
                                                                            .or_default();
                                                                        seed_next_id_from_disk(state, thread_path.as_deref());
                                                                        if !is_internal {
                                                                            // Assign monotonic per-thread id BEFORE persisting to
                                                                            // disk and pushing to the in-memory buffer, so the log
                                                                            // carries the same ids the dashboard uses for dedup.
                                                                            entry.id = state.next_id;
                                                                            state.next_id = state.next_id.wrapping_add(1);
                                                                            if let Some(ref path) = thread_path
                                                                                && let Err(e) = ActivityLogStore::append(path, &entry)
                                                                            {
                                                                                tracing::warn!(error = %e, thread = %name, "Failed to persist activity entry");
                                                                            }
                                                                            state.entries.push_back(entry.clone());
                                                                            if state.entries.len() > MAX_ACTIVITY_ENTRIES {
                                                                                state.entries.pop_front();
                                                                            }
                                                                            // Fan out to the inspect-broadcast bus so dashboard
                                                                            // WebSocket clients receive live events.
                                                                            publish_activity_event(
                                                                                &inspect_broadcast_for_task,
                                                                                &channel_for_task,
                                                                                &name,
                                                                                &entry,
                                                                            );
                                                                        }

                                                                        if let Some(mut msg) = chat_msg {
                                                                            msg.id = state.next_id;
                                                                            state.next_id = state.next_id.wrapping_add(1);
                                                                            state.recent_messages.push_back(msg.clone());
                                                                            if state.recent_messages.len() > MAX_RECENT_MESSAGES {
                                                                                state.recent_messages.pop_front();
                                                                            }
                                                                            publish_chat_message_event(
                                                                                &inspect_broadcast_for_task,
                                                                                &channel_for_task,
                                                                                &name,
                                                                                &msg,
                                                                            );
                                                                        }
                                                                        // Clear thinking text only when a new processing
                                                                        // cycle starts or the current one completes.
                                                                        // Do NOT clear on ProcessingProgress heartbeats,
                                                                        // ToolStarted, or LLMRequestStarted — those are
                                                                        // mid-cycle events that should keep the thinking
                                                                        // display visible.
                                                                        if matches!(
                                                                            &event,
                                                                            ThreadEvent::ProcessingStarted { .. }
                                                                            | ThreadEvent::ProcessingCompleted { .. }
                                                                        ) {
                                                                            state.thinking_text = None;
                                                                        }
                                                                        state.last_active_at = Some(event.timestamp());
                                                                        if is_processing {
                                                                            state.is_processing = true;
                                                                            state.has_error = false;
                                                                        } else if is_completed {
                                                                            state.is_processing = false;
                                                                            }
                                                                        if is_error {
                                                                            state.has_error = true;
                                                                        }
                                                                        // Publish processing-status AFTER the state
                                                                        // update so that ProcessingCompleted sends
                                                                        // is_processing=false, not the stale true value.
                                                                        if matches!(
                                                                            &event,
                                                                            ThreadEvent::ProcessingStarted { .. }
                                                                            | ThreadEvent::ProcessingCompleted { .. }
                                                                        ) {
                                                                            publish_processing_event(
                                                                                &inspect_broadcast_for_task,
                                                                                &channel_for_task,
                                                                                &name,
                                                                                state.is_processing,
                                                                                state.has_error,
                                                                            );
                                                                        }
                                                                    } else {
                                                                        // Thinking event: update thinking_text and fan out.
                                                                        if let ThreadEvent::Thinking { ref text, .. } = event {
                                                                            let mut map = map.lock().await;
                                                                            let state = map
                                                                                .entry((channel_for_task.clone(), name.clone()))
                                                                                .or_default();
                                                                            state.thinking_text = Some(text.clone());
                                                                            state.last_active_at = Some(event.timestamp());
                                                                            publish_thinking_event(
                                                                                &inspect_broadcast_for_task,
                                                                                &channel_for_task,
                                                                                &name,
                                                                                text,
                                                                            );
                                                                        }
                                                                    }

                                                                    // Live-duration ticker: LoopTick is `is_internal` so it
                                                                    // was skipped above (no activity.jsonl write, no
                                                                    // activity-pane entry), but we still want to fan it
                                                                    // out over WS so the dashboard can render a live
                                                                    // "12.4s" indicator. LoopTick fires at 1 Hz (with
                                                                    // the first tick at t=0); the elapsed_ms value
                                                                    // on the variant is what we forward.
                                                                    if let ThreadEvent::LoopTick { elapsed_ms, .. } = &event {
                                                                        publish_loop_tick_event(
                                                                            &inspect_broadcast_for_task,
                                                                            &channel_for_task,
                                                                            &name,
                                                                            *elapsed_ms,
                                                                        );
                                                                    }
                                                                }
                                                                None => break,
                                                            }
                                                        }
                                                        _ = cancel_inner.cancelled() => break,
                                                    }
                                                }
                                            }).catch_unwind().await;

                                            // Always clean up subscribed on exit — whether normal
                                            // (event bus replaced, cancel) or panic. Without this,
                                            // the key stays in `subscribed` forever and the thread
                                            // is never re-subscribed, causing activity events to
                                            // silently stop appearing in the dashboard.
                                            let mut sub = subscribed_clone.lock().await;
                                            sub.remove(&key_clone);

                                            if let Err(panic) = result {
                                                tracing::error!(
                                                    thread = %name,
                                                    panic = ?panic,
                                                    "Activity tracker task panicked; will re-subscribe on next interval"
                                                );
                                            }
                                        });
                                    }
                            }
                        }
                    }
                    _ = cancel.cancelled() => break,
                }
            }
        })
    }
}

/// Whether a `ThreadEvent` should be marked internal (filtered from
/// user-facing surfaces like the activity pane and REST API).
pub(crate) fn is_event_internal(event: &ThreadEvent) -> bool {
    // ProcessingProgress heartbeats are emitted frequently during long
    // tool runs to indicate the agent is still working. They're useful
    // for debug logs but noisy in the UI - filter them.
    //
    // LoopTick is a 1 Hz wall-clock heartbeat that drives the dashboard's
    // live-duration ticker (with the first tick fired at t=0). Same
    // reasoning as ProcessingProgress: useful for the WS ticker
    // payload, not for the activity pane.
    matches!(
        event,
        ThreadEvent::ProcessingProgress { .. } | ThreadEvent::LoopTick { .. }
    )
}

/// Convert a ThreadEvent into a human-readable ActivityEntry.
pub(crate) fn event_to_activity(event: &ThreadEvent) -> ActivityEntry {
    let severity = match event {
        ThreadEvent::SessionStatus { status_type, .. } => match status_type.as_str() {
            "error" | "timeout" => Severity::Error,
            "retry" | "rate_limit" | "no_reply" => Severity::Warning,
            _ => Severity::Info,
        },
        ThreadEvent::ToolCompleted { success: false, .. } => Severity::Error,
        ThreadEvent::ProcessingCompleted { success: false, .. } => Severity::Error,
        _ => Severity::Info,
    };

    let text = match event {
        ThreadEvent::ProcessingStarted { .. } => "Processing started".to_string(),
        ThreadEvent::ProcessingProgress {
            elapsed_secs,
            activity,
            output_length,
            ..
        } => {
            format!("{activity} ({elapsed_secs}s, {output_length} chars)")
        }
        ThreadEvent::ProcessingCompleted {
            success,
            duration_secs,
            ..
        } => {
            if *success {
                format!("Completed ({duration_secs}s)")
            } else {
                format!("Failed ({duration_secs}s)")
            }
        }
        ThreadEvent::LLMRequestStarted { iteration, .. } => {
            format!("Thinking... (iteration {iteration})")
        }
        ThreadEvent::ToolStarted {
            tool_name, input, ..
        } => {
            if tool_name == "edit" {
                // Store the full edit data as JSON so consumers can render
                // differently: activity pane shows the JSON string as-is while
                // AI progress parses it and renders a full git diff.
                let parsed: Option<serde_json::Value> =
                    input.as_deref().and_then(|s| serde_json::from_str(s).ok());
                let file_path = parsed
                    .as_ref()
                    .and_then(|v| v.get("file_path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let old_str = parsed
                    .as_ref()
                    .and_then(|v| v.get("old_string"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let new_str = parsed
                    .as_ref()
                    .and_then(|v| v.get("new_string"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                serde_json::json!({
                    "type": "edit",
                    "file_path": file_path,
                    "old_string": old_str,
                    "new_string": new_str,
                })
                .to_string()
            } else if tool_name == "write" {
                // Store write data as JSON for multi-line rendering in AI progress.
                let parsed: Option<serde_json::Value> =
                    input.as_deref().and_then(|s| serde_json::from_str(s).ok());
                let file_path = parsed
                    .as_ref()
                    .and_then(|v| v.get("file_path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let content = parsed
                    .as_ref()
                    .and_then(|v| v.get("content"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                serde_json::json!({
                    "type": "write",
                    "file_path": file_path,
                    "content": content,
                })
                .to_string()
            } else {
                match input {
                    Some(inp) => format!("Tool: {tool_name} — {inp}"),
                    None => format!("Tool: {tool_name} (running)"),
                }
            }
        }
        ThreadEvent::ToolCompleted {
            tool_name,
            success,
            duration_secs,
            output,
            input,
            ..
        } => {
            if *success {
                if tool_name == "edit" {
                    // Store the full edit data as JSON so consumers can render
                    // differently: activity pane shows as-is, AI progress shows
                    // git diff.
                    let parsed: Option<serde_json::Value> =
                        input.as_deref().and_then(|s| serde_json::from_str(s).ok());
                    let file_path = parsed
                        .as_ref()
                        .and_then(|v| v.get("file_path"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let old_str = parsed
                        .as_ref()
                        .and_then(|v| v.get("old_string"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let new_str = parsed
                        .as_ref()
                        .and_then(|v| v.get("new_string"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    // Parse line number from the edit tool's output message
                    // (format: "Edited 'file' at line N: M replacement(s) made")
                    let line_no = output.as_deref().and_then(|s| {
                        s.find("at line ")
                            .and_then(|pos| {
                                let rest = &s[pos + 8..];
                                rest.find(':').map(|end| &rest[..end])
                            })
                            .and_then(|n| n.trim().parse::<usize>().ok())
                    });
                    serde_json::json!({
                        "type": "edit",
                        "file_path": file_path,
                        "line_no": line_no,
                        "old_string": old_str,
                        "new_string": new_str,
                        "duration_secs": duration_secs,
                    })
                    .to_string()
                } else if tool_name == "write" {
                    // Store write data as JSON for multi-line rendering in AI progress.
                    let parsed: Option<serde_json::Value> =
                        input.as_deref().and_then(|s| serde_json::from_str(s).ok());
                    let file_path = parsed
                        .as_ref()
                        .and_then(|v| v.get("file_path"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let content = parsed
                        .as_ref()
                        .and_then(|v| v.get("content"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    serde_json::json!({
                        "type": "write",
                        "file_path": file_path,
                        "content": content,
                        "duration_secs": duration_secs,
                    })
                    .to_string()
                } else {
                    match input {
                        Some(inp) => {
                            format!("Tool: {tool_name} (done, {duration_secs}s) — {inp}")
                        }
                        None => format!("Tool: {tool_name} (done, {duration_secs}s)"),
                    }
                }
            } else {
                match output {
                    Some(err) => {
                        let oneline = err.replace('\n', " ");
                        format!("Tool: {tool_name} (FAILED, {duration_secs}s) {oneline}")
                    }
                    None => format!("Tool: {tool_name} (FAILED, {duration_secs}s)"),
                }
            }
        }
        ThreadEvent::Thinking {
            text, full_length, ..
        } => {
            if *full_length > text.len() {
                format!("Thinking: {text}...")
            } else {
                format!("Thinking: {text}")
            }
        }
        ThreadEvent::IncomingMessage { sender, text, .. } => {
            let oneline = text.replace('\n', " ");
            format!("Message from {sender}: {oneline}")
        }
        ThreadEvent::ReplySent { text, .. } => {
            let oneline = text.replace('\n', " ");
            let preview: String = oneline.chars().take(100).collect();
            format!("Reply sent: {preview}")
        }
        ThreadEvent::SessionStatus {
            status_type,
            attempt,
            message,
            ..
        } => {
            let label = match status_type.as_str() {
                "retry" => "RETRY",
                "error" => "ERROR",
                "rate_limit" => "RATE LIMITED",
                "timeout" => "TIMEOUT",
                "no_reply" => "NO REPLY",
                other => other,
            };
            let mut text = match attempt {
                Some(n) => format!("{label} (attempt #{n})"),
                None => label.to_string(),
            };
            if let Some(msg) = message {
                let oneline = msg.replace('\n', " ");
                text.push_str(&format!(": {oneline}"));
            }
            text
        }
        // LoopTick is `is_internal` so this match arm should never run in
        // practice, but the match must be exhaustive. Format it as a
        // short debug string so a future regression doesn't produce a
        // confusing fall-through.
        ThreadEvent::LoopTick { elapsed_ms, .. } => {
            format!("LoopTick ({elapsed_ms}ms)")
        }
    };
    ActivityEntry {
        text,
        timestamp: Some(event.timestamp().to_rfc3339()),
        severity,
        id: 0, // assigned by ActivityTracker on push (see fanout step)
        is_internal: is_event_internal(event),
    }
}
