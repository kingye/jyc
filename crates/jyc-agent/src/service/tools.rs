//! Tool registry + provider construction for the in-process agent.
//!
//! Extracted from the monolithic `service.rs`.

use anyhow::Result;
use std::path::Path;

use jyc_core::thread_event_bus::ThreadEventBusRef;
use jyc_types::{McpServerConfig, ThreadConfig};

use crate::provider;
use crate::tools::registry::ToolRegistry;

use super::JycAgentService;

/// Source layer of an MCP server after the L1 (global) / L2 (channel) /
/// L3 (thread-local) overlay resolution. Used only to tag entries for the
/// per-thread observability log; it does not affect tool registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpLayer {
    Global,
    Channel,
    Pattern,
    ThreadOverride,
    ThreadReplace,
}

impl JycAgentService {
    /// Create the tool registry for a thread.
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
        thread_name: &str,
        thread_path: &Path,
        thread_cfg: Option<&ThreadConfig>,
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

        // Register jyc_publish_file with the base URL for shareable links
        // ([inspect] base_url, falling back to http://<bind>).
        let publish_base_url = self
            .config
            .load()
            .inspect
            .clone()
            .unwrap_or_default()
            .effective_base_url();
        crate::tools::builtin::register_publish_file(&mut registry, publish_base_url);

        // Add MCP bridge tools (reply_message, etc.)
        crate::tools::mcp_bridge::register_mcp_tools(&mut registry);

        // Find matched pattern for per-pattern overrides
        let matched_pattern =
            matched_pattern_name.and_then(|name| self.patterns.iter().find(|p| p.name == name));

        // --- L3 thread-local config lifecycle log ---
        // Always emit one of three outcomes so a remote-deploy grep on
        // "thread-local MCP" or "thread config loaded" reveals whether the
        // L3 file was found, valid, and applied. This is the only signal
        // that tells operators whether the thread-local overlay engaged.
        let thread_cfg_path = thread_path.join(".jyc").join("config.toml");
        let configured_mcps = thread_cfg
            .and_then(|t| t.mcps.as_ref())
            .map(|v| v.len())
            .unwrap_or(0);
        let mcps_replace = thread_cfg.map(|t| t.mcps_replace).unwrap_or(false);
        match thread_cfg {
            None => tracing::debug!(
                channel = %self.channel_name,
                thread = %thread_name,
                path = %thread_cfg_path.display(),
                "no thread-local MCP overlay (L3 file absent or unreadable)"
            ),
            Some(cfg) if cfg.mcps.is_none() && !cfg.mcps_replace => tracing::debug!(
                channel = %self.channel_name,
                thread = %thread_name,
                path = %thread_cfg_path.display(),
                configured_mcps = configured_mcps,
                mcps_replace = mcps_replace,
                "thread config loaded but has no [[mcps]]; L3 is a no-op"
            ),
            Some(cfg) => {
                let names: Vec<&str> = cfg
                    .mcps
                    .as_ref()
                    .map(|v| v.iter().map(|m| m.name.as_str()).collect())
                    .unwrap_or_default();
                tracing::info!(
                    channel = %self.channel_name,
                    thread = %thread_name,
                    path = %thread_cfg_path.display(),
                    configured_mcps = configured_mcps,
                    mcps_replace = mcps_replace,
                    thread_mcp_names = ?names,
                    "Applied thread-local MCP overlay (L3)"
                );
            }
        }

        // --- MCP server exclusion (disabled_mcp_servers) ---
        // Merge channel-level + pattern-level disabled MCP servers
        let disabled_mcp_servers: Vec<&str> = {
            let mut set = Vec::new();
            if let Some(ref servers) = self.channel_disabled_mcp_servers {
                for s in servers {
                    set.push(s.as_str());
                }
            }
            if let Some(servers) = matched_pattern.and_then(|p| p.disabled_mcp_servers.as_ref()) {
                for s in servers {
                    if !set.contains(&s.as_str()) {
                        set.push(s.as_str());
                    }
                }
            }
            set
        };

        // Resolve MCP configs: pattern → channel → global. Tag each baseline
        // entry with its source layer so the per-thread log can show exactly
        // which MCP came from which config layer.
        let (base_mcps, base_layer): (&[McpServerConfig], McpLayer) =
            if let Some(p) = matched_pattern.and_then(|p| p.mcps.as_ref()) {
                (p.as_slice(), McpLayer::Pattern)
            } else if let Some(c) = self.channel_mcp_configs.as_deref() {
                (c, McpLayer::Channel)
            } else {
                (self.mcp_configs.as_slice(), McpLayer::Global)
            };

        // Layer thread (L3) MCPs from the pre-loaded <thread_path>/.jyc/config.toml
        // on top: additive by default, opt-in full replace via `mcps_replace = true`.
        // (Caller in `process` loads thread_cfg once per message and shares it
        //  with the [agent] model-resolution block, avoiding a duplicate disk read.)
        let effective_mcps: Vec<McpServerConfig> =
            jyc_types::apply_thread_mcp_overlay(base_mcps, thread_cfg);

        // Re-tag the resolved list with the source layer each MCP came from.
        // `apply_thread_mcp_overlay` keeps base entries in place for unchanged
        // names and appends new ones; we attribute each by comparing against
        // the baseline. When `mcps_replace=true` every entry is from the L3.
        let mut filtered_mcp_configs: Vec<(McpServerConfig, McpLayer)> = effective_mcps
            .into_iter()
            .map(|c| {
                let layer = if mcps_replace || !base_mcps.iter().any(|b| b.name == c.name) {
                    McpLayer::ThreadOverride
                } else {
                    base_layer
                };
                (c, layer)
            })
            .collect();

        // Filter out disabled MCP servers before loading
        filtered_mcp_configs.retain(|(c, _)| !disabled_mcp_servers.contains(&c.name.as_str()));

        if !disabled_mcp_servers.is_empty() {
            tracing::debug!(
                disabled = ?disabled_mcp_servers,
                "MCP servers disabled by config"
            );
        }

        // --- Tool exclusion (disabled_tools) ---
        // Merge channel-level + pattern-level + backward-compatible alias
        let disabled_tools: Vec<&str> = {
            let mut set = Vec::new();
            if let Some(ref tools) = self.channel_disabled_tools {
                for t in tools {
                    set.push(t.as_str());
                }
            }
            if let Some(tools) = matched_pattern.and_then(|p| p.disabled_tools.as_ref()) {
                for t in tools {
                    if !set.contains(&t.as_str()) {
                        set.push(t.as_str());
                    }
                }
            }
            // Backward-compatible alias: disabled_builtin_tools
            if let Some(tools) = matched_pattern.and_then(|p| p.disabled_builtin_tools.as_ref()) {
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

        // Per-thread structured log: which MCPs were actually loaded and
        // which config layer each originated from. Replaces the previous
        // count-only "Loading external MCP tools" line.
        if mcps_replace {
            for (_, l) in filtered_mcp_configs.iter_mut() {
                *l = McpLayer::ThreadReplace;
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
                    McpLayer::ThreadOverride => format!("{}:thread", c.name),
                    McpLayer::ThreadReplace => format!("{}:thread-replace", c.name),
                })
                .collect();
            tracing::info!(
                channel = %self.channel_name,
                thread = %thread_name,
                pattern = ?matched_pattern_name,
                mcps_total = mcps_total,
                from_global = mcps.iter().filter(|s| s.ends_with(":global")).count(),
                from_channel = mcps.iter().filter(|s| s.ends_with(":channel")).count(),
                from_pattern = mcps.iter().filter(|s| s.ends_with(":pattern")).count(),
                from_thread = mcps.iter().filter(|s| s.ends_with(":thread")).count(),
                from_thread_replace = mcps.iter().filter(|s| s.ends_with(":thread-replace")).count(),
                mcps = ?mcps,
                "Resolved MCP servers for thread"
            );
        }

        // Load external MCP tools from filtered configs
        let configs: Vec<McpServerConfig> = filtered_mcp_configs
            .iter()
            .map(|(c, _)| c.clone())
            .collect();
        if !configs.is_empty() {
            let mcp_tools = crate::tools::mcp_client::load_mcp_tools(&configs).await;
            for tool in mcp_tools {
                // Skip tools matching disabled_server_tools (server/tool format)
                let source = tool.source();
                let name = tool.name();
                let should_skip = disabled_server_tools.iter().any(|dt| {
                    if let Some((server, tool_name)) = dt.split_once('/') {
                        source == Some(server) && name == tool_name
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
                registry.register(tool);
            }
        }

        // Apply plain-name exclusions (built-in, bridge, and MCP tools)
        for tool_name in &disabled_plain_tools {
            tracing::debug!(
                tool = %tool_name,
                pattern = %matched_pattern_name.unwrap_or("?"),
                "Removing disabled tool"
            );
            registry.remove(tool_name);
        }

        registry
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
        agent_cfg: &jyc_types::AgentConfig,
        model_override: Option<&str>,
    ) -> Result<Box<dyn provider::Provider>> {
        let model = model_override
            .or(agent_cfg.model.as_deref())
            .ok_or_else(|| {
                anyhow::anyhow!("No model configured. Set [agent].model in config.toml")
            })?;

        provider::create_provider(model, &agent_cfg.providers)
    }

    /// Get event bus for a thread.
    pub(crate) async fn get_event_bus(&self, thread_name: &str) -> Option<ThreadEventBusRef> {
        self.event_buses.lock().await.get(thread_name).cloned()
    }
}
