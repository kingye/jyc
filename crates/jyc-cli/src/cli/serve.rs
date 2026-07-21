use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use clap::Args;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

/// RAII guard that removes a PID file on drop.
struct PidFileGuard {
    path: PathBuf,
}

impl PidFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

use jyc_agent::JycAgentService;

use jyc_services::job_scheduler::JobScheduler;
use std::collections::HashMap;

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
use jyc_channels::wecom::kf_inbound::WecomKfInboundAdapter;
use jyc_channels::wecom::kf_inbound::WecomKfMatcher;
use jyc_channels::wecom::kf_outbound::WecomKfOutboundAdapter;
use jyc_channels::wecom::outbound::WecomOutboundAdapter;
use jyc_channels::wecom::server::WecomWebhookServer;
use jyc_channels::wecom::token_cache::AccessTokenCache;
use jyc_channels::wecom_bot::client::WecomBotConnectionHandle;
use jyc_channels::wecom_bot::inbound::{WecomBotInboundAdapter, WecomBotMatcher};
use jyc_channels::wecom_bot::outbound::WecomBotOutboundAdapter;
use jyc_core::message_router::MessageRouter;
use jyc_core::message_storage::MessageStorage;
use jyc_core::metrics::MetricsCollector;
use jyc_core::state_manager::StateManager;
use jyc_core::thread_manager::ThreadManager;
use jyc_services::imap::monitor::ImapMonitor;
use jyc_types::InboundAdapter;
use jyc_types::MonitorConfig;
use jyc_types::OutboundAdapter;
use jyc_types::{load_config_layered, validation};

/// Serve command — start the agent, monitor inbound channels, process messages.
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Config file path (default: <config_home>/config.toml, e.g.
    /// ~/.config/jyc/config.toml; or config.toml in --workdir when given)
    #[arg(short, long)]
    pub config: Option<String>,

    /// Use polling instead of IMAP IDLE
    #[arg(long)]
    pub no_idle: bool,

    /// Reset monitoring state before starting
    #[arg(long)]
    pub reset: bool,
}

/// Wait for a shutdown signal (Ctrl+C on all platforms, plus SIGTERM on Unix).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to create SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Received Ctrl+C, shutting down...");
            }
            _ = sigterm.recv() => {
                tracing::info!("Received SIGTERM, shutting down...");
            }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("Received Ctrl+C, shutting down...");
    }
}

pub async fn run(args: &ServeArgs, workdir: &Path, workdir_explicit: bool) -> Result<()> {
    // 1. Resolve config locations, provision default config on first run
    let resolution =
        super::resolve::resolve_config(workdir, args.config.as_deref(), workdir_explicit)?;
    if super::resolve::provision_default_config(&resolution).await? {
        return Ok(());
    }

    // 2. Load (layered: global base + workdir overlay) and validate config
    let config_path = resolution.config_path.clone();
    let global_config_path = resolution.global_config_path.clone();
    tracing::info!(
        config = %config_path.display(),
        global = ?global_config_path,
        "Loading configuration"
    );

    let config = load_config_layered(global_config_path.as_deref(), &config_path)?;
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

    // 3. Setup cancellation (Ctrl+C and SIGTERM)
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        cancel_clone.cancel();
    });

    // Write PID file for `jyc stop` command (removed automatically on drop)
    let pid_path = workdir.join("jyc.pid");
    let _pid_guard = if let Err(e) =
        tokio::fs::write(&pid_path, std::process::id().to_string()).await
    {
        tracing::warn!(path = %pid_path.display(), error = %e, "Failed to write PID file");
        None
    } else {
        tracing::info!(pid = std::process::id(), path = %pid_path.display(), "PID file written");
        Some(PidFileGuard::new(pid_path))
    };

    // 4. Start metrics collector
    let metrics_collector = MetricsCollector::new(cancel.clone());
    let (metrics_handle, shared_stats, metrics_task) = metrics_collector.start();

    // 5. Process each configured channel
    let mut tasks = Vec::new();
    // Collect JycAgentService instances for wiring cross-channel thread managers
    let mut all_agent_services: Vec<Arc<JycAgentService>> = Vec::new();
    // Collect outbound adapters keyed by channel name for cross-channel messaging
    let mut all_outbounds: Vec<(String, Arc<dyn OutboundAdapter>)> = Vec::new();

    let orchestrator = Arc::new(jyc_core::channel_orchestrator::ChannelOrchestrator::new(
        config.clone(),
        workdir,
    ));

    let config_snapshot = config.load();
    let agent_config = Arc::new(config_snapshot.agent.clone());
    let config_for_spawn = Arc::clone(&config);

    // Initialize shared WeCom webhook server (if any wecom or wecomkf channel is configured)
    let has_wecom = config_snapshot
        .channels
        .values()
        .any(|c| c.channel_type == "wecom" || c.channel_type == "wecomkf");
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

    // Collect websocket inbound adapters to register with the inspect server later
    let mut websocket_handlers: Vec<Arc<WebsocketInboundAdapter>> = vec![];
    // Map for setting ThreadManager on websocket handlers after creation
    let mut ws_handler_for_channel: HashMap<String, Arc<WebsocketInboundAdapter>> = HashMap::new();

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
        // For wecom_bot, we share the WebSocket connection handle between inbound and outbound
        let mut wecom_bot_handle_arc: Option<
            std::sync::Arc<tokio::sync::Mutex<Option<WecomBotConnectionHandle>>>,
        > = None;
        // For wecomkf, we share the KfApiClient between inbound and outbound
        let mut wecomkf_kf_client: Option<Arc<KfApiClient>> = None;
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
            "gitee" => {
                let gitee_config = channel_config
                    .gitee
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("channel '{channel_name}': missing gitee config")
                    })?
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
            "wecom_bot" => {
                let adapter = WecomBotOutboundAdapter::new_with_attachments(
                    storage.clone(),
                    outbound_attachment_config,
                    footer_enabled,
                );
                wecom_bot_handle_arc = Some(adapter.handle_arc());
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
                wecomkf_kf_client = Some(kf_client.clone());

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
                let mut handler = WebsocketInboundAdapter::new(
                    channel_name.to_string(),
                    Some(config.clone()),
                    broadcast_tx,
                );
                handler.set_workspace_dir(workspace_dir.clone());
                let handler = Arc::new(handler);
                ws_handler_for_channel.insert(channel_name.clone(), handler.clone());
                websocket_handlers.push(handler);
                Arc::new(adapter)
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

        // Collect outbound adapter for cross-channel messaging
        all_outbounds.push((channel_name.clone(), outbound.clone()));

        // Create agent based on configured mode
        let agent_result = crate::cli::agent_builder::build_agent_service(
            &agent_config,
            channel_config,
            workdir,
            outbound.clone(),
            patterns.clone(),
            config_snapshot.mcps.clone(),
            inbound_attachment_config.clone(),
            channel_name,
        )?;
        if let Some(ref jyc_svc) = agent_result.jyc_agent {
            all_agent_services.push(jyc_svc.clone());
        }
        let agent = agent_result.agent;

        // Layered template dirs (low → high priority): L1 global < L2 workdir.
        // Thread-level (L3) .jyc/templates/ is checked first at lookup time.
        let template_dirs = jyc_core::template_dirs::TemplateDirs::new(
            [
                jyc_utils::paths::global_templates_dir(),
                Some(workdir.join("templates")),
            ]
            .into_iter()
            .flatten()
            .collect(),
        );

        let thread_manager = Arc::new(ThreadManager::new_with_options(
            config_snapshot.general.max_concurrent_threads,
            config_snapshot.general.max_queue_size_per_thread,
            storage.clone(),
            outbound.clone(),
            agent,
            cancel.clone(),
            true, // enable_events: true for Thread Event system
            template_dirs,
            config.clone(),
            channel_name.clone(),
            channel_type.to_string(),
            workdir.to_path_buf(),
            workspace_dir.clone(),
            metrics_handle.clone(),
        ));

        // Wire thread_manager to websocket handler for custom thread_path resolution
        if let Some(ws_handler) = ws_handler_for_channel.get(channel_name) {
            ws_handler.set_thread_manager(thread_manager.clone());
        }

        // Collect for inspect server
        let channel_info = jyc_types::ChannelInfo {
            name: channel_name.clone(),
            channel_type: channel_type.to_string(),
            active_workers: 0,
            max_concurrent: 0,
        };

        let router = Arc::new(MessageRouter::new(
            thread_manager.clone(),
            storage.clone(),
            config.clone(),
            channel_name.clone(),
        ));

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

                    use jyc_types::InboundAdapter;

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
                    use jyc_types::InboundAdapter;

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
                    use jyc_types::InboundAdapter;

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
                    use jyc_types::InboundAdapter;
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
                    use jyc_types::InboundAdapter;

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
                    use jyc_types::InboundAdapter;
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
                    use jyc_types::InboundAdapter;

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
            _ => continue, // Gracefully skip unknown channel types
        }
    }

    if tasks.is_empty() {
        anyhow::bail!("No channels configured");
    }

    // 5.5. Wire cross-channel thread managers and outbound adapters into agent services
    {
        let tms = orchestrator.thread_managers().load();
        let tm_map: HashMap<String, Arc<ThreadManager>> = tms
            .iter()
            .map(|tm| (tm.channel_name().to_string(), tm.clone()))
            .collect();
        let tm_map = Arc::new(tokio::sync::Mutex::new(tm_map));
        for svc in &all_agent_services {
            svc.set_thread_managers(tm_map.clone());
        }
        tracing::info!(
            "Wired thread managers into {} agent service(s)",
            all_agent_services.len()
        );

        // Build and inject outbound adapters map
        let outbounds_map: HashMap<String, Arc<dyn OutboundAdapter>> =
            all_outbounds.into_iter().collect();
        let outbounds_map = Arc::new(tokio::sync::Mutex::new(outbounds_map));
        for svc in &all_agent_services {
            svc.set_outbounds(outbounds_map.clone());
        }
        tracing::info!(
            "Wired outbound adapters into {} agent service(s)",
            all_agent_services.len()
        );

        // Start JobScheduler (if scheduler is enabled in config)
        let scheduler_config = config_snapshot.scheduler.clone();
        if scheduler_config.enabled {
            let workspace_dirs = orchestrator.workspace_dirs().load();
            let workspace_dirs: Vec<std::path::PathBuf> = workspace_dirs.iter().cloned().collect();
            let scheduler = JobScheduler::new(
                tm_map,
                workspace_dirs,
                scheduler_config.scan_interval_secs,
                scheduler_config.max_jobs_per_thread,
                true,
            );

            let scheduler_cancel = cancel.clone();
            tasks.push(tokio::spawn(async move {
                scheduler.run(scheduler_cancel).await;
            }));

            tracing::info!("Job scheduler started");
        }
    }

    // 6. Start inspect server (if configured)
    let inspect_task = if config_snapshot.inspect.as_ref().is_some_and(|i| i.enabled) {
        let inspect_config = config_snapshot.inspect.as_ref().unwrap();
        let activity_map: jyc_inspect::server::SharedActivityMap =
            Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let context = Arc::new(jyc_inspect::server::InspectContext {
            thread_managers: orchestrator.thread_managers(),
            channels: orchestrator.channel_infos(),
            health_stats: shared_stats,
            activity_map: activity_map.clone(),
            start_time: std::time::Instant::now(),
            config_path: Some(config_path.clone()),
            global_config_path: global_config_path.clone(),
            config: Some(Arc::clone(&config)),
            workspace_dirs: orchestrator.workspace_dirs(),
            websocket_handlers: {
                let handlers: HashMap<String, Arc<dyn jyc_inspect::server::WebsocketHandler>> =
                    websocket_handlers
                        .into_iter()
                        .map(|h| {
                            (
                                h.channel_name().to_string(),
                                h as Arc<dyn jyc_inspect::server::WebsocketHandler>,
                            )
                        })
                        .collect();
                if handlers.is_empty() {
                    None
                } else {
                    Some(handlers)
                }
            },
            reload_callback: {
                let orch = orchestrator.clone();
                Some(Arc::new(move || {
                    let orch = orch.clone();
                    let fut: Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> =
                        Box::pin(async move { orch.reload().await });
                    fut
                }) as jyc_inspect::server::ReloadCallback)
            },
        });

        // Restore custom thread_path mappings from disk so threads with
        // non-default paths survive process restarts.
        {
            let tms = orchestrator.thread_managers().load();
            for tm in tms.iter() {
                tm.restore_custom_thread_paths().await;
            }
        }

        // Start activity tracker (subscribes to thread event buses)
        let _activity_task = jyc_inspect::server::ActivityTracker::start(
            context.thread_managers.clone(),
            activity_map,
            context.workspace_dirs.clone(),
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
        "Serve started, press Ctrl+C to stop"
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

    tracing::info!("Serve stopped");
    Ok(())
}
