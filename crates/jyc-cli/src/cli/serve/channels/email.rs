//! `email` channel adapter wiring (extracted from serve/channels.rs).

use anyhow::Result;
use jyc_channels::email::inbound::EmailMatcher;
use jyc_core::state_manager::StateManager;
use jyc_services::imap::monitor::ImapMonitor;
use jyc_types::{ChannelConfig, ChannelMatcher, MonitorConfig};
use serde_json;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use super::*;

/// State recorded per piped topic so the email reply forwarder can
/// reply into the original mail thread.
///
/// Known limitation (same as feishu/wecom_bot): the map is in-memory and
/// keyed by resolved topic, so it is rebuilt from inbound traffic after a
/// restart, and two senders sharing one subject share one entry
/// (last writer wins).
#[derive(Debug, Clone)]

struct EmailReplyState {
    /// Recipient address (the original sender).
    recipient: String,
    /// Original (prefix-stripped) subject; SMTP adds the `Re:` prefix.
    subject: String,
    /// Original `Message-ID`, echoed back as `In-Reply-To`.
    in_reply_to: Option<String>,
    /// `References` chain: the original chain plus the original Message-ID.
    references: Vec<String>,
}

/// Spawn a pipe-only email adapter: the IMAP monitor plus one reply
/// forwarder per distinct pipe target channel.
///
/// Mirrors `spawn_feishu_adapter` (see `docs/architecture/overview.md`).
/// Differences specific to email:
///
/// - Keeps a `StateManager` (`<workdir>/channels/<channel>/.imap/`): it
///   tracks the mailbox cursor (sequence number + processed UIDs), which
///   is protocol-level dedup state, not conversation state.
/// - When the matched pattern's `pipe` has no explicit `topic`, the
///   subject-derived topic name is used (email's natural topic identity).
/// - Replies are plain text — no model/mode/token footer.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn spawn_email_adapter(
    channel_config: &ChannelConfig,
    channel_name: String,
    workdir: &Path,
    args: &crate::cli::serve::ServeArgs,
    cancel: CancellationToken,
    tasks: &mut Vec<JoinHandle<()>>,
    config_for_spawn: Arc<arc_swap::ArcSwap<jyc_types::AppConfig>>,
    ws_broadcasts: std::sync::Arc<std::sync::Mutex<HashMap<String, broadcast::Sender<String>>>>,
    routers: HubRegistry,
) -> Result<()> {
    let imap_config = channel_config
        .inbound
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing inbound config"))?
        .clone();
    let smtp_config = channel_config
        .outbound
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing outbound config"))?
        .clone();

    // `--no-idle` forces polling.
    let monitor_config = channel_config.monitor.clone().unwrap_or_default();
    let monitor_config = if args.no_idle {
        MonitorConfig {
            mode: "poll".to_string(),
            ..monitor_config
        }
    } else {
        monitor_config
    };

    // Email is pipe-only: every enabled pattern must name a pipe target
    // (a websocket hub channel). Patterns without one are a configuration
    // error — warn at startup, drop matching messages at runtime.
    let pipe_channels =
        collect_pipe_target_channels(channel_config.patterns.as_deref().unwrap_or(&[]));
    warn_on_bad_pipe_patterns("email", &channel_name, channel_config);

    // Mailbox cursor state lives under <workdir>/channels/<channel>/.imap/.
    let mut state_manager = StateManager::for_channel(&workdir.join("channels"), &channel_name);
    state_manager.initialize().await?;
    if args.reset {
        state_manager.reset().await?;
        tracing::info!(channel = %channel_name, "State reset");
    }
    tracing::info!(
        channel = %channel_name,
        last_seq = state_manager.last_sequence_number(),
        processed_uids = state_manager.processed_uid_count(),
        "State loaded"
    );

    let channel_span = tracing::info_span!("in", ch = %channel_name);
    let workdir_for_task = workdir.to_path_buf();

    let task = tokio::spawn(
        async move {
            // Shared SMTP client + topic -> reply state for pipe relaying.
            let smtp = Arc::new(Mutex::new(jyc_services::smtp::client::SmtpClient::new(
                smtp_config.clone(),
            )));
            let from_address = smtp_config
                .from_address
                .clone()
                .unwrap_or_else(|| smtp_config.username.clone());
            let from_name = smtp_config.from_name.clone();
            let topic_state: std::sync::Arc<std::sync::Mutex<HashMap<String, EmailReplyState>>> =
                std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));

            // Attachment relay needs the inspect server (reply broadcasts
            // carry download paths served by its files endpoint). `None`
            // when inspect is disabled — text relaying is unaffected,
            // attachments are dropped with a warning (same as feishu).
            let inspect_client = {
                let cfg = config_for_spawn.load();
                cfg.inspect.as_ref().filter(|i| i.enabled).map(|i| {
                    let token = jyc_utils::auth_token::read_token(
                        &jyc_utils::auth_token::token_path(&workdir_for_task),
                    )
                    .ok();
                    jyc_inspect::client::InspectClient::with_token(
                        &loopback_addr(&i.bind),
                        token.as_deref(),
                    )
                })
            };

            // One reply forwarder per distinct pipe target channel.
            for channel in &pipe_channels {
                let ws_broadcasts = ws_broadcasts.clone();
                let topic_state = topic_state.clone();
                let smtp = smtp.clone();
                let from_address = from_address.clone();
                let from_name = from_name.clone();
                let channel = channel.clone();
                let inspect_client = inspect_client.clone();
                let config_for_relay = config_for_spawn.clone();
                tokio::spawn(async move {
                    let Some(broadcast_tx) = wait_for_broadcast(&ws_broadcasts, &channel).await
                    else {
                        tracing::error!(
                            channel = %channel,
                            "email pipe: target channel broadcast never appeared (is it a websocket channel?), reply forwarder not started"
                        );
                        return;
                    };
                    let mut rx = broadcast_tx.subscribe();
                    tracing::info!(channel = %channel, "email pipe reply forwarder subscribed");
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
                        let Some(state) = topic_state.lock().unwrap().get(topic).cloned() else {
                            tracing::debug!(
                                topic = %topic,
                                "email pipe: no address mapping for reply, skipping"
                            );
                            continue;
                        };

                        // Download reply attachments (inspect files endpoint),
                        // applying the operator's outbound policy.
                        let mut email_attachments = Vec::new();
                        for att in parse_reply_attachments(&v) {
                            let Some(inspect) = &inspect_client else {
                                tracing::warn!(
                                    filename = %att.filename,
                                    "email pipe: attachment dropped (inspect server disabled)"
                                );
                                continue;
                            };
                            match load_reply_attachment(inspect, &att, &config_for_relay).await {
                                Ok(loaded) => email_attachments.push(loaded),
                                Err(e) => tracing::warn!(
                                    filename = %att.filename,
                                    error = format!("{e:#}"),
                                    "email pipe: failed to load attachment"
                                ),
                            }
                        }

                        let body = jyc_core::email_parser::strip_trailing_separators(text);
                        let mut smtp = smtp.lock().await;
                        // Lazy connect: the transport is built on first use
                        // (and rebuilt by send_with_retry on drops).
                        if !smtp.is_connected()
                            && let Err(e) = smtp.connect().await
                        {
                            tracing::error!(error = %e, "email pipe: SMTP connect failed, reply dropped");
                            continue;
                        }
                        if let Err(e) = smtp
                            .send_reply(
                                &from_address,
                                from_name.as_deref(),
                                &state.recipient,
                                &state.subject,
                                &body,
                                state.in_reply_to.as_deref(),
                                if state.references.is_empty() {
                                    None
                                } else {
                                    Some(&state.references)
                                },
                                if email_attachments.is_empty() {
                                    None
                                } else {
                                    Some(&email_attachments)
                                },
                            )
                            .await
                        {
                            tracing::error!(
                                error = format!("{e:#}"),
                                topic = %topic,
                                "email pipe: failed to relay reply"
                            );
                        }
                    }
                });
            }

            let channel_name_for_monitor = channel_name.clone();
            let mut monitor = ImapMonitor::new(
                channel_name.clone(),
                imap_config,
                monitor_config,
                state_manager,
                cancel,
                Box::new(move |message| {
                    let config_for_pipe = config_for_spawn.clone();
                    let topic_state = topic_state.clone();
                    let channel_name_self = channel_name_for_monitor.clone();
                    let routers = routers.clone();
                    tokio::spawn(async move {
                        let mut message = message;
                        let patterns = config_for_pipe
                            .load()
                            .channels
                            .get(&channel_name_self)
                            .and_then(|c| c.patterns.clone())
                            .unwrap_or_default();
                        let Some((pm, pattern)) =
                            match_pipe("email", &EmailMatcher, &message, &patterns)
                        else {
                            return;
                        };
                        let pipe = pattern
                            .pipe
                            .as_ref()
                            .expect("match_pipe guarantees a pipe target");

                        // Reply state, captured before re-targeting
                        // rewrites channel/topic.
                        let mut references = message.references.clone().unwrap_or_default();
                        if let Some(ext_id) = &message.external_id {
                            references.push(ext_id.clone());
                        }
                        let reply_state = EmailReplyState {
                            recipient: message.sender_address.clone(),
                            subject: message.topic.clone(),
                            in_reply_to: message.external_id.clone(),
                            references,
                        };

                        // Re-target into the target channel/topic. Without
                        // an explicit `pipe.topic`, the pattern's
                        // `topic_name` override wins, else the
                        // subject-derived topic name (same precedence the
                        // MessageRouter applied before the migration).
                        // The derived name also replaces `message.topic`
                        // (the parse-time subject, already `Re:`/`Fw:`
                        // stripped but not pattern-prefix stripped or
                        // sanitized) so `${msg.topic}` in a template
                        // resolves to the same name the no-template path
                        // would use. `apply_pipe_retarget` overwrites
                        // `message.topic` with the resolved template anyway.
                        let derived_topic = pattern.topic_name.clone().unwrap_or_else(|| {
                            EmailMatcher.derive_topic_name(&message, &patterns, Some(&pm))
                        });
                        let pipe = email_pipe_with_topic(pipe, &derived_topic);
                        message.topic = derived_topic;
                        let Some(message) = retarget_or_drop("email", message, &pipe) else {
                            return;
                        };

                        // Record resolved topic -> reply state.
                        topic_state
                            .lock()
                            .unwrap()
                            .insert(message.topic.clone(), reply_state);

                        // Route through the target's own MessageRouter —
                        // the same path as a chat-pane message.
                        route_into_pipe_target("email", &routers, &pipe, message).await;
                    });
                    Ok(())
                }),
            );

            if let Err(e) = monitor.start().await {
                tracing::error!(error = %e, "IMAP monitor error");
            }
        }
        .instrument(channel_span),
    );
    tasks.push(task);
    Ok(())
}

/// Effective pipe target for an email message: when the pattern's `pipe`
/// names no `topic`, the subject-derived topic name is used — email's
/// natural topic identity (one thread per subject).
pub(super) fn email_pipe_with_topic(
    pipe: &jyc_types::PipeTarget,
    derived_topic: &str,
) -> jyc_types::PipeTarget {
    if pipe.topic.is_some() {
        return pipe.clone();
    }
    jyc_types::PipeTarget {
        topic: Some(derived_topic.to_string()),
        ..pipe.clone()
    }
}

/// Download one reply attachment from the inspect server, apply the
/// operator's outbound policy, and return it as an SMTP attachment.
async fn load_reply_attachment(
    inspect: &jyc_inspect::client::InspectClient,
    att: &ReplyAttachmentRef,
    config: &arc_swap::ArcSwap<jyc_types::AppConfig>,
) -> Result<jyc_services::smtp::client::EmailAttachment> {
    let (bytes, _tmp) = fetch_reply_attachment(inspect, att, config).await?;
    Ok(jyc_services::smtp::client::EmailAttachment {
        filename: att.filename.clone(),
        content_type: att.content_type.clone(),
        data: bytes,
    })
}
