use anyhow::Result;
use regex::Regex;

use crate::channel::ChannelPattern;
use crate::config::AppConfig;

/// A single validation error with context.
#[derive(Debug)]
pub struct ValidationError {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "  {}: {}", self.path, self.message)
    }
}

/// Parse a human-readable file size string into bytes.
fn parse_file_size(input: &str) -> Result<u64> {
    let input = input.trim().to_lowercase();
    if input.is_empty() {
        anyhow::bail!("empty file size string");
    }

    let re = Regex::new(r"^(\d+(?:\.\d+)?)\s*(b|kb?|mb?|gb?|tb?|bytes?)?$").unwrap();
    let caps = re
        .captures(&input)
        .ok_or_else(|| anyhow::anyhow!("invalid file size format: '{input}'"))?;

    let number: f64 = caps[1].parse()?;
    let multiplier: u64 = match caps.get(2).map(|m| m.as_str()) {
        None | Some("") | Some("b") | Some("byte") | Some("bytes") => 1,
        Some("k") | Some("kb") => 1024,
        Some("m") | Some("mb") => 1024 * 1024,
        Some("g") | Some("gb") => 1024 * 1024 * 1024,
        Some("t") | Some("tb") => 1024 * 1024 * 1024 * 1024,
        Some(unit) => anyhow::bail!("unknown file size unit: '{unit}'"),
    };

    Ok((number * multiplier as f64) as u64)
}

/// Validate that a regex pattern compiles without error.
fn validate_regex(pattern: &str) -> Result<Regex> {
    Regex::new(pattern).map_err(|e| anyhow::anyhow!("invalid regex '{}': {}", pattern, e))
}

/// Validate the application configuration.
///
/// Returns a list of validation errors. Empty list means valid.
pub fn validate_config(config: &AppConfig) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    // General
    if config.general.max_concurrent_threads == 0 {
        errors.push(ValidationError {
            path: "general.max_concurrent_threads".into(),
            message: "must be at least 1".into(),
        });
    }
    if config.general.max_queue_size_per_thread == 0 {
        errors.push(ValidationError {
            path: "general.max_queue_size_per_thread".into(),
            message: "must be at least 1".into(),
        });
    }

    // Channels
    if config.channels.is_empty() {
        errors.push(ValidationError {
            path: "channels".into(),
            message: "at least one channel must be configured".into(),
        });
    }

    for (name, channel) in &config.channels {
        let prefix = format!("channels.{name}");

        if channel.channel_type.is_empty() {
            errors.push(ValidationError {
                path: format!("{prefix}.type"),
                message: "channel type is required".into(),
            });
        }

        // Validate email channel specifics
        if channel.channel_type == "email" {
            if let Some(ref inbound) = channel.inbound {
                if inbound.host.is_empty() {
                    errors.push(ValidationError {
                        path: format!("{prefix}.inbound.host"),
                        message: "IMAP host is required".into(),
                    });
                }
                if inbound.username.is_empty() {
                    errors.push(ValidationError {
                        path: format!("{prefix}.inbound.username"),
                        message: "IMAP username is required".into(),
                    });
                }
                if inbound.password.is_empty() {
                    errors.push(ValidationError {
                        path: format!("{prefix}.inbound.password"),
                        message: "IMAP password is required (use ${ENV_VAR} syntax)".into(),
                    });
                }
            }

            if let Some(ref outbound) = channel.outbound {
                if outbound.host.is_empty() {
                    errors.push(ValidationError {
                        path: format!("{prefix}.outbound.host"),
                        message: "SMTP host is required".into(),
                    });
                }
                if outbound.username.is_empty() {
                    errors.push(ValidationError {
                        path: format!("{prefix}.outbound.username"),
                        message: "SMTP username is required".into(),
                    });
                }
                if outbound.password.is_empty() {
                    errors.push(ValidationError {
                        path: format!("{prefix}.outbound.password"),
                        message: "SMTP password is required (use ${ENV_VAR} syntax)".into(),
                    });
                }
            }

            if let Some(ref monitor) = channel.monitor {
                if monitor.mode != "idle" && monitor.mode != "poll" {
                    errors.push(ValidationError {
                        path: format!("{prefix}.monitor.mode"),
                        message: format!("must be 'idle' or 'poll', got '{}'", monitor.mode),
                    });
                }
                if monitor.poll_interval_secs == 0 {
                    errors.push(ValidationError {
                        path: format!("{prefix}.monitor.poll_interval_secs"),
                        message: "must be at least 1".into(),
                    });
                }
            }
        } else if channel.channel_type == "feishu" {
            // Validate Feishu channel specifics
            if let Some(ref feishu_config) = channel.feishu {
                if feishu_config.app_id.is_empty() {
                    errors.push(ValidationError {
                        path: format!("{prefix}.feishu.app_id"),
                        message: "Feishu app_id is required".into(),
                    });
                }
                if feishu_config.app_secret.is_empty() {
                    errors.push(ValidationError {
                        path: format!("{prefix}.feishu.app_secret"),
                        message: "Feishu app_secret is required (use ${ENV_VAR} syntax)".into(),
                    });
                }
                if !feishu_config.base_url.starts_with("https://") {
                    errors.push(ValidationError {
                        path: format!("{prefix}.feishu.base_url"),
                        message: "Feishu base_url must start with https://".into(),
                    });
                }

                // Validate WebSocket configuration
                if feishu_config.websocket.enabled {
                    if feishu_config.websocket.reconnect_delay_secs == 0 {
                        errors.push(ValidationError {
                            path: format!("{prefix}.feishu.websocket.reconnect_delay_secs"),
                            message: "must be greater than 0".into(),
                        });
                    }
                    if feishu_config.websocket.heartbeat_interval_secs < 10 {
                        errors.push(ValidationError {
                            path: format!("{prefix}.feishu.websocket.heartbeat_interval_secs"),
                            message: "must be at least 10".into(),
                        });
                    }
                }
            } else {
                errors.push(ValidationError {
                    path: format!("{prefix}.feishu"),
                    message: "Feishu configuration is required for feishu channel type".into(),
                });
            }
        } else if channel.channel_type == "wecom" {
            // Validate WeCom channel specifics
            if let Some(ref wecom_config) = channel.wecom {
                if wecom_config.token.is_empty() {
                    errors.push(ValidationError {
                        path: format!("{prefix}.wecom.token"),
                        message: "WeCom token is required".into(),
                    });
                }
                if wecom_config.encoding_aes_key.is_empty() {
                    errors.push(ValidationError {
                        path: format!("{prefix}.wecom.encoding_aes_key"),
                        message: "WeCom encoding_aes_key is required (use ${ENV_VAR} syntax)"
                            .into(),
                    });
                }
                if wecom_config.corp_id.is_empty() {
                    errors.push(ValidationError {
                        path: format!("{prefix}.wecom.corp_id"),
                        message: "WeCom corp_id is required".into(),
                    });
                }
                if wecom_config.corp_secret.is_empty() {
                    errors.push(ValidationError {
                        path: format!("{prefix}.wecom.corp_secret"),
                        message: "WeCom corp_secret is required (use ${ENV_VAR} syntax)".into(),
                    });
                }
            } else {
                errors.push(ValidationError {
                    path: format!("{prefix}.wecom"),
                    message: "WeCom configuration is required for wecom channel type".into(),
                });
            }
        } else if channel.channel_type == "wecom_bot" {
            // Validate WeCom Smart Robot channel specifics
            if let Some(ref bot_config) = channel.wecom_bot {
                if bot_config.bot_id.is_empty() {
                    errors.push(ValidationError {
                        path: format!("{prefix}.wecom_bot.bot_id"),
                        message: "WeCom bot_id is required".into(),
                    });
                }
                if bot_config.secret.is_empty() {
                    errors.push(ValidationError {
                        path: format!("{prefix}.wecom_bot.secret"),
                        message: "WeCom bot secret is required (use ${ENV_VAR} syntax)".into(),
                    });
                }
                if !bot_config.ws_url.starts_with("wss://")
                    && !bot_config.ws_url.starts_with("ws://")
                {
                    errors.push(ValidationError {
                        path: format!("{prefix}.wecom_bot.ws_url"),
                        message: "WeCom bot ws_url must start with wss:// or ws://".into(),
                    });
                }
                if bot_config.heartbeat_interval_secs < 10 {
                    errors.push(ValidationError {
                        path: format!("{prefix}.wecom_bot.heartbeat_interval_secs"),
                        message: "must be at least 10".into(),
                    });
                }
                if bot_config.reconnect_delay_secs == 0 {
                    errors.push(ValidationError {
                        path: format!("{prefix}.wecom_bot.reconnect_delay_secs"),
                        message: "must be greater than 0".into(),
                    });
                }
            } else {
                errors.push(ValidationError {
                    path: format!("{prefix}.wecom_bot"),
                    message: "WeCom bot configuration is required for wecom_bot channel type"
                        .into(),
                });
            }
        }

        // Validate channel-level disabled_tools / disabled_mcp_servers
        if let Some(ref tools) = channel.disabled_tools {
            for (i, name) in tools.iter().enumerate() {
                if name.is_empty() {
                    errors.push(ValidationError {
                        path: format!("{prefix}.disabled_tools[{i}]"),
                        message: "tool name must not be empty".into(),
                    });
                }
            }
        }
        if let Some(ref servers) = channel.disabled_mcp_servers {
            for (i, name) in servers.iter().enumerate() {
                if name.is_empty() {
                    errors.push(ValidationError {
                        path: format!("{prefix}.disabled_mcp_servers[{i}]"),
                        message: "MCP server name must not be empty".into(),
                    });
                }
            }
        }

        // Validate patterns
        if let Some(ref patterns) = channel.patterns {
            for (i, pattern) in patterns.iter().enumerate() {
                let pp = format!("{prefix}.patterns[{i}]");
                validate_pattern(&pp, pattern, &mut errors);

                // Feishu-specific pattern validation
                if channel.channel_type == "feishu" && pattern.enabled {
                    // Validate mentions list is non-empty if present
                    if let Some(ref mentions) = pattern.rules.mentions
                        && mentions.is_empty()
                    {
                        errors.push(ValidationError {
                            path: format!("{pp}.rules.mentions"),
                            message: "mentions list must not be empty".into(),
                        });
                    }
                    // Validate keywords list is non-empty if present
                    if let Some(ref keywords) = pattern.rules.keywords
                        && keywords.is_empty()
                    {
                        errors.push(ValidationError {
                            path: format!("{pp}.rules.keywords"),
                            message: "keywords list must not be empty".into(),
                        });
                    }
                }
            }
        }
    }

    // Agent
    if config.agent.mode != "agent" && config.agent.mode != "static" {
        errors.push(ValidationError {
            path: "agent.mode".into(),
            message: format!("must be 'agent' or 'static', got '{}'", config.agent.mode),
        });
    }

    if config.agent.mode == "static" && config.agent.text.is_none() {
        errors.push(ValidationError {
            path: "agent.text".into(),
            message: "required when agent.mode is 'static'".into(),
        });
    }

    // Validate agent attachment config
    if let Some(ref att) = config.agent.attachments {
        validate_outbound_attachment_config("agent.attachments", att, &mut errors);
    }

    // Validate unified attachment config
    if let Some(ref unified_att) = config.attachments {
        if let Some(ref inbound) = unified_att.inbound {
            validate_inbound_attachment_config("attachments.inbound", inbound, &mut errors);
        }
        if let Some(ref outbound) = unified_att.outbound {
            validate_outbound_attachment_config("attachments.outbound", outbound, &mut errors);
        }
    }

    // Inspect server
    if let Some(ref inspect) = config.inspect {
        if inspect.enabled && inspect.bind.is_empty() {
            errors.push(ValidationError {
                path: "inspect.bind".into(),
                message: "required when inspect is enabled".into(),
            });
        }
        if inspect.enabled && inspect.bind.parse::<std::net::SocketAddr>().is_err() {
            errors.push(ValidationError {
                path: "inspect.bind".into(),
                message: "must be a valid socket address (e.g., 127.0.0.1:9876)".into(),
            });
        }
    }

    errors
}

fn validate_pattern(prefix: &str, pattern: &ChannelPattern, errors: &mut Vec<ValidationError>) {
    if pattern.name.is_empty() {
        errors.push(ValidationError {
            path: format!("{prefix}.name"),
            message: "pattern name is required".into(),
        });
    }

    // Validate sender regex if present
    if let Some(ref sender) = pattern.rules.sender
        && let Some(ref regex_str) = sender.regex
        && let Err(e) = validate_regex(regex_str)
    {
        errors.push(ValidationError {
            path: format!("{prefix}.rules.sender.regex"),
            message: e.to_string(),
        });
    }

    // Validate subject regex if present
    if let Some(ref subject) = pattern.rules.subject
        && let Some(ref regex_str) = subject.regex
        && let Err(e) = validate_regex(regex_str)
    {
        errors.push(ValidationError {
            path: format!("{prefix}.rules.subject.regex"),
            message: e.to_string(),
        });
    }

    // Validate attachment config if present
    if let Some(ref att) = pattern.attachments {
        validate_inbound_attachment_config(&format!("{prefix}.attachments"), att, errors);
    }

    // Validate per-pattern disabled_tools / disabled_mcp_servers
    if let Some(ref tools) = pattern.disabled_tools {
        for (i, name) in tools.iter().enumerate() {
            if name.is_empty() {
                errors.push(ValidationError {
                    path: format!("{prefix}.disabled_tools[{i}]"),
                    message: "tool name must not be empty".into(),
                });
            }
        }
    }
    if let Some(ref servers) = pattern.disabled_mcp_servers {
        for (i, name) in servers.iter().enumerate() {
            if name.is_empty() {
                errors.push(ValidationError {
                    path: format!("{prefix}.disabled_mcp_servers[{i}]"),
                    message: "MCP server name must not be empty".into(),
                });
            }
        }
    }

    // Validate per-pattern MCP configs if present
    if let Some(ref mcps) = pattern.mcps {
        for (j, mcp) in mcps.iter().enumerate() {
            let mcp_prefix = format!("{prefix}.mcps[{j}]");
            if mcp.name.is_empty() {
                errors.push(ValidationError {
                    path: format!("{mcp_prefix}.name"),
                    message: "MCP server name is required".into(),
                });
            }
            match &mcp.kind {
                crate::config::McpServerKind::Local { command, .. } => {
                    if command.is_empty() {
                        errors.push(ValidationError {
                            path: format!("{mcp_prefix}.command"),
                            message: format!("MCP '{}' local command is required", mcp.name),
                        });
                    }
                }
                crate::config::McpServerKind::Remote { url, .. } => {
                    if url.is_empty() {
                        errors.push(ValidationError {
                            path: format!("{mcp_prefix}.url"),
                            message: format!("MCP '{}' remote url is required", mcp.name),
                        });
                    }
                }
            }
        }
    }
}

/// Validate inbound attachment configuration.
fn validate_inbound_attachment_config(
    prefix: &str,
    att: &crate::config::InboundAttachmentConfig,
    errors: &mut Vec<ValidationError>,
) {
    if let Some(ref size_str) = att.max_file_size
        && let Err(e) = parse_file_size(size_str)
    {
        errors.push(ValidationError {
            path: format!("{prefix}.max_file_size"),
            message: format!("invalid file size '{}': {}", size_str, e),
        });
    }

    for ext in &att.allowed_extensions {
        if !ext.starts_with('.') {
            errors.push(ValidationError {
                path: format!("{prefix}.allowed_extensions"),
                message: format!("extension '{}' must start with '.'", ext),
            });
        }
    }

    if let Some(max_per_message) = att.max_per_message
        && max_per_message == 0
    {
        errors.push(ValidationError {
            path: format!("{prefix}.max_per_message"),
            message: "must be at least 1".into(),
        });
    }
}

/// Validate outbound attachment configuration.
fn validate_outbound_attachment_config(
    prefix: &str,
    att: &crate::config::OutboundAttachmentConfig,
    errors: &mut Vec<ValidationError>,
) {
    if let Some(ref size_str) = att.max_file_size
        && let Err(e) = parse_file_size(size_str)
    {
        errors.push(ValidationError {
            path: format!("{prefix}.max_file_size"),
            message: format!("invalid file size '{}': {}", size_str, e),
        });
    }

    for ext in &att.allowed_extensions {
        if !ext.starts_with('.') {
            errors.push(ValidationError {
                path: format!("{prefix}.allowed_extensions"),
                message: format!("extension '{}' must start with '.'", ext),
            });
        }
    }

    if let Some(max_per_message) = att.max_per_message
        && max_per_message == 0
    {
        errors.push(ValidationError {
            path: format!("{prefix}.max_per_message"),
            message: "must be at least 1".into(),
        });
    }
}

/// Convenience: validate and return a Result.
#[allow(dead_code)]
pub fn validate_config_strict(config: &AppConfig) -> Result<()> {
    let errors = validate_config(config);
    if errors.is_empty() {
        Ok(())
    } else {
        let msg = errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::bail!("Configuration validation failed:\n{msg}")
    }
}

#[cfg(test)]
mod tests {
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
        assert!(errors.iter().any(|e| {
            e.path.contains("disabled_tools") && e.message.contains("must not be empty")
        }));
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
}
