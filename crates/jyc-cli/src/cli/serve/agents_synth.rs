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
use jyc_types::{AgentConfig, AppConfig, ChannelConfig, ChannelPattern};

/// Build one `ChannelPattern` from one `[agents.<name>]` entry.
///
/// Thin wrapper over `AgentConfig::fill_into_pattern` — kept as a
/// function so the CLI callers don't have to know about the resolved
/// default `topic_path`. Adding a new behavior field to
/// `AgentConfig` requires updating `fill_into_pattern` only; both
/// this function and `validate_agent` route through it.
pub fn synthesize_agent_pattern(agent_name: &str, agent: &AgentConfig) -> ChannelPattern {
    let mut pattern = ChannelPattern::default();
    let default_topic_path = jyc_core::topic_path::resolve_agent_workspace(agent_name);
    agent.fill_into_pattern(&mut pattern, agent_name, default_topic_path);
    pattern
}

/// Build the synthesized `channels.agents` entry from the current
/// `config_snapshot.agents`. Returns `None` when there are no agents.
///
/// Deterministic pattern order: agent names are sorted before
/// iteration so `pattern_names()` (the dashboard's pattern-select UI)
/// is stable across restarts.
pub fn synthesize_agents_channel(snapshot: &AppConfig) -> Option<ChannelConfig> {
    if snapshot.agents.is_empty() {
        return None;
    }
    let mut names: Vec<&String> = snapshot.agents.keys().collect();
    names.sort();
    let patterns: Vec<ChannelPattern> = names
        .into_iter()
        .map(|name| synthesize_agent_pattern(name, &snapshot.agents[name]))
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
