//! Config loading: file/env parsing, TOML merge, layered resolution.
//!
//! Extracted from the monolithic `config.rs`.

use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::path::Path;

use crate::config::{
    AppConfig, McpServerConfig, parse_and_deserialize, parse_and_deserialize_from_value,
    read_and_parse,
};

pub fn load_config(path: &Path) -> Result<AppConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;
    parse_and_deserialize(&content, &path.display().to_string())
}

/// Topic-level configuration (L3), loaded from `<topic_path>/.jyc/config.toml`.
///
/// Restricted subset of the app config:
/// - `[agent]`: model overrides. Precedence: `.jyc/<mode>-model-override` >
///   `.jyc/config.toml` > pattern > channel > global.
/// - `[mcps]`: MCP overrides (additive by default, opt-in full replace via
///   `mcps_replace`). Precedence: `.jyc/config.toml` > pattern > channel >
///   global. No `<mode>-model-override` higher layer exists for MCPs.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TopicConfig {
    /// Agent overrides for this topic.
    pub agent: Option<TopicAgentConfig>,

    /// MCPs added for this topic.
    ///
    /// Default merge = additive: topic MCPs union with pattern/channel/global
    /// MCPs, and a topic MCP with the same `name` as an inherited one wins.
    /// Set `mcps_replace = true` to fully replace the inherited set (mirror of
    /// how `ChannelPattern.mcps` overrides channel-level MCPs).
    #[serde(default)]
    pub mcps: Option<Vec<McpServerConfig>>,

    /// When `true`, ignore the matched pattern/channel/global MCPs entirely
    /// and use only `mcps`. Default `false` (additive).
    #[serde(default)]
    pub mcps_replace: bool,
}

/// Agent model overrides for a single topic.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TopicAgentConfig {
    /// Model override for all modes.
    pub model: Option<String>,
    /// Model override for plan mode.
    pub plan_model: Option<String>,
    /// Model override for build mode.
    pub build_model: Option<String>,
    /// Small model override (used for lightweight tasks).
    pub small_model: Option<String>,
}

/// Load topic-level overrides from `<topic_path>/.jyc/config.toml`.
///
/// Returns `None` when the file does not exist, when it cannot be read
/// (e.g. EACCES in remote deployments — the agent runs under a different
/// user than the config owner), or when it fails to parse. All non-`Ok`
/// outcomes are logged at `warn` so the failure mode is visible in
/// production logs; a broken topic config must not crash the agent.
///
/// Structurally mirrors [`load_config_from_str`] but returns
/// `Option<TopicConfig>` and swallows errors. `${VAR}` expansion runs
/// on every string field (via [`parse_and_deserialize`]).
pub fn load_topic_config(topic_path: &Path) -> Option<TopicConfig> {
    let path = topic_path.join(".jyc").join("config.toml");
    let path_label = path.display().to_string();
    if !path.exists() {
        return None;
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                path = %path_label,
                error = %e,
                "Failed to read topic config; topic-local MCP overlay will be skipped"
            );
            return None;
        }
    };
    // Same parse + expand + deserialize pipeline as
    // load_config_from_str / load_config_layered (all four public
    // loaders now share [`parse_and_deserialize`] for the parse+expand
    // step). Errors are swallowed (warn + None) per the docstring
    // above: a broken topic config must not crash the agent.
    match parse_and_deserialize::<TopicConfig>(&content, &path_label) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            tracing::warn!(path = %path_label, error = %e, "Ignoring invalid topic config");
            None
        }
    }
}

/// Load configuration from a TOML string.
///
/// Expands `${VAR}` environment variable references, then deserializes.
pub fn load_config_from_str(content: &str) -> Result<AppConfig> {
    parse_and_deserialize(content, "<inline>")
}

/// Apply the topic-level (L3) MCP overlay onto a base list.
///
/// - When `topic_cfg` is `None` or its `mcps` is `None`, returns `base` unchanged.
/// - When `topic_cfg.mcps_replace` is `true`, returns the topic's MCPs only.
/// - Otherwise (additive default): union of `base` + topic MCPs; on name
///   conflict, the topic version wins (last-writer-wins).
pub fn apply_topic_mcp_overlay(
    base: &[McpServerConfig],
    topic_cfg: Option<&TopicConfig>,
) -> Vec<McpServerConfig> {
    let Some(t) = topic_cfg else {
        return base.to_vec();
    };
    let Some(topic_mcps) = t.mcps.as_ref() else {
        return base.to_vec();
    };
    if t.mcps_replace {
        return topic_mcps.clone();
    }
    let mut out: Vec<McpServerConfig> = base.to_vec();
    for tm in topic_mcps {
        if let Some(slot) = out.iter_mut().find(|c| c.name == tm.name) {
            *slot = tm.clone();
        } else {
            out.push(tm.clone());
        }
    }
    out
}

/// Deep-merge two TOML values: tables merge recursively; all other values
/// (strings, arrays, scalars) are replaced by the overlay.
///
/// Used for layered configuration: the workdir config (overlay) overrides
/// the global config (base) on a per-key basis.
pub(crate) fn merge_toml(base: toml::Value, overlay: toml::Value) -> toml::Value {
    match (base, overlay) {
        (toml::Value::Table(mut base_table), toml::Value::Table(overlay_table)) => {
            for (key, overlay_value) in overlay_table {
                let merged = match base_table.remove(&key) {
                    Some(base_value) => merge_toml(base_value, overlay_value),
                    None => overlay_value,
                };
                base_table.insert(key, merged);
            }
            toml::Value::Table(base_table)
        }
        (_base, overlay) => overlay,
    }
}

/// Load configuration with global/workdir layering.
///
/// When `global` is `Some` and differs from `path`, the global config is
/// loaded first as the base layer and `path` is merged on top of it via
/// [`merge_toml`]. `${VAR}` expansion happens after the merge.
///
/// A missing global config file is silently ignored (layering is optional);
/// a missing `path` config file is an error.
pub fn load_config_layered(global: Option<&Path>, path: &Path) -> Result<AppConfig> {
    // Read + parse the workdir file. L2 is the overlay; L1 (if any) is
    // the base, deep-merged underneath.
    let mut value = read_and_parse(path)?;

    if let Some(global_path) = global.filter(|g| *g != path && g.exists()) {
        let global_value = read_and_parse(global_path)?;
        value = merge_toml(global_value, value);
    }

    // Expansion happens after the merge so `${VAR}` resolves identically
    // regardless of which layer defined the key. Same expand+deserialize
    // tail as L1/L3 (see [`parse_and_deserialize_from_value`]).
    parse_and_deserialize_from_value(value, &path.display().to_string())
}

/// Recursively expand `${VAR}` patterns in TOML string values
/// with values from environment variables.
///
/// Missing env vars are replaced with empty strings.
pub(crate) fn expand_env_vars(value: &mut toml::Value) {
    let re = Regex::new(r"\$\{(\w+)\}").unwrap();

    match value {
        toml::Value::String(s) if s.contains("${") => {
            *s = re
                .replace_all(s, |caps: &regex::Captures| {
                    std::env::var(&caps[1]).unwrap_or_default()
                })
                .to_string();
        }
        toml::Value::Table(t) => {
            for (_, v) in t.iter_mut() {
                expand_env_vars(v);
            }
        }
        toml::Value::Array(a) => {
            for v in a.iter_mut() {
                expand_env_vars(v);
            }
        }
        _ => {}
    }
}
