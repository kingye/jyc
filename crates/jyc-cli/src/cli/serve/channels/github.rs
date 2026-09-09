//! `github` channel adapter wiring (extracted from serve/channels.rs).

use anyhow::Context;
use anyhow::Result;
use jyc_channels::github::inbound::GithubMatcher;
use jyc_core::topic_manager::TopicManager;
use jyc_types::{ChannelConfig, InboundAdapter, InboundAttachmentConfig};
use serde_json;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

/// Spawn a pipe-only GitHub adapter: the poller inbound adapter plus one
/// reply forwarder per distinct pipe target channel.
///
/// Mirrors spawn_email_adapter. Unlike full channels, owns no
/// TopicManager/agent/orchestrator — all topics live in the pipe target
/// (hub) channel. Keeps a GithubInboundAdapter under
/// <workdir>/channels/<channel>/.github/ for dedup/cursor state.
///
/// Differences from the old full-channel architecture:
/// - No template/metadata injection (initialization is a skill on the agent side).
/// - No shared-repo directory grouping (that feature was removed).
/// - Comments carry the [Role] prefix (GPT summarizer, GitHub reviewer, etc.)
///   but no model/mode/token footer.
/// - Close events (issue/PR closed) use the hub registry's TopicManager to
///   close the routed topics in the hub workspace.
#[allow(clippy::too_many_arguments)]
use super::*;

pub(crate) fn spawn_github_adapter(
    channel_config: &ChannelConfig,
    channel_name: String,
    workdir: &Path,
    inbound_attachment_config: Option<InboundAttachmentConfig>,
    cancel: CancellationToken,
    tasks: &mut Vec<JoinHandle<()>>,
    config_for_spawn: std::sync::Arc<arc_swap::ArcSwap<jyc_types::AppConfig>>,
    ws_broadcasts: std::sync::Arc<std::sync::Mutex<HashMap<String, broadcast::Sender<String>>>>,
    routers: HubRegistry,
) -> Result<()> {
    use jyc_channels::github::inbound::GithubInboundAdapter;

    let github_config = channel_config
        .github
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing github config"))?
        .clone();

    // Pipe-only validation: every enabled pattern must have a pipe target.
    let pipe_channels =
        collect_pipe_target_channels(channel_config.patterns.as_deref().unwrap_or(&[]));
    warn_on_bad_pipe_patterns("github", &channel_name, channel_config);

    // State (dedup, cursor) lives under <workdir>/channels/<channel>/.github/.
    // One-time rename migration from the old location.
    let old_state_dir = workdir.join(&channel_name).join(".github");
    let new_state_dir = workdir.join("channels").join(&channel_name).join(".github");
    if old_state_dir.exists() && !new_state_dir.exists() {
        if let Some(parent) = new_state_dir.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::rename(&old_state_dir, &new_state_dir) {
            tracing::warn!(
                from = %old_state_dir.display(),
                to = %new_state_dir.display(),
                error = %e,
                "github state dir migration failed (dedup will start fresh)"
            );
        }
    }

    // Build the client before spawning: an unusable token (invalid header bytes)
    // must fail startup, not panic a detached task and leave a silently dead channel.
    let client = Arc::new(
        jyc_channels::github::client::GithubClient::new(&github_config)
            .with_context(|| format!("github client for channel '{channel_name}'"))?,
    );

    let channel_span = tracing::info_span!("in", ch = %channel_name);
    let workdir_for_task = workdir.to_path_buf();
    let channel_name_for_task = channel_name.clone();
    let cancel_child = cancel.child_token();

    let task = tokio::spawn(
        async move {
            // Shared topic -> reply state for pipe relaying.
            let topic_state: std::sync::Arc<
                std::sync::Mutex<HashMap<String, (u64, String) /* number, role */>>,
            > = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));

            // One reply forwarder per distinct pipe target channel.
            for channel in &pipe_channels {
                let ws_broadcasts = ws_broadcasts.clone();
                let topic_state = topic_state.clone();
                let client = client.clone();
                let channel = channel.clone();
                tokio::spawn(async move {
                    let Some(broadcast_tx) = wait_for_broadcast(&ws_broadcasts, &channel).await
                    else {
                        tracing::error!(
                            channel = %channel,
                            "github pipe: target channel broadcast never appeared (is it a websocket channel?), reply forwarder not started"
                        );
                        return;
                    };
                    let mut rx = broadcast_tx.subscribe();
                    tracing::info!(channel = %channel, "github pipe reply forwarder subscribed");
                    while let Ok(payload) = rx.recv().await {
                        let v: serde_json::Value = match serde_json::from_str(&payload) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        if v.get("type").and_then(|t| t.as_str()) != Some("reply") {
                            continue;
                        }
                        let (Some(topic), Some(text)) = (
                            v.get("topic").and_then(|t| t.as_str()),
                            v.get("text").and_then(|t| t.as_str()),
                        ) else {
                            continue;
                        };
                        let Some((number, role)) =
                            topic_state.lock().unwrap().get(topic).cloned()
                        else {
                            tracing::debug!(
                                topic = %topic,
                                "github pipe: no number mapping for reply, skipping"
                            );
                            continue;
                        };

                        // Build comment body: [Role] prefix, no footer
                        let body = role_prefixed_body(text, &role);

                        // Post comment via GitHub API (attachments not supported)
                        if let Err(e) = client.create_comment(number, &body).await {
                            tracing::error!(
                                error = format!("{e:#}"),
                                topic = %topic,
                                number = number,
                                "github pipe: failed to relay reply"
                            );
                        }
                    }
                });
            }

            // Hub TopicManager lookup for close events.
            let routers = routers.clone();

            // Inbound adapter: poller + pattern matching + pipe retarget.
            // Passing <workdir>/channels makes the adapter compute its
            // state_dir as <workdir>/channels/<channel>/.github (same
            // convention as the email adapter's StateManager).
            let adapter = GithubInboundAdapter::new(
                &github_config,
                channel_name_for_task.clone(),
                &workdir_for_task.join("channels"),
                Some(config_for_spawn.clone()),
            );
            // The close handler needs its own handles (on_message takes ownership).
            let topic_state_for_close = topic_state.clone();
            let routers_for_close = routers.clone();
            let config_for_close = config_for_spawn.clone();
            let channel_name_for_close = channel_name_for_task.clone();
            let repo_for_close = github_config.repo.clone();
            let options = jyc_types::InboundAdapterOptions {
                on_message: Box::new(move |message| {
                    let config_for_pipe = config_for_spawn.clone();
                    let topic_state = topic_state.clone();
                    let channel_name_self = channel_name_for_task.clone();
                    let routers = routers.clone();
                    tokio::spawn(async move {
                        let patterns = config_for_pipe
                            .load()
                            .channels
                            .get(&channel_name_self)
                            .and_then(|c| c.patterns.clone())
                            .unwrap_or_default();
                        let Some((_pm, pattern)) =
                            match_pipe("github", &GithubMatcher, &message, &patterns)
                        else {
                            return;
                        };
                        let pipe = pattern
                            .pipe
                            .as_ref()
                            .expect("match_pipe guarantees a pipe target");

                        // Capture number and role for reply routing. Without a
                        // number a reply could not be addressed, so drop loudly
                        // rather than commenting on issue #0.
                        let Some(number) = message
                            .metadata
                            .get("github_number")
                            .and_then(|v| v.as_u64())
                        else {
                            tracing::warn!(
                                message_id = %message.id,
                                "github: message has no github_number metadata, dropping"
                            );
                            return;
                        };
                        let role = pattern.role.as_deref().unwrap_or("").to_string();

                        // Re-target into the target channel/topic.
                        let Some(message) = retarget_or_drop("github", message, pipe) else {
                            return;
                        };

                        // Record resolved topic -> (number, role).
                        topic_state
                            .lock()
                            .unwrap()
                            .insert(message.topic.clone(), (number, role));

                        // Route through the target's own MessageRouter.
                        route_into_pipe_target("github", &routers, pipe, message).await;
                    });
                    Ok(())
                }),
                on_topic_close: None,
                on_close_event: Some(Box::new(move |number: u64, github_type: &str| {
                    let topic_state = topic_state_for_close.clone();
                    let routers = routers_for_close.clone();
                    // Derive the routed topics from config (restart-proof) and
                    // union with whatever this process actually routed (covers
                    // topics whose template changed since routing).
                    let patterns = config_for_close
                        .load()
                        .channels
                        .get(&channel_name_for_close)
                        .and_then(|c| c.patterns.clone())
                        .unwrap_or_default();
                    let mut targets =
                        close_event_topics(&patterns, number, github_type, &repo_for_close);
                    let github_type = github_type.to_string();
                    tokio::spawn(async move {
                        // Collect out of the std mutexes before awaiting:
                        // their guards are not Send.
                        {
                            let state = topic_state.lock().unwrap();
                            for topic in state
                                .iter()
                                .filter(|(_, v)| v.0 == number)
                                .map(|(t, _)| t.clone())
                            {
                                if !targets.iter().any(|(t, _)| *t == topic) {
                                    // Hub unknown for remembered topics: try every hub.
                                    targets.push((topic, String::new()));
                                }
                            }
                        }
                        if targets.is_empty() {
                            tracing::info!(
                                number = number,
                                github_type = %github_type,
                                "github pipe: close event resolved no topics (no pipe pattern with a number-dependent topic template)"
                            );
                            return;
                        }
                        let hubs: Vec<(String, Arc<TopicManager>)> = {
                            let reg = routers.lock().unwrap();
                            reg.iter()
                                .map(|(name, (_, tm))| (name.clone(), tm.clone()))
                                .collect()
                        };
                        for (topic, target_hub) in &targets {
                            for (hub_name, tm) in &hubs {
                                if !target_hub.is_empty() && hub_name != target_hub {
                                    continue;
                                }
                                if let Err(e) = tm.auto_close_topic(topic).await {
                                    tracing::debug!(
                                        hub = %hub_name,
                                        topic = %topic,
                                        number = number,
                                        error = %e,
                                        "github pipe: auto_close_topic ignored (no such topic in this hub)"
                                    );
                                }
                            }
                        }
                        topic_state.lock().unwrap().retain(|_, v| v.0 != number);
                    });
                })),
                on_error: Box::new(|error| {
                    tracing::error!(error = %error, "GitHub inbound error");
                }),
                attachment_config: inbound_attachment_config.clone(),
            };

            if let Err(e) = adapter.start(options, cancel_child).await {
                tracing::error!(error = %e, "GitHub inbound adapter error");
            }
        }
        .instrument(channel_span),
    );
    tasks.push(task);
    Ok(())
}
