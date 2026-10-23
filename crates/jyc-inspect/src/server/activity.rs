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
use jyc_core::topic_event::TopicEvent;
use jyc_core::topic_manager::TopicManagers;
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
///   {"type":"activity","channel":"...","topic":"...","id":N,"entry":{...}}
fn publish_activity_event(
    bus: &tokio::sync::broadcast::Sender<String>,
    channel: &str,
    topic: &str,
    entry: &ActivityEntry,
) {
    let payload = serde_json::json!({
        "type": "activity",
        "channel": channel,
        "topic": topic,
        "id": entry.id,
        "entry": entry,
    });
    let _ = bus.send(payload.to_string());
}

/// Publish a raw typed topic event to the inspect-broadcast bus.
///
/// Payload format:
///   {"type":"topic_event","channel":"...","topic":"...","event":{...}}
///
/// Unlike the dashboard-oriented `activity` / `thinking` / `processing`
/// frames, this carries the complete serialized `TopicEvent` — consumed by
/// external pipe processes (`jyc-pipe`) for status cards and progress
/// display. Published for every event except the 1 Hz `LoopTick`
/// heartbeat (dashboard-only). Existing dashboard clients ignore the
/// unknown frame type.
fn publish_topic_event(
    bus: &tokio::sync::broadcast::Sender<String>,
    channel: &str,
    topic: &str,
    event: &jyc_core::topic_event::TopicEvent,
) {
    let payload = serde_json::json!({
        "type": "topic_event",
        "channel": channel,
        "topic": topic,
        "event": event,
    });
    let _ = bus.send(payload.to_string());
}

/// Publish a chat message to the inspect-broadcast bus.
///
/// Payload format:
///   {"type":"chat_message","channel":"...","topic":"...","id":N,"entry":{...}}
fn publish_chat_message_event(
    bus: &tokio::sync::broadcast::Sender<String>,
    channel: &str,
    topic: &str,
    msg: &ChatMessageEntry,
) {
    let payload = serde_json::json!({
        "type": "chat_message",
        "channel": channel,
        "topic": topic,
        "id": msg.id,
        "entry": msg,
    });
    let _ = bus.send(payload.to_string());
}

/// Publish a thinking event to the inspect-broadcast bus.
///
/// Payload format:
///   {"type":"thinking","channel":"...","topic":"...","text":"..."}
fn publish_thinking_event(
    bus: &tokio::sync::broadcast::Sender<String>,
    channel: &str,
    topic: &str,
    text: &str,
) {
    let payload = serde_json::json!({
        "type": "thinking",
        "channel": channel,
        "topic": topic,
        "text": text,
    });
    let _ = bus.send(payload.to_string());
}

/// Publish a processing-status event to the inspect-broadcast bus.
///
/// Payload format:
///   {"type":"processing","channel":"...","topic":"...","is_processing":bool,"has_error":bool}
fn publish_processing_event(
    bus: &tokio::sync::broadcast::Sender<String>,
    channel: &str,
    topic: &str,
    is_processing: bool,
    has_error: bool,
) {
    let payload = serde_json::json!({
        "type": "processing",
        "channel": channel,
        "topic": topic,
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
///   {"type":"loop_tick","channel":"...","topic":"...","elapsed_ms":u64}
fn publish_loop_tick_event(
    bus: &tokio::sync::broadcast::Sender<String>,
    channel: &str,
    topic: &str,
    elapsed_ms: u64,
) {
    let payload = serde_json::json!({
        "type": "loop_tick",
        "channel": channel,
        "topic": topic,
        "elapsed_ms": elapsed_ms,
    });
    let _ = bus.send(payload.to_string());
}
/// activity entries for the inspect server.
pub struct ActivityTracker;

impl ActivityTracker {
    /// Start tracking activity for all topic managers.
    /// Periodically discovers new topics and subscribes to their event buses.
    /// Persists activity entries to `.jyc/activity.jsonl` per topic.
    /// Fans out events to the inspect-broadcast bus for dashboard WS clients.
    /// On startup, loads historical activity from disk.
    pub fn start(
        topic_managers: Arc<TopicManagers>,
        activity_map: SharedActivityMap,
        _workspace_dirs: Arc<ArcSwap<Vec<PathBuf>>>,
        inspect_broadcast: Arc<tokio::sync::broadcast::Sender<String>>,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let subscribed: Arc<Mutex<HashSet<(String, String)>>> =
                Arc::new(Mutex::new(HashSet::new()));
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));

            // Load historical activity from disk for all existing topics
            let tms = topic_managers.all();
            for tm in tms.iter() {
                let channel = tm.channel_name().to_string();
                for (name, path) in tm.list_topic_names_and_paths().await {
                    if let Ok(entries) =
                        ActivityLogStore::load_recent(&name, &path, MAX_ACTIVITY_ENTRIES)
                        && !entries.is_empty()
                    {
                        let mut map = activity_map.lock().await;
                        let state = map.entry((channel.clone(), name.clone())).or_default();
                        state.entries = entries.into_iter().collect();
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
                        // Discover new topics and subscribe to their event buses
                        let tms = topic_managers.all();
                        for tm in tms.iter() {
                            let channel = tm.channel_name().to_string();
                            for (name, topic_path) in tm.list_topic_names_and_paths().await {
                                let key = (channel.clone(), name.clone());
                                {
                                    let sub = subscribed.lock().await;
                                    if sub.contains(&key) {
                                        continue;
                                    }
                                }
                                // Try to get an existing event bus. If none exists but
                                // the topic has an active queue (worker running or
                                // pending messages), force-create one so we don't miss
                                // events. If no active queue, the topic is idle — clear
                                // any stale `is_processing` flag and mark as subscribed
                                // to avoid retrying every 2s.
                                let bus = match tm.get_event_bus(&name).await {
                                    Some(b) => Some(b),
                                    None if tm.has_active_queue(&name).await => {
                                        tracing::info!(
                                            topic = %name,
                                            "Event bus missing but queue active, force-creating event bus"
                                        );
                                        tm.get_or_create_event_bus(&name).await
                                    }
                                    None => {
                                        // Topic is idle (no active queue, no event bus).
                                        // Do NOT insert into `subscribed` — that would
                                        // permanently exclude this topic from future checks,
                                        // so if the event bus is created just after this tick
                                        // (race with create_and_enqueue), the ActivityTracker
                                        // would never subscribe.
                                        //
                                        // The topic's processing flag is not touched here:
                                        // it lives in the runtime's live status (the one
                                        // source of truth for it), which the turn itself
                                        // closes.
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
                                        let subscribed_for_task = subscribed.clone();
                                        let channel_for_task = channel.clone();
                                        let cancel_inner = cancel.clone();
                                        let inspect_broadcast_for_task = inspect_broadcast.clone();
                                        // The topic's manager — it holds the live
                                        // status the processing frames report.
                                        let tm_for_task = tm.clone();
                                        tokio::spawn(async move {
                                            loop {
                                                tokio::select! {
                                                    event = rx.recv() => {
                                                        match event {
                                                            Some(event) => {
                                                                // Fan the raw typed event out
                                                                // for external pipe processes
                                                                // before any dashboard-oriented
                                                                // branching below. The 1 Hz
                                                                // LoopTick heartbeat is
                                                                // dashboard-only (it drives
                                                                // the live-duration ticker)
                                                                // and is excluded, mirroring
                                                                // the persistence path which
                                                                // marks it internal.
                                                                if !matches!(event, TopicEvent::LoopTick { .. }) {
                                                                    publish_topic_event(
                                                                        &inspect_broadcast_for_task,
                                                                        &channel_for_task,
                                                                        &name,
                                                                        &event,
                                                                    );
                                                                }

                                                                // Capture chat messages for live dashboard display
                                                                let chat_msg: Option<ChatMessageEntry> = match &event {
                                                                    TopicEvent::IncomingMessage { id, sender, text, client_msg_id, timestamp, .. } => {
                                                                        Some(ChatMessageEntry {
                                                                            sender: sender.clone(),
                                                                            text: text.clone(),
                                                                            timestamp: Some(timestamp.to_rfc3339()),
                                                                            // Allocated where the message entered the
                                                                            // system; the chat log stores the same number,
                                                                            // so clients dedup on it alone.
                                                                            id: *id,
                                                                            client_msg_id: client_msg_id.clone(),
                                                                        })
                                                                    }
                                                                    TopicEvent::ReplySent { id, text, timestamp, .. } => {
                                                                        Some(ChatMessageEntry {
                                                                            sender: "ai".to_string(),
                                                                            text: text.clone(),
                                                                            timestamp: Some(timestamp.to_rfc3339()),
                                                                            id: *id,
                                                                            client_msg_id: None,
                                                                        })
                                                                    }
                                                                    _ => None,
                                                                };

                                                                let is_thinking =
                                                                    matches!(&event, TopicEvent::Thinking { .. });

                                                                // Thinking events are NOT persisted to
                                                                // activity.jsonl or the activity buffer.
                                                                // They update `thinking_text` instead
                                                                // (displayed in the chat pane, not the
                                                                // activity pane).
                                                                if !is_thinking {
                                                                    let mut entry = event_to_activity(&event);
                                                                    // Internal events (ProcessingProgress heartbeats) are
                                                                    // debug-only: skip persisting to disk and skip the
                                                                    // in-memory log + WS broadcast so they don't flood the
                                                                    // activity pane / chat progress.
                                                                    let is_internal = is_event_internal(&event);
                                                                    let mut map = map.lock().await;
                                                                    let state = map
                                                                        .entry((channel_for_task.clone(), name.clone()))
                                                                        .or_default();
                                                                    seed_next_id_from_disk(state, &name, Some(topic_path.as_path()));
                                                                    if !is_internal {
                                                                        // Assign monotonic per-topic id BEFORE persisting to
                                                                        // disk and pushing to the in-memory buffer, so the log
                                                                        // carries the same ids the dashboard uses for dedup.
                                                                        entry.id = state.next_id;
                                                                        state.next_id = state.next_id.wrapping_add(1);
                                                                        if let Err(e) = ActivityLogStore::append(&name, &topic_path, &entry)
                                                                        {
                                                                            tracing::warn!(error = %e, topic = %name, "Failed to persist activity entry");
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

                                                                    if let Some(msg) = chat_msg {
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
                                                                        TopicEvent::ProcessingStarted { .. }
                                                                        | TopicEvent::ProcessingCompleted { .. }
                                                                    ) {
                                                                        state.thinking_text = None;
                                                                    }
                                                                    state.last_active_at = Some(event.timestamp());
                                                                    // The topic's processing state is not tracked here: it lives in
                                                                    // the runtime's live status — the one source of truth for it, the
                                                                    // same value `/info` and the dashboard's topic table report. The
                                                                    // turn that published this event wrote it before the event went
                                                                    // out, so reading it now gives the state the event describes:
                                                                    // ProcessingCompleted sends is_processing=false, not the stale
                                                                    // true value.
                                                                    if matches!(
                                                                        &event,
                                                                        TopicEvent::ProcessingStarted { .. }
                                                                        | TopicEvent::ProcessingCompleted { .. }
                                                                    ) {
                                                                        let live = tm_for_task.topic_live_status(&name);
                                                                        publish_processing_event(
                                                                            &inspect_broadcast_for_task,
                                                                            &channel_for_task,
                                                                            &name,
                                                                            live.is_processing,
                                                                            live.has_error,
                                                                        );
                                                                    }
                                                                } else {
                                                                    // Thinking event: update thinking_text and fan out.
                                                                    // (Accumulation across the turn is a
                                                                    // display concern, handled client-side.)
                                                                    if let TopicEvent::Thinking { ref text, .. } = event {
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
                                                                if let TopicEvent::LoopTick { elapsed_ms, .. } = &event {
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

                                            // Clean up subscribed on exit (event bus replaced,
                                            // cancel). Without this the key stays in `subscribed`
                                            // forever and the topic is never re-subscribed, causing
                                            // activity events to silently stop appearing in the
                                            // dashboard. A panic does not reach here: the release
                                            // profile aborts the process instead of unwinding.
                                            let mut sub = subscribed_for_task.lock().await;
                                            sub.remove(&key);
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

/// Whether a `TopicEvent` should be marked internal (filtered from
/// user-facing surfaces like the activity pane and REST API).
pub(crate) fn is_event_internal(event: &TopicEvent) -> bool {
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
        TopicEvent::ProcessingProgress { .. } | TopicEvent::LoopTick { .. }
    )
}

/// Convert a TopicEvent into an ActivityEntry that carries the raw event.
/// Rendering (prose, severity, visibility) is each client's job.
pub(crate) fn event_to_activity(event: &TopicEvent) -> ActivityEntry {
    ActivityEntry {
        event: event.to_activity_json(),
        timestamp: Some(event.timestamp().to_rfc3339()),
        id: 0, // assigned by ActivityTracker on push (see fanout step)
        text: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn test_publish_topic_event_frame_shape() {
        // Locks the wire format consumed by external pipe processes
        // (jyc-pipe): {"type":"topic_event","channel","topic","event"}.
        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        let event = jyc_core::topic_event::TopicEvent::ProcessingStarted {
            topic_name: "dev-jyc".to_string(),
            message_id: "m1".to_string(),
            timestamp: Utc::now(),
        };
        publish_topic_event(&tx, "agents", "dev-jyc", &event);
        let frame: serde_json::Value =
            serde_json::from_str(&rx.try_recv().expect("frame should be published")).unwrap();
        assert_eq!(frame["type"], "topic_event");
        assert_eq!(frame["channel"], "agents");
        assert_eq!(frame["topic"], "dev-jyc");
        // TopicEvent serializes as an externally-tagged enum: the variant
        // name is the single key of `event` (jyc-pipe extracts it this
        // way — keep this shape locked).
        let event_obj = frame["event"]
            .as_object()
            .expect("event should be an object");
        assert_eq!(
            event_obj.keys().next().map(String::as_str),
            Some("ProcessingStarted")
        );
        assert_eq!(frame["event"]["ProcessingStarted"]["topic_name"], "dev-jyc");
        assert_eq!(frame["event"]["ProcessingStarted"]["message_id"], "m1");
    }
}
