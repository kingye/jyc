//! Channel adapter construction for `jyc serve`.
//!
//! Extracted from the monolithic `serve.rs` run() function.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use anyhow::Result;

use jyc_channels::email::inbound::EmailMatcher;
use jyc_channels::email::outbound::EmailOutboundAdapter;
use jyc_channels::feishu::inbound::{FeishuInboundAdapter, FeishuMatcher};
use jyc_channels::feishu::outbound::FeishuOutboundAdapter;
use jyc_channels::gitee::inbound::GiteeMatcher;
use jyc_channels::gitee::outbound::GiteeOutboundAdapter;
use jyc_channels::github::inbound::GithubMatcher;
use jyc_channels::github::outbound::GithubOutboundAdapter;
use jyc_channels::websocket::inbound::{WebsocketInboundAdapter, WebsocketMatcher};
use jyc_channels::websocket::outbound::WebsocketOutboundAdapter;

use jyc_channels::wechat::inbound::WechatInboundAdapter;
use jyc_channels::wechat::outbound::WechatOutboundAdapter;
use jyc_channels::wecom::inbound::WecomInboundAdapter;
use jyc_channels::wecom::kf_client::KfApiClient;
use jyc_channels::wecom::kf_cursor::KfCursorStore;
use jyc_channels::wecom::kf_dedup::KfDedupStore;
use jyc_channels::wecom::kf_inbound::{WecomKfInboundAdapter, WecomKfMatcher};
use jyc_channels::wecom::kf_outbound::WecomKfOutboundAdapter;
use jyc_channels::wecom::outbound::WecomOutboundAdapter;
use jyc_channels::wecom::server::WecomWebhookServer;
use jyc_channels::wecom::token_cache::AccessTokenCache;
use jyc_channels::wecom_bot::client::WecomBotConnectionHandle;
use jyc_channels::wecom_bot::inbound::{WecomBotInboundAdapter, WecomBotMatcher};
use jyc_channels::wecom_bot::outbound::WecomBotOutboundAdapter;
use jyc_core::channel_orchestrator::ChannelOrchestrator;
use jyc_core::message_router::MessageRouter;
use jyc_core::message_storage::MessageStorage;
use jyc_core::state_manager::StateManager;
use jyc_core::thread_manager::ThreadManager;
use jyc_services::imap::monitor::ImapMonitor;
use jyc_types::{
    ChannelConfig, ChannelInfo, InboundAdapter, InboundAttachmentConfig, MonitorConfig,
    OutboundAdapter, OutboundAttachmentConfig,
};

/// Build the outbound adapter for a channel.
///
/// Returns `Ok(None)` for unsupported channel types (the caller skips them,
/// matching the original inline `continue` behavior).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_outbound_adapter(
    channel_type: &str,
    channel_config: &ChannelConfig,
    channel_name: &str,
    storage: Arc<MessageStorage>,
    outbound_attachment_config: Option<OutboundAttachmentConfig>,
    footer_enabled: bool,
    workspace_dir: &std::path::Path,
    inspect_broadcast: Arc<broadcast::Sender<String>>,
    wechat_sender_arc: &mut Option<Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>>,
    wecom_bot_handle_arc: &mut Option<Arc<Mutex<Option<WecomBotConnectionHandle>>>>,
    wecomkf_kf_client: &mut Option<Arc<KfApiClient>>,
    ws_handler_for_channel: &mut HashMap<String, Arc<WebsocketInboundAdapter>>,
    websocket_handlers: &mut Vec<Arc<WebsocketInboundAdapter>>,
) -> Result<Option<Arc<dyn OutboundAdapter>>> {
    let outbound: Arc<dyn OutboundAdapter> = match channel_type {
        "email" => {
            let outbound_config = channel_config.outbound.as_ref().ok_or_else(|| {
                anyhow::anyhow!("channel '{channel_name}': missing outbound config")
            })?;
            Arc::new(EmailOutboundAdapter::new_with_attachments(
                outbound_config,
                storage.clone(),
                outbound_attachment_config,
                footer_enabled,
            ))
        }
        "feishu" => {
            let feishu_config = channel_config
                .feishu
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing feishu config"))?
                .clone();
            Arc::new(FeishuOutboundAdapter::new_with_attachments(
                feishu_config,
                storage.clone(),
                outbound_attachment_config,
                footer_enabled,
            ))
        }
        "gitee" => {
            let gitee_config = channel_config
                .gitee
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing gitee config"))?
                .clone();
            Arc::new(GiteeOutboundAdapter::with_footer_enabled(
                gitee_config,
                storage.clone(),
                footer_enabled,
            )?)
        }
        "github" => {
            let github_config = channel_config
                .github
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing github config"))?
                .clone();
            Arc::new(GithubOutboundAdapter::with_footer_enabled(
                github_config,
                storage.clone(),
                footer_enabled,
            )?)
        }
        "wechat" => {
            // WeChat config is validated and cloned in the inbound section.
            // Outbound only needs sender, storage, and footer config.
            let adapter = WechatOutboundAdapter::new_with_attachments(
                storage.clone(),
                outbound_attachment_config,
                footer_enabled,
            );
            // Store the sender_arc for later use in the inbound section
            *wechat_sender_arc = Some(adapter.sender_arc());
            Arc::new(adapter)
        }
        "wecom_bot" => {
            let adapter = WecomBotOutboundAdapter::new_with_attachments(
                storage.clone(),
                outbound_attachment_config,
                footer_enabled,
            );
            *wecom_bot_handle_arc = Some(adapter.handle_arc());
            Arc::new(adapter)
        }
        "wecom" => {
            let wecom_config = channel_config
                .wecom
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing wecom config"))?
                .clone();
            Arc::new(WecomOutboundAdapter::new_with_attachments(
                wecom_config.corp_id,
                wecom_config.corp_secret,
                storage.clone(),
                outbound_attachment_config,
                footer_enabled,
            ))
        }
        "wecomkf" => {
            let wecomkf_config = channel_config
                .wecom_kf
                .as_ref()
                .ok_or_else(|| {
                    anyhow::anyhow!("channel '{channel_name}': missing wecom_kf config")
                })?
                .clone();

            let access_token_cache = Arc::new(AccessTokenCache::new(
                wecomkf_config.corp_id.clone(),
                wecomkf_config.corp_secret.clone(),
            ));
            let kf_client = Arc::new(KfApiClient::new(access_token_cache));
            *wecomkf_kf_client = Some(kf_client.clone());

            Arc::new(WecomKfOutboundAdapter::new(
                kf_client,
                storage.clone(),
                outbound_attachment_config,
                footer_enabled,
            ))
        }
        "websocket" => {
            let (broadcast_tx, _) = tokio::sync::broadcast::channel(64);
            let adapter = WebsocketOutboundAdapter::new(broadcast_tx.clone(), storage.clone());
            // Store the inbound adapter for later registration with the inspect server
            let mut handler = WebsocketInboundAdapter::new(channel_name.to_string(), broadcast_tx);
            handler.set_workspace_dir(workspace_dir.to_path_buf());
            handler.set_inspect_broadcast(inspect_broadcast.clone());
            let handler = Arc::new(handler);
            ws_handler_for_channel.insert(channel_name.to_string(), handler.clone());
            websocket_handlers.push(handler);
            Arc::new(adapter)
        }
        other => {
            tracing::warn!(
                channel = %channel_name,
                channel_type = %other,
                "Unsupported channel type, skipping"
            );
            return Ok(None);
        }
    };
    Ok(Some(outbound))
}
/// Shared per-channel context for spawning the inbound monitor task(s).
///
/// `state_manager` is consumed (moved into the IMAP monitor closure).
pub(crate) struct InboundSpawner<'a> {
    pub(crate) channel_type: &'a str,
    pub(crate) channel_config: &'a ChannelConfig,
    pub(crate) channel_name: String,
    pub(crate) workdir: &'a Path,
    pub(crate) workspace_dir: PathBuf,
    pub(crate) args: &'a crate::cli::serve::ServeArgs,
    pub(crate) inbound_attachment_config: Option<InboundAttachmentConfig>,
    pub(crate) thread_manager: Arc<ThreadManager>,
    pub(crate) router: Arc<MessageRouter>,
    pub(crate) state_manager: StateManager,
    pub(crate) cancel: CancellationToken,
    pub(crate) cancel_child: CancellationToken,
    pub(crate) tasks: &'a mut Vec<JoinHandle<()>>,
    pub(crate) wechat_sender_arc: &'a mut Option<Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>>,
    pub(crate) wecom_bot_handle_arc: &'a mut Option<Arc<Mutex<Option<WecomBotConnectionHandle>>>>,
    pub(crate) wecomkf_kf_client: &'a mut Option<Arc<KfApiClient>>,
    pub(crate) orchestrator: Arc<ChannelOrchestrator>,
    pub(crate) channel_info: ChannelInfo,
    pub(crate) config_for_spawn: Arc<arc_swap::ArcSwap<jyc_types::AppConfig>>,
    pub(crate) wecom_server: Option<Arc<WecomWebhookServer>>,
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
            channel_config,
            channel_name,
            workdir,
            workspace_dir,
            args,
            inbound_attachment_config,
            thread_manager,
            router,
            state_manager,
            cancel,
            cancel_child,
            tasks,
            wechat_sender_arc,
            wecom_bot_handle_arc,
            wecomkf_kf_client,
            orchestrator,
            channel_info,
            config_for_spawn,
            wecom_server,
            websocket_handlers,
        } = self;
        let channel_name_owned = channel_name.clone();
        let tm = thread_manager.clone();
        let channel_span = tracing::info_span!("in", ch = %channel_name);
        match channel_type {
            "email" => {
                let inbound_config = channel_config
                    .inbound
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("channel '{channel_name}': missing inbound config")
                    })?
                    .clone();

                let monitor_config = channel_config.monitor.clone().unwrap_or_default();

                // Override IDLE mode if --no-idle flag
                let monitor_config = if args.no_idle {
                    MonitorConfig {
                        mode: "poll".to_string(),
                        ..monitor_config
                    }
                } else {
                    monitor_config
                };

                let task = tokio::spawn(
                    async move {
                        let mut monitor = ImapMonitor::new(
                            channel_name_owned.clone(),
                            inbound_config,
                            monitor_config,
                            router,
                            state_manager,
                            cancel_child,
                            Arc::new(EmailMatcher),
                        );

                        if let Err(e) = monitor.start().await {
                            tracing::error!(
                                error = %e,
                                "IMAP monitor error"
                            );
                        }

                        // Shutdown thread manager for this channel
                        tm.shutdown().await;
                    }
                    .instrument(channel_span),
                );

                orchestrator
                    .register_channel(
                        channel_name.to_string(),
                        jyc_core::channel_orchestrator::ChannelHandle {
                            cancel: cancel.clone(),

                            thread_manager: thread_manager.clone(),

                            channel_info: channel_info.clone(),

                            workspace_dir: workspace_dir.clone(),
                        },
                    )
                    .await;

                tasks.push(task);
            }
            "feishu" => {
                let feishu_config = channel_config
                    .feishu
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("channel '{channel_name}': missing feishu config")
                    })?
                    .clone();

                let router_for_callback = router.clone();

                let thread_manager_for_task = thread_manager.clone();

                let task = tokio::spawn(async move {
                // Clone configs before moving into closures
                let feishu_config_cloned = feishu_config.clone();

                let adapter = FeishuInboundAdapter::new(&feishu_config_cloned, channel_name_owned.clone());

                // Wire on_message to route through FeishuMatcher → MessageRouter


                let thread_manager_clone = thread_manager_for_task.clone();
                let options = jyc_types::InboundAdapterOptions {
                    on_message: Box::new(move |message| {
                        let router = router_for_callback.clone();
                        tokio::spawn(async move {
                            // Attachments are saved inside process_message()
                            // after template initialization, so there's no
                            // need for a pre-route save here.
                            router.route(&FeishuMatcher, message).await;
                        });
                        Ok(())
                    }),
                    on_thread_close: Some(Box::new(move |thread_name: String| {
                        let tm = thread_manager_clone.clone();
                        tokio::spawn(async move {
                            if let Err(e) = tm.close_thread(&thread_name).await {
                                tracing::error!(error = %e, thread = %thread_name, "Failed to close thread");
                            }
                        });
                        Ok(())
                    })),
                    on_error: Box::new(|error| {
                        tracing::error!(error = %error, "Feishu inbound error");
                    }),
                    attachment_config: inbound_attachment_config.clone(),
                };

                if let Err(e) = adapter.start(options, cancel_child).await {
                    tracing::error!(
                        error = %e,
                        "Feishu inbound adapter error"
                    );
                }

                // Shutdown thread manager for this channel
                tm.shutdown().await;
            }.instrument(channel_span));

                orchestrator
                    .register_channel(
                        channel_name.to_string(),
                        jyc_core::channel_orchestrator::ChannelHandle {
                            cancel: cancel.clone(),

                            thread_manager: thread_manager.clone(),

                            channel_info: channel_info.clone(),

                            workspace_dir: workspace_dir.clone(),
                        },
                    )
                    .await;

                tasks.push(task);
            }
            "gitee" => {
                let gitee_config = channel_config
                    .gitee
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("channel '{channel_name}': missing gitee config")
                    })?
                    .clone();

                let router_for_callback = router.clone();
                let workdir_owned = workdir.to_path_buf();

                let thread_manager_for_task = thread_manager.clone();

                let task = tokio::spawn(async move {
                use jyc_channels::gitee::inbound::GiteeInboundAdapter;

                let adapter = GiteeInboundAdapter::new(&gitee_config, channel_name_owned.clone(), &workdir_owned);

                let thread_manager_clone = thread_manager_for_task.clone();
                let options = jyc_types::InboundAdapterOptions {
                    on_message: Box::new(move |message| {
                        let router = router_for_callback.clone();

                        tokio::spawn(async move {
                            router.route(&GiteeMatcher, message).await;
                        });

                        Ok(())
                    }),
                    on_thread_close: Some(Box::new(move |thread_name: String| {
                        let tm = thread_manager_clone.clone();
                        tokio::spawn(async move {
                            if let Err(e) = tm.close_thread(&thread_name).await {
                                tracing::error!(error = %e, thread = %thread_name, "Failed to close thread");
                            }
                        });
                        Ok(())
                    })),
                    on_error: Box::new(|error| {
                        tracing::error!(error = %error, "Gitee inbound error");
                    }),
                    attachment_config: inbound_attachment_config.clone(),
                };

                if let Err(e) = adapter.start(options, cancel_child).await {
                    tracing::error!(error = %e, "Gitee inbound adapter error");
                }

                tm.shutdown().await;
            }.instrument(channel_span));

                orchestrator
                    .register_channel(
                        channel_name.to_string(),
                        jyc_core::channel_orchestrator::ChannelHandle {
                            cancel: cancel.clone(),

                            thread_manager: thread_manager.clone(),

                            channel_info: channel_info.clone(),

                            workspace_dir: workspace_dir.clone(),
                        },
                    )
                    .await;

                tasks.push(task);
            }
            "github" => {
                let github_config = channel_config
                    .github
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("channel '{channel_name}': missing github config")
                    })?
                    .clone();

                let config_for_adapter = config_for_spawn.clone();
                let router_for_callback = router.clone();
                let workdir_owned = workdir.to_path_buf();

                let thread_manager_for_task = thread_manager.clone();

                let task = tokio::spawn(async move {
                use jyc_channels::github::inbound::GithubInboundAdapter;

                let adapter = GithubInboundAdapter::new(&github_config, channel_name_owned.clone(), &workdir_owned, Some(config_for_adapter));

                let thread_manager_clone = thread_manager_for_task.clone();
                let options = jyc_types::InboundAdapterOptions {
                    on_message: Box::new(move |message| {
                        let router = router_for_callback.clone();

                        tokio::spawn(async move {
                            router.route(&GithubMatcher, message).await;
                        });

                        Ok(())
                    }),
                    on_thread_close: Some(Box::new(move |thread_name: String| {
                        let tm = thread_manager_clone.clone();
                        tokio::spawn(async move {
                            if let Err(e) = tm.close_thread(&thread_name).await {
                                tracing::error!(error = %e, thread = %thread_name, "Failed to close thread");
                            }
                        });
                        Ok(())
                    })),
                    on_error: Box::new(|error| {
                        tracing::error!(error = %error, "GitHub inbound error");
                    }),
                    attachment_config: inbound_attachment_config.clone(),
                };

                if let Err(e) = adapter.start(options, cancel_child).await {
                    tracing::error!(
                        error = %e,
                        "GitHub inbound adapter error"
                    );
                }

                // Shutdown thread manager for this channel
                tm.shutdown().await;
            }.instrument(channel_span));

                orchestrator
                    .register_channel(
                        channel_name.to_string(),
                        jyc_core::channel_orchestrator::ChannelHandle {
                            cancel: cancel.clone(),

                            thread_manager: thread_manager.clone(),

                            channel_info: channel_info.clone(),

                            workspace_dir: workspace_dir.clone(),
                        },
                    )
                    .await;

                tasks.push(task);
            }
            "wechat" => {
                let wechat_config = channel_config
                    .wechat
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("channel '{channel_name}': missing wechat config")
                    })?
                    .clone();

                let router_for_callback = router.clone();
                let wechat_sender_arc_clone = wechat_sender_arc.clone().unwrap();

                let thread_manager_for_task = thread_manager.clone();

                let task = tokio::spawn(async move {
                use jyc_channels::wechat::inbound::WechatMatcher;

                // Create the adapter with the shared sender Arc so it can
                // update the outbound sender on each reconnection.
                let adapter = WechatInboundAdapter::with_shared_sender(
                    &wechat_config,
                    channel_name_owned.clone(),
                    wechat_sender_arc_clone,
                );

                let thread_manager_clone = thread_manager_for_task.clone();
                let options = jyc_types::InboundAdapterOptions {
                    on_message: Box::new(move |message| {
                        let router = router_for_callback.clone();

                        tokio::spawn(async move {
                            router.route(&WechatMatcher, message).await;
                        });

                        Ok(())
                    }),
                    on_thread_close: Some(Box::new(move |thread_name: String| {
                        let tm = thread_manager_clone.clone();
                        tokio::spawn(async move {
                            if let Err(e) = tm.close_thread(&thread_name).await {
                                tracing::error!(error = %e, thread = %thread_name, "Failed to close thread");
                            }
                        });
                        Ok(())
                    })),
                    on_error: Box::new(|error| {
                        tracing::error!(error = %error, "WeChat inbound error");
                    }),
                    attachment_config: inbound_attachment_config.clone(),
                };

                if let Err(e) = adapter.start(options, cancel_child).await {
                    tracing::error!(
                        error = %e,
                        "WeChat inbound adapter error"
                    );
                }

                // Shutdown thread manager for this channel
                tm.shutdown().await;
            }.instrument(channel_span));

                orchestrator
                    .register_channel(
                        channel_name.to_string(),
                        jyc_core::channel_orchestrator::ChannelHandle {
                            cancel: cancel.clone(),

                            thread_manager: thread_manager.clone(),

                            channel_info: channel_info.clone(),

                            workspace_dir: workspace_dir.clone(),
                        },
                    )
                    .await;

                tasks.push(task);
            }
            "wecom_bot" => {
                let wecom_bot_config = channel_config
                    .wecom_bot
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("channel '{channel_name}': missing wecom_bot config")
                    })?
                    .clone();

                let router_for_callback = router.clone();
                let wecom_bot_handle_arc_clone = wecom_bot_handle_arc.clone().unwrap();

                let thread_manager_for_task = thread_manager.clone();

                let task = tokio::spawn(async move {

                let adapter = WecomBotInboundAdapter::with_shared_handle(
                    &wecom_bot_config,
                    channel_name_owned.clone(),
                    wecom_bot_handle_arc_clone,
                );

                let thread_manager_clone = thread_manager_for_task.clone();
                let options = jyc_types::InboundAdapterOptions {
                    on_message: Box::new(move |message| {
                        let router = router_for_callback.clone();

                        tokio::spawn(async move {
                            router.route(&WecomBotMatcher, message).await;
                        });

                        Ok(())
                    }),
                    on_thread_close: Some(Box::new(move |thread_name: String| {
                        let tm = thread_manager_clone.clone();
                        tokio::spawn(async move {
                            if let Err(e) = tm.close_thread(&thread_name).await {
                                tracing::error!(error = %e, thread = %thread_name, "Failed to close thread");
                            }
                        });
                        Ok(())
                    })),
                    on_error: Box::new(|error| {
                        tracing::error!(error = %error, "WeCom Bot inbound error");
                    }),
                    attachment_config: inbound_attachment_config.clone(),
                };

                if let Err(e) = adapter.start(options, cancel_child).await {
                    tracing::error!(
                        error = %e,
                        "WeCom Bot inbound adapter error"
                    );
                }

                tm.shutdown().await;
            }.instrument(channel_span));

                orchestrator
                    .register_channel(
                        channel_name.to_string(),
                        jyc_core::channel_orchestrator::ChannelHandle {
                            cancel: cancel.clone(),

                            thread_manager: thread_manager.clone(),

                            channel_info: channel_info.clone(),

                            workspace_dir: workspace_dir.clone(),
                        },
                    )
                    .await;

                tasks.push(task);
            }
            "wecom" => {
                let wecom_config = channel_config
                    .wecom
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("channel '{channel_name}': missing wecom config")
                    })?
                    .clone();

                let wecom_server = wecom_server
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("WeCom webhook server not initialized"))?;
                let router_for_callback = router.clone();
                let channel_name_owned = channel_name.clone();

                let thread_manager_for_task = thread_manager.clone();

                let task = tokio::spawn(async move {
                use jyc_channels::wecom::inbound::WecomMatcher;

                let adapter = WecomInboundAdapter::new(
                    &wecom_config,
                    &channel_name_owned,
                    wecom_server,
                );

                let thread_manager_clone = thread_manager_for_task.clone();
                let options = jyc_types::InboundAdapterOptions {
                    on_message: Box::new(move |message| {
                        let router = router_for_callback.clone();

                        tokio::spawn(async move {
                            router.route(&WecomMatcher, message).await;
                        });

                        Ok(())
                    }),
                    on_thread_close: Some(Box::new(move |thread_name: String| {
                        let tm = thread_manager_clone.clone();
                        tokio::spawn(async move {
                            if let Err(e) = tm.close_thread(&thread_name).await {
                                tracing::error!(error = %e, thread = %thread_name, "Failed to close thread");
                            }
                        });
                        Ok(())
                    })),
                    on_error: Box::new(|error| {
                        tracing::error!(error = %error, "WeCom inbound error");
                    }),
                    attachment_config: inbound_attachment_config.clone(),
                };

                if let Err(e) = adapter.start(options, cancel_child).await {
                    tracing::error!(
                        error = %e,
                        "WeCom inbound adapter error"
                    );
                }

                // Shutdown thread manager for this channel
                tm.shutdown().await;
            }.instrument(channel_span));

                orchestrator
                    .register_channel(
                        channel_name.to_string(),
                        jyc_core::channel_orchestrator::ChannelHandle {
                            cancel: cancel.clone(),

                            thread_manager: thread_manager.clone(),

                            channel_info: channel_info.clone(),

                            workspace_dir: workspace_dir.clone(),
                        },
                    )
                    .await;

                tasks.push(task);
            }
            "wecomkf" => {
                let wecomkf_config = channel_config
                    .wecom_kf
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("channel '{channel_name}': missing wecom_kf config")
                    })?
                    .clone();

                let wecom_server = wecom_server
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("WeCom webhook server not initialized"))?;
                let router_for_callback = router.clone();
                let channel_name_owned = channel_name.clone();

                let kf_client = wecomkf_kf_client.clone().ok_or_else(|| {
                    anyhow::anyhow!("KfApiClient not initialized for wecomkf channel")
                })?;

                let cursor_store = Arc::new(KfCursorStore::new(
                    wecomkf_config
                        .cursor_store_path
                        .as_ref()
                        .map(std::path::PathBuf::from),
                ));
                let dedup_store = Arc::new(KfDedupStore::new());

                let thread_manager_for_task = thread_manager.clone();

                let task = tokio::spawn(async move {

                let adapter = WecomKfInboundAdapter::new(
                    &wecomkf_config,
                    &channel_name_owned,
                    wecom_server,
                    kf_client,
                    cursor_store,
                    dedup_store,
                );

                let thread_manager_clone = thread_manager_for_task.clone();
                let options = jyc_types::InboundAdapterOptions {
                    on_message: Box::new(move |message| {
                        let router = router_for_callback.clone();

                        tokio::spawn(async move {
                            router.route(&WecomKfMatcher, message).await;
                        });

                        Ok(())
                    }),
                    on_thread_close: Some(Box::new(move |thread_name: String| {
                        let tm = thread_manager_clone.clone();
                        tokio::spawn(async move {
                            if let Err(e) = tm.close_thread(&thread_name).await {
                                tracing::error!(error = %e, thread = %thread_name, "Failed to close thread");
                            }
                        });
                        Ok(())
                    })),
                    on_error: Box::new(|error| {
                        tracing::error!(error = %error, "WeCom KF inbound error");
                    }),
                    attachment_config: inbound_attachment_config.clone(),
                };

                if let Err(e) = adapter.start(options, cancel_child).await {
                    tracing::error!(
                        error = %e,
                        "WeCom KF inbound adapter error"
                    );
                }

                // Shutdown thread manager for this channel
                tm.shutdown().await;
            }.instrument(channel_span));

                orchestrator
                    .register_channel(
                        channel_name.to_string(),
                        jyc_core::channel_orchestrator::ChannelHandle {
                            cancel: cancel.clone(),

                            thread_manager: thread_manager.clone(),

                            channel_info: channel_info.clone(),

                            workspace_dir: workspace_dir.clone(),
                        },
                    )
                    .await;

                tasks.push(task);
            }
            "websocket" => {
                let router_for_callback = router.clone();
                let channel_name_for_matcher = channel_name_owned.clone();

                // The websocket handler was already created when the outbound adapter was built.
                // Find it in the list and start it (sets the on_message callback).
                let handler = websocket_handlers.last().cloned().ok_or_else(|| {
                    anyhow::anyhow!("channel '{channel_name}': websocket handler not found")
                })?;

                let thread_manager_clone = thread_manager.clone();
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
                    on_thread_close: Some(Box::new(move |thread_name: String| {
                        let tm = thread_manager_clone.clone();
                        tokio::spawn(async move {
                            if let Err(e) = tm.close_thread(&thread_name).await {
                                tracing::error!(error = %e, thread = %thread_name, "Failed to close thread");
                            }
                        });
                        Ok(())
                    })),
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
                // But we still need to keep the thread_manager alive, so we push a no-op task
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

                            thread_manager: thread_manager.clone(),

                            channel_info: channel_info.clone(),

                            workspace_dir: workspace_dir.clone(),
                        },
                    )
                    .await;

                tasks.push(task);
            }
            _ => {} // Gracefully skip unknown channel types
        }
        Ok(())
    }
}
