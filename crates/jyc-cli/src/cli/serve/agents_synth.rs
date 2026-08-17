//! Synthesize the implicit `channels.agents` entry from
//! `[agents.<name>]` declarations.
//!
//! Each `[agents.<name>]` becomes a `ChannelPattern` inside a single
//! synthesized channel named `"agents"` with `channel_type =
//! "websocket"`. The synthesized channel goes through the same
//! websocket construction path as the legacy
//! `[channels.<name>] type = "websocket"` form, so no WebSocket
//! adapter code is touched.
//!
//! The synthesis runs once at startup and stores the augmented config
//! back into the `ArcSwap`; reloads pick it up automatically.

use std::sync::Arc;

use arc_swap::ArcSwap;
use jyc_types::{AgentConfig, AppConfig, ChannelConfig, ChannelPattern, PatternRules};

/// Build one `ChannelPattern` from one `[agents.<name>]` entry.
///
/// The agent's `topic_path` (if set) takes priority; otherwise we point
/// the pattern at `<data_home>/agents/<agent_name>/` via
/// `resolve_agent_workspace`, so topics under this agent land in its
/// own subtree.
pub fn synthesize_agent_pattern(agent_name: &str, agent: &AgentConfig) -> ChannelPattern {
    let topic_path = agent.topic_path.clone().unwrap_or_else(|| {
        jyc_core::topic_path::resolve_agent_workspace(agent_name)
            .to_string_lossy()
            .into_owned()
    });
    ChannelPattern {
        name: agent_name.to_string(),
        channel: "agents".to_string(),
        enabled: true,
        rules: PatternRules::default(),
        pipe: None,
        attachments: agent.attachments.clone(),
        template: agent.template.clone(),
        topic_name: None,
        topic_prefix: None,
        topic_path: Some(topic_path),
        role: agent.role.clone(),
        live_injection: agent.live_injection,
        repo_group: None,
        inject_inbound_images: agent.inject_inbound_images,
        model: agent.model.clone(),
        plan_model: agent.plan_model.clone(),
        build_model: agent.build_model.clone(),
        small_model: agent.small_model.clone(),
        mode: agent.mode.clone(),
        mcps: agent.mcps.clone(),
        disabled_tools: agent.disabled_tools.clone(),
        disabled_builtin_tools: agent.disabled_builtin_tools.clone(),
        disabled_mcp_servers: agent.disabled_mcp_servers.clone(),
        skills: agent.skills.clone(),
        disabled_skills: agent.disabled_skills.clone(),
        reset_compression: agent.reset_compression.clone(),
        auto_reset_threshold: agent.auto_reset_threshold,
        access: agent.access.clone(),
    }
}

/// Build the synthesized `channels.agents` entry from the current
/// `config_snapshot.agents`. Returns `None` when there are no agents.
pub fn synthesize_agents_channel(snapshot: &AppConfig) -> Option<ChannelConfig> {
    if snapshot.agents.is_empty() {
        return None;
    }
    let patterns: Vec<ChannelPattern> = snapshot
        .agents
        .iter()
        .map(|(name, agent)| synthesize_agent_pattern(name, agent))
        .collect();
    tracing::info!(
        count = patterns.len(),
        "Synthesized 'agents' channel from [agents.<name>] entries"
    );
    Some(ChannelConfig {
        channel_type: "websocket".to_string(),
        patterns: Some(patterns),
        ..Default::default()
    })
}

/// Inject the synthesized "agents" channel into the live config and
/// publish the augmented snapshot to the `ArcSwap` so subsequent
/// reloads see it. No-op when `[agents.*]` is empty.
pub fn install_agents_channel(config: &Arc<ArcSwap<AppConfig>>) {
    let snapshot = config.load();
    let Some(synthesized) = synthesize_agents_channel(&snapshot) else {
        return;
    };
    let mut new_config = (**snapshot).clone();
    new_config
        .channels
        .insert("agents".to_string(), synthesized);
    config.store(Arc::new(new_config));
}
