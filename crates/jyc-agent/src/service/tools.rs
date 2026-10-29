//! Tool registry + provider construction for the in-process agent.
//!
//! Extracted from the monolithic `service.rs`.

use anyhow::Result;
use jyc_types::state_dir::resolve_state_dir;
use std::path::Path;
use std::sync::Arc;

use jyc_core::topic_event_bus::TopicEventBusRef;
use jyc_types::{ChannelPattern, McpServerConfig, TopicConfig};

use crate::provider;
use crate::tools::registry::{McpGroup, McpToolEntry, ToolRegistry};

use super::JycAgentService;

/// Source layer of an MCP server after the L1 (global) / L2 (channel) /
/// L3 (topic-local) overlay resolution. Used only to tag entries for the
/// per-topic observability log; it does not affect tool registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpLayer {
    Global,
    Channel,
    Pattern,
    TopicOverride,
    TopicReplace,
}

impl JycAgentService {
    /// Create the tool registry for a topic.
    ///
    /// `supports_images` gates the `read_image` built-in: when the active
    /// model can accept image content blocks, the agent gets a way to load
    /// local files or URLs into subsequent user turns. When the model is
    /// text-only, the tool is omitted to keep the schema honest (no point
    /// advertising a capability the model can't act on).
    ///
    /// `matched_pattern_name` optionally selects per-pattern MCP configurations.
    /// When the matched pattern has `mcps: Some(list)`, only those MCP servers
    /// are loaded. When `None`, the global `[[mcps]]` list is used (backward
    /// compatible fallback).
    pub(crate) async fn build_tool_registry(
        &self,
        topic_name: &str,
        topic_path: &Path,
        topic_cfg: Option<&TopicConfig>,
        supports_images: bool,
        matched_pattern_name: Option<&str>,
    ) -> ToolRegistry {
        // Start with all built-in tools
        let mut registry = crate::tools::builtin::create_builtin_registry();

        // Always register read_image. When the model supports images, images
        // are queued for injection into the next user turn. When the model is
        // text-only and a VisionClient is configured, the tool falls back to
        // the vision model for analysis. When neither condition is met, the
        // tool returns a helpful error message.
        crate::tools::builtin::register_read_image(
            &mut registry,
            supports_images,
            self.vision_client.clone(),
        );

        // Derive MCP/pattern/disabled config from the LIVE config (via the
        // shared ArcSwap) so a config reload picks up newly added/edited/
        // removed MCP servers without a restart — same pattern as
        // `agent_config()`. The frozen constructor snapshots are gone.
        let live_cfg = self.config.load();
        let publish_base_url = live_cfg
            .inspect
            .clone()
            .unwrap_or_default()
            .effective_base_url();
        crate::tools::builtin::register_publish_file(&mut registry, publish_base_url);

        // Subagents are only reachable on the synthesized "agents" (websocket)
        // channel: it is the one whose workspace is the agents root that agent
        // topics live in, and the one a spawned sibling topic is addressable
        // on. Everywhere else the tool's name would be a promise it cannot
        // keep, so it is left out entirely.
        if self.channel_name == "agents" {
            registry.register(Box::new(
                crate::tools::builtin::spawn_subagent::SpawnSubagentTool {
                    // The same live config this registry's other settings come
                    // from, so `type` resolves against what the instance runs
                    // with rather than a fresh read of the default path.
                    config: self.config.clone(),
                },
            ));
        }

        // Add MCP bridge tools (reply_message, etc.)
        crate::tools::mcp_bridge::register_mcp_tools(&mut registry);

        let live_ch = live_cfg.channels.get(&self.channel_name);
        let patterns: Vec<ChannelPattern> =
            live_ch.and_then(|c| c.patterns.clone()).unwrap_or_default();
        let channel_mcp_configs = live_ch.and_then(|c| c.mcps.clone());
        let channel_disabled_mcps = live_ch.and_then(|c| c.disabled_mcps.clone());
        let channel_disabled_tools = live_ch.and_then(|c| c.disabled_tools.clone());
        let mcp_configs: Vec<McpServerConfig> = live_cfg.mcps.clone();
        // Release the ArcSwap guard before any `.await` below.
        drop(live_cfg);

        // Find matched pattern for per-pattern overrides
        let matched_pattern =
            matched_pattern_name.and_then(|name| patterns.iter().find(|p| p.name == name));

        // --- L3 topic-local config lifecycle log ---
        // Always emit one of three outcomes so a remote-deploy grep on
        // "topic-local MCP" or "topic config loaded" reveals whether the
        // L3 file was found, valid, and applied. This is the only signal
        // that tells operators whether the topic-local overlay engaged.
        let topic_cfg_path = resolve_state_dir(topic_name, topic_path).join("config.toml");
        let configured_mcps = topic_cfg
            .and_then(|t| t.mcps.as_ref())
            .map(|v| v.len())
            .unwrap_or(0);
        let mcps_replace = topic_cfg.map(|t| t.mcps_replace).unwrap_or(false);
        match topic_cfg {
            None => tracing::debug!(
                channel = %self.channel_name,
                topic = %topic_name,
                path = %topic_cfg_path.display(),
                "no topic-local MCP overlay (L3 file absent or unreadable)"
            ),
            Some(cfg) if cfg.mcps.is_none() && !cfg.mcps_replace => tracing::debug!(
                channel = %self.channel_name,
                topic = %topic_name,
                path = %topic_cfg_path.display(),
                configured_mcps = configured_mcps,
                mcps_replace = mcps_replace,
                "topic config loaded but has no [[mcps]]; L3 is a no-op"
            ),
            Some(cfg) => {
                let names: Vec<&str> = cfg
                    .mcps
                    .as_ref()
                    .map(|v| v.iter().map(|m| m.name.as_str()).collect())
                    .unwrap_or_default();
                tracing::info!(
                    channel = %self.channel_name,
                    topic = %topic_name,
                    path = %topic_cfg_path.display(),
                    configured_mcps = configured_mcps,
                    mcps_replace = mcps_replace,
                    topic_mcp_names = ?names,
                    "Applied topic-local MCP overlay (L3)"
                );
            }
        }

        // --- MCP server exclusion (disabled_mcps) ---
        // Merge channel-level + pattern-level disabled MCP servers
        let mut disabled_mcps: Vec<String> = {
            let mut set = Vec::new();
            if let Some(ref servers) = channel_disabled_mcps {
                for s in servers {
                    set.push(s.clone());
                }
            }
            if let Some(servers) = matched_pattern.and_then(|p| p.disabled_mcps.as_ref()) {
                for s in servers {
                    if !set.contains(s) {
                        set.push(s.clone());
                    }
                }
            }
            set
        };

        // Runtime `/mcp on|off` toggle override (persisted until `/mcp reset`):
        // `on` un-disables config-level entries, `off` adds more.
        if let Some(ovr) = jyc_core::session_state::read_mcp_override(topic_name, topic_path).await
        {
            disabled_mcps = ovr.apply(&disabled_mcps);
        }

        // Resolve MCP configs: pattern → channel → global. Tag each baseline
        // entry with its source layer so the per-topic log can show exactly
        // which MCP came from which config layer.
        let (base_mcps, base_layer): (&[McpServerConfig], McpLayer) =
            if let Some(p) = matched_pattern.and_then(|p| p.mcps.as_ref()) {
                (p.as_slice(), McpLayer::Pattern)
            } else if let Some(c) = channel_mcp_configs.as_deref() {
                (c, McpLayer::Channel)
            } else {
                (mcp_configs.as_slice(), McpLayer::Global)
            };

        // Layer topic (L3) MCPs from the pre-loaded <topic_path>/.jyc/config.toml
        // on top: additive by default, opt-in full replace via `mcps_replace = true`.
        // (Caller in `process` loads topic_cfg once per message and shares it
        //  with the [agent] model-resolution block, avoiding a duplicate disk read.)
        let effective_mcps: Vec<McpServerConfig> =
            jyc_types::apply_topic_mcp_overlay(base_mcps, topic_cfg);

        // Re-tag the resolved list with the source layer each MCP came from.
        // `apply_topic_mcp_overlay` keeps base entries in place for unchanged
        // names and appends new ones; we attribute each by comparing against
        // the baseline. When `mcps_replace=true` every entry is from the L3.
        let mut filtered_mcp_configs: Vec<(McpServerConfig, McpLayer)> = effective_mcps
            .into_iter()
            .map(|c| {
                let layer = if mcps_replace || !base_mcps.iter().any(|b| b.name == c.name) {
                    McpLayer::TopicOverride
                } else {
                    base_layer
                };
                (c, layer)
            })
            .collect();

        // Filter out disabled MCP servers before loading
        filtered_mcp_configs.retain(|(c, _)| !disabled_mcps.contains(&c.name));

        if !disabled_mcps.is_empty() {
            tracing::debug!(
                disabled = ?disabled_mcps,
                "MCP servers disabled by config"
            );
        }

        // --- Tool exclusion (disabled_tools) ---
        // Merge channel-level + pattern-level + topic-level (L3) + the
        // backward-compatible alias. Every layer adds to the set: disabling a
        // tool is subtractive, and a subagent type says `disabled_tools` here.
        let disabled_tools: Vec<&str> = {
            let mut set = Vec::new();
            if let Some(ref tools) = channel_disabled_tools {
                for t in tools {
                    set.push(t.as_str());
                }
            }
            for tools in [
                matched_pattern.and_then(|p| p.disabled_tools.as_ref()),
                topic_cfg.and_then(|t| t.disabled_tools.as_ref()),
                // Backward-compatible alias: disabled_builtin_tools
                matched_pattern.and_then(|p| p.disabled_builtin_tools.as_ref()),
            ]
            .into_iter()
            .flatten()
            {
                for t in tools {
                    if !set.contains(&t.as_str()) {
                        set.push(t.as_str());
                    }
                }
            }
            set
        };

        // Separate server/tool format (e.g. "jin_public_mcp/product_list") from plain names.
        // Server/tool entries are applied before MCP tools are registered, allowing
        // precise filtering when multiple MCP servers expose the same tool name.
        let (disabled_server_tools, disabled_plain_tools): (Vec<&str>, Vec<&str>) =
            disabled_tools.into_iter().partition(|t| t.contains('/'));

        // Per-topic structured log: which MCPs were actually loaded and
        // which config layer each originated from. Replaces the previous
        // count-only "Loading external MCP tools" line.
        if mcps_replace {
            for (_, l) in filtered_mcp_configs.iter_mut() {
                *l = McpLayer::TopicReplace;
            }
        }
        let mcps_total = filtered_mcp_configs.len();
        if mcps_total > 0 {
            let mcps: Vec<String> = filtered_mcp_configs
                .iter()
                .map(|(c, l)| match l {
                    McpLayer::Global => format!("{}:global", c.name),
                    McpLayer::Channel => format!("{}:channel", c.name),
                    McpLayer::Pattern => format!("{}:pattern", c.name),
                    McpLayer::TopicOverride => format!("{}:topic", c.name),
                    McpLayer::TopicReplace => format!("{}:topic-replace", c.name),
                })
                .collect();
            tracing::info!(
                channel = %self.channel_name,
                topic = %topic_name,
                pattern = ?matched_pattern_name,
                mcps_total = mcps_total,
                from_global = mcps.iter().filter(|s| s.ends_with(":global")).count(),
                from_channel = mcps.iter().filter(|s| s.ends_with(":channel")).count(),
                from_pattern = mcps.iter().filter(|s| s.ends_with(":pattern")).count(),
                from_topic = mcps.iter().filter(|s| s.ends_with(":topic")).count(),
                from_topic_replace = mcps.iter().filter(|s| s.ends_with(":topic-replace")).count(),
                mcps = ?mcps,
                "Resolved MCP servers for topic"
            );
        } else {
            tracing::debug!(
                channel = %self.channel_name,
                topic = %topic_name,
                disabled = ?disabled_mcps,
                "No MCP servers resolved for topic"
            );
        }

        // Load external MCP tools from filtered configs
        let configs: Vec<McpServerConfig> = filtered_mcp_configs
            .iter()
            .map(|(c, _)| c.clone())
            .collect();
        // What each MCP server contributes to this registry.
        let mcp_entries: Vec<McpEntry> = if !configs.is_empty() {
            let mcp_tools =
                crate::tools::mcp_client::load_mcp_tools(&configs, topic_name, topic_path).await;
            admit_mcp_tools(&mut registry, mcp_tools, &disabled_server_tools)
        } else {
            Vec::new()
        };

        // Apply plain-name exclusions (built-in, bridge, and MCP tools)
        for tool_name in &disabled_plain_tools {
            tracing::debug!(
                tool = %tool_name,
                pattern = %matched_pattern_name.unwrap_or("?"),
                "Removing disabled tool"
            );
            registry.remove(tool_name);
        }

        // The MCP groups: what actually landed in this registry. They keep
        // the tools out of the advertised list until the model expands them
        // through `load_mcp_schema`.
        registry.set_mcp_groups(build_mcp_groups(&registry, mcp_entries));
        crate::tools::mcp_schema::register_mcp_schema_tool(&mut registry);

        // The extended group: registered, but kept out of the advertised tool
        // list until the model loads it. Done after the exclusions above, so
        // the group is exactly what this registry ended up with — a tool
        // disabled here is not part of it, and a tool that is not registered
        // (e.g. `jyc_spawn_subagent` off the `agents` channel) is not
        // advertised as something the call would fail on.
        crate::tools::builtin::extended::register_extended_tools(&mut registry);

        // The loader is a tool like any other, and the exclusion pass above
        // ran before it existed — so `disabled_tools = ["load_extended_tools"]`
        // would find nothing to remove and leave the loader in place. Apply
        // the exclusion to it here. The group stays set: with no loader
        // nothing can flip it, so the group's tools stay out of the request
        // (and remain callable by name, like every extended tool).
        if disabled_plain_tools.contains(&"load_extended_tools") {
            tracing::debug!(
                tool = "load_extended_tools",
                pattern = %matched_pattern_name.unwrap_or("?"),
                "Removing disabled tool"
            );
            registry.remove("load_extended_tools");
        }

        // Same for the MCP loader: the exclusion pass above ran before it
        // existed. The groups stay set — with no loader nothing can expand
        // them, so the MCP tools stay out of the request (and remain
        // callable by name, like every deferred tool).
        if disabled_plain_tools.contains(&"load_mcp_schema") {
            tracing::debug!(
                tool = "load_mcp_schema",
                pattern = %matched_pattern_name.unwrap_or("?"),
                "Removing disabled tool"
            );
            registry.remove("load_mcp_schema");
        }

        // External hooks for tool events: global [[hooks]] first, then
        // [[agents.<name>.hooks]] for the routed agent (the pattern name
        // is the agent key — same merge rule as [[commands]]). The
        // registry cache key includes the config snapshot pointer, so a
        // config reload rebuilds the hook set without a restart.
        {
            let cfg = self.config.load();
            let set = jyc_utils::hooks::HookSet::merged_for_agent(
                &cfg.hooks,
                &cfg.agents,
                matched_pattern_name.unwrap_or_default(),
            );
            drop(cfg);
            if !set.is_empty() {
                tracing::info!(
                    topic = %topic_name,
                    agent = %matched_pattern_name.unwrap_or("(global)"),
                    hooks = set.len(),
                    "Tool-event hooks enabled"
                );
                registry.set_hooks(Arc::new(set));
            }
        }

        registry
    }

    /// Cache-aware variant of [`Self::build_tool_registry`].
    ///
    /// The MCP-loaded portion of the registry can be expensive
    /// (subprocess spawn + handshake + `list_tools()` for each
    /// server, even when run in parallel), and historically this ran
    /// on every inbound message. To avoid that, we cache the result
    /// keyed by `(topic, current_config_snapshot_ptr, mcp_override_mtime)`:
    ///
    /// - **Same `topic` + same config snapshot + same override mtime** →
    ///   return a clone of the cached `Arc<ToolRegistry>` (no I/O).
    /// - **Different config snapshot** (e.g. after `config.toml`
    ///   reload via `ArcSwap::store`) → the pointer changes, the
    ///   lookup misses, and we rebuild and re-cache.
    /// - **`/mcp` toggle** rewrites `.jyc/mcp-override.json` → its mtime
    ///   changes → the lookup misses even though no config was reloaded.
    /// - **Different topic** → separate cache entry; topic-level
    ///   (`L3`) `.jyc/config.toml` MCP overlays require it.
    pub(crate) async fn get_or_build_tool_registry(
        &self,
        topic_name: &str,
        topic_path: &Path,
        topic_cfg: Option<&TopicConfig>,
        supports_images: bool,
        matched_pattern_name: Option<&str>,
    ) -> Arc<ToolRegistry> {
        let config_ptr = Arc::as_ptr(&self.config.load()) as usize;
        let key = (
            topic_name.to_string(),
            config_ptr,
            mcp_override_stamp(topic_name, topic_path),
        );

        // Fast path: cache hit with a previously-successful build.
        if let Some(arc) = self.registry_cache.lock().await.get(&key).cloned() {
            return arc;
        }

        // Slow path: rebuild (cache miss or previously-failed key).
        let built = self
            .build_tool_registry(
                topic_name,
                topic_path,
                topic_cfg,
                supports_images,
                matched_pattern_name,
            )
            .await;
        let arc = Arc::new(built);
        let mut cache = self.registry_cache.lock().await;
        // Keep one entry per topic: evict registries left behind by older
        // config snapshots or superseded `/mcp` overrides, so repeated
        // toggles don't accumulate live entries (and their MCP clients).
        cache.retain(|k, _| k.0 != topic_name);
        cache.insert(key, arc.clone());
        drop(cache);
        arc
    }

    /// Get or create the provider for the current model, using the given
    /// pre-derived `agent_cfg`.
    ///
    /// Taking the config as a parameter (instead of calling
    /// [`Self::agent_config`] internally) keeps the entire request
    /// consistent within a single live config read — `process()` snapshots
    /// the config once at the top, and every downstream call sees the same
    /// values, even if a reload happens mid-request.
    pub(crate) fn create_provider(
        &self,
        agent_cfg: &jyc_types::AiConfig,
        model_override: Option<&str>,
    ) -> Result<Box<dyn provider::Provider>> {
        // Both the override and the fallback may be a reference to an `[ai]`
        // value (`model = "${ai.models.review}"`). Resolving here — the choke
        // point every caller on the main chain passes through — is what keeps
        // the fallback honest: it is the one layer with nothing behind it, so its
        // literal text would otherwise reach the provider as a model id. The two
        // small-model sites that build their provider directly (`service::mod`'s
        // turn setup and `reset_session`) resolve the same reference themselves.
        // References resolve against the `[ai]` values in effect for this agent,
        // the same base `resolve_model` uses.
        let model = model_override
            .or(agent_cfg.model.as_deref())
            .ok_or_else(|| {
                anyhow::anyhow!("No model configured. Set [agent].model in config.toml")
            })?;
        let model = jyc_types::resolve_model_reference(model, agent_cfg)
            .map_err(|err| anyhow::anyhow!(err))?;

        provider::create_provider(&model, &agent_cfg.providers)
    }

    /// Get event bus for a topic.
    pub(crate) async fn get_event_bus(&self, topic_name: &str) -> Option<TopicEventBusRef> {
        self.event_buses.lock().await.get(topic_name).cloned()
    }
}

/// One MCP tool as it landed in the registry: which server it came from,
/// its name, and the server's description of it.
struct McpEntry {
    server: String,
    name: String,
    description: String,
}

/// Whether a tool name belongs to one of the built-in loaders.
///
/// The loaders are how the model reaches the deferred groups at all, so an
/// MCP server that exposes a tool under one of those names does not take
/// the name over: the loader is registered after the MCP tools, and a
/// loader the model cannot call is an MCP server it can never expand.
fn is_reserved_loader_name(name: &str) -> bool {
    name == crate::tools::mcp_schema::LOAD_MCP_SCHEMA
        || name == crate::tools::builtin::extended::LOAD_EXTENDED_TOOLS
}

/// Register the MCP tools that survive the filters, and report what each
/// server contributes to the registry.
///
/// Two filters apply before a tool lands:
///
/// - `disabled_tools` in its `server/tool` form excludes one tool of one
///   server, and the tool is not registered at all.
/// - The built-in loaders keep their names. A server that exposes a tool
///   under one of those names does not take the name over — its copy is
///   skipped, and the server's other tools stay reachable through the
///   loader. Registering it would leave the loader shadowed, and a loader
///   the model cannot call is a server it can never expand.
///
/// The returned entries are what the servers contribute to the index. A tool
/// that is not registered contributes nothing: it is not in the registry, so
/// it is neither callable nor in any group's list.
fn admit_mcp_tools(
    registry: &mut ToolRegistry,
    tools: Vec<Box<dyn crate::tools::Tool>>,
    disabled_server_tools: &[&str],
) -> Vec<McpEntry> {
    let mut mcp_entries = Vec::new();
    for tool in tools {
        let source = tool.source().map(|s| s.to_string());
        let name = tool.name().to_string();
        let description = tool.description().to_string();
        let should_skip = disabled_server_tools.iter().any(|dt| {
            if let Some((server, tool_name)) = dt.split_once('/') {
                source.as_deref() == Some(server) && name == tool_name
            } else {
                false
            }
        });
        if should_skip {
            tracing::debug!(
                tool = %name,
                source = ?source,
                "Skipping disabled MCP tool (server/tool format)"
            );
            continue;
        }
        if is_reserved_loader_name(&name) {
            tracing::warn!(
                tool = %name,
                source = ?source,
                "Skipping MCP tool: the name belongs to a built-in loader"
            );
            continue;
        }
        if let Some(server) = source {
            mcp_entries.push(McpEntry {
                server,
                name,
                description,
            });
        }
        registry.register(tool);
    }
    mcp_entries
}

/// Build the MCP groups for a registry from the MCP tools that landed in it.
///
/// A tool removed by `disabled_tools` (plain name) is not part of a group,
/// and a name two servers both expose belongs to the one that won the
/// registration — the registry holds one tool per name, so the other
/// server's copy is not in the index either, or the model would load a
/// server and find a tool it does not own.
///
/// Ownership is read from the registry rather than from the order the
/// tools arrived in: the registry is keyed by name, so whatever holds a
/// name last is the tool the model would call. Reading it back from the
/// registry means a name that a later registration took over is not
/// claimed by the group that lost it.
fn build_mcp_groups(registry: &ToolRegistry, entries: Vec<McpEntry>) -> Vec<Arc<McpGroup>> {
    let mut by_server: Vec<(String, Vec<McpToolEntry>)> = Vec::new();
    for entry in entries {
        // The live tool under this name must still be this server's copy.
        // A tool that was removed, or whose name a later registration took
        // over, is not part of any group.
        if registry.tool_source(&entry.name) != Some(entry.server.as_str()) {
            continue;
        }
        let tool = McpToolEntry {
            name: entry.name,
            description: entry.description,
        };
        let index = by_server
            .iter()
            .position(|(known, _)| known == &entry.server);
        match index {
            Some(index) => by_server[index].1.push(tool),
            None => by_server.push((entry.server, vec![tool])),
        }
    }

    by_server
        .into_iter()
        .map(|(server, tools)| Arc::new(McpGroup::new(server, tools)))
        .collect()
}

/// Modification time (ns since the Unix epoch) of the topic's
/// `mcp-override.json`, or `0` when the file is absent or unreadable.
///
/// Part of the tool-registry cache key: a `/mcp` toggle rewrites the
/// override file without touching the config snapshot, so this stamp is
/// what makes [`JycAgentService::get_or_build_tool_registry`] miss the
/// cache and rebuild on the next message.
fn mcp_override_stamp(topic_name: &str, topic_path: &Path) -> u128 {
    let path =
        resolve_state_dir(topic_name, topic_path).join(jyc_core::session_state::MCP_OVERRIDE_FILE);
    match std::fs::metadata(&path).and_then(|m| m.modified()) {
        Ok(t) => t
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::tests::app_config_with_channels;
    use crate::tools::builtin::read::ReadTool;
    use std::collections::HashMap;

    /// `disabled_tools` must be able to remove the loader itself.
    ///
    /// The exclusion pass runs before the extended group is wired, so the
    /// loader does not exist yet when the pass looks for it — without a
    /// second pass `disabled_tools = ["load_extended_tools"]` would be a
    /// no-op, and the tool that `disabled_tools` is supposed to remove would
    /// sit in every request advertising a group nobody may load.
    #[tokio::test]
    async fn the_loader_can_be_disabled() {
        let tname = "the_loader_can_be_disabled";
        let tmp = tempfile::tempdir().unwrap();
        jyc_types::state_dir::register(tname, &tmp.path().join(".jyc"));

        let channel = jyc_types::ChannelConfig {
            patterns: Some(vec![ChannelPattern::default()]),
            disabled_tools: Some(vec!["load_extended_tools".to_string()]),
            ..Default::default()
        };
        let mut channels = HashMap::new();
        channels.insert("test".to_string(), channel);
        let svc = JycAgentService::new(
            app_config_with_channels(None, channels),
            tmp.path().to_path_buf(),
            vec![ChannelPattern::default()],
            None,
            None,
            None,
            None,
            "test".to_string(),
        );

        let registry = svc
            .build_tool_registry(tname, tmp.path(), None, false, None)
            .await;

        assert!(
            !registry.has_tool("load_extended_tools"),
            "disabled_tools must remove the loader, not only the group's tools"
        );
        // With no loader nothing can flip the group, so its tools must stay
        // out of the request — the same hiding the load would have undone.
        let definitions = registry.definitions();
        let advertised: Vec<&str> = definitions.iter().map(|d| d.name.as_str()).collect();
        assert!(
            !advertised.contains(&"jyc_send_to_topic"),
            "the group's tools must stay out of the request: {advertised:?}"
        );
    }

    /// A tool the registry does not hold is not part of a group: the model
    /// would be told about a tool it cannot call.
    #[test]
    fn a_removed_mcp_tool_is_not_in_a_group() {
        let registry = ToolRegistry::new();
        let groups = build_mcp_groups(
            &registry,
            vec![McpEntry {
                server: "github".to_string(),
                name: "create_issue".to_string(),
                description: "Create an issue".to_string(),
            }],
        );
        assert!(
            groups.is_empty(),
            "a tool that is not in the registry must not be in a group"
        );
    }

    /// A stand-in for a tool an MCP server provides: it reports its server,
    /// which is how the registry knows who owns the name.
    struct FakeMcpTool {
        name: String,
        server: String,
    }

    #[async_trait::async_trait]
    impl crate::tools::Tool for FakeMcpTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn source(&self) -> Option<&str> {
            Some(&self.server)
        }

        fn description(&self) -> &str {
            "an MCP tool"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &crate::tools::ToolContext<'_>,
        ) -> anyhow::Result<crate::tools::ToolOutput> {
            Ok(crate::tools::ToolOutput::success("ok"))
        }
    }

    /// A name two servers both expose belongs to the one that won the
    /// registration — the other server's copy is not in the index, or the
    /// model would load a server and find a tool it does not own.
    #[test]
    fn a_duplicated_tool_name_belongs_to_the_last_server() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(FakeMcpTool {
            name: "read".to_string(),
            server: "first".to_string(),
        }));
        // The second registration takes the name over: the registry holds
        // one tool per name.
        registry.register(Box::new(FakeMcpTool {
            name: "read".to_string(),
            server: "second".to_string(),
        }));
        let groups = build_mcp_groups(
            &registry,
            vec![
                McpEntry {
                    server: "first".to_string(),
                    name: "read".to_string(),
                    description: "From the first server".to_string(),
                },
                McpEntry {
                    server: "second".to_string(),
                    name: "read".to_string(),
                    description: "From the second server".to_string(),
                },
            ],
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].server_name(), "second");
        assert_eq!(groups[0].tool_names(), vec!["read"]);
    }

    /// A group's tools are the server's.
    #[test]
    fn the_groups_hold_the_servers_tools() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(FakeMcpTool {
            name: "read".to_string(),
            server: "github".to_string(),
        }));
        let groups = build_mcp_groups(
            &registry,
            vec![McpEntry {
                server: "github".to_string(),
                name: "read".to_string(),
                description: "Read a file".to_string(),
            }],
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].server_name(), "github");
        assert_eq!(groups[0].tool_names(), vec!["read"]);
    }

    /// A built-in that an MCP server took the name over is not in the MCP
    /// group: the registry holds the server's copy, so the group would
    /// advertise a tool the model cannot call.
    #[test]
    fn a_name_taken_over_by_a_later_registration_is_not_in_the_group() {
        let mut registry = ToolRegistry::new();
        // The MCP server's copy, then a built-in takes the name over.
        registry.register(Box::new(FakeMcpTool {
            name: "read".to_string(),
            server: "github".to_string(),
        }));
        registry.register(Box::new(ReadTool));
        let groups = build_mcp_groups(
            &registry,
            vec![McpEntry {
                server: "github".to_string(),
                name: "read".to_string(),
                description: "Read a file".to_string(),
            }],
        );
        assert!(
            groups.is_empty(),
            "a name a later registration took over must not be in a group"
        );
    }

    /// The loaders keep their names: an MCP server that exposes a tool
    /// under one of those names does not take the name over — the loader is
    /// how the model reaches the MCP tools at all.
    #[test]
    fn the_loader_names_are_reserved() {
        assert!(is_reserved_loader_name("load_mcp_schema"));
        assert!(is_reserved_loader_name("load_extended_tools"));
        assert!(!is_reserved_loader_name("create_issue"));
        assert!(!is_reserved_loader_name("load_something_else"));
    }

    /// The reservation is enforced where the tools are admitted, not only
    /// described: an MCP server's copy of a loader's name is neither
    /// registered nor grouped.
    ///
    /// This is the production path — the same function the service calls
    /// when it loads the servers — so a reserved name cannot reach the
    /// registry and shadow the loader that makes the other MCP tools
    /// reachable in the first place.
    #[test]
    fn a_reserved_name_is_neither_registered_nor_grouped() {
        let mut registry = ToolRegistry::new();
        let entries = admit_mcp_tools(
            &mut registry,
            vec![
                Box::new(FakeMcpTool {
                    name: "load_mcp_schema".to_string(),
                    server: "github".to_string(),
                }),
                Box::new(FakeMcpTool {
                    name: "load_extended_tools".to_string(),
                    server: "github".to_string(),
                }),
                Box::new(FakeMcpTool {
                    name: "create_issue".to_string(),
                    server: "github".to_string(),
                }),
            ],
            &[],
        );

        assert!(
            !registry.has_tool("load_mcp_schema"),
            "a reserved name must not be registered: the loader would be shadowed"
        );
        assert!(
            !registry.has_tool("load_extended_tools"),
            "a reserved name must not be registered: the loader would be shadowed"
        );
        assert!(
            registry.has_tool("create_issue"),
            "the server's other tools stay reachable"
        );
        assert!(
            !entries.iter().any(|e| e.name == "load_mcp_schema")
                && !entries.iter().any(|e| e.name == "load_extended_tools"),
            "a reserved name must not be in the index: it is not a tool of the server"
        );
        assert!(
            entries.iter().any(|e| e.name == "create_issue"),
            "the server's other tools stay in the index"
        );
    }

    /// A tool excluded in `server/tool` form is neither registered nor in
    /// the index — the exclusion removes the tool, not just its schema.
    #[test]
    fn an_excluded_server_tool_is_neither_registered_nor_grouped() {
        let mut registry = ToolRegistry::new();
        let entries = admit_mcp_tools(
            &mut registry,
            vec![
                Box::new(FakeMcpTool {
                    name: "create_issue".to_string(),
                    server: "github".to_string(),
                }),
                Box::new(FakeMcpTool {
                    name: "delete_repo".to_string(),
                    server: "github".to_string(),
                }),
            ],
            &["github/delete_repo"],
        );

        assert!(
            !registry.has_tool("delete_repo"),
            "an excluded tool must not be registered"
        );
        assert!(
            registry.has_tool("create_issue"),
            "the server's other tools stay reachable"
        );
        assert!(
            !entries.iter().any(|e| e.name == "delete_repo"),
            "an excluded tool must not be in the index"
        );
    }

    /// `disabled_tools` must be able to remove the MCP loader as well: it
    /// is registered after the exclusion pass, so without a second pass the
    /// tool `disabled_tools` is supposed to remove would sit in every
    /// request.
    #[tokio::test]
    async fn the_mcp_loader_can_be_disabled() {
        let tname = "the_mcp_loader_can_be_disabled";
        let tmp = tempfile::tempdir().unwrap();
        jyc_types::state_dir::register(tname, &tmp.path().join(".jyc"));

        let channel = jyc_types::ChannelConfig {
            patterns: Some(vec![ChannelPattern::default()]),
            disabled_tools: Some(vec!["load_mcp_schema".to_string()]),
            ..Default::default()
        };
        let mut channels = HashMap::new();
        channels.insert("test".to_string(), channel);
        let svc = JycAgentService::new(
            app_config_with_channels(None, channels),
            tmp.path().to_path_buf(),
            vec![ChannelPattern::default()],
            None,
            None,
            None,
            None,
            "test".to_string(),
        );

        let registry = svc
            .build_tool_registry(tname, tmp.path(), None, false, None)
            .await;

        assert!(
            !registry.has_tool("load_mcp_schema"),
            "disabled_tools must remove the MCP loader"
        );
    }

    #[test]
    fn mcp_override_stamp_tracks_file() {
        let tmp = tempfile::tempdir().unwrap();
        jyc_types::state_dir::register("stamp-test-topic", &tmp.path().join(".jyc"));
        // Absent file -> 0.
        assert_eq!(mcp_override_stamp("stamp-test-topic", tmp.path()), 0);

        let dir = resolve_state_dir("stamp-test-topic", tmp.path());
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(jyc_core::session_state::MCP_OVERRIDE_FILE);
        std::fs::write(&file, b"{}").unwrap();
        let s1 = mcp_override_stamp("stamp-test-topic", tmp.path());
        assert_ne!(s1, 0);

        // A rewrite (what a `/mcp` toggle does) must change the stamp.
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(&file, b"{}").unwrap();
        let s2 = mcp_override_stamp("stamp-test-topic", tmp.path());
        assert_ne!(s2, s1);
    }
}
