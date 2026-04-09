use anyhow::Result;

use super::types::AppConfig;
use crate::utils::helpers::{parse_file_size, validate_regex};

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
        }

        // Validate patterns
        if let Some(ref patterns) = channel.patterns {
            for (i, pattern) in patterns.iter().enumerate() {
                let pp = format!("{prefix}.patterns[{i}]");
                validate_pattern(&pp, pattern, &mut errors);

                // Feishu-specific pattern validation
                if channel.channel_type == "feishu" && pattern.enabled {
                    // Validate mentions list is non-empty if present
                    if let Some(ref mentions) = pattern.rules.mentions {
                        if mentions.is_empty() {
                            errors.push(ValidationError {
                                path: format!("{pp}.rules.mentions"),
                                message: "mentions list must not be empty".into(),
                            });
                        }
                    }
                    // Validate keywords list is non-empty if present
                    if let Some(ref keywords) = pattern.rules.keywords {
                        if keywords.is_empty() {
                            errors.push(ValidationError {
                                path: format!("{pp}.rules.keywords"),
                                message: "keywords list must not be empty".into(),
                            });
                        }
                    }
                }
            }
        }
    }

    // Agent
    if config.agent.mode != "opencode" && config.agent.mode != "static" {
        errors.push(ValidationError {
            path: "agent.mode".into(),
            message: format!(
                "must be 'opencode' or 'static', got '{}'",
                config.agent.mode
            ),
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
        validate_attachment_config("agent.attachments", att, &mut errors);
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

    // Heartbeat
    if config.heartbeat.enabled {
        if config.heartbeat.interval_secs == 0 {
            errors.push(ValidationError {
                path: "heartbeat.interval_secs".into(),
                message: "must be at least 1 second".into(),
            });
        }
        if config.heartbeat.interval_secs > 0 && config.heartbeat.interval_secs < 30 {
            errors.push(ValidationError {
                path: "heartbeat.interval_secs".into(),
                message: "must be at least 30 seconds to avoid rate limits".into(),
            });
        }
        if config.heartbeat.min_elapsed_secs == 0 {
            errors.push(ValidationError {
                path: "heartbeat.min_elapsed_secs".into(),
                message: "must be at least 1 second".into(),
            });
        }
    }

    // Alerting
    if let Some(ref alerting) = config.alerting {
        if alerting.enabled && alerting.recipient.is_empty() {
            errors.push(ValidationError {
                path: "alerting.recipient".into(),
                message: "required when alerting is enabled".into(),
            });
        }
    }

    errors
}

/// Validate a single channel pattern's rules.
fn validate_pattern(
    prefix: &str,
    pattern: &crate::channels::types::ChannelPattern,
    errors: &mut Vec<ValidationError>,
) {
    if pattern.name.is_empty() {
        errors.push(ValidationError {
            path: format!("{prefix}.name"),
            message: "pattern name is required".into(),
        });
    }

    // Validate sender regex if present
    if let Some(ref sender) = pattern.rules.sender {
        if let Some(ref regex_str) = sender.regex {
            if let Err(e) = validate_regex(regex_str) {
                errors.push(ValidationError {
                    path: format!("{prefix}.rules.sender.regex"),
                    message: e.to_string(),
                });
            }
        }
    }

    // Validate subject regex if present
    if let Some(ref subject) = pattern.rules.subject {
        if let Some(ref regex_str) = subject.regex {
            if let Err(e) = validate_regex(regex_str) {
                errors.push(ValidationError {
                    path: format!("{prefix}.rules.subject.regex"),
                    message: e.to_string(),
                });
            }
        }
    }

    // Validate attachment config if present
    if let Some(ref att) = pattern.attachments {
        validate_attachment_config(&format!("{prefix}.attachments"), att, errors);
    }
}

/// Validate an attachment configuration.
fn validate_attachment_config(
    prefix: &str,
    att: &crate::channels::types::AttachmentConfig,
    errors: &mut Vec<ValidationError>,
) {
    if let Some(ref size_str) = att.max_file_size {
        if let Err(e) = parse_file_size(size_str) {
            errors.push(ValidationError {
                path: format!("{prefix}.max_file_size"),
                message: format!("invalid file size '{}': {}", size_str, e),
            });
        }
    }

    for ext in &att.allowed_extensions {
        if !ext.starts_with('.') {
            errors.push(ValidationError {
                path: format!("{prefix}.allowed_extensions"),
                message: format!("extension '{}' must start with '.'", ext),
            });
        }
    }
}

/// Validate inbound attachment configuration.
fn validate_inbound_attachment_config(
    prefix: &str,
    att: &crate::config::types::InboundAttachmentConfig,
    errors: &mut Vec<ValidationError>,
) {
    if let Some(ref size_str) = att.max_file_size {
        if let Err(e) = parse_file_size(size_str) {
            errors.push(ValidationError {
                path: format!("{prefix}.max_file_size"),
                message: format!("invalid file size '{}': {}", size_str, e),
            });
        }
    }

    for ext in &att.allowed_extensions {
        if !ext.starts_with('.') {
            errors.push(ValidationError {
                path: format!("{prefix}.allowed_extensions"),
                message: format!("extension '{}' must start with '.'", ext),
            });
        }
    }

    if let Some(max_per_message) = att.max_per_message {
        if max_per_message == 0 {
            errors.push(ValidationError {
                path: format!("{prefix}.max_per_message"),
                message: "must be at least 1".into(),
            });
        }
    }
}

/// Validate outbound attachment configuration.
fn validate_outbound_attachment_config(
    prefix: &str,
    att: &crate::config::types::OutboundAttachmentConfig,
    errors: &mut Vec<ValidationError>,
) {
    if let Some(ref size_str) = att.max_file_size {
        if let Err(e) = parse_file_size(size_str) {
            errors.push(ValidationError {
                path: format!("{prefix}.max_file_size"),
                message: format!("invalid file size '{}': {}", size_str, e),
            });
        }
    }

    for ext in &att.allowed_extensions {
        if !ext.starts_with('.') {
            errors.push(ValidationError {
                path: format!("{prefix}.allowed_extensions"),
                message: format!("extension '{}' must start with '.'", ext),
            });
        }
    }

    if let Some(max_per_message) = att.max_per_message {
        if max_per_message == 0 {
            errors.push(ValidationError {
                path: format!("{prefix}.max_per_message"),
                message: "must be at least 1".into(),
            });
        }
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
        Err(crate::utils::JycError::ConfigValidation(msg).into())
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
mode = "opencode"
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
mode = "opencode"
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
mode = "opencode"
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
mode = "opencode"
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
mode = "opencode"

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
mode = "opencode"

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
}
