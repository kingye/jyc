use anyhow::Result;

/// Run the MCP reply tool server (stdio transport).
///
/// This is a hidden subcommand invoked by OpenCode as a subprocess.
/// It runs an rmcp stdio server with the `reply_message` tool.
///
/// Environment:
/// - `JYC_ROOT`: path to the project root (for config loading)
/// - `cwd`: set by OpenCode to the thread directory
pub async fn run() -> Result<()> {
    crate::mcp::reply_tool::run_server().await
}
