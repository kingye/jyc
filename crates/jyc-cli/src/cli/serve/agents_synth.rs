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
//! The synthesis logic (`synthesize_agents_channel`) lives in
//! `jyc-types` so it is callable from both this module (startup) and
//! `jyc-inspect::api::post_reload_config` (config reload). Reloading
//! loads a fresh config from disk that lacks the synthesized `agents`
//! channel, so the reload path must re-run the synthesis before
//! storing — otherwise the orchestrator's reload diff treats `agents`
//! as removed and cancels the websocket channel the dashboard is
//! connected to.

use std::sync::Arc;

use arc_swap::ArcSwap;
use jyc_types::AppConfig;

// Re-export the pure synthesis helper so call sites and tests in this
// module keep resolving to `agents_synth::synthesize_agents_channel`.
pub use jyc_types::synthesize_agents_channel;

/// Inject the synthesized "agents" channel into the live config and
/// publish the augmented snapshot to the `ArcSwap`. No-op when
/// `[agents.*]` is empty.
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
    use jyc_types::{AgentConfig, AiConfig, ChannelConfig, ChannelPattern};

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
    fn synthesize_agent_pattern_leaves_topic_path_unset() {
        let mut p = ChannelPattern::default();
        agent("jyc", "jyc").fill_into_pattern(&mut p, "jyc");
        assert_eq!(p.name, "jyc");
        assert_eq!(p.channel, "agents");
        assert!(p.enabled);
        assert_eq!(p.template.as_deref(), Some("jyc"));
        // No explicit topic_path → stays None so the router falls back to
        // <agents-workspace>/<topic_name>, giving each topic of the agent
        // its own directory instead of collapsing them into one.
        assert!(p.topic_path.is_none());
        // Pattern identity fields that agents don't carry are None.
        assert!(p.topic_name.is_none());
        assert!(p.topic_prefix.is_none());
        assert!(p.pipe.is_none());
    }

    #[test]
    fn synthesize_agent_pattern_user_topic_path_wins() {
        let a = AgentConfig {
            topic_path: Some("~/projects/jyc".to_string()),
            ..Default::default()
        };
        let mut p = ChannelPattern::default();
        a.fill_into_pattern(&mut p, "jyc");
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

    /// Regression for the snapshot-order bug (PR #582): taking
    /// `config.load()` BEFORE `install_agents_channel` returns the
    /// pre-synth view (no "agents" channel). The serve::run startup
    /// must read the snapshot AFTER synth — see serve/mod.rs.
    ///
    /// This pins the contract: any code that wants the synthesized
    /// entry in its view of `config.channels` must call
    /// `install_agents_channel` first.
    #[test]
    fn snapshot_before_install_does_not_contain_synthesized_channel() {
        let mut snap = minimal_app_config();
        snap.agents.insert("jyc".to_string(), agent("jyc", "jyc"));
        let cfg = Arc::new(ArcSwap::from_pointee(snap));

        // Snapshot BEFORE install — mimics the old (buggy) ordering.
        let before = cfg.load();
        assert!(
            !before.channels.contains_key("agents"),
            "snapshot before install must not contain the synthesized 'agents' channel"
        );

        // Install synthesizes the channel.
        install_agents_channel(&cfg);

        // Snapshot AFTER install — the correct ordering.
        let after = cfg.load();
        assert!(
            after.channels.contains_key("agents"),
            "snapshot after install must contain the synthesized 'agents' channel"
        );
        assert_eq!(after.channels["agents"].channel_type, "websocket");
    }
}
