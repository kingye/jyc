use anyhow::Result;

/// Run the MCP reply tool server (stdio transport).
///
/// This is a hidden subcommand invoked by the agent as a subprocess.
/// It runs an rmcp stdio server with the `reply_message` tool.
///
/// Environment:
/// - `JYC_ROOT`: path to the project root (for config loading)
/// - `cwd`: set by agent to the topic directory
/// - `JYC_WORKDIR`: instance workdir for the topic state-dir registry
///   (defaults to the platform data home). Needed because adopted topic
///   state lives outside the topic dir and this subprocess starts with an
///   empty registry.
pub async fn run() -> Result<()> {
    // Rebuild the topic-dir -> state-dir mapping from breadcrumb scan so
    // jyc-mcp's jyc_dir() lookups resolve relocated state.
    let workdir = std::env::var_os("JYC_WORKDIR")
        .map(std::path::PathBuf::from)
        .or_else(jyc_utils::paths::data_home)
        .unwrap_or_default();
    jyc_core::topic_path::restore_state_registry(&jyc_core::topic_path::state_root(&workdir));
    jyc_mcp::reply_tool::run_server().await
}
