use anyhow::Result;
use clap::Subcommand;
use std::path::Path;

use crate::channels::types::LabelRule;
use crate::config::load_config;
use crate::utils::constants::DEFAULT_CONFIG_FILENAME;

#[derive(Debug, Subcommand)]
pub enum PatternsAction {
    /// List all configured patterns
    List {
        /// Config file path
        #[arg(short, long, default_value = DEFAULT_CONFIG_FILENAME)]
        config: String,
    },
}

pub async fn run(action: &PatternsAction, workdir: &Path) -> Result<()> {
    match action {
        PatternsAction::List { config } => run_list(workdir, config).await,
    }
}

async fn run_list(workdir: &Path, config_file: &str) -> Result<()> {
    let config_path = workdir.join(config_file);
    let config = load_config(&config_path)?;

    let mut total = 0;

    for (channel_name, channel_config) in &config.channels {
        if let Some(ref patterns) = channel_config.patterns {
            for pattern in patterns {
                total += 1;
                let status = if pattern.enabled { "enabled" } else { "disabled" };
                println!(
                    "[{channel_name}] {} ({status})",
                    pattern.name
                );

                if let Some(ref sender) = pattern.rules.sender {
                    if let Some(ref exact) = sender.exact {
                        println!("  sender.exact: {}", exact.join(", "));
                    }
                    if let Some(ref domain) = sender.domain {
                        println!("  sender.domain: {}", domain.join(", "));
                    }
                    if let Some(ref regex) = sender.regex {
                        println!("  sender.regex: {regex}");
                    }
                }

                if let Some(ref subject) = pattern.rules.subject {
                    if let Some(ref prefix) = subject.prefix {
                        println!("  subject.prefix: {}", prefix.join(", "));
                    }
                    if let Some(ref regex) = subject.regex {
                        println!("  subject.regex: {regex}");
                    }
                }

                // GitHub rules
                if let Some(ref github_type) = pattern.rules.github_type {
                    println!("  github_type: {}", github_type.join(", "));
                }
                if let Some(ref labels) = pattern.rules.labels {
                    match labels {
                        LabelRule::Flat(list) => {
                            println!("  labels: {}", list.join(", "));
                        }
                        LabelRule::Nested(groups) => {
                            let display: Vec<String> = groups
                                .iter()
                                .map(|group| format!("({})", group.join(" OR ")))
                                .collect();
                            println!("  labels: {}", display.join(" AND "));
                        }
                    }
                }
                if let Some(ref assignees) = pattern.rules.assignees {
                    println!("  assignees: {}", assignees.join(", "));
                }

                // Feishu rules
                if let Some(ref mentions) = pattern.rules.mentions {
                    println!("  mentions: {}", mentions.join(", "));
                }
                if let Some(ref keywords) = pattern.rules.keywords {
                    println!("  keywords: {}", keywords.join(", "));
                }
                if let Some(ref chat_name) = pattern.rules.chat_name {
                    println!("  chat_name: {}", chat_name.join(", "));
                }

                // Role and template
                if let Some(ref role) = pattern.role {
                    println!("  role: {role}");
                }
                if let Some(ref template) = pattern.template {
                    println!("  template: {template}");
                }
            }
        }
    }

    if total == 0 {
        println!("No patterns configured.");
    } else {
        println!("\n{total} pattern(s) total.");
    }

    Ok(())
}
