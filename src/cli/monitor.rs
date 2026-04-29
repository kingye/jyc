use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use clap::Args;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::services::agent::AgentService;
use crate::services::opencode::OpenCodeServer;
use crate::services::opencode::service::OpenCodeService;
use crate::services::static_agent::StaticAgentService;

use crate::channels::email::outbound::EmailOutboundAdapter;
use crate::channels::feishu::inbound::{FeishuInboundAdapter, FeishuMatcher};
use crate::channels::feishu::outbound::FeishuOutboundAdapter;
use crate::channels::github::inbound::GithubMatcher;
use crate::channels::github::outbound::GithubOutboundAdapter;
use crate::channels::types::OutboundAdapter;
use crate::config::types::MonitorConfig;
use crate::config::{load_config, validation};
use crate::core::metrics::MetricsCollector;
use crate::core::message_router::MessageRouter;
use crate::core::message_storage::MessageStorage;
use crate::core::state_manager::StateManager;
use crate::core::thread_manager::ThreadManager;
use crate::services::imap::monitor::ImapMonitor;

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
    let mut all_channels: Vec<crate::inspect::types::ChannelInfo> = Vec::new();
    let mut all_workspace_dirs: Vec<std::path::PathBuf> = Vec::new();
    let config_snapshot = config.load();
    let agent_config = Arc::new(config_snapshot.agent.clone());
    let opencode_server = Arc::new(OpenCodeServer::new());

    for (channel_name, channel_config) in &config_snapshot.channels {
        let channel_type = channel_config.channel_type.as_str();

        // Workspace directory: always <workdir>/<channel>/workspace/
        let workspace_dir = crate::core::thread_path::resolve_workspace(workdir, channel_name);
        let storage = Arc::new(MessageStorage::new(&workspace_dir));

        let patterns = channel_config
            .patterns
            .clone()
            .unwrap_or_default();

        // Get attachment configuration from unified config
        let outbound_attachment_config = config_snapshot.attachments
            .as_ref()
            .and_then(|att| att.outbound.clone());
        let inbound_attachment_config = config_snapshot.attachments
            .as_ref()
            .and_then(|att| att.inbound.clone());

        // Create the outbound adapter based on channel type
        let outbound: Arc<dyn OutboundAdapter> = match channel_type {
            "email" => {
                let outbound_config = channel_config
                    .outbound
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("channel '{channel_name}': missing outbound config")
                    })?;
                Arc::new(EmailOutboundAdapter::new_with_attachments(
                    outbound_config,
                    storage.clone(),
                    outbound_attachment_config,
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
                Arc::new(GithubOutboundAdapter::new(
                    github_config,
                    storage.clone(),
                )?)
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
        outbound.connect().await.with_context(|| {
            format!("channel '{channel_name}': outbound connection failed")
        })?;
        tracing::info!(channel = %channel_name, channel_type = %channel_type, "Outbound connected");

        // Create agent based on configured mode
        let agent: Arc<dyn AgentService> = match agent_config.mode.as_str() {
            "opencode" => {
                Arc::new(OpenCodeService::new(
                    opencode_server.clone(),
                    agent_config.clone(),
                    config.clone(),
                    workdir.to_path_buf(),
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

        let heartbeat_template = channel_config
            .heartbeat_template
            .clone()
            .unwrap_or_else(|| "Still working on your request... ({elapsed} elapsed)".to_string());

        let template_dir = workdir.join("templates");
        
        let thread_manager = Arc::new(ThreadManager::new_with_options(
            config_snapshot.general.max_concurrent_threads,
            config_snapshot.general.max_queue_size_per_thread,
            storage.clone(),
            outbound.clone(),
            agent,
            cancel.clone(),
            true, // enable_events: true for Thread Event system
            config_snapshot.heartbeat.clone(),
            heartbeat_template,
            template_dir,
            config.clone(),
            channel_name.clone(),
            workspace_dir.clone(),
            metrics_handle.clone(),
        ));

        // Collect for inspect server
        all_thread_managers.push(thread_manager.clone());
        all_channels.push(crate::inspect::types::ChannelInfo {
            name: channel_name.clone(),
            channel_type: channel_type.to_string(),
        });
        all_workspace_dirs.push(workspace_dir);

        let router = Arc::new(MessageRouter::new(thread_manager.clone(), storage.clone()));

        let mut state_manager =
            StateManager::for_channel(workdir, channel_name);
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
                    .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing inbound config"))?
                    .clone();

                let monitor_config = channel_config
                    .monitor
                    .clone()
                    .unwrap_or_default();

                // Override IDLE mode if --no-idle flag
                let monitor_config = if args.no_idle {
                    MonitorConfig {
                        mode: "poll".to_string(),
                        ..monitor_config
                    }
                } else {
                    monitor_config
                };

                let task = tokio::spawn(async move {
                    let mut monitor = ImapMonitor::new(
                        channel_name_owned.clone(),
                        inbound_config,
                        monitor_config,
                        patterns,
                        router,
                        state_manager,
                        cancel_child,
                    );

                    if let Err(e) = monitor.start().await {
                        tracing::error!(
                            error = %e,
                            "IMAP monitor error"
                        );
                    }

                    // Shutdown thread manager for this channel
                    tm.shutdown().await;
                }.instrument(channel_span));

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

                    use crate::channels::types::InboundAdapter;
                    
                    let thread_manager_clone = thread_manager.clone();
                    let options = crate::channels::types::InboundAdapterOptions {
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
                let router_for_callback = router.clone();
                let workdir_owned = workdir.to_path_buf();

                let task = tokio::spawn(async move {
                    use crate::channels::github::inbound::GithubInboundAdapter;
                    use crate::channels::types::InboundAdapter;

                    let adapter = GithubInboundAdapter::new(&github_config, channel_name_owned.clone(), &workdir_owned);

                    let thread_manager_clone = thread_manager.clone();
                    let options = crate::channels::types::InboundAdapterOptions {
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
            _ => unreachable!(), // Already handled above with continue
        }
    }

    if tasks.is_empty() {
        anyhow::bail!("No channels configured");
    }

    // 5. Start inspect server (if configured)
    let inspect_task = if config_snapshot.inspect.as_ref().map_or(false, |i| i.enabled) {
        let inspect_config = config_snapshot.inspect.as_ref().unwrap();
        let activity_map: crate::inspect::server::SharedActivityMap =
            Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let context = Arc::new(crate::inspect::server::InspectContext {
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
        let _activity_task = crate::inspect::server::ActivityTracker::start(
            all_thread_managers,
            activity_map,
            all_workspace_dirs,
            cancel.clone(),
        );

        let server = crate::inspect::server::InspectServer::new(
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

    // Stop the OpenCode server
    opencode_server.stop().await.ok();

    // Wait for inspect server to stop
    if let Some(task) = inspect_task {
        task.await.ok();
    }

    // Wait for metrics collector to stop
    metrics_task.await.ok();

    tracing::info!("Monitor stopped");
    Ok(())
}
