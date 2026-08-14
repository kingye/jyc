
use super::*;
use crate::config::load_config_from_str;

fn valid_config_toml() -> &'static str {
    r#"
[general]
max_concurrent_threads = 3

[channels.work]
type = "email"

[channels.work.inbound]
host = "imap.example.com"
port = 993
username = "user"
password = "pass"

[channels.work.outbound]
host = "smtp.example.com"
port = 465
username = "user"
password = "pass"

[agent]
enabled = true
mode = "agent"
"#
}

#[test]
fn test_valid_config_passes() {
    let config = load_config_from_str(valid_config_toml()).unwrap();
    let errors = validate_config(&config);
    assert!(errors.is_empty(), "expected no errors, got: {:?}", errors);
}

/// `base_url` is an opaque link prefix, so scheme + host,
/// optional port, and an optional subpath must all be accepted.
#[test]
fn test_base_url_accepts_scheme_port_and_subpath() {
    for base in [
        "https://jyc.example.com",
        "http://192.168.1.50:9876",
        "https://jyc.example.com:8443",
        "https://example.com/jyc",
        "http://192.168.1.50:9876/",
    ] {
        let toml = format!(
            "{}\n[inspect]\nenabled = true\nbase_url = \"{base}\"\n",
            valid_config_toml()
        );
        let config = load_config_from_str(&toml).unwrap();
        let errors = validate_config(&config);
        assert!(
            !errors.iter().any(|e| e.path == "inspect.base_url"),
            "'{base}' must be accepted, got: {errors:?}"
        );
    }
}

/// Without a scheme a browser reads the value as a relative path, so the
/// link breaks silently — must fail at startup instead.
#[test]
fn test_base_url_rejects_missing_scheme_or_empty() {
    for base in ["jyc.example.com", "192.168.1.50:9876", "", "  "] {
        let toml = format!(
            "{}\n[inspect]\nenabled = true\nbase_url = \"{base}\"\n",
            valid_config_toml()
        );
        let config = load_config_from_str(&toml).unwrap();
        let errors = validate_config(&config);
        assert!(
            errors.iter().any(|e| e.path == "inspect.base_url"),
            "'{base}' must be rejected"
        );
    }
}

/// Omitting the setting stays legal — the fallback covers local use.
#[test]
fn test_base_url_may_be_omitted() {
    let toml = format!(
        "{}\n[inspect]\nenabled = true\nbind = \"0.0.0.0:9876\"\n",
        valid_config_toml()
    );
    let config = load_config_from_str(&toml).unwrap();
    let errors = validate_config(&config);
    assert!(errors.is_empty(), "expected no errors, got: {errors:?}");
}

#[test]
fn test_empty_channels_fails() {
    let toml = r#"
[general]
[agent]
enabled = true
mode = "agent"
"#;
    let config = load_config_from_str(toml).unwrap();
    let errors = validate_config(&config);
    assert!(errors.iter().any(|e| e.path == "channels"));
}

#[test]
fn test_invalid_monitor_mode() {
    let toml = r#"
[general]
[channels.work]
type = "email"
[channels.work.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.work.outbound]
host = "h"
port = 465
username = "u"
password = "p"
[channels.work.monitor]
mode = "websocket"
[agent]
enabled = true
mode = "agent"
"#;
    let config = load_config_from_str(toml).unwrap();
    let errors = validate_config(&config);
    assert!(errors.iter().any(|e| e.path.contains("monitor.mode")));
}

#[test]
fn test_invalid_regex_in_pattern() {
    let toml = r#"
[general]
[channels.work]
type = "email"
[channels.work.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.work.outbound]
host = "h"
port = 465
username = "u"
password = "p"

[[channels.work.patterns]]
name = "test"
[channels.work.patterns.rules.sender]
regex = "[invalid"

[agent]
enabled = true
mode = "agent"
"#;
    let config = load_config_from_str(toml).unwrap();
    let errors = validate_config(&config);
    assert!(errors.iter().any(|e| e.path.contains("sender.regex")));
}

#[test]
fn test_static_mode_requires_text() {
    let toml = r#"
[general]
[channels.work]
type = "email"
[channels.work.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.work.outbound]
host = "h"
port = 465
username = "u"
password = "p"
[agent]
enabled = true
mode = "static"
"#;
    let config = load_config_from_str(toml).unwrap();
    let errors = validate_config(&config);
    assert!(errors.iter().any(|e| e.path == "agent.text"));
}

#[test]
fn test_invalid_mcp_in_pattern() {
    let toml = r#"
[general]
[channels.work]
type = "email"
[channels.work.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.work.outbound]
host = "h"
port = 465
username = "u"
password = "p"

[[channels.work.patterns]]
name = "mcp-test"
[channels.work.patterns.rules]

[[channels.work.patterns.mcps]]
name = ""
type = "local"
command = []

[agent]
enabled = true
mode = "agent"
"#;
    let config = load_config_from_str(toml).unwrap();
    let errors = validate_config(&config);
    assert!(
        errors.iter().any(|e| e.path.contains("mcps[0].name")),
        "expected mcps[0].name error, got: {:?}",
        errors
    );
}

#[test]
fn test_invalid_mcp_local_no_command() {
    let toml = r#"
[general]
[channels.work]
type = "email"
[channels.work.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.work.outbound]
host = "h"
port = 465
username = "u"
password = "p"

[[channels.work.patterns]]
name = "mcp-test"
[channels.work.patterns.rules]

[[channels.work.patterns.mcps]]
name = "my-mcp"
type = "local"
command = []

[agent]
enabled = true
mode = "agent"
"#;
    let config = load_config_from_str(toml).unwrap();
    let errors = validate_config(&config);
    assert!(
        errors.iter().any(|e| e.path.contains("mcps[0].command")),
        "expected mcps[0].command error, got: {:?}",
        errors
    );
}

#[test]
fn test_invalid_mcp_remote_no_url() {
    let toml = r#"
[general]
[channels.work]
type = "email"
[channels.work.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.work.outbound]
host = "h"
port = 465
username = "u"
password = "p"

[[channels.work.patterns]]
name = "mcp-test"
[channels.work.patterns.rules]

[[channels.work.patterns.mcps]]
name = "my-remote"
type = "remote"
url = ""

[agent]
enabled = true
mode = "agent"
"#;
    let config = load_config_from_str(toml).unwrap();
    let errors = validate_config(&config);
    assert!(
        errors.iter().any(|e| e.path.contains("mcps[0].url")),
        "expected mcps[0].url error, got: {:?}",
        errors
    );
}

#[test]
fn test_valid_mcp_in_pattern_passes() {
    let toml = r#"
[general]
[channels.work]
type = "email"
[channels.work.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.work.outbound]
host = "h"
port = 465
username = "u"
password = "p"

[[channels.work.patterns]]
name = "mcp-test"
[channels.work.patterns.rules]

[[channels.work.patterns.mcps]]
name = "my-local"
type = "local"
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[agent]
enabled = true
mode = "agent"
"#;
    let config = load_config_from_str(toml).unwrap();
    let errors = validate_config(&config);
    let mcp_errors: Vec<_> = errors.iter().filter(|e| e.path.contains("mcps")).collect();
    assert!(
        mcp_errors.is_empty(),
        "expected no mcp errors, got: {:?}",
        mcp_errors
    );
}

#[test]
fn test_invalid_mcp_remote_oauth_and_auth_header_conflict() {
    let toml = r#"
[general]
[channels.work]
type = "email"
[channels.work.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.work.outbound]
host = "h"
port = 465
username = "u"
password = "p"

[[channels.work.patterns]]
name = "mcp-test"
[channels.work.patterns.rules]

[[channels.work.patterns.mcps]]
name = "my-remote"
type = "remote"
url = "https://mcp.example.com"
auth_header = "static-token"
[channels.work.patterns.mcps.oauth]
client_id = "id"
client_secret = "secret"
token_endpoint = "https://idp/token"

[agent]
enabled = true
mode = "agent"
"#;
    let config = load_config_from_str(toml).unwrap();
    let errors = validate_config(&config);
    assert!(
        errors.iter().any(
            |e| e.path.contains("mcps[0].auth_header") && e.message.contains("cannot set both")
        ),
        "expected auth_header+oauth conflict error, got: {:?}",
        errors
    );
}

#[test]
fn test_invalid_mcp_remote_oauth_empty_client_id() {
    let toml = r#"
[general]
[channels.work]
type = "email"
[channels.work.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.work.outbound]
host = "h"
port = 465
username = "u"
password = "p"

[[channels.work.patterns]]
name = "mcp-test"
[channels.work.patterns.rules]

[[channels.work.patterns.mcps]]
name = "my-remote"
type = "remote"
url = "https://mcp.example.com"
[channels.work.patterns.mcps.oauth]
client_id = ""
client_secret = "secret"
token_endpoint = "https://idp/token"

[agent]
enabled = true
mode = "agent"
"#;
    let config = load_config_from_str(toml).unwrap();
    let errors = validate_config(&config);
    assert!(
        errors
            .iter()
            .any(|e| e.path.contains("mcps[0].oauth.client_id")),
        "expected oauth.client_id error, got: {:?}",
        errors
    );
}

#[test]
fn test_unified_attachment_config() {
    let toml = r#"
[general]
max_concurrent_threads = 3

[channels.work]
type = "email"

[channels.work.inbound]
host = "imap.example.com"
port = 993
username = "user"
password = "pass"

[channels.work.outbound]
host = "smtp.example.com"
port = 465
username = "user"
password = "pass"

[agent]
enabled = true
mode = "agent"

[attachments]

[attachments.inbound]
enabled = true
allowed_extensions = [".pdf", ".docx"]
max_file_size = "25mb"
max_per_message = 10

[attachments.outbound]
enabled = true
allowed_extensions = [".pdf", ".pptx"]
max_file_size = "10mb"
max_per_message = 5
"#;
    let config = load_config_from_str(toml).unwrap();

    // Test that unified config is loaded
    assert!(config.attachments.is_some());
    let attachments = config.attachments.as_ref().unwrap();

    // Test inbound config
    assert!(attachments.inbound.is_some());
    let inbound = attachments.inbound.as_ref().unwrap();
    assert!(inbound.enabled);
    assert_eq!(inbound.allowed_extensions, vec![".pdf", ".docx"]);
    assert_eq!(inbound.max_file_size, Some("25mb".to_string()));
    assert_eq!(inbound.max_per_message, Some(10));

    // Test outbound config
    assert!(attachments.outbound.is_some());
    let outbound = attachments.outbound.as_ref().unwrap();
    assert!(outbound.enabled);
    assert_eq!(outbound.allowed_extensions, vec![".pdf", ".pptx"]);
    assert_eq!(outbound.max_file_size, Some("10mb".to_string()));
    assert_eq!(outbound.max_per_message, Some(5));

    // Test validation passes
    let errors = validate_config(&config);
    assert!(errors.is_empty(), "expected no errors, got: {:?}", errors);
}

#[test]
fn test_invalid_unified_attachment_config() {
    let toml = r#"
[general]
max_concurrent_threads = 3

[channels.work]
type = "email"

[channels.work.inbound]
host = "imap.example.com"
port = 993
username = "user"
password = "pass"

[channels.work.outbound]
host = "smtp.example.com"
port = 465
username = "user"
password = "pass"

[agent]
enabled = true
mode = "agent"

[attachments]

[attachments.inbound]
enabled = true
allowed_extensions = ["pdf", ".docx"]  # Missing dot in first extension
max_file_size = "invalid_size"
max_per_message = 0  # Invalid: must be at least 1

[attachments.outbound]
enabled = true
allowed_extensions = [".pdf", ".pptx"]
max_file_size = "10mb"
max_per_message = 5
"#;
    let config = load_config_from_str(toml).unwrap();
    let errors = validate_config(&config);

    // Should have errors for invalid extension and max_per_message
    assert!(errors.iter().any(|e| e.path.contains("allowed_extensions")));
    assert!(errors.iter().any(|e| e.path.contains("max_file_size")));
    assert!(errors.iter().any(|e| e.path.contains("max_per_message")));
}

#[test]
fn test_wecom_valid_config_passes() {
    let toml = r#"
[general]
max_concurrent_threads = 3

[channels.wecom_bot]
type = "wecom"

[channels.wecom_bot.wecom]
token = "wecom_token_xxx"
encoding_aes_key = "abc123abc123abc123abc123abc123abc123abc123abc123abc12"
corp_id = "ww1234567890abcdef"
corp_secret = "my_corp_secret_value"

[agent]
enabled = true
mode = "agent"
"#;
    let config = load_config_from_str(toml).unwrap();
    let errors = validate_config(&config);
    assert!(errors.is_empty(), "expected no errors, got: {:?}", errors);
}

#[test]
fn test_wecom_missing_config_fails() {
    let toml = r#"
[general]
[channels.wecom_bot]
type = "wecom"

[agent]
enabled = true
mode = "agent"
"#;
    let config = load_config_from_str(toml).unwrap();
    let errors = validate_config(&config);
    assert!(errors.iter().any(|e| e.path.contains("wecom")));
}

#[test]
fn test_wecom_missing_token_fails() {
    let toml = r#"
[general]
[channels.wecom_bot]
type = "wecom"

[channels.wecom_bot.wecom]
token = ""
encoding_aes_key = "abc123abc123abc123abc123abc123abc123abc123abc123abc12"
corp_id = "ww1234567890abcdef"
corp_secret = "my_corp_secret_value"

[agent]
enabled = true
mode = "agent"
"#;
    let config = load_config_from_str(toml).unwrap();
    let errors = validate_config(&config);
    assert!(errors.iter().any(|e| e.path.contains("wecom.token")));
}

#[test]
fn test_wecom_missing_corp_secret_fails() {
    let toml = r#"
[general]
[channels.wecom_bot]
type = "wecom"

[channels.wecom_bot.wecom]
token = "valid_token"
encoding_aes_key = "abc123abc123abc123abc123abc123abc123abc123abc123abc12"
corp_id = "ww1234567890abcdef"
corp_secret = ""

[agent]
enabled = true
mode = "agent"
"#;
    let config = load_config_from_str(toml).unwrap();
    let errors = validate_config(&config);
    assert!(errors.iter().any(|e| e.path.contains("wecom.corp_secret")));
}

#[test]
fn test_disabled_tools_empty_entry_fails() {
    let toml = r#"
[general]
[channels.work]
type = "email"
disabled_tools = ["bash", ""]

[channels.work.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.work.outbound]
host = "h"
port = 465
username = "u"
password = "p"

[agent]
enabled = true
mode = "agent"
"#;
    let config = load_config_from_str(toml).unwrap();
    let errors = validate_config(&config);
    assert!(
        errors.iter().any(|e| {
            e.path.contains("disabled_tools") && e.message.contains("must not be empty")
        })
    );
}

#[test]
fn test_disabled_mcp_servers_empty_entry_fails() {
    let toml = r#"
[general]
[channels.work]
type = "email"
disabled_mcp_servers = ["invoice", ""]

[channels.work.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.work.outbound]
host = "h"
port = 465
username = "u"
password = "p"

[agent]
enabled = true
mode = "agent"
"#;
    let config = load_config_from_str(toml).unwrap();
    let errors = validate_config(&config);
    assert!(errors.iter().any(|e| {
        e.path.contains("disabled_mcp_servers") && e.message.contains("must not be empty")
    }));
}

#[test]
fn test_disabled_tools_valid_passes() {
    let toml = r#"
[general]
[channels.work]
type = "email"
disabled_tools = ["bash", "jyc_send_message"]
disabled_mcp_servers = ["invoice"]

[channels.work.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.work.outbound]
host = "h"
port = 465
username = "u"
password = "p"

[[channels.work.patterns]]
name = "p1"
disabled_tools = ["write"]
disabled_mcp_servers = ["other"]

[channels.work.patterns.rules]

[agent]
enabled = true
mode = "agent"
"#;
    let config = load_config_from_str(toml).unwrap();
    let errors = validate_config(&config);
    assert!(
        errors.iter().all(|e| {
            !e.path.contains("disabled_tools") && !e.path.contains("disabled_mcp_servers")
        }),
        "expected no disabled_tools/mcp_servers errors, got: {:?}",
        errors
    );
}

#[test]
fn test_pattern_disabled_tools_empty_entry_fails() {
    let toml = r#"
[general]
[channels.work]
type = "email"

[channels.work.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.work.outbound]
host = "h"
port = 465
username = "u"
password = "p"

[[channels.work.patterns]]
name = "p1"
disabled_tools = ["bash", ""]

[channels.work.patterns.rules]

[agent]
enabled = true
mode = "agent"
"#;
    let config = load_config_from_str(toml).unwrap();
    let errors = validate_config(&config);
    assert!(errors.iter().any(|e| {
        e.path.contains("patterns[0].disabled_tools") && e.message.contains("must not be empty")
    }));
}

/// Base config with a valid channel + agent, plus arbitrary extra TOML.
fn config_with(extra: &str) -> String {
    format!(
        r#"
[channels.work]
type = "email"
[channels.work.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.work.outbound]
host = "h"
port = 465
username = "u"
password = "p"
[agent]
enabled = true
mode = "agent"
{extra}
"#
    )
}

#[test]
fn custom_command_valid_passes() {
    let toml = config_with(
        r#"
[[commands]]
name = "review"
description = "Review the PR"
mode = "plan"
skills = ["pr-review"]
user_prompt = "Review it."
"#,
    );
    let config = load_config_from_str(&toml).unwrap();
    let errors = validate_config(&config);
    assert!(
        !errors.iter().any(|e| e.path.starts_with("commands")),
        "expected no command errors, got: {errors:?}"
    );
    assert_eq!(config.commands.len(), 1);
    assert_eq!(config.commands[0].mode.as_deref(), Some("plan"));
}

#[test]
fn custom_command_mode_optional_and_defaults_none() {
    let toml = config_with(
        r#"
[[commands]]
name = "summarize"
user_prompt = "Summarize."
"#,
    );
    let config = load_config_from_str(&toml).unwrap();
    assert!(config.commands[0].mode.is_none());
    assert!(config.commands[0].skills.is_none());
    assert!(
        !validate_config(&config)
            .iter()
            .any(|e| e.path.starts_with("commands"))
    );
}

#[test]
fn custom_command_rejects_builtin_shadow() {
    let toml = config_with(
        r#"
[[commands]]
name = "plan"
user_prompt = "x"
"#,
    );
    let config = load_config_from_str(&toml).unwrap();
    let errors = validate_config(&config);
    assert!(
        errors
            .iter()
            .any(|e| e.path == "commands[0].name" && e.message.contains("shadows a built-in")),
        "got: {errors:?}"
    );
}

#[test]
fn custom_command_rejects_bad_mode_and_empty_prompt() {
    let toml = config_with(
        r#"
[[commands]]
name = "review"
mode = "yolo"
user_prompt = "   "
"#,
    );
    let config = load_config_from_str(&toml).unwrap();
    let errors = validate_config(&config);
    assert!(errors.iter().any(|e| e.path == "commands[0].mode"));
    assert!(errors.iter().any(|e| e.path == "commands[0].user_prompt"));
}

#[test]
fn custom_command_rejects_duplicates_and_empty_name() {
    let toml = config_with(
        r#"
[[commands]]
name = "review"
user_prompt = "a"

[[commands]]
name = "review"
user_prompt = "b"

[[commands]]
name = ""
user_prompt = "c"
"#,
    );
    let config = load_config_from_str(&toml).unwrap();
    let errors = validate_config(&config);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("duplicate command name"))
    );
    assert!(errors.iter().any(|e| e.path == "commands[2].name"));
}

#[test]
fn commands_default_to_empty() {
    let config = load_config_from_str(&config_with("")).unwrap();
    assert!(config.commands.is_empty());
}

#[test]
fn custom_command_rejects_uppercase_name() {
    let toml = config_with(
        r#"
[[commands]]
name = "Review"
user_prompt = "x"
"#,
    );
    let config = load_config_from_str(&toml).unwrap();
    let errors = validate_config(&config);
    assert!(
        errors
            .iter()
            .any(|e| e.path == "commands[0].name" && e.message.contains("must be lowercase")),
        "uppercase names are unreachable (registry lowercases lookups), got: {errors:?}"
    );
}

/// `review` and `/review` normalize to the same registered name, so the
/// duplicate check must compare the normalized form.
#[test]
fn custom_command_duplicate_detection_normalizes_slash() {
    let toml = config_with(
        r#"
[[commands]]
name = "review"
user_prompt = "a"

[[commands]]
name = "/review"
user_prompt = "b"
"#,
    );
    let config = load_config_from_str(&toml).unwrap();
    let errors = validate_config(&config);
    assert!(
        errors
            .iter()
            .any(|e| e.path == "commands[1].name" && e.message.contains("duplicate")),
        "'/review' collides with 'review' at registration, got: {errors:?}"
    );
}
