use anyhow::{Context, Result};
use clap::Args;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::channels::email::outbound::EmailOutboundAdapter;
use crate::config::types::MonitorConfig;
use crate::config::{load_config, validation};
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

    // 3. Process each email channel
    let mut tasks = Vec::new();
    let agent_config = Arc::new(config.agent.clone());

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

        let thread_manager = Arc::new(ThreadManager::new(
            config.general.max_concurrent_threads,
            config.general.max_queue_size_per_thread,
            storage.clone(),
            outbound.clone(),
            agent_config.clone(),
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
            last_seq = state_manager.last_sequence_number(),
            processed_uids = state_manager.processed_uid_count(),
            "State loaded"
        );

        // Spawn the IMAP monitor as a task
        let cancel_child = cancel.clone();
        let channel_name_owned = channel_name.clone();
        let tm = thread_manager.clone();

        let task = tokio::spawn(async move {
            let mut monitor = ImapMonitor::new(
                inbound_config,
                monitor_config,
                patterns,
                router,
                state_manager,
                cancel_child,
            );

            if let Err(e) = monitor.start().await {
                tracing::error!(
                    channel = %channel_name_owned,
                    error = %e,
                    "IMAP monitor error"
                );
            }

            // Shutdown thread manager for this channel
            tm.shutdown().await;
        });

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

    tracing::info!("Monitor stopped");
    Ok(())
}
