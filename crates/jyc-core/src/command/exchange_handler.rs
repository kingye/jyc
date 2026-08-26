use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::topic_manager::TopicManager;

/// /exchange command — show shareable URLs for this topic's published files.
///
/// Usage:
///   /exchange           List every published file as a markdown list
///   /exchange <name>    Show only that file's URL
///
/// Output is a markdown bullet list with raw URLs so URLs stay
/// copy-pasteable. Reads the existing exchange token but never creates one:
/// listing must not grant access to a topic that has published nothing.
pub struct ExchangeCommandHandler {
    topic_manager: Arc<TopicManager>,
}

impl ExchangeCommandHandler {
    pub fn new(topic_manager: Arc<TopicManager>) -> Self {
        Self { topic_manager }
    }

    /// Topic name as the `/exchange/...` route expects it.
    ///
    /// The directory basename is NOT always the topic name — a pattern with
    /// a `topic_path` override or a shared repo layout puts topic
    /// `issue-197` in `.../repos/jin-197`. `topic_paths` holds the
    /// authoritative name→path map; fall back to the basename when the
    /// topic has no override registered.
    async fn topic_name(&self, topic_path: &Path) -> String {
        let registered = self
            .topic_manager
            .custom_topic_paths()
            .await
            .into_iter()
            .find(|(_, path)| path == topic_path)
            .map(|(name, _)| name);

        registered.unwrap_or_else(|| {
            topic_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string()
        })
    }
}

#[async_trait]
impl CommandHandler for ExchangeCommandHandler {
    fn name(&self) -> &str {
        "/exchange"
    }

    fn description(&self) -> &str {
        "Show shareable URLs for this topic's published files"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let jyc_dir = context.topic_path.join(".jyc");

        // No token means nothing was ever published (or /reset killed the
        // links). Never mint one here.
        let token = match jyc_utils::auth_token::read_token(
            &jyc_dir.join(crate::EXCHANGE_TOKEN_FILENAME),
        ) {
            Ok(token) => token,
            Err(_) => return Ok(ok(NO_FILES.into())),
        };

        let mut names = read_published_names(&jyc_dir.join(crate::EXCHANGE_DIR_NAME)).await;
        names.sort();
        if names.is_empty() {
            return Ok(ok(NO_FILES.into()));
        }

        if let Some(wanted) = context.args.first() {
            if !names.iter().any(|n| n == wanted) {
                return Ok(CommandResult {
                    success: false,
                    message: format!("/exchange: '{wanted}' is not published"),
                    error: Some(format!(
                        "'{wanted}' is not published in this topic. Published files: {}",
                        names.join(", ")
                    )),
                    append_body: None,
                });
            }
            names.retain(|n| n == wanted);
        }

        let base = context
            .config
            .inspect
            .clone()
            .unwrap_or_default()
            .effective_base_url();
        let channel = &context.channel;
        let topic = self.topic_name(&context.topic_path).await;

        Ok(ok(format_lines(&names, &base, channel, &topic, &token)))
    }
}

/// Message used both when no token exists and when the directory is empty —
/// from the user's side these are the same situation.
const NO_FILES: &str = "/exchange: no published files in this topic.";

/// Wrap a message as a successful result.
fn ok(message: String) -> CommandResult {
    CommandResult {
        success: true,
        message,
        error: None,
        append_body: None,
    }
}

/// Names of the files directly inside the exchange directory.
///
/// Flat by design: `jyc_publish_file` rejects names containing a separator,
/// so published files never nest.
async fn read_published_names(exchange_dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(exchange_dir).await else {
        return names;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.path().is_file()
            && let Some(name) = entry.file_name().to_str()
        {
            names.push(name.to_string());
        }
    }
    names
}

/// Render a markdown bullet list with one `name` and raw `url` per file.
/// For multiple files a short header is included.
fn format_lines(names: &[String], base: &str, channel: &str, topic: &str, token: &str) -> String {
    let list = names
        .iter()
        .map(|name| {
            let url = crate::exchange_url(base, channel, topic, name, token);
            format!("- {name}\n  {url}")
        })
        .collect::<Vec<_>>()
        .join("\n");

    if names.len() > 1 {
        format!("/exchange: {} published files.\n{list}", names.len())
    } else {
        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn renders_markdown_list() {
        let out = format_lines(
            &names(&["a.txt", "report.pdf"]),
            "https://x.example.com",
            "email",
            "weather",
            "tok",
        );
        assert_eq!(
            out,
            "/exchange: 2 published files.\n\
             - a.txt\n\
               https://x.example.com/exchange/email/weather/a.txt?token=tok\n\
             - report.pdf\n\
               https://x.example.com/exchange/email/weather/report.pdf?token=tok"
        );
        // URLs stay raw — not wrapped in markdown links.
        assert!(!out.contains('[') && !out.contains('('));
    }

    /// The name stays human-readable on the left while the URL is encoded.
    #[test]
    fn special_chars_encoded_in_url_only() {
        let out = format_lines(
            &names(&["a b#c.pdf"]),
            "https://x.example.com",
            "email",
            "weather",
            "tok",
        );
        assert_eq!(
            out,
            "- a b#c.pdf\n  https://x.example.com/exchange/email/weather/a%20b%23c.pdf?token=tok"
        );
    }

    #[test]
    fn single_file_renders_one_bullet() {
        let out = format_lines(&names(&["only.pdf"]), "http://h:1", "c", "t", "tok");
        assert!(out.starts_with("- only.pdf\n  http://h:1/exchange/c/t/only.pdf?token=tok"));
        assert!(!out.contains("published files"));
    }

    #[tokio::test]
    async fn read_published_names_skips_dirs_and_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            read_published_names(&tmp.path().join("nope"))
                .await
                .is_empty()
        );

        let dir = tmp.path().join("exchange");
        tokio::fs::create_dir_all(dir.join("subdir")).await.unwrap();
        tokio::fs::write(dir.join("a.txt"), b"a").await.unwrap();
        assert_eq!(read_published_names(&dir).await, names(&["a.txt"]));
    }
}
