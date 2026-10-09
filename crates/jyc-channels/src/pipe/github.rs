//! GitHub pipe wiring: the github poller adapter running inside `jyc-pipe`,
//! replacing the deleted in-process `spawn_github_adapter`
//! (`jyc-cli/src/cli/serve/channels/github.rs`) over the hub websocket.
//!
//! - Inbound: `GithubInboundAdapter` (poller + dedup state under
//!   `<workdir>/channels/<channel>/.github/`) → pattern match → retarget →
//!   `message` frame on the target channel's [`HubPipe`].
//! - Replies: the pipe's reply stream → `[Role]`-prefixed comment via
//!   `GithubClient::create_comment`.
//! - Close events (issue/PR closed): `close_topic` frame to every
//!   connected hub pipe whose patterns resolve a topic for the number.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use jyc_types::{ChannelConfig, InboundAdapter};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::github::client::GithubClient;
use crate::github::inbound::{GithubInboundAdapter, GithubMatcher};
use crate::pipe::{
    HubPipe, close_event_topics, collect_pipe_target_channels, match_pipe, retarget_or_drop,
    role_prefixed_body, warn_on_bad_pipe_patterns,
};

/// Spawn the github pipe wiring: the poller inbound adapter plus one
/// reply forwarder per distinct pipe target channel.
///
/// Mirrors `pipe::feishu::spawn_feishu_pipe`. Unlike full channels, owns
/// no TopicManager/agent/orchestrator — all topics live in the pipe target
/// (hub) channel. Keeps the adapter state under
/// `<workdir>/channels/<channel>/.github/` for dedup/cursor continuity.
pub(crate) fn spawn_github_pipe(
    channel_config: &ChannelConfig,
    channel_name: String,
    config: Arc<jyc_types::AppConfig>,
    hubs: Arc<HashMap<String, Arc<HubPipe>>>,
    workdir: &Path,
    cancel: CancellationToken,
    tasks: &mut Vec<JoinHandle<()>>,
) -> Result<()> {
    let github_config = channel_config
        .github
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing github config"))?
        .clone();

    // Pipe-only validation: every enabled pattern must have a pipe target.
    let pipe_channels =
        collect_pipe_target_channels(channel_config.patterns.as_deref().unwrap_or(&[]));
    warn_on_bad_pipe_patterns("github", &channel_name, channel_config);

    // Build the client before spawning: an unusable token (invalid header
    // bytes) must fail startup, not panic a detached task and leave a
    // silently dead channel.
    let client = Arc::new(
        GithubClient::new(&github_config)
            .with_context(|| format!("github client for channel '{channel_name}'"))?,
    );

    let channel_span = tracing::info_span!("in", ch = %channel_name);
    let workdir = workdir.to_path_buf();
    let cancel_child = cancel.child_token();

    tasks.push(tokio::spawn(
        async move {
            // Shared topic -> reply state for pipe relaying.
            let topic_state: Arc<std::sync::Mutex<HashMap<String, (u64, String)>>> =
                Arc::new(std::sync::Mutex::new(HashMap::new()));

            // One reply forwarder per distinct pipe target channel. The
            // handle is dropped (detached); the forwarder exits when the
            // hub pipe's broadcast closes at shutdown.
            for target in &pipe_channels {
                let Some(hub) = hubs.get(target).cloned() else {
                    continue;
                };
                let state = topic_state.clone();
                let client = client.clone();
                let target = target.clone();
                let span = tracing::info_span!("out", ch = %channel_name, target = %target);
                tokio::spawn(
                    async move {
                        let mut rx = hub.subscribe_replies();
                        tracing::info!(channel = %target, "github pipe reply forwarder subscribed");
                        loop {
                            let payload = match rx.recv().await {
                                Ok(payload) => payload,
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                    continue;
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                            };
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
                            let Some((number, role)) = state.lock().unwrap().get(topic).cloned()
                            else {
                                tracing::debug!(
                                    topic = %topic,
                                    "github pipe: no number mapping for reply, skipping"
                                );
                                continue;
                            };

                            // Build comment body: [Role] prefix, no footer.
                            let body = role_prefixed_body(text, &role);

                            if let Err(e) = client.create_comment(number, &body).await {
                                tracing::error!(
                                    error = format!("{e:#}"),
                                    topic = %topic,
                                    number = number,
                                    "github pipe: failed to relay reply"
                                );
                            }
                        }
                    }
                    .instrument(span),
                );
            }

            // Patterns are a startup snapshot: the ArcSwap is seeded once
            // and never reloaded — hot pattern reload was a hub-side
            // convenience; restart jyc-pipe to apply pattern changes
            // (same trade-off as the feishu pipe). The adapter itself
            // reads patterns (PR topic prefixes), so it gets the same
            // snapshot.
            let snapshot = Arc::new(arc_swap::ArcSwap::from_pointee((*config).clone()));
            let adapter = GithubInboundAdapter::new(
                &github_config,
                channel_name.clone(),
                &workdir.join("channels"),
                Some(snapshot),
            );
            let patterns = adapter_patterns(&config, &channel_name);

            let topic_state_for_close = topic_state.clone();
            let hubs_for_close = hubs.clone();
            let repo_for_close = github_config.repo.clone();
            let patterns_for_close = patterns.clone();
            let options = jyc_types::InboundAdapterOptions {
                on_message: Box::new(move |message| {
                    let hubs = hubs.clone();
                    let state = topic_state.clone();
                    let patterns = patterns.clone();
                    tokio::spawn(async move {
                        handle_inbound(hubs, state, patterns, message).await;
                    });
                    Ok(())
                }),
                on_topic_close: None,
                on_close_event: Some(Box::new(move |number: u64, github_type: &str| {
                    let topic_state = topic_state_for_close.clone();
                    let hubs = hubs_for_close.clone();
                    let repo = repo_for_close.clone();
                    let patterns = patterns_for_close.clone();
                    let github_type = github_type.to_string();
                    tokio::spawn(async move {
                        handle_close_event(
                            patterns,
                            topic_state,
                            hubs,
                            number,
                            &github_type,
                            &repo,
                        )
                        .await;
                    });
                })),
                on_error: Box::new(|error| {
                    tracing::error!(error = %error, "GitHub pipe inbound error");
                }),
                attachment_config: None,
            };

            if let Err(e) = adapter.start(options, cancel_child).await {
                tracing::error!(error = %e, "GitHub pipe inbound adapter error");
            }
        }
        .instrument(channel_span),
    ));
    Ok(())
}

/// Patterns for this channel from the startup config snapshot. Shared
/// lookup for the inbound and close-event handlers.
fn adapter_patterns(
    config: &jyc_types::AppConfig,
    channel_name: &str,
) -> Vec<jyc_types::ChannelPattern> {
    config
        .channels
        .get(channel_name)
        .and_then(|c| c.patterns.clone())
        .unwrap_or_default()
}

/// Route one inbound github event into the hub: pattern match, capture
/// the issue/PR number and role for reply routing, retarget, send.
async fn handle_inbound(
    hubs: Arc<HashMap<String, Arc<HubPipe>>>,
    topic_state: Arc<std::sync::Mutex<HashMap<String, (u64, String)>>>,
    patterns: Vec<jyc_types::ChannelPattern>,
    message: jyc_types::InboundMessage,
) {
    let Some((_pm, pattern)) = match_pipe("github", &GithubMatcher, &message, &patterns) else {
        return;
    };
    let pipe = pattern
        .pipe
        .as_ref()
        .expect("match_pipe guarantees a pipe target");

    // Capture number and role for reply routing. Without a number a
    // reply could not be addressed, so drop loudly rather than
    // commenting on issue #0.
    let Some(number) = message
        .metadata
        .get("github_number")
        .and_then(|v| v.as_u64())
    else {
        tracing::warn!(
            message_id = %message.id,
            "github pipe: message has no github_number metadata, dropping"
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

    // Send the frame to the hub pipe for the retargeted channel.
    let Some(hub) = hubs.get(&message.channel) else {
        tracing::warn!(
            channel = %message.channel,
            "github pipe: no hub pipe for target channel, dropping"
        );
        return;
    };
    hub.send_message(
        &message.topic,
        message.content.text.as_deref().unwrap_or(""),
        &message.sender,
        &message.sender_address,
        message.metadata,
    );
}

/// Issue/PR closed: derive the routed topics from config (restart-proof)
/// and union with whatever this process actually routed (covers topics
/// whose template changed since routing), then close them on every
/// matching hub pipe.
async fn handle_close_event(
    patterns: Vec<jyc_types::ChannelPattern>,
    topic_state: Arc<std::sync::Mutex<HashMap<String, (u64, String)>>>,
    hubs: Arc<HashMap<String, Arc<HubPipe>>>,
    number: u64,
    github_type: &str,
    repo: &str,
) {
    let mut targets = close_event_topics(&patterns, number, github_type, repo);
    {
        let state = topic_state.lock().unwrap();
        for topic in state
            .iter()
            .filter(|(_, v)| v.0 == number)
            .map(|(t, _)| t.clone())
        {
            if !targets.iter().any(|(t, _)| *t == topic) {
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
    for (topic, target_hub) in &targets {
        for (hub_name, hub) in hubs.iter() {
            if !target_hub.is_empty() && hub_name != target_hub {
                continue;
            }
            hub.send_close_topic(topic);
        }
    }
    topic_state.lock().unwrap().retain(|_, v| v.0 != number);
}
