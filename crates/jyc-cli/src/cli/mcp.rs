//! `jyc mcp` — MCP server maintenance commands.

use anyhow::Result;
use clap::Subcommand;
use std::path::Path;

use jyc_types::{AppConfig, McpServerKind, OAuthDcrConfig, load_config_layered};

use super::resolve::resolve_config;

#[derive(Debug, Subcommand)]
pub enum McpAction {
    /// One-time browser authorization for a remote MCP server that uses the
    /// MCP OAuth 2.1 flow (`[mcps.oauth_dcr]`): prints an authorization URL,
    /// you approve in a browser, paste the redirect URL back, and the tokens
    /// are stored. `jyc serve` refreshes them automatically afterwards.
    Auth {
        /// MCP server name (the `[[mcps]]` entry that has `[mcps.oauth_dcr]`)
        name: String,
        /// Config file path
        #[arg(short, long)]
        config: Option<String>,
    },
}

pub async fn run(action: &McpAction, workdir: &Path, workdir_explicit: bool) -> Result<()> {
    match action {
        McpAction::Auth { name, config } => {
            let resolution = resolve_config(workdir, config.as_deref(), workdir_explicit)?;
            let config_path = &resolution.config_path;
            let app = load_config_layered(resolution.global_config_path.as_deref(), config_path)?;
            let (url, dcr) = find_oauth_dcr_server(&app, name, config_path)?;
            jyc_agent::tools::mcp_auth::authorize_interactively(name, &url, &dcr).await
        }
    }
}

/// Locate `name` among MCP definitions — global `[[mcps]]` first, then
/// pattern-level `mcps` overrides — and require it to use `oauth_dcr`.
fn find_oauth_dcr_server(
    app: &AppConfig,
    name: &str,
    config_path: &Path,
) -> Result<(String, OAuthDcrConfig)> {
    let defs = app.mcps.iter().chain(
        app.channels
            .values()
            .flat_map(|c| c.patterns.iter().flatten())
            .flat_map(|p| p.mcps.iter().flatten()),
    );
    for mcp in defs {
        if mcp.name != name {
            continue;
        }
        return match &mcp.kind {
            McpServerKind::Remote {
                url,
                oauth_dcr: Some(dcr),
                ..
            } => Ok((url.clone(), dcr.clone())),
            McpServerKind::Remote { .. } => Err(anyhow::anyhow!(
                "MCP '{name}' has no [mcps.oauth_dcr] block; add it to {} and retry",
                config_path.display()
            )),
            McpServerKind::Local { .. } => Err(anyhow::anyhow!(
                "MCP '{name}' is local; OAuth does not apply"
            )),
        };
    }
    Err(anyhow::anyhow!(
        "no MCP server named '{name}' in {}",
        config_path.display()
    ))
}
