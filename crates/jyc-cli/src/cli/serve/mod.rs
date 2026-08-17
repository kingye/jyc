use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// RAII guard that removes a PID file on drop.
use jyc_agent::JycAgentService;

use jyc_services::job_scheduler::JobScheduler;
use std::collections::HashMap;

use jyc_channels::websocket::inbound::WebsocketInboundAdapter;
use jyc_channels::wecom::kf_client::KfApiClient;
use jyc_channels::wecom::server::WecomWebhookServer;
use jyc_channels::wecom_bot::client::WecomBotConnectionHandle;
use jyc_core::message_router::MessageRouter;
use jyc_core::message_storage::MessageStorage;
use jyc_core::metrics::MetricsCollector;
use jyc_core::state_manager::StateManager;
use jyc_core::topic_manager::TopicManager;
use jyc_types::OutboundAdapter;
use jyc_types::{load_config_layered, validation};

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
    // Collect JycAgentService instances for wiring cross-channel topic managers
    let mut all_agent_services: Vec<Arc<JycAgentService>> = Vec::new();
    // Collect outbound adapters keyed by channel name for cross-channel messaging
    let mut all_outbounds: Vec<(String, Arc<dyn OutboundAdapter>)> = Vec::new();

    let orchestrator = Arc::new(jyc_core::channel_orchestrator::ChannelOrchestrator::new(
        config.clone(),
        workdir,
    ));

    let config_snapshot = config.load();
    let agent_config = Arc::new(config_snapshot.ai.clone());
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

    // Generate and persist the inspect auth token BEFORE spawning channels:
    // piped-channel reply forwarders (e.g. feishu) read this file at startup
    // to build their inspect client — writing it later would race them and
    // leave them holding a stale (or no) token, failing every attachment
    // relay with 401 until the next restart.
    let inspect_auth_token: Option<String> =
        if config_snapshot.inspect.as_ref().is_some_and(|i| i.enabled) {
            let auth_token = jyc_utils::auth_token::generate_token();
            let token_path = jyc_utils::auth_token::token_path(workdir);
            jyc_utils::auth_token::write_token(&token_path, &auth_token).with_context(|| {
                format!(
                    "Failed to write authorization token to {}. \
                     Dashboard will not be able to connect. Fix the path \
                     and rerun `jyc serve`.",
                    token_path.display()
                )
            })?;
            tracing::info!(
                path = %token_path.display(),
                "Authorization token written; retrieve with `jyc token show`"
            );
            Some(auth_token)
        } else {
            None
        };

    // Collect websocket inbound adapters to register with the inspect server later
    let mut websocket_handlers: Vec<Arc<WebsocketInboundAdapter>> = vec![];
    // Map for setting TopicManager on websocket handlers after creation
    let mut ws_handler_for_channel: HashMap<String, Arc<WebsocketInboundAdapter>> = HashMap::new();
    // Per-channel websocket broadcast senders, keyed by channel name. Used by
    // piped channels (e.g. feishu with `pipe = "local_dev"`) to receive the
    // target channel's replies.
    let ws_broadcasts: std::sync::Arc<
        std::sync::Mutex<HashMap<String, broadcast::Sender<String>>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
    // Per-channel MessageRouters, keyed by channel name. Used by piped
    // channels (e.g. feishu with `pipe`) to route re-targeted messages
    // through the target channel's router (identical to a chat-pane message).
    let routers: std::sync::Arc<std::sync::Mutex<HashMap<String, Arc<MessageRouter>>>> =
        std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
    // Create the inspect-broadcast bus that ActivityTracker publishes to.
    // Shared with websocket inbound adapters so they can forward live
    // activity/thinking events to dashboard WebSocket clients.
    let inspect_broadcast: Arc<tokio::sync::broadcast::Sender<String>> =
        Arc::new(tokio::sync::broadcast::channel(256).0);

    for (channel_name, channel_config) in &config_snapshot.channels {
        let channel_type = channel_config.channel_type.as_str();

        // Get attachment configuration from unified config
        let inbound_attachment_config = config_snapshot
            .attachments
            .as_ref()
            .and_then(|att| att.inbound.clone());

        // Feishu is a pipe-only adapter (see docs/core-hub-adapters.md):
        // skip the full channel construction (outbound/agent/TopicManager/
        // StateManager/orchestrator) — spawn only the inbound adapter plus
        // pipe reply forwarders.
        if channel_type == "feishu" {
            crate::cli::serve::channels::spawn_feishu_adapter(
                channel_config,
                channel_name.clone(),
                workdir,
                inbound_attachment_config,
                cancel.clone(),
                &mut tasks,
                config_for_spawn.clone(),
                ws_broadcasts.clone(),
                routers.clone(),
            )?;
            continue;
        }

        // Workspace directory: always <workdir>/<channel>/workspace/
        let workspace_dir = jyc_core::topic_path::resolve_workspace(workdir, channel_name);
        let storage = Arc::new(MessageStorage::new(&workspace_dir));

        let patterns = channel_config.patterns.clone().unwrap_or_default();

        let outbound_attachment_config = config_snapshot
            .attachments
            .as_ref()
            .and_then(|att| att.outbound.clone());

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
        let Some(outbound) = crate::cli::serve::channels::build_outbound_adapter(
            channel_type,
            channel_config,
            channel_name,
            storage.clone(),
            outbound_attachment_config,
            footer_enabled,
            &workspace_dir,
            inspect_broadcast.clone(),
            &mut wechat_sender_arc,
            &mut wecom_bot_handle_arc,
            &mut wecomkf_kf_client,
            &mut ws_handler_for_channel,
            &mut websocket_handlers,
            ws_broadcasts.clone(),
        )?
        else {
            continue;
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
            config.clone(),
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
        // Topic-level (L3) .jyc/templates/ is checked first at lookup time.
        let template_dirs = jyc_core::template_dirs::TemplateDirs::new(
            [
                jyc_utils::paths::global_templates_dir(),
                Some(workdir.join("templates")),
            ]
            .into_iter()
            .flatten()
            .collect(),
        );

        let topic_manager = Arc::new(TopicManager::new_with_options(
            config_snapshot.general.max_concurrent_topics,
            config_snapshot.general.max_queue_size_per_topic,
            storage.clone(),
            outbound.clone(),
            agent,
            cancel.clone(),
            true, // enable_events: true for Topic Event system
            template_dirs,
            config.clone(),
            channel_name.clone(),
            channel_type.to_string(),
            workdir.to_path_buf(),
            workspace_dir.clone(),
            metrics_handle.clone(),
            Some(config_path.clone()),
        ));

        // Wire topic_manager to websocket handler for custom topic_path resolution
        if let Some(ws_handler) = ws_handler_for_channel.get(channel_name) {
            ws_handler.set_topic_manager(topic_manager.clone());
        }

        // Collect for inspect server
        let channel_info = jyc_types::ChannelInfo {
            name: channel_name.clone(),
            channel_type: channel_type.to_string(),
            active_workers: 0,
            max_concurrent: 0,
        };

        let router = Arc::new(MessageRouter::new(
            topic_manager.clone(),
            storage.clone(),
            config.clone(),
            channel_name.clone(),
        ));
        // Expose the router so piped channels can route through it.
        routers
            .lock()
            .unwrap()
            .insert(channel_name.clone(), router.clone());

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
        let _channel_name_owned = channel_name.clone();
        let _tm = topic_manager.clone();
        let _channel_span = tracing::info_span!("in", ch = %channel_name);

        // NOTE: unsupported channel types are skipped inside spawn() (the
        // original inline `continue`); keep this call the LAST statement of
        // the loop body so the skip is equivalent.
        let spawner = crate::cli::serve::channels::InboundSpawner {
            channel_type,
            channel_config,
            channel_name: channel_name.clone(),
            workdir,
            workspace_dir: workspace_dir.clone(),
            args,
            inbound_attachment_config,
            topic_manager: topic_manager.clone(),
            router: router.clone(),
            state_manager,
            cancel: cancel.clone(),
            cancel_child,
            tasks: &mut tasks,
            wechat_sender_arc: &mut wechat_sender_arc,
            wecom_bot_handle_arc: &mut wecom_bot_handle_arc,
            wecomkf_kf_client: &mut wecomkf_kf_client,
            orchestrator: orchestrator.clone(),
            channel_info,
            config_for_spawn: config_for_spawn.clone(),
            wecom_server: wecom_server.clone(),
            websocket_handlers: &mut websocket_handlers,
        };
        spawner.spawn().await?;
    }

    if tasks.is_empty() {
        anyhow::bail!("No channels configured");
    }

    // 5.5. Wire cross-channel topic managers and outbound adapters into agent services
    {
        let tms = orchestrator.topic_managers().load();
        let tm_map: HashMap<String, Arc<TopicManager>> = tms
            .iter()
            .map(|tm| (tm.channel_name().to_string(), tm.clone()))
            .collect();
        let tm_map = Arc::new(tokio::sync::Mutex::new(tm_map));
        for svc in &all_agent_services {
            svc.set_topic_managers(tm_map.clone());
        }
        tracing::info!(
            "Wired topic managers into {} agent service(s)",
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
                scheduler_config.max_jobs_per_topic,
                true,
            );

            let scheduler_cancel = cancel.clone();
            tasks.push(tokio::spawn(async move {
                scheduler.run(scheduler_cancel).await;
            }));

            tracing::info!("Job scheduler started");
        }
    }

    // 6. Start inspect server (if configured). The auth token was already
    // generated and persisted before the channel spawn loop above.
    let inspect_task = if let Some(auth_token) = inspect_auth_token {
        let inspect_config = config_snapshot.inspect.as_ref().unwrap();
        let activity_map: jyc_inspect::server::SharedActivityMap =
            Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let context = Arc::new(jyc_inspect::server::InspectContext {
            topic_managers: orchestrator.topic_managers(),
            channels: orchestrator.channel_infos(),
            health_stats: shared_stats,
            activity_map: activity_map.clone(),
            start_time: std::time::Instant::now(),
            config_path: Some(config_path.clone()),
            global_config_path: global_config_path.clone(),
            config: Some(Arc::clone(&config)),
            workspace_dirs: orchestrator.workspace_dirs(),
            websocket_handlers: {
                let handlers: HashMap<String, jyc_inspect::server::DynWebsocketHandler> =
                    websocket_handlers
                        .into_iter()
                        .map(|h| {
                            (
                                h.channel_name().to_string(),
                                h as jyc_inspect::server::DynWebsocketHandler,
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
            inspect_broadcast: inspect_broadcast.clone(),
            auth_token: Some(auth_token),
        });

        // Restore custom topic_path mappings from disk so topics with
        // non-default paths survive process restarts.
        {
            let tms = orchestrator.topic_managers().load();
            for tm in tms.iter() {
                tm.restore_custom_topic_paths().await;
            }
        }

        // Start activity tracker (subscribes to topic event buses)
        let _activity_task = jyc_inspect::server::ActivityTracker::start(
            context.topic_managers.clone(),
            activity_map,
            context.workspace_dirs.clone(),
            context.inspect_broadcast.clone(),
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

mod channels;
mod shutdown;

pub use shutdown::ServeArgs;
use shutdown::{PidFileGuard, shutdown_signal};
