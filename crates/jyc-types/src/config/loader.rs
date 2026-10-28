//! Config loading: file/env parsing, TOML merge, layered resolution.
//!
//! Extracted from the monolithic `config.rs`.

use crate::state_dir::resolve_state_dir;
use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::channel::ContextStrategyConfig;
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
/// - `[ai]`: model overrides (legacy key `[agent]` accepted). Precedence:
///   `.jyc/<mode>-model-override` > `.jyc/config.toml` > pattern > channel >
///   global.
/// - `[mcps]`: MCP overrides (additive by default, opt-in full replace via
///   `mcps_replace`). Precedence: `.jyc/config.toml` > pattern > channel >
///   global. No `<mode>-model-override` higher layer exists for MCPs.
/// - `mode`, `skills`, `disabled_tools`, `instructions`,
///   `context_strategy`: settings the topic sets for itself, above the
///   pattern/channel layers and below the runtime override files. A **subagent
///   type**
///   (`[subagents.<type>]`, or `<config dir>/subagents/<type>/config.toml`) is
///   written into a child's `.jyc/config.toml` at spawn time, which is how one
///   file declares a role's model, skills, tools and instructions at once.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TopicConfig {
    /// AI overrides for this topic.
    /// Legacy key `agent` is accepted as an alias (deprecated).
    #[serde(
        rename = "ai",
        alias = "agent",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub ai: Option<TopicAiConfig>,

    /// MCPs added for this topic.
    ///
    /// Default merge = additive: topic MCPs union with pattern/channel/global
    /// MCPs, and a topic MCP with the same `name` as an inherited one wins.
    /// Set `mcps_replace = true` to fully replace the inherited set (mirror of
    /// how `ChannelPattern.mcps` overrides channel-level MCPs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcps: Option<Vec<McpServerConfig>>,

    /// When `true`, ignore the matched pattern/channel/global MCPs entirely
    /// and use only `mcps`. Default `false` (additive).
    #[serde(default, skip_serializing_if = "is_false")]
    pub mcps_replace: bool,

    /// Mode this topic starts in (`"plan"` or `"build"`).
    ///
    /// A config layer, not an override: precedence is `.jyc/mode-override`
    /// (the user's `/mode`, `/plan`, `/build`) > this > pattern/channel >
    /// default build — the same shape as `[ai]` models, so config-derived mode
    /// never lands in `.jyc/mode-override`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,

    /// Skill allow-list for this topic: when set, only these skills exist here.
    ///
    /// Replaces (does not extend) a pattern/channel allow-list — "only these"
    /// has to mean only these. When unset, no skill is listed: skills are
    /// opt-in. The runtime `/skill` toggle layers on top of the result, so
    /// `on` can add a skill and `off` can remove one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,

    /// Built-in tool names disabled for this topic (plain names, or
    /// `server/tool` for one MCP tool — see `docs/tools.md`).
    ///
    /// Unioned with the channel/pattern lists, not replaced: disabling is
    /// subtractive at every layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_tools: Option<Vec<String>>,

    /// Extra instructions for this topic's system prompt, injected as a
    /// `## Topic Instructions` section.
    ///
    /// Static text, so it stays cache-friendly: it changes when the file does,
    /// not per turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,

    /// Context management strategy for this topic (window size, turn
    /// tool-result cap, history token budget).
    ///
    /// The same layer as `mode`: a config layer, not an override — it beats the
    /// pattern/channel/global `[ai].context_strategy` and loses to
    /// `.jyc/context-strategy.json`, which is the user's `/context`. A subagent
    /// type carries one this way, which is how a role starts with a small
    /// window (see `subagents/ci-watch`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_strategy: Option<ContextStrategyConfig>,
}

impl TopicConfig {
    /// Overlay `self` (higher priority) onto `base`, key by key.
    ///
    /// This is how a `[subagents.<type>]` table in `config.toml` refines the
    /// same type's `<config dir>/subagents/<type>/config.toml`: the two are the
    /// same shape, and the in-config table (the running instance's own setting)
    /// wins per key over the installed file. `mcps` and `mcps_replace` are taken
    /// as a pair, since a list means nothing without its replace flag.
    pub fn overlay(self, base: TopicConfig) -> TopicConfig {
        let ai = match (self.ai, base.ai) {
            (Some(over), Some(under)) => Some(TopicAiConfig {
                model: over.model.or(under.model),
                plan_model: over.plan_model.or(under.plan_model),
                build_model: over.build_model.or(under.build_model),
                small_model: over.small_model.or(under.small_model),
            }),
            (over, under) => over.or(under),
        };
        let (mcps, mcps_replace) = if self.mcps.is_some() {
            (self.mcps, self.mcps_replace)
        } else {
            (base.mcps, base.mcps_replace)
        };
        TopicConfig {
            ai,
            mcps,
            mcps_replace,
            mode: self.mode.or(base.mode),
            skills: self.skills.or(base.skills),
            disabled_tools: self.disabled_tools.or(base.disabled_tools),
            instructions: self.instructions.or(base.instructions),
            context_strategy: self.context_strategy.or(base.context_strategy),
        }
    }
}

/// `false` is the default of every bool here, so it stays out of a written
/// config file ([`TopicConfig`] is serialized when a spawn expands a subagent
/// type into a child's `.jyc/config.toml`).
fn is_false(value: &bool) -> bool {
    !*value
}

/// Agent model overrides for a single topic.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TopicAiConfig {
    /// Model override for all modes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Model override for plan mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_model: Option<String>,
    /// Model override for build mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_model: Option<String>,
    /// Small model override (used for lightweight tasks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub small_model: Option<String>,
}

/// How a spawn should start the child of a role: a type's own defaults for the
/// two flags `jyc_spawn_subagent` takes, under a `[spawn]` table.
///
/// Only an installed type file carries one — `[subagents.<type>]` in the config
/// is a topic config and has no place for it. An argument passed at spawn time
/// always wins over these, so a stored default never locks a role out of a
/// one-off choice.
///
/// No `deny_unknown_fields` here: serde refuses to combine it with the flatten
/// on the struct that holds this table, and with it the *whole* file's keys were
/// being offered to this struct. [`load_subagent_type`] checks the table's keys
/// by hand instead, which is the part that matters.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SpawnPolicy {
    /// Default for `inherit_context`: start the child with this topic's
    /// conversation and transcript (`/spawn -i/--inherit`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherit_context: Option<bool>,

    /// Default for `share_topic_path`: run the child in this topic's working
    /// directory instead of one of its own (what a reviewer of uncommitted work
    /// needs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share_topic_path: Option<bool>,
}

/// `<config dir>/subagents/<type>/config.toml` as a whole: the topic-shaped
/// settings a spawn expands into the child's state ([`TopicConfig`]), plus the
/// two things only a *role* has — a one-line [`description`](Self::description)
/// for the agent choosing a type, and its `[spawn]` defaults.
///
/// The topic part is `#[serde(flatten)]`ed, so every key that worked before
/// still parses unchanged. Two consequences of that flatten are deliberate: no
/// `deny_unknown_fields` is possible anywhere in this struct (serde cannot
/// combine the two, and a mistyped key inside `[spawn]` — the one table whose
/// keys are its own — is refused by hand in [`load_subagent_type`] instead),
/// and the way a flatten buffers values makes it worth keeping `TopicConfig`
/// field types simple (a non-string, non-bool field is where that shows).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SubagentTypeFile {
    /// The role's settings, written into the child's `.jyc/config.toml`.
    #[serde(flatten)]
    pub topic: TopicConfig,

    /// What this role is for, in the terms an agent choosing one needs — phrase
    /// it as *when* to spawn it, not as a capability list: this line is all a
    /// parent that was never told about the role has to match against its
    /// situation. Listed beside the type's name in the `type` argument of
    /// `jyc_spawn_subagent`.
    #[serde(default)]
    pub description: Option<String>,

    /// How a spawn should start this role's child.
    #[serde(default)]
    pub spawn: Option<SpawnPolicy>,
}

/// Load a subagent type from `<config dir>/subagents/<type>/config.toml`.
///
/// The file is a topic config (see [`TopicConfig`], flattened into
/// [`SubagentTypeFile`]) standing outside any topic: a preset a spawn expands
/// into the child's own `.jyc/config.toml`. It is the second half of the type
/// lookup — `[subagents.<type>]` in the config file is the first, and
/// [`TopicConfig::overlay`] combines them.
///
/// Unlike [`load_topic_config`] the errors are *not* swallowed: a type the user
/// asked for by name must either load or say why, and a spawn that silently
/// dropped a role's settings would be worse than one that refused.
pub fn load_subagent_type(
    type_name: &str,
    config_dir: Option<&Path>,
) -> Result<Option<SubagentTypeFile>> {
    let Some(dir) = config_dir else {
        return Ok(None);
    };
    let path = subagent_type_path(dir, type_name);
    if !path.exists() {
        return Ok(None);
    }
    let path_label = path.display().to_string();
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read subagent type config: {path_label}"))?;
    // `[spawn]` keys are this file's own, and serde cannot guard them from
    // inside the flattened [`SubagentTypeFile`] (see [`SpawnPolicy`]), so they
    // are checked here. It is worth the few lines: an ignored typo means a role
    // silently starting the way its file says it must *not* — a reviewer that
    // quietly works in a copy of the tree it was meant to read as it is.
    let value: toml::Value = toml::from_str(&content)
        .with_context(|| format!("invalid subagent type config: {path_label}"))?;
    if let Some(spawn) = value.get("spawn").and_then(|v| v.as_table()) {
        for key in spawn.keys() {
            if !matches!(key.as_str(), "inherit_context" | "share_topic_path") {
                bail!(
                    "invalid subagent type config: {path_label}: unknown key `spawn.{key}` \
                     (expected `inherit_context` or `share_topic_path`)"
                );
            }
        }
    }
    parse_and_deserialize::<SubagentTypeFile>(&content, &path_label)
        .map(Some)
        .with_context(|| format!("invalid subagent type config: {path_label}"))
}

/// `<config dir>/subagents/<type>/config.toml` — where an installed type's
/// settings live, beside the `skills/` and `templates/` collections.
pub fn subagent_type_path(config_dir: &Path, type_name: &str) -> std::path::PathBuf {
    config_dir
        .join("subagents")
        .join(type_name)
        .join("config.toml")
}

/// The subagent type names available on this machine, from both sources
/// (`[subagents.<type>]` tables and installed `subagents/<type>/` dirs).
///
/// Used to answer "what can I pass as `type`?" — the error a bad type name gets,
/// and the tool description.
pub fn available_subagent_types(config: &AppConfig, config_dir: Option<&Path>) -> Vec<String> {
    let mut names: Vec<String> = config.subagents.keys().cloned().collect();
    if let Some(dir) = config_dir
        && let Ok(entries) = std::fs::read_dir(dir.join("subagents"))
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.join("config.toml").is_file() {
                continue;
            }
            if let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned())
                && !names.contains(&name)
            {
                names.push(name);
            }
        }
    }
    names.sort();
    names
}

/// Load topic-level overrides from the topic's resolved state dir
/// (`resolve_state_dir(topic_name, topic_path)/config.toml`; in-dir `.jyc` for
/// unpinned topics).
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
pub fn load_topic_config(topic_name: &str, topic_path: &Path) -> Option<TopicConfig> {
    let path = resolve_state_dir(topic_name, topic_path).join("config.toml");
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
/// Used for layered configuration: the data_dir config (overlay) overrides
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

/// Resolve `extends = "<base>"` inheritance between `[agents.<name>]`
/// entries, in place on the parsed TOML tree.
///
/// Semantics (single inheritance, override):
/// - the child overrides the base field-by-field on `merge_toml` rules
///   (tables merge recursively, arrays/scalars are fully replaced);
/// - an empty-string value in the child (`topic_path = ""`) *clears* the
///   inherited value: the key is dropped so the child gets the field's
///   default (`None`) instead of the base's value;
/// - chains resolve recursively (`A extends B extends C`);
/// - a missing base or an extends cycle is a config error.
///
/// The `extends` key itself is consumed here (removed before
/// deserialization), so `AgentConfig` needs no `extends` field.
///
/// Runs before `${VAR}` expansion ([`expand_env_vars`]) so env vars
/// resolve uniformly whether they came from the base or the child.
pub(crate) fn resolve_agent_extends(value: &mut toml::Value, ctx: &str) -> Result<()> {
    let Some(toml::Value::Table(agents)) = value.get_mut("agents") else {
        // No [agents] table in this config layer — nothing to inherit.
        return Ok(());
    };

    // Resolve each agent against a snapshot, then swap the table in.
    // Snapshotting avoids aliasing while we borrow the map recursively.
    let names: Vec<String> = agents.keys().cloned().collect();
    let snapshot = agents.clone();
    let mut resolved = toml::map::Map::new();
    for name in &names {
        resolved.insert(
            name.clone(),
            resolve_agent_extends_one(name, &snapshot, &mut Vec::new(), ctx)?,
        );
    }
    *agents = resolved;
    Ok(())
}

/// Recursively resolve one agent's `extends` chain and merge its base
/// table underneath. `stack` holds the chain for cycle detection.
fn resolve_agent_extends_one(
    name: &str,
    agents: &toml::map::Map<String, toml::Value>,
    stack: &mut Vec<String>,
    ctx: &str,
) -> Result<toml::Value> {
    let raw = agents.get(name).with_context(|| {
        format!("config {ctx}: agent '{name}' (referenced by extends) not found in [agents]")
    })?;
    let toml::Value::Table(agent_table) = raw else {
        bail!("config {ctx}: agent '{name}' must be a TOML table");
    };

    let mut merged = match agent_table.get("extends") {
        None => toml::Value::Table(toml::map::Map::new()),
        Some(extends) => {
            let parent = extends.as_str().with_context(|| {
                format!("config {ctx}: agent '{name}'.extends must be a string")
            })?;
            if stack.iter().any(|s| s == name) {
                bail!(
                    "config {ctx}: agent extends cycle detected: {} -> {name}",
                    stack.join(" -> ")
                );
            }
            stack.push(name.to_string());
            let parent_value = resolve_agent_extends_one(parent, agents, stack, ctx);
            stack.pop();
            parent_value?
        }
    };

    // Child fields win. Drop `extends` (consumed). Empty-string values
    // are kept *through* the merge so they override the base value, then
    // cleared afterwards (post-merge) so the field deserializes to its
    // default (`None`) instead of inheriting the base's value — that is
    // the "clear the inherited value" marker (`topic_path = ""`).
    let mut child = agent_table.clone();
    child.remove("extends");
    merged = merge_toml(merged, toml::Value::Table(child));
    drop_empty_string_markers(&mut merged);
    Ok(merged)
}

/// Recursively remove keys whose value is an empty string.
///
/// This runs *after* [`merge_toml`], so a child's `field = ""` has
/// already overridden the base's value; dropping the key then makes the
/// field deserialize as `None` — the documented "clear" semantics.
fn drop_empty_string_markers(value: &mut toml::Value) {
    if let toml::Value::Table(t) = value {
        t.retain(|_, v| {
            if matches!(v, toml::Value::String(s) if s.is_empty()) {
                false // empty-string leaf = "clear" marker → drop
            } else {
                drop_empty_string_markers(v);
                true
            }
        });
    }
}

/// Load configuration with global/data dir layering.
///
/// When `global` is `Some` and differs from `path`, the global config is
/// loaded first as the base layer and `path` is merged on top of it via
/// [`merge_toml`]. `${VAR}` expansion happens after the merge.
///
/// A missing global config file is silently ignored (layering is optional);
/// a missing `path` config file is an error.
pub fn load_config_layered(global: Option<&Path>, path: &Path) -> Result<AppConfig> {
    // Read + parse the data_dir file. L2 is the overlay; L1 (if any) is
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

/// Recursively expand `${VAR}` patterns in TOML string values.
///
/// Resolution order per name: process environment variable, then jyc
/// built-ins ([`builtin_var`]), then empty string. `config_path` is the
/// file being loaded, used to resolve `${JYC_CONFIG_PATH}`.
pub(crate) fn expand_env_vars(value: &mut toml::Value, config_path: &str) {
    let re = Regex::new(r"\$\{(\w+)\}").unwrap();

    match value {
        toml::Value::String(s) if s.contains("${") => {
            *s = re
                .replace_all(s, |caps: &regex::Captures| {
                    std::env::var(&caps[1])
                        .unwrap_or_else(|_| builtin_var(&caps[1], config_path).unwrap_or_default())
                })
                .to_string();
        }
        toml::Value::Table(t) => {
            for (_, v) in t.iter_mut() {
                expand_env_vars(v, config_path);
            }
        }
        toml::Value::Array(a) => {
            for v in a.iter_mut() {
                expand_env_vars(v, config_path);
            }
        }
        _ => {}
    }
}

/// Built-in values for `${VAR}` names jyc resolves itself when the
/// process environment does not define them. An actual env var always
/// wins (explicit override).
///
/// - `JYC_CONFIG_PATH` → the directory containing the config file being
///   loaded (typically `~/.config/jyc` on Linux/macOS), so paths beside
///   it compose: `${JYC_CONFIG_PATH}/skills`. With layered configs the
///   merged tree expands against the overlay file's directory.
/// - `JYC_TOPIC_PATH` / `JYC_TOPIC_STATE_PATH` → deliberately *not*
///   resolved at load time: they are per-topic and only known at hook
///   spawn, so they pass through verbatim (the hooks executor substitutes
///   them). Writing them outside hooks keeps the literal too.
fn builtin_var(name: &str, config_path: &str) -> Option<String> {
    match name {
        "JYC_CONFIG_PATH" => std::path::Path::new(config_path)
            .parent()
            .map(|p| p.display().to_string()),
        "JYC_TOPIC_PATH" | "JYC_TOPIC_STATE_PATH" => Some(format!("${{{name}}}")),
        _ => None,
    }
}
