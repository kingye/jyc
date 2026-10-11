//! Email pipe wiring: the peripheral IMAP/SMTP adapter running inside
//! `jyc-pipe`, mirroring the deleted in-process `spawn_email_adapter`
//! (`jyc-cli/src/cli/serve/channels/email.rs`) over the hub websocket.
//!
//! - Inbound mail: `ImapMonitor` → pattern match + retarget →
//!   `message` frame on the target channel's [`HubPipe`], with the
//!   attachments uploaded to the hub's inbound endpoint first (the pipe
//!   and the hub need not share a filesystem) and named in the frame.
//! - Replies: the pipe's reply stream → SMTP reply threaded into the
//!   original mail thread (plain text — no model/mode footer), with
//!   attachments downloaded from the hub's files endpoint.
//! - Mailbox cursor state: a `StateManager`
//!   (`<workdir>/channels/<channel>/.imap/`) tracks the sequence number
//!   and processed UIDs — protocol-level dedup state, not conversation
//!   state, so it stays with the adapter.
//!
//! A mail with an empty body but attachments is handed to the agent like any
//! other message: the hub's body check keeps attachment-only messages, and the
//! prompt gets the attachment placeholder text.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use jyc_types::{ChannelConfig, ChannelMatcher, MonitorConfig};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::email::inbound::EmailMatcher;
use crate::imap::monitor::ImapMonitor;
use crate::pipe::hub::HubPipe;
use crate::pipe::{
    collect_pipe_target_channels, fetch_topic_file, match_pipe, parse_reply_attachments,
    retarget_or_drop, warn_on_bad_pipe_patterns,
};
use crate::state_manager::StateManager;

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

/// Spawn the email pipe channel: the IMAP monitor plus one reply
/// forwarder per distinct pipe target channel.
///
/// `hubs` must contain one [`HubPipe`] per pipe-target channel this
/// channel's patterns reference (built by `pipe::run`). `files_base` is
/// the hub's HTTP origin for attachment download (`None` when the inspect
/// server is disabled — attachments are then dropped with a warning).
/// `no_idle` forces polling instead of IMAP IDLE; `reset` clears the
/// mailbox cursor state before monitoring starts.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_email_pipe(
    channel_config: &ChannelConfig,
    channel_name: String,
    workdir: &Path,
    config: Arc<jyc_types::AppConfig>,
    hubs: Arc<HashMap<String, Arc<HubPipe>>>,
    files_base: Option<String>,
    token: Option<String>,
    no_idle: bool,
    reset: bool,
    cancel: CancellationToken,
    tasks: &mut Vec<JoinHandle<()>>,
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
    let monitor_config = if no_idle {
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
    if reset {
        state_manager.reset().await?;
        tracing::info!(channel = %channel_name, "State reset");
    }
    tracing::info!(
        channel = %channel_name,
        last_seq = state_manager.last_sequence_number(),
        last_uid = ?state_manager.last_processed_uid(),
        processed_uids = state_manager.processed_uid_count(),
        "State loaded"
    );

    let channel_span = tracing::info_span!("in", ch = %channel_name);

    let task = tokio::spawn(
        async move {
            // Shared SMTP client + topic -> reply state for pipe relaying.
            let smtp = Arc::new(Mutex::new(crate::smtp::client::SmtpClient::new(
                smtp_config.clone(),
            )));
            let from_address = smtp_config
                .from_address
                .clone()
                .unwrap_or_else(|| smtp_config.username.clone());
            let from_name = smtp_config.from_name.clone();
            let topic_state: std::sync::Arc<std::sync::Mutex<HashMap<String, EmailReplyState>>> =
                std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));

            // One reply forwarder per distinct pipe target channel.
            for target in &pipe_channels {
                let Some(hub) = hubs.get(target).cloned() else {
                    continue;
                };
                let topic_state = topic_state.clone();
                let smtp = smtp.clone();
                let from_address = from_address.clone();
                let from_name = from_name.clone();
                let config = config.clone();
                let files_base = files_base.clone();
                let token = token.clone();
                let target = target.clone();
                let span = tracing::info_span!("out", ch = %channel_name, target = %target);
                // Detached, like the pre-migration hub wiring: the forwarder
                // ends when its reply stream closes (process exit).
                tokio::spawn(
                    async move {
                        let mut rx = hub.subscribe_replies();
                        tracing::info!(channel = %target, "email pipe reply forwarder subscribed");
                        loop {
                            // Lagged: a burst overflowed the broadcast buffer —
                            // skip the missed frames and keep relaying. Closed:
                            // the pipe is gone for good.
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
                            let Some(state) = topic_state.lock().unwrap().get(topic).cloned()
                            else {
                                tracing::debug!(
                                    topic = %topic,
                                    "email pipe: no address mapping for reply, skipping"
                                );
                                continue;
                            };

                            // Download reply attachments (hub files endpoint),
                            // applying the operator's outbound policy.
                            let mut email_attachments = Vec::new();
                            for att in parse_reply_attachments(&v) {
                                let Some(base) = &files_base else {
                                    tracing::warn!(
                                        filename = %att.filename,
                                        "email pipe: attachment dropped (inspect server disabled)"
                                    );
                                    continue;
                                };
                                match load_reply_attachment(base, &token, &att, &config).await {
                                    Ok(loaded) => email_attachments.push(loaded),
                                    Err(e) => tracing::warn!(
                                        filename = %att.filename,
                                        error = format!("{e:#}"),
                                        "email pipe: failed to load attachment"
                                    ),
                                }
                            }

                            let body =
                                jyc_core::email_parser::strip_trailing_separators(text);
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
                    }
                    .instrument(span),
                );
            }

            let channel_name_for_monitor = channel_name.clone();
            let mut monitor = ImapMonitor::new(
                channel_name.clone(),
                imap_config,
                monitor_config,
                state_manager,
                cancel,
                Box::new(move |message| {
                    let config_for_pipe = config.clone();
                    let topic_state = topic_state.clone();
                    let channel_name_self = channel_name_for_monitor.clone();
                    let hubs = hubs.clone();
                    let hub_files = crate::pipe::HubFiles::new(
                        files_base.clone(),
                        token.clone(),
                    );
                    tokio::spawn(async move {
                        let mut message = message;
                        let patterns = config_for_pipe
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

                        // Send the frame to the hub pipe for the retargeted
                        // channel — the same path as a chat-pane message.
                        let Some(hub) = hubs.get(&message.channel) else {
                            tracing::warn!(
                                channel = %message.channel,
                                "email pipe: no hub pipe for target channel, dropping"
                            );
                            return;
                        };

                        // Upload the attachment bytes to the hub first (the pipe
                        // has no access to its filesystem), then announce them in
                        // the frame: the hub stages them and the topic worker
                        // moves each one into the topic it routes the mail to.
                        let attachments = hub_files
                            .stage_attachments(
                                &message.channel,
                                &message.attachments,
                                &config_for_pipe,
                            )
                            .await;

                        hub.send_message_with_attachments(
                            &message.topic,
                            message.content.text.as_deref().unwrap_or(""),
                            &message.sender,
                            &message.sender_address,
                            message.metadata,
                            &attachments,
                        );
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

/// Download one reply attachment from the hub's files endpoint, apply the
/// operator's outbound policy, and return it as an SMTP attachment.
async fn load_reply_attachment(
    files_base: &str,
    token: &Option<String>,
    att: &crate::pipe::ReplyAttachmentRef,
    config: &jyc_types::AppConfig,
) -> Result<crate::smtp::client::EmailAttachment> {
    let tmp = fetch_topic_file(files_base, token.as_deref(), att, config).await?;
    let data = tokio::fs::read(tmp.path())
        .await
        .context("email pipe: failed to read downloaded attachment")?;
    Ok(crate::smtp::client::EmailAttachment {
        filename: att.filename.clone(),
        content_type: att.content_type.clone(),
        data,
    })
}
