use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use clap::Args;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use jyc_agent::JycAgentService;
use jyc_core::agent::AgentService;
use jyc_core::static_agent::StaticAgentService;

use jyc_channels::email::inbound::EmailMatcher;
use jyc_channels::email::outbound::EmailOutboundAdapter;
use jyc_channels::feishu::inbound::{FeishuInboundAdapter, FeishuMatcher};
use jyc_channels::feishu::outbound::FeishuOutboundAdapter;
use jyc_channels::github::inbound::GithubMatcher;
use jyc_channels::github::outbound::GithubOutboundAdapter;
use jyc_channels::wechat::inbound::WechatInboundAdapter;
use jyc_channels::wechat::outbound::WechatOutboundAdapter;
use jyc_channels::wecom::inbound::WecomInboundAdapter;
use jyc_channels::wecom::outbound::WecomOutboundAdapter;
use jyc_channels::wecom::server::WecomWebhookServer;
use jyc_core::message_router::MessageRouter;
use jyc_core::message_storage::MessageStorage;
use jyc_core::metrics::MetricsCollector;
use jyc_core::state_manager::StateManager;
use jyc_core::thread_manager::ThreadManager;
use jyc_services::imap::monitor::ImapMonitor;
use jyc_types::MonitorConfig;
use jyc_types::OutboundAdapter;
use jyc_types::{load_config, validation};

/// Monitor command — start the agent, monitor inbound channels, process messages.
#[derive(Debug, Args)]
pub struct MonitorArgs {
    /// Config file path (default: config.toml in workdir)
    #[arg(short, long, default_value = "config.toml")]
    pub config: String,

    /// Use polling instead of IMAP IDLE
    #[arg(long)]
    pub no_idle: bool,

    /// Reset monitoring state before starting
    #[arg(long)]
    pub reset: bool,
}

pub async fn run(args: &MonitorArgs, workdir: &Path) -> Result<()> {
    // 1. Load and validate config
    let config_path = workdir.join(&args.config);
    tracing::info!(config = %config_path.display(), "Loading configuration");

    let config = load_config(&config_path)?;
    let errors = validation::validate_config(&config);
    if !errors.is_empty() {
        let msg = errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::bail!("Configuration validation failed:\n{msg}");
    }
    let config = Arc::new(ArcSwap::from_pointee(config));

    // 2. Setup cancellation (Ctrl+C)
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("Received Ctrl+C, shutting down...");
        cancel_clone.cancel();
    });

    // 3. Start metrics collector
    let metrics_collector = MetricsCollector::new(cancel.clone());
    let (metrics_handle, shared_stats, metrics_task) = metrics_collector.start();

    // 4. Process each configured channel
    let mut tasks = Vec::new();
    let mut all_thread_managers: Vec<Arc<ThreadManager>> = Vec::new();
    let mut all_channels: Vec<jyc_types::ChannelInfo> = Vec::new();
    let mut all_workspace_dirs: Vec<std::path::PathBuf> = Vec::new();
    let config_snapshot = config.load();
    let agent_config = Arc::new(config_snapshot.agent.clone());

    // Initialize shared WeCom webhook server (if any wecom channel is configured)
    let has_wecom = config_snapshot
        .channels
        .values()
        .any(|c| c.channel_type == "wecom");
    let wecom_server: Option<Arc<WecomWebhookServer>> = if has_wecom {
        let bind_addr = config_snapshot
            .wecom
            .as_ref()
            .map(|w| w.bind_addr.clone())
            .unwrap_or_else(|| "127.0.0.1:10001".to_string());
        let server = Arc::new(WecomWebhookServer::new(&bind_addr));
        // Use a oneshot channel to detect server startup success/failure
        let (startup_tx, startup_rx) = tokio::sync::oneshot::channel::<Result<()>>();
        let server_for_task = server.clone();
        let cancel_wecom = cancel.clone();
        tokio::spawn(async move {
            let result = server_for_task.start(cancel_wecom).await;
            if let Err(ref e) = result {
                tracing::error!(error = %e, "WeCom webhook server failed to start");
            }
            let _ = startup_tx.send(result);
        });
        // Wait briefly to detect binding failures (port in use, etc.)
        match tokio::time::timeout(std::time::Duration::from_secs(5), startup_rx).await {
            Ok(Ok(Ok(()))) => {
                tracing::info!(bind_addr = %bind_addr, "WeCom webhook server started");
            }
            Ok(Ok(Err(e))) => {
                anyhow::bail!("WeCom webhook server failed to start: {}", e);
            }
            Ok(Err(_)) => {
                // Channel closed without sending — server task panicked
                anyhow::bail!("WeCom webhook server task panicked during startup");
            }
            Err(_) => {
                // Timeout — server is still binding or serving, assume success
                tracing::info!(
                    bind_addr = %bind_addr,
                    "WeCom webhook server startup pending (may be slow to bind)"
                );
            }
        }
        Some(server)
    } else {
        None
    };

    for (channel_name, channel_config) in &config_snapshot.channels {
        let channel_type = channel_config.channel_type.as_str();

        // Workspace directory: always <workdir>/<channel>/workspace/
        let workspace_dir = jyc_core::thread_path::resolve_workspace(workdir, channel_name);
        let storage = Arc::new(MessageStorage::new(&workspace_dir));

        let patterns = channel_config.patterns.clone().unwrap_or_default();

        // Get attachment configuration from unified config
        let outbound_attachment_config = config_snapshot
            .attachments
            .as_ref()
            .and_then(|att| att.outbound.clone());
        let inbound_attachment_config = config_snapshot
            .attachments
            .as_ref()
            .and_then(|att| att.inbound.clone());

        let footer_enabled = channel_config.footer.as_ref().is_none_or(|f| f.enabled);

        // Create the outbound adapter based on channel type
        // For wechat, we need to share the WebSocket sender between inbound and outbound
        let mut wechat_sender_arc: Option<
            std::sync::Arc<tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>>,
        > = None;
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
                    .ok_or_else(|| {
                        anyhow::anyhow!("channel '{channel_name}': missing feishu config")
                    })?
                    .clone();
                Arc::new(FeishuOutboundAdapter::new_with_attachments(
                    feishu_config,
                    storage.clone(),
                    outbound_attachment_config,
                    footer_enabled,
                ))
            }
            "github" => {
                let github_config = channel_config
                    .github
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("channel '{channel_name}': missing github config")
                    })?
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
                wechat_sender_arc = Some(adapter.sender_arc());
                Arc::new(adapter)
            }
            "wecom" => {
                let wecom_config = channel_config
                    .wecom
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("channel '{channel_name}': missing wecom config")
                    })?
                    .clone();
                Arc::new(WecomOutboundAdapter::new_with_attachments(
                    wecom_config.webhook_url,
                    storage.clone(),
                    outbound_attachment_config,
                    footer_enabled,
                ))
            }
            other => {
                tracing::warn!(
                    channel = %channel_name,
                    channel_type = %other,
                    "Unsupported channel type, skipping"
                );
                continue;
            }
        };

        // Connect the outbound adapter
        outbound
            .connect()
            .await
            .with_context(|| format!("channel '{channel_name}': outbound connection failed"))?;
        tracing::info!(channel = %channel_name, channel_type = %channel_type, "Outbound connected");

        // Create agent based on configured mode
        // Resolve effective model per-channel: channel-level override beats global
        let effective_model = channel_config
            .model
            .clone()
            .or_else(|| agent_config.model.clone());
        let effective_small_model = channel_config
            .small_model
            .clone()
            .or_else(|| agent_config.small_model.clone());
        let agent: Arc<dyn AgentService> = match agent_config.mode.as_str() {
            "agent" => {
                // In-process agent
                let model = effective_model;
                tracing::info!(channel = %channel_name, model = ?model, "Using agent: jyc-agent (in-process)");
                let providers = agent_config
                    .providers
                    .iter()
                    .map(|(name, def)| {
                        let models = def
                            .models
                            .iter()
                            .map(|(model_name, model_def)| {
                                (
                                    model_name.clone(),
                                    jyc_agent::types::ModelConfig {
                                        context_window: model_def.context_window,
                                        supports_images: model_def.supports_images,
                                        params: model_def.params.clone(),
                                    },
                                )
                            })
                            .collect();
                        (
                            name.clone(),
                            jyc_agent::types::ProviderConfig {
                                provider_type: def.provider_type.clone(),
                                base_url: def.base_url.clone(),
                                api_key_env: def.api_key_env.clone(),
                                context_window: def.context_window,
                                supports_images: def.supports_images,
                                params: def.params.clone(),
                                models,
                            },
                        )
                    })
                    .collect();
                let agent_cfg = jyc_agent::types::AgentConfig {
                    model,
                    small_model: effective_small_model.clone(),
                    providers,
                    max_iterations: agent_config.max_iterations,
                    vision: agent_config
                        .vision
                        .clone()
                        .map(|v| jyc_agent::types::VisionConfig {
                            enabled: v.enabled,
                            provider: v.provider,
                            model: v.model,
                            prompt: v.prompt,
                        }),
                };
                // Use only this channel's patterns so per-pattern overrides
                // (model, small_model, mcps, disabled_builtin_tools,
                // inject_inbound_images) are resolved deterministically.
                // Previously, patterns were flattened from every channel and
                // find(|p| p.name == name) could return a different channel's
                // pattern with the same name (HashMap iteration order is
                // non-deterministic), causing per-pattern overrides to be
                // silently ignored.
                let channel_patterns = patterns.clone();
                // Build optional VisionClient for vision fallback
                let vision_client: Option<std::sync::Arc<jyc_agent::vision::VisionClient>> = {
                    agent_config
                        .vision
                        .as_ref()
                        .filter(|v| v.enabled)
                        .and_then(|v| {
                            let provider_def = agent_config.providers.get(&v.provider)?;
                            let base_url = provider_def
                                .base_url
                                .clone()
                                .unwrap_or_else(|| "https://api.deepseek.com".to_string());
                            let api_key_env = provider_def
                                .api_key_env
                                .clone()
                                .unwrap_or_else(|| "DEEPSEEK_API_KEY".to_string());
                            let api_key = std::env::var(&api_key_env).unwrap_or_default();
                            if api_key.is_empty() {
                                tracing::warn!(
                                    provider = %v.provider,
                                    api_key_env = %api_key_env,
                                    "Vision fallback: API key not found in environment"
                                );
                                return None;
                            }
                            Some(std::sync::Arc::new(jyc_agent::vision::VisionClient::new(
                                base_url,
                                api_key,
                                v.model.clone(),
                                v.prompt.clone(),
                            )))
                        })
                };
                Arc::new(JycAgentService::new(
                    agent_cfg,
                    workdir.to_path_buf(),
                    config_snapshot.mcps.clone(),
                    channel_patterns,
                    inbound_attachment_config.clone(),
                    vision_client,
                ))
            }
            "static" => {
                let text = agent_config
                    .text
                    .as_deref()
                    .unwrap_or("Thank you for your message.");
                Arc::new(StaticAgentService::new(text))
            }
            other => {
                anyhow::bail!("unsupported agent mode: '{other}'");
            }
        };

        let template_dir = workdir.join("templates");

        let thread_manager = Arc::new(ThreadManager::new_with_options(
            config_snapshot.general.max_concurrent_threads,
            config_snapshot.general.max_queue_size_per_thread,
            storage.clone(),
            outbound.clone(),
            agent,
            cancel.clone(),
            true, // enable_events: true for Thread Event system
            template_dir,
            config.clone(),
            channel_name.clone(),
            workspace_dir.clone(),
            metrics_handle.clone(),
        ));

        // Collect for inspect server
        all_thread_managers.push(thread_manager.clone());
        all_channels.push(jyc_types::ChannelInfo {
            name: channel_name.clone(),
            channel_type: channel_type.to_string(),
            active_workers: 0,
            max_concurrent: 0,
        });
        all_workspace_dirs.push(workspace_dir);

        let router = Arc::new(MessageRouter::new(thread_manager.clone(), storage.clone()));

        let mut state_manager = StateManager::for_channel(workdir, channel_name);
        state_manager.initialize().await?;

        if args.reset {
            state_manager.reset().await?;
            tracing::info!(channel = %channel_name, "State reset");
        }

        tracing::info!(
            channel = %channel_name,
            channel_type = %channel_type,
            mode = %agent_config.mode,
            last_seq = state_manager.last_sequence_number(),
            processed_uids = state_manager.processed_uid_count(),
            "State loaded"
        );

        // Spawn the inbound monitor as a task (channel-type-specific)
        let cancel_child = cancel.clone();
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
                            patterns,
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

                let patterns_for_callback = patterns.clone();
                let router_for_callback = router.clone();

                let task = tokio::spawn(async move {
                    // Clone configs before moving into closures
                    let feishu_config_cloned = feishu_config.clone();
                    let inbound_attachment_config_for_callback = inbound_attachment_config.clone();

                    let adapter = FeishuInboundAdapter::new(&feishu_config_cloned, channel_name_owned.clone());

                    // Wire on_message to route through FeishuMatcher → MessageRouter

                    use jyc_types::InboundAdapter;

                    let thread_manager_clone = thread_manager.clone();
                    let options = jyc_types::InboundAdapterOptions {
                        on_message: Box::new(move |mut message| {
                            let router = router_for_callback.clone();
                            let patterns = patterns_for_callback.clone();

                            // Clone values for the async move closure

                            let feishu_config_clone = feishu_config_cloned.clone();
                            let channel_name_clone = channel_name_owned.clone();
                            let attachment_config_clone = inbound_attachment_config_for_callback.clone();

                            tokio::spawn(async move {
                                // 1. Create adapter and save attachments to thread directory


                                // The adapter will calculate thread name internally

                                let adapter = FeishuInboundAdapter::new(&feishu_config_clone, channel_name_clone);

                                if let Err(e) = adapter.save_attachments_to_thread_directory(
                                    &mut message,
                                    &patterns,
                                    attachment_config_clone.as_ref(),
                                ).await {
                                    tracing::warn!("Failed to save attachments: {}", e);
                                }

                                // 2. Route the message

                                router.route(&FeishuMatcher, message, &patterns).await;
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

                let patterns_for_callback = patterns.clone();
                let patterns_for_adapter = patterns.clone();
                let router_for_callback = router.clone();
                let workdir_owned = workdir.to_path_buf();

                let task = tokio::spawn(async move {
                    use jyc_channels::github::inbound::GithubInboundAdapter;
                    use jyc_types::InboundAdapter;

                    let adapter = GithubInboundAdapter::new(&github_config, channel_name_owned.clone(), &workdir_owned)
                        .with_patterns(patterns_for_adapter);

                    let thread_manager_clone = thread_manager.clone();
                    let options = jyc_types::InboundAdapterOptions {
                        on_message: Box::new(move |message| {
                            let router = router_for_callback.clone();
                            let patterns = patterns_for_callback.clone();

                            tokio::spawn(async move {
                                router.route(&GithubMatcher, message, &patterns).await;
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

                let patterns_for_callback = patterns.clone();
                let router_for_callback = router.clone();
                let wechat_sender_arc_clone = wechat_sender_arc.clone().unwrap();

                let task = tokio::spawn(async move {
                    use jyc_types::InboundAdapter;
                    use jyc_channels::wechat::inbound::WechatMatcher;

                    // Create the adapter with the shared sender Arc so it can
                    // update the outbound sender on each reconnection.
                    let adapter = WechatInboundAdapter::with_shared_sender(
                        &wechat_config,
                        channel_name_owned.clone(),
                        wechat_sender_arc_clone,
                    );

                    let thread_manager_clone = thread_manager.clone();
                    let options = jyc_types::InboundAdapterOptions {
                        on_message: Box::new(move |message| {
                            let router = router_for_callback.clone();
                            let patterns = patterns_for_callback.clone();

                            // NOTE on attachments: the WeChat parser
                            // populates `message.attachments` (with
                            // inline `Vec<u8>` content) inside
                            // `WechatWebSocket::handle_incoming`. We do
                            // NOT save them to disk here — the canonical
                            // post-route saver in
                            // `thread_manager::process_message`
                            // (`save_attachments_to_dir`) does that
                            // AFTER thread name resolution, ensuring
                            // attachments land in the same directory
                            // the agent thread actually runs in. A
                            // pre-route save here would just produce a
                            // duplicate copy under a sibling directory.
                            tokio::spawn(async move {
                                router.route(&WechatMatcher, message, &patterns).await;
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
                let patterns_for_callback = patterns.clone();
                let router_for_callback = router.clone();
                let channel_name_owned = channel_name.clone();

                let task = tokio::spawn(async move {
                    use jyc_types::InboundAdapter;
                    use jyc_channels::wecom::inbound::WecomMatcher;

                    let adapter = WecomInboundAdapter::new(
                        &wecom_config,
                        &channel_name_owned,
                        wecom_server,
                    );

                    let thread_manager_clone = thread_manager.clone();
                    let options = jyc_types::InboundAdapterOptions {
                        on_message: Box::new(move |message| {
                            let router = router_for_callback.clone();
                            let patterns = patterns_for_callback.clone();

                            tokio::spawn(async move {
                                router.route(&WecomMatcher, message, &patterns).await;
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

                tasks.push(task);
            }
            _ => unreachable!(), // Already handled above with continue
        }
    }

    if tasks.is_empty() {
        anyhow::bail!("No channels configured");
    }

    // 5. Start inspect server (if configured)
    let inspect_task = if config_snapshot.inspect.as_ref().is_some_and(|i| i.enabled) {
        let inspect_config = config_snapshot.inspect.as_ref().unwrap();
        let activity_map: jyc_inspect::server::SharedActivityMap =
            Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let context = Arc::new(jyc_inspect::server::InspectContext {
            thread_managers: all_thread_managers.clone(),
            channels: all_channels,
            health_stats: shared_stats,
            activity_map: activity_map.clone(),
            start_time: std::time::Instant::now(),
            config_path: Some(config_path.clone()),
            config: Some(config.clone()),
            workspace_dirs: all_workspace_dirs.clone(),
        });

        // Start activity tracker (subscribes to thread event buses)
        let _activity_task = jyc_inspect::server::ActivityTracker::start(
            all_thread_managers,
            activity_map,
            all_workspace_dirs,
            cancel.clone(),
        );

        let server = jyc_inspect::server::InspectServer::new(
            inspect_config.bind.clone(),
            context,
            cancel.clone(),
        );
        Some(server.start())
    } else {
        None
    };

    tracing::info!(
        channels = tasks.len(),
        "Monitor started, press Ctrl+C to stop"
    );

    // Wait for all channel tasks to complete
    for task in tasks {
        task.await.ok();
    }

    // Wait for inspect server to stop
    if let Some(task) = inspect_task {
        task.await.ok();
    }

    // Wait for metrics collector to stop
    metrics_task.await.ok();

    tracing::info!("Monitor stopped");
    Ok(())
}
