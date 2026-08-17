use anyhow::Result;
use regex::Regex;

use crate::channel::ChannelPattern;
use crate::config::AppConfig;
use crate::config::agent::AgentConfig;

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
    if config.general.max_concurrent_topics == 0 {
        errors.push(ValidationError {
            path: "general.max_concurrent_topics".into(),
            message: "must be at least 1".into(),
        });
    }
    if config.general.max_queue_size_per_topic == 0 {
        errors.push(ValidationError {
            path: "general.max_queue_size_per_topic".into(),
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

        // [channels.<name>] type="websocket" with no matching
        // [agents.<name>] is deprecated — but `channels.agents` is the
        // synthesized target of `[agents.*]`, so it is exempted.
        if channel.channel_type == "websocket"
            && name != "agents"
            && !config.agents.contains_key(name)
        {
            // Allowed (deprecated). The CLI emits a runtime warn; we
            // don't fail validation so existing configs keep loading.
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

        // Validate channel-level skills / disabled_skills
        if let Some(ref skills) = channel.skills {
            for (i, name) in skills.iter().enumerate() {
                if name.is_empty() {
                    errors.push(ValidationError {
                        path: format!("{prefix}.skills[{i}]"),
                        message: "skill name must not be empty".into(),
                    });
                }
            }
        }
        if let Some(ref skills) = channel.disabled_skills {
            for (i, name) in skills.iter().enumerate() {
                if name.is_empty() {
                    errors.push(ValidationError {
                        path: format!("{prefix}.disabled_skills[{i}]"),
                        message: "skill name must not be empty".into(),
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
    if config.ai.mode != "agent" && config.ai.mode != "static" {
        errors.push(ValidationError {
            path: "ai.mode".into(),
            message: format!("must be 'agent' or 'static', got '{}'", config.ai.mode),
        });
    }

    if config.ai.mode == "static" && config.ai.text.is_none() {
        errors.push(ValidationError {
            path: "ai.text".into(),
            message: "required when ai.mode is 'static'".into(),
        });
    }

    // Validate agent attachment config
    if let Some(ref att) = config.ai.attachments {
        validate_outbound_attachment_config("ai.attachments", att, &mut errors);
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
        // A base URL without a scheme is read by browsers as a relative path,
        // so any generated link silently breaks. Catch it at startup instead
        // of in a link a user already received.
        if let Some(ref base) = inspect.base_url {
            let base = base.trim();
            if base.is_empty() {
                errors.push(ValidationError {
                    path: "inspect.base_url".into(),
                    message: "must not be empty".into(),
                });
            } else if !base.starts_with("http://") && !base.starts_with("https://") {
                errors.push(ValidationError {
                    path: "inspect.base_url".into(),
                    message: "must start with http:// or https:// (e.g., https://jyc.example.com)"
                        .into(),
                });
            }
        }
    }

    // Custom commands
    let mut seen_commands: Vec<String> = Vec::new();
    for (i, cmd) in config.commands.iter().enumerate() {
        let prefix = format!("commands[{i}]");

        let name = cmd.name.trim();
        if name.is_empty() {
            errors.push(ValidationError {
                path: format!("{prefix}.name"),
                message: "must not be empty".into(),
            });
        } else if name.split_whitespace().count() > 1 {
            errors.push(ValidationError {
                path: format!("{prefix}.name"),
                message: format!("must not contain whitespace (got '{name}')"),
            });
        } else if name.chars().any(char::is_uppercase) {
            // Command lookup is case-insensitive (the registry lowercases the
            // incoming line), so an uppercase name could never be matched.
            // Fail loudly rather than silently registering a dead command.
            errors.push(ValidationError {
                path: format!("{prefix}.name"),
                message: format!("must be lowercase (got '{name}')"),
            });
        } else {
            // Compare the normalized form: `review` and `/review` both register
            // as `/review` and would collide at registration time.
            let slashed = format!("/{}", name.trim_start_matches('/'));
            if crate::config::BUILTIN_COMMAND_NAMES.contains(&slashed.as_str()) {
                errors.push(ValidationError {
                    path: format!("{prefix}.name"),
                    message: format!("'{slashed}' shadows a built-in command"),
                });
            }
            if seen_commands.contains(&slashed) {
                errors.push(ValidationError {
                    path: format!("{prefix}.name"),
                    message: format!("duplicate command name '{slashed}'"),
                });
            }
            seen_commands.push(slashed);
        }

        if let Some(ref mode) = cmd.mode
            && mode != "plan"
            && mode != "build"
        {
            errors.push(ValidationError {
                path: format!("{prefix}.mode"),
                message: format!("must be 'plan' or 'build' (got '{mode}')"),
            });
        }

        if cmd.user_prompt.trim().is_empty() {
            errors.push(ValidationError {
                path: format!("{prefix}.user_prompt"),
                message: "must not be empty".into(),
            });
        }
    }

    // Agents: per-agent skill / tool / mcps sanity, pipe exclusivity,
    // reserved-name check, and the synthesized "agents" collision.
    validate_agents(config, &mut errors);

    // The synthesized "agents" channel is unconditionally inserted by
    // install_agents_channel at startup. If the user also wrote a
    // [channels.agents] block (legacy or otherwise), it would be
    // silently overwritten — surface this as a config error instead.
    if config.agents.contains_key("agents") {
        errors.push(ValidationError {
            path: "agents.agents".into(),
            message: "agent name \"agents\" is reserved (it's the synthesized channel name)".into(),
        });
    }
    if !config.agents.is_empty()
        && config.channels.contains_key("agents")
        && config.channels["agents"].channel_type == "websocket"
    {
        errors.push(ValidationError {
            path: "channels.agents".into(),
            message: "channels.agents is reserved (synthesized from [agents.<name>]); \
                     rename your agent or remove the [channels.agents] block"
                .into(),
        });
    }

    errors
}

fn validate_agent(
    prefix: &str,
    agent_name: &str,
    agent: &AgentConfig,
    errors: &mut Vec<ValidationError>,
) {
    // [channels.<name>] type="websocket" + [agents.<name>] — name
    // collision: the synthesized channel would overwrite the legacy
    // one. Reject to surface the conflict instead of silently
    // dropping the user's legacy config.
    if agent_name == "agents" {
        errors.push(ValidationError {
            path: format!("{prefix}.{agent_name}"),
            message: "agent name \"agents\" is reserved (it's the synthesized channel name)".into(),
        });
    }

    // Use AgentConfig::fill_into_pattern as the single source of
    // truth so any new behavior field added to AgentConfig is
    // automatically validated here.
    let mut pattern = ChannelPattern::default();
    // For validation, the default topic_path doesn't need to point
    // at a real directory — the per-field checks below ignore it.
    // Passing an empty PathBuf keeps the synthetic pattern minimal.
    agent.fill_into_pattern(&mut pattern, agent_name, std::path::PathBuf::new());
    validate_pattern(prefix, &pattern, errors);
}

fn validate_agents(config: &AppConfig, errors: &mut Vec<ValidationError>) {
    for (name, agent) in &config.agents {
        let prefix = format!("agents.{name}");
        validate_agent(&prefix, name, agent, errors);
    }
}

fn validate_pattern(prefix: &str, pattern: &ChannelPattern, errors: &mut Vec<ValidationError>) {
    if pattern.name.is_empty() {
        errors.push(ValidationError {
            path: format!("{prefix}.name"),
            message: "pattern name is required".into(),
        });
    }

    // Pipe target: agent XOR (channel OR pattern). Mixing them is a
    // configuration error caught at load time.
    if let Some(pipe) = &pattern.pipe
        && pipe.agent.is_some()
        && (pipe.channel.is_some() || pipe.pattern.is_some())
    {
        errors.push(ValidationError {
            path: format!("{prefix}.pipe"),
            message: "pipe.agent is mutually exclusive with pipe.channel / pipe.pattern".into(),
        });
    }
    // Legacy form requires pipe.channel (channel used to be required).
    if let Some(pipe) = &pattern.pipe
        && pipe.agent.is_none()
        && pipe.channel.is_none()
        && (pipe.pattern.is_some() || pipe.topic.is_some())
    {
        errors.push(ValidationError {
            path: format!("{prefix}.pipe"),
            message:
                "pipe.channel is required when pipe.pattern or pipe.topic is set without pipe.agent"
                    .into(),
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

    // Validate per-pattern skills / disabled_skills
    if let Some(ref skills) = pattern.skills {
        for (i, name) in skills.iter().enumerate() {
            if name.is_empty() {
                errors.push(ValidationError {
                    path: format!("{prefix}.skills[{i}]"),
                    message: "skill name must not be empty".into(),
                });
            }
        }
    }
    if let Some(ref skills) = pattern.disabled_skills {
        for (i, name) in skills.iter().enumerate() {
            if name.is_empty() {
                errors.push(ValidationError {
                    path: format!("{prefix}.disabled_skills[{i}]"),
                    message: "skill name must not be empty".into(),
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
                crate::config::McpServerKind::Remote {
                    url,
                    auth_header,
                    oauth,
                    ..
                } => {
                    if url.is_empty() {
                        errors.push(ValidationError {
                            path: format!("{mcp_prefix}.url"),
                            message: format!("MCP '{}' remote url is required", mcp.name),
                        });
                    }
                    if auth_header.is_some() && oauth.is_some() {
                        errors.push(ValidationError {
                            path: format!("{mcp_prefix}.auth_header"),
                            message: format!(
                                "MCP '{}' cannot set both auth_header and oauth; pick one",
                                mcp.name
                            ),
                        });
                    }
                    if let Some(oauth_cfg) = oauth {
                        let required = [
                            ("client_id", &oauth_cfg.client_id),
                            ("client_secret", &oauth_cfg.client_secret),
                            ("token_endpoint", &oauth_cfg.token_endpoint),
                        ];
                        for (field, value) in &required {
                            if value.is_empty() {
                                errors.push(ValidationError {
                                    path: format!("{mcp_prefix}.oauth.{field}"),
                                    message: format!(
                                        "MCP '{}' oauth.{field} must not be empty",
                                        mcp.name
                                    ),
                                });
                            }
                        }
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
mod tests;
