//! Reproduction for the `[[agents.<name>.mcps]]` pipeline: confirm
//! that the array-of-tables form is parsed and that the synthesized
//! pattern (via `fill_into_pattern`) carries `mcps` through.
//!
//! User report: with `pipe = { agent = "..." }` and
//! `[[agents.xxx.mcps]] = [ jin_full_mcp, ... ]`, the agent's MCP
//! tools never register. With the same MCP under global `[[mcps]]`
//! (or the legacy `[[channels.xxx.mcps]]`) the tools load fine.
//!
//! Symptom in the agent: `Resolved MCP servers for topic ... mcps=[]`
//! (i.e. `filtered_mcp_configs` is empty at `register_tools`).
//!
//! This test isolates two stages:
//! 1. **Deserialization**: does `config.agents["..."].mcps` get
//!    populated from `[[agents.x.mcps]]`?
//! 2. **Synthesis**: does `fill_into_pattern` copy `mcps` to
//!    `pattern.mcps` on the synthesized `ChannelPattern`?
//!
//! Both are pure-config tests, no MCP server required. URL/auth_header
//! are env-var dependent so we assert only on name + variant kind —
//! the presence/absence of the entry is what matters here.

use jyc_types::load_config_from_str;

const AGENT_NAME: &str = "newbee_order_bot";
const MCP_NAME: &str = "jin_full_mcp";

fn user_config() -> String {
    format!(
        r#"
[general]
max_concurrent_topics = 3

[channels.{AGENT_NAME}]
type = "wecom_bot"

[channels.{AGENT_NAME}.wecom_bot]
bot_id = "x"
secret = "${{WECOM_BOT_NEW_BEE_SECRET}}"

[[channels.{AGENT_NAME}.patterns]]
pipe = {{ agent = "{AGENT_NAME}", topic = "bot-${{msg.channel_uid}}" }}
name = "newbee-sales-order"
enabled = true

[agents.{AGENT_NAME}]
model = "deepseek/deepseek-v4-flash"
disabled_tools = ["edit"]
disabled_mcp_servers = ["invoice"]
template = "issue-sales-order"

[agents.{AGENT_NAME}.access]
write = ["/exchange"]

[agents.{AGENT_NAME}.reset_compression]
mode = "heuristic"
keep_pairs = 3

[[agents.{AGENT_NAME}.mcps]]
name = "{MCP_NAME}"
type = "remote"
url = "${{MCP_FULL_URL}}"
auth_header = "${{MCP_FULL_API_KEY}}"

[ai]
enabled = true
mode = "agent"
model = "deepseek/deepseek-v4-pro"
"#,
    )
}

fn parsed_agent(config: &jyc_types::AppConfig) -> &jyc_types::AgentConfig {
    config
        .agents
        .get(AGENT_NAME)
        .expect("agent entry must be loaded")
}

#[test]
fn agent_mcps_array_of_tables_populates_mcps_field() {
    let config = load_config_from_str(&user_config()).expect("config must parse and deserialize");
    let agent = parsed_agent(&config);

    let mcps = agent.mcps.as_ref().unwrap_or_else(|| {
        panic!("agent.mcps must be Some([..]]) after parsing [[agents.x.mcps]]")
    });

    assert_eq!(mcps.len(), 1, "exactly one MCP should be loaded");
    assert_eq!(mcps[0].name, MCP_NAME);

    // flatten + tag = "remote" → McpServerKind::Remote
    match &mcps[0].kind {
        jyc_types::McpServerKind::Remote { .. } => {}
        other => panic!("expected Remote variant, got {other:?}"),
    }
}

#[test]
fn synthesize_agent_pattern_carries_mcps() {
    let config = load_config_from_str(&user_config()).expect("config must parse and deserialize");
    let agent = parsed_agent(&config);

    // Drive the same `fill_into_pattern` the runtime uses (the
    // `synthesize_agent_pattern` helper is a thin wrapper around it).
    let mut pattern = jyc_types::ChannelPattern::default();
    let default_topic_path = std::path::PathBuf::from(format!("/data/agents/{AGENT_NAME}"));
    agent.fill_into_pattern(&mut pattern, AGENT_NAME, default_topic_path);

    let mcps = pattern.mcps.as_ref().unwrap_or_else(|| {
        panic!(
            "pattern.mcps must be Some([..]]) after fill_into_pattern — \
             fill_into_pattern does copy it (line 172) so this is unexpected"
        )
    });

    assert_eq!(mcps.len(), 1);
    assert_eq!(mcps[0].name, MCP_NAME);
    match &mcps[0].kind {
        jyc_types::McpServerKind::Remote { .. } => {}
        other => panic!("expected Remote variant, got {other:?}"),
    }
}
