//! Bridge configuration + route table.
//!
//! Mirrors `~/.config/jyc/bridges/feishu/config.toml`. The bridge owns the
//! channel-native routing (chat name → jyc channel/thread) plus the feishu
//! platform credentials; jyc itself never sees these.

use anyhow::{Context, Result};
use serde::Deserialize;

/// Bridge manifest + platform credentials + route table.
#[derive(Debug, Clone, Deserialize)]
pub struct BridgeConfig {
    /// Bridge name (directory name, for discovery/logging).
    pub name: String,
    /// Default jyc channel — fallback for routes without an explicit `channel`.
    #[serde(default)]
    pub channel: String,
    /// Spawn command (argv). Read by jyc for discovery/spawn; the bridge
    /// itself never executes it.
    #[serde(default)]
    #[allow(dead_code)]
    pub command: Option<Vec<String>>,
    /// jyc inspect server URL for externally-managed bridges
    /// (spawned bridges get `JYC_URL` from the environment instead).
    #[serde(default)]
    pub jyc_url: Option<String>,

    // Feishu platform credentials.
    pub app_id: String,
    pub app_secret: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,

    /// Route table: channel-native identity → (channel, thread).
    #[serde(default)]
    pub routes: Vec<Route>,
}

/// One routing entry: which feishu chat maps to which jyc channel/thread.
#[derive(Debug, Clone, Deserialize)]
pub struct Route {
    /// Feishu chat name (native identity). Matched case-insensitively.
    pub chat_name: String,
    /// Optional feishu chat_id as a fallback key — matched exactly when the
    /// chat name is unavailable (e.g. the name API call failed).
    #[serde(default)]
    pub chat_id: Option<String>,
    /// Target jyc channel. Falls back to the bridge's default `channel`.
    #[serde(default)]
    pub channel: Option<String>,
    /// Target jyc thread (= jyc pattern name).
    pub thread: String,
    /// Optional respond filter: only forward when one of these is @-mentioned.
    #[serde(default)]
    pub mentions: Option<Vec<String>>,
}

impl BridgeConfig {
    /// Load the config from a TOML file.
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read bridge config {}", path))?;
        toml::from_str(&text).with_context(|| format!("failed to parse bridge config {}", path))
    }

    /// Resolve the matching route for a chat (case-insensitive name match,
    /// exact `chat_id` fallback).
    ///
    /// Returns `None` when no route matches — the bridge then drops the event.
    pub fn route(&self, chat_name: &str, chat_id: &str) -> Option<&Route> {
        self.routes.iter().find(|r| {
            r.chat_name.eq_ignore_ascii_case(chat_name)
                || (r.chat_id.is_some() && r.chat_id.as_deref() == Some(chat_id))
        })
    }

    /// Distinct jyc channels this bridge serves (default + route channels).
    pub fn channels(&self) -> Vec<String> {
        let mut set = vec![self.channel.clone()];
        for r in &self.routes {
            if let Some(c) = &r.channel
                && !set.contains(c)
            {
                set.push(c.clone());
            }
        }
        set
    }
}

fn default_base_url() -> String {
    "https://open.feishu.cn".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> BridgeConfig {
        toml::from_str(
            r#"
name = "feishu"
channel = "feishu_bot"
app_id = "cli_x"
app_secret = "s"

[[routes]]
chat_name = "greenfield"
channel = "channel-b"
thread = "thread-xxx"

[[routes]]
chat_name = "invoice"
thread = "invoice-processing"
mentions = ["jyc"]
"#,
        )
        .unwrap()
    }

    #[test]
    fn route_maps_chat_to_channel_and_thread() {
        let cfg = test_config();
        let r = cfg.route("greenfield", "oc_1").unwrap();
        assert_eq!(r.channel.as_deref(), Some("channel-b"));
        assert_eq!(r.thread, "thread-xxx");
    }

    #[test]
    fn route_falls_back_to_default_channel() {
        let cfg = test_config();
        let r = cfg.route("invoice", "oc_2").unwrap();
        assert_eq!(r.channel, None); // route has no explicit channel
        assert_eq!(r.thread, "invoice-processing");
        // The default channel is applied at use time, not stored on the route.
        assert_eq!(cfg.channel, "feishu_bot");
    }

    #[test]
    fn route_is_case_insensitive() {
        let cfg = test_config();
        assert_eq!(cfg.route("GreenField", "").unwrap().thread, "thread-xxx");
    }

    #[test]
    fn route_matches_by_chat_id_fallback() {
        let cfg: BridgeConfig = toml::from_str(
            r#"
name = "feishu"
app_id = "a"
app_secret = "b"

[[routes]]
chat_name = "greenfield"
chat_id = "oc_greenfield"
thread = "thread-xxx"
"#,
        )
        .unwrap();
        // Name is unknown (API failed) but the chat_id matches.
        let r = cfg.route("", "oc_greenfield").unwrap();
        assert_eq!(r.thread, "thread-xxx");
        assert!(cfg.route("", "oc_other").is_none());
    }

    #[test]
    fn route_unknown_chat_returns_none() {
        let cfg = test_config();
        assert!(cfg.route("some-other-group", "oc_x").is_none());
    }

    #[test]
    fn channels_is_union_of_default_and_routes() {
        let cfg = test_config();
        let channels = cfg.channels();
        assert_eq!(channels.len(), 2);
        assert!(channels.contains(&"feishu_bot".to_string()));
        assert!(channels.contains(&"channel-b".to_string()));
    }

    #[test]
    fn defaults_apply() {
        let cfg = test_config();
        assert_eq!(cfg.base_url, "https://open.feishu.cn");
        assert!(cfg.command.is_none());
        assert!(cfg.jyc_url.is_none());
        assert_eq!(
            cfg.routes[1].mentions.as_deref(),
            Some(&["jyc".to_string()][..])
        );
    }

    #[test]
    fn load_reads_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "name = \"feishu\"\napp_id = \"a\"\napp_secret = \"b\"\n",
        )
        .unwrap();
        let cfg = BridgeConfig::load(path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.name, "feishu");
    }
}
