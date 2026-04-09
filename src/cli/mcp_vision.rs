/// CLI entry point for the MCP vision tool server.
pub async fn run() -> anyhow::Result<()> {
    crate::mcp::vision_tool::run_server().await
}
