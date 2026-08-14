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
    /// Spawn command (argv). `None` for externally-managed bridges.
    #[serde(default)]
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
    /// Feishu chat name (native identity).
    pub chat_name: String,
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

    /// Resolve the target `(channel, thread)` for a chat name.
    ///
    /// Matches case-insensitively (chat names from feishu are display names).
    /// Returns `None` when no route matches — the bridge then drops the event.
    pub fn route_for(&self, chat_name: &str) -> Option<(&str, &str)> {
        self.routes
            .iter()
            .find(|r| r.chat_name.eq_ignore_ascii_case(chat_name))
            .map(|r| {
                (
                    r.channel.as_deref().unwrap_or(&self.channel),
                    r.thread.as_str(),
                )
            })
    }

    /// Distinct jyc channels this bridge serves (default + route channels).
    pub fn channels(&self) -> Vec<String> {
        let mut set = vec![self.channel.clone()];
        for r in &self.routes {
            if let Some(c) = &r.channel {
                if !set.contains(c) {
                    set.push(c.clone());
                }
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
    fn route_for_maps_chat_to_channel_and_thread() {
        let cfg = test_config();
        let (channel, thread) = cfg.route_for("greenfield").unwrap();
        assert_eq!(channel, "channel-b");
        assert_eq!(thread, "thread-xxx");
    }

    #[test]
    fn route_for_falls_back_to_default_channel() {
        let cfg = test_config();
        let (channel, thread) = cfg.route_for("invoice").unwrap();
        assert_eq!(channel, "feishu_bot"); // route has no explicit channel
        assert_eq!(thread, "invoice-processing");
    }

    #[test]
    fn route_for_is_case_insensitive() {
        let cfg = test_config();
        assert_eq!(cfg.route_for("GreenField").unwrap().1, "thread-xxx");
    }

    #[test]
    fn route_for_unknown_chat_returns_none() {
        let cfg = test_config();
        assert!(cfg.route_for("some-other-group").is_none());
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
