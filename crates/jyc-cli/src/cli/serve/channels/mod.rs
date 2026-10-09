//! Channel adapter construction for `jyc serve`.
//!
//! Extracted from the monolithic `serve.rs` run() function.

use anyhow::Result;
use jyc_core::channel_orchestrator::ChannelOrchestrator;
use jyc_core::message_router::MessageRouter;
use jyc_core::topic_manager::TopicManager;
use jyc_inspect::server::websocket::inbound::{WebsocketInboundAdapter, WebsocketMatcher};
use jyc_types::{ChannelInfo, InboundAdapter, InboundAttachmentConfig};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

/// Shared per-channel context for spawning the inbound monitor task(s).
pub(crate) struct InboundSpawner<'a> {
    pub(crate) channel_type: &'a str,
    pub(crate) channel_name: String,
    pub(crate) workspace_dir: PathBuf,
    pub(crate) inbound_attachment_config: Option<InboundAttachmentConfig>,
    pub(crate) topic_manager: Arc<TopicManager>,
    pub(crate) router: Arc<MessageRouter>,
    pub(crate) cancel: CancellationToken,
    pub(crate) cancel_child: CancellationToken,
    pub(crate) tasks: &'a mut Vec<JoinHandle<()>>,
    pub(crate) orchestrator: Arc<ChannelOrchestrator>,
    pub(crate) channel_info: ChannelInfo,
    pub(crate) websocket_handlers: &'a mut [Arc<WebsocketInboundAdapter>],
}

impl InboundSpawner<'_> {
    /// Spawn the channel-type-specific inbound monitor task(s).
    ///
    /// Destructures the context into locals so the per-channel arms read
    /// exactly like the original inline match in `serve.rs`.
    pub(crate) async fn spawn(self) -> Result<()> {
        let InboundSpawner {
            channel_type,
            channel_name,
            workspace_dir,
            inbound_attachment_config,
            topic_manager,
            router,
            cancel,
            cancel_child,
            tasks,
            orchestrator,
            channel_info,
            websocket_handlers,
        } = self;
        let channel_name_owned = channel_name.clone();
        let tm = topic_manager.clone();
        let channel_span = tracing::info_span!("in", ch = %channel_name);
        if channel_type == "websocket" {
            let router_for_callback = router.clone();
            let channel_name_for_matcher = channel_name_owned.clone();

            // The websocket handler was already created when the outbound adapter was built.
            // Find it in the list and start it (sets the on_message callback).
            let handler = websocket_handlers.last().cloned().ok_or_else(|| {
                anyhow::anyhow!("channel '{channel_name}': websocket handler not found")
            })?;

            let topic_manager_clone = topic_manager.clone();
            let options = jyc_types::InboundAdapterOptions {
                on_message: Box::new(move |message| {
                    let router = router_for_callback.clone();
                    let channel_name = channel_name_for_matcher.clone();

                    tokio::spawn(async move {
                        router
                            .route(&WebsocketMatcher::new(channel_name), message)
                            .await;
                    });

                    Ok(())
                }),
                on_topic_close: Some(Box::new(move |topic_name: String| {
                    let tm = topic_manager_clone.clone();
                    tokio::spawn(async move {
                        if let Err(e) = tm.auto_close_topic(&topic_name).await {
                            tracing::error!(error = %e, topic = %topic_name, "Failed to close topic");
                        }
                    });
                    Ok(())
                })),
                on_close_event: None,
                on_error: Box::new(|error| {
                    tracing::error!(error = %error, "WebSocket inbound error");
                }),
                attachment_config: inbound_attachment_config.clone(),
            };

            // Start the adapter (sets the on_message callback; no independent listener)
            if let Err(e) = handler.start(options, cancel_child.clone()).await {
                tracing::error!(
                    error = %e,
                    "WebSocket inbound adapter error"
                );
            }

            // WebSocket channel does not need a background task (handler is registered on the inspect server)
            // But we still need to keep the topic_manager alive, so we push a no-op task
            let task = tokio::spawn(
                async move {
                    // Wait for cancellation
                    cancel_child.cancelled().await;
                    tm.shutdown().await;
                }
                .instrument(channel_span),
            );

            orchestrator
                .register_channel(
                    channel_name.to_string(),
                    jyc_core::channel_orchestrator::ChannelHandle {
                        cancel: cancel.clone(),

                        topic_manager: topic_manager.clone(),

                        channel_info: channel_info.clone(),

                        workspace_dir: workspace_dir.clone(),
                    },
                )
                .await;

            tasks.push(task);
        }
        Ok(())
    }
}
