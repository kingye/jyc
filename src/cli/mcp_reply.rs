use anyhow::Result;

/// Run the MCP reply tool server (stdio transport).
///
/// This is a hidden subcommand invoked by OpenCode as a subprocess.
/// It runs an rmcp stdio server with the `reply_message` tool.
pub async fn run() -> Result<()> {
    // Phase 5: Full MCP reply tool implementation
    tracing::warn!("MCP reply tool not yet implemented (Phase 5)");
    anyhow::bail!("MCP reply tool not yet implemented")
}
