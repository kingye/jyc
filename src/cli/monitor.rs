use anyhow::{Context, Result};
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
use crate::config::types::MonitorConfig;
use crate::config::{load_config, validation};
use crate::core::alert_service::{AppLogger, AlertService};
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

    // 2. Setup cancellation (Ctrl+C)
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("Received Ctrl+C, shutting down...");
        cancel_clone.cancel();
    });

    // 3. Start alert service (if configured)
    // We need a reference outbound adapter for alerts — we'll create it from the first channel's config
    // For now, alert service is started per-channel below (using that channel's outbound)
    let mut alert_handle = AppLogger::noop();
    let mut alert_task: Option<tokio::task::JoinHandle<()>> = None;

    // 4. Process each email channel
    let mut tasks = Vec::new();
    let agent_config = Arc::new(config.agent.clone());
    let opencode_server = Arc::new(OpenCodeServer::new());

    for (channel_name, channel_config) in &config.channels {
        if channel_config.channel_type != "email" {
            tracing::warn!(
                channel = %channel_name,
                channel_type = %channel_config.channel_type,
                "Unsupported channel type, skipping"
            );
            continue;
        }

        let inbound_config = channel_config
            .inbound
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing inbound config"))?
            .clone();

        let outbound_config = channel_config
            .outbound
            .as_ref()
            .ok_or_else(|| {
                anyhow::anyhow!("channel '{channel_name}': missing outbound config")
            })?;

        let monitor_config = channel_config
            .monitor
            .clone()
            .unwrap_or_default();

        let patterns = channel_config
            .patterns
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

        // Workspace directory: always <workdir>/<channel>/workspace/
        let workspace_dir = workdir.join(channel_name).join("workspace");

        // Create components
        let storage = Arc::new(MessageStorage::new(&workspace_dir));

        let outbound = Arc::new(EmailOutboundAdapter::new(outbound_config));
        outbound.connect().await.with_context(|| {
            format!("channel '{channel_name}': SMTP connection failed")
        })?;
        tracing::info!(channel = %channel_name, "SMTP connected");

        // Start alert service on first channel (uses that channel's outbound for alerts)
        if alert_task.is_none() {
            if let Some(ref alerting_config) = config.alerting {
                if alerting_config.enabled {
                    let alert_service = AlertService::new(
                        alerting_config.clone(),
                        outbound.clone(),
                        cancel.clone(),
                    );
                    let (handle, task) = alert_service.start();
                    alert_handle = handle;
                    alert_task = Some(task);
                    tracing::info!("Alert service started");

                    // Send startup notification
                    let startup_msg = format!(
                        "**JYC Monitor Started**\n\n\
                         Version: {}\n\
                         Time: {}\n\
                         Channels: {}\n\
                         Agent mode: {}",
                        env!("CARGO_PKG_VERSION"),
                        chrono::Utc::now().to_rfc3339(),
                        config.channels.len(),
                        config.agent.mode,
                    );
                    let prefix = alerting_config.subject_prefix.as_deref().unwrap_or("JYC");
                    let _ = outbound
                        .send_alert(
                            &alerting_config.recipient,
                            &format!("{prefix}: Monitor Started"),
                            &startup_msg,
                        )
                        .await;
                    tracing::info!("Startup notification sent");
                }
            }
        }

        // Create agent based on configured mode
        let agent: Arc<dyn AgentService> = match agent_config.mode.as_str() {
            "opencode" => {
                Arc::new(OpenCodeService::new(
                    opencode_server.clone(),
                    agent_config.clone(),
                    storage.clone(),
                    outbound.clone(),
                    workdir.to_path_buf(),
                ))
            }
            "static" => {
                let text = agent_config
                    .text
                    .as_deref()
                    .unwrap_or("Thank you for your message.");
                Arc::new(StaticAgentService::new(text, storage.clone(), outbound.clone()))
            }
            other => {
                anyhow::bail!("unsupported agent mode: '{other}'");
            }
        };

        let thread_manager = Arc::new(ThreadManager::new(
            config.general.max_concurrent_threads,
            config.general.max_queue_size_per_thread,
            storage.clone(),
            outbound.clone(),
            agent,
            cancel.clone(),
        ));

        let router = Arc::new(MessageRouter::new(thread_manager.clone()));

        let mut state_manager =
            StateManager::for_channel(workdir, channel_name);
        state_manager.initialize().await?;

        if args.reset {
            state_manager.reset().await?;
            tracing::info!(channel = %channel_name, "State reset");
        }

        tracing::info!(
            channel = %channel_name,
            mode = %agent_config.mode,
            last_seq = state_manager.last_sequence_number(),
            processed_uids = state_manager.processed_uid_count(),
            "State loaded"
        );

        // Spawn the IMAP monitor as a task
        let cancel_child = cancel.clone();
        let channel_name_owned = channel_name.clone();
        let tm = thread_manager.clone();
        let channel_span = tracing::info_span!("inbound", channel = %channel_name);

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

    if tasks.is_empty() {
        anyhow::bail!("No email channels configured");
    }

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

    // Wait for alert service to flush final errors
    if let Some(task) = alert_task {
        task.await.ok();
    }

    tracing::info!("Monitor stopped");
    Ok(())
}
