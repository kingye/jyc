/// CLI entry point for the MCP question tool server.
pub async fn run() -> anyhow::Result<()> {
    crate::mcp::question_tool::run_server().await
}
