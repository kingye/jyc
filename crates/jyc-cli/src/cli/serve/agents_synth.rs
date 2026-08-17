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

#[cfg(test)]
mod tests {
    use super::*;
    use jyc_types::AiConfig;

    fn minimal_app_config() -> AppConfig {
        // AppConfig doesn't derive Default; build the minimum needed
        // for synthesize_agents_channel / install_agents_channel.
        AppConfig {
            general: Default::default(),
            channels: Default::default(),
            agents: Default::default(),
            ai: AiConfig::default(),
            inspect: None,
            attachments: None,
            wecom: None,
            mcps: vec![],
            scheduler: Default::default(),
            commands: vec![],
        }
    }

    fn agent(name: &str, template: &str) -> AgentConfig {
        let _ = name; // Name is the TOML table key, not a field on the config.
        AgentConfig {
            template: Some(template.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn synthesize_agent_pattern_default_topic_path() {
        let mut p = ChannelPattern::default();
        agent("jyc", "jyc").fill_into_pattern(
            &mut p,
            "jyc",
            std::path::PathBuf::from("/data/jyc/agents/jyc"),
        );
        assert_eq!(p.name, "jyc");
        assert_eq!(p.channel, "agents");
        assert!(p.enabled);
        assert_eq!(p.template.as_deref(), Some("jyc"));
        // No explicit topic_path on the agent → default is used.
        assert_eq!(p.topic_path.as_deref(), Some("/data/jyc/agents/jyc"));
        // Pattern identity fields that agents don't carry are None.
        assert!(p.topic_name.is_none());
        assert!(p.topic_prefix.is_none());
        assert!(p.repo_group.is_none());
        assert!(p.pipe.is_none());
    }

    #[test]
    fn synthesize_agent_pattern_user_topic_path_wins() {
        let mut a = AgentConfig::default();
        a.topic_path = Some("~/projects/jyc".to_string());
        let mut p = ChannelPattern::default();
        a.fill_into_pattern(
            &mut p,
            "jyc",
            std::path::PathBuf::from("/default/should/not/be/used"),
        );
        assert_eq!(p.topic_path.as_deref(), Some("~/projects/jyc"));
    }

    #[test]
    fn synthesize_agents_channel_none_when_empty() {
        let snap = minimal_app_config();
        assert!(synthesize_agents_channel(&snap).is_none());
    }

    #[test]
    fn synthesize_agents_channel_one_pattern_per_agent() {
        let mut snap = minimal_app_config();
        snap.agents.insert("jyc".to_string(), agent("jyc", "jyc"));
        snap.agents.insert("jin".to_string(), agent("jin", "jin"));
        let cc = synthesize_agents_channel(&snap).expect("synth");
        assert_eq!(cc.channel_type, "websocket");
        let pats = cc.patterns.expect("patterns");
        assert_eq!(pats.len(), 2);
        let names: Vec<&str> = pats.iter().map(|p| p.name.as_str()).collect();
        // Sorted for deterministic dashboard order.
        assert_eq!(names, vec!["jin", "jyc"]);
    }

    #[test]
    fn install_agents_channel_publishes_augmented_snapshot() {
        let mut snap = minimal_app_config();
        snap.agents.insert("jyc".to_string(), agent("jyc", "jyc"));
        let cfg = Arc::new(ArcSwap::from_pointee(snap));
        install_agents_channel(&cfg);
        let after = cfg.load();
        assert!(after.channels.contains_key("agents"));
        let ch = &after.channels["agents"];
        assert_eq!(ch.channel_type, "websocket");
        assert_eq!(ch.patterns.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn install_agents_channel_noop_when_no_agents() {
        let mut snap = minimal_app_config();
        snap.channels.insert(
            "work".to_string(),
            ChannelConfig {
                channel_type: "email".to_string(),
                ..Default::default()
            },
        );
        let cfg = Arc::new(ArcSwap::from_pointee(snap));
        install_agents_channel(&cfg);
        // No agents → no synth → channels unchanged.
        let after = cfg.load();
        assert!(!after.channels.contains_key("agents"));
        assert!(after.channels.contains_key("work"));
    }
}
