//! `/backlog` command — save and replay user messages per topic.
//!
//! Storage: `<topic_path>/.jyc/backlog.jsonl`, one JSON object per line:
//!
//! ```text
//! {"text":"..."}
//! ```
//!
//! Each item's `text` is a free-form user message. After the command
//! name and subcommand, the registry collapses the rest of the
//! command line into a single first-line string at `args[1]` (so
//! `/backlog push hello world` stores "hello world"), and continuation
//! lines (until the first blank line) become `args[2..]`. The push
//! handler joins the first-line content and continuation lines with
//! `\n` only when both are present. Items are referenced by 1-based
//! position, so `pop 2` removes the second entry and `rm 2` does the
//! same without injecting into the next agent turn.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::handler::{CommandContext, CommandHandler, CommandResult};

/// File name (relative to `<topic_path>/.jyc/`).
const BACKLOG_FILENAME: &str = "backlog.jsonl";

/// One persisted backlog item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct BacklogItem {
    text: String,
}

/// Handler for `/backlog push|list|pop|rm`.
pub struct BacklogCommandHandler;

impl BacklogCommandHandler {
    pub fn new() -> Self {
        Self
    }

    /// Returns `<topic_path>/.jyc/backlog.jsonl`.
    fn backlog_path(topic_path: &Path) -> PathBuf {
        topic_path.join(".jyc").join(BACKLOG_FILENAME)
    }

    /// Reads the file, returning `Ok(vec![])` if it does not exist.
    fn read_items(path: &Path) -> Result<Vec<BacklogItem>> {
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(e).with_context(|| format!("read backlog {}", path.display()));
            }
        };
        let mut items = Vec::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let item: BacklogItem = serde_json::from_str(line)
                .with_context(|| format!("parse backlog line: {line:?}"))?;
            items.push(item);
        }
        Ok(items)
    }

    /// Writes the items atomically (tempfile + rename) so a crash mid-write
    /// leaves the previous file intact instead of truncating it to nothing.
    fn write_items(path: &Path, items: &[BacklogItem]) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create dir {}", parent.display()))?;
        }
        let mut content = String::new();
        for item in items {
            // `serde_json::to_string` produces one line per item (no internal
            // newlines), which keeps the file valid JSONL even though `text`
            // itself contains newlines.
            content.push_str(&serde_json::to_string(item)?);
            content.push('\n');
        }
        let tmp = path.with_extension("jsonl.tmp");
        fs::write(&tmp, &content)
            .with_context(|| format!("write tmp backlog {}", tmp.display()))?;
        // `fs::rename` is atomic on POSIX and Windows; on a crash the
        // either the old file is still there, or the new one is fully
        // written — never a half-written file.
        fs::rename(&tmp, path).with_context(|| {
            format!("rename tmp backlog {} → {}", tmp.display(), path.display())
        })?;
        Ok(())
    }

    /// Parse a 1-based index from an arg slot. Returns `Ok(None)` when the
    /// slot is absent, leaving the default up to the caller; returns
    /// `Err` for non-numeric input or `0`.
    fn parse_n(args: &[String], slot: usize) -> Result<Option<usize>, String> {
        let Some(raw) = args.get(slot) else {
            return Ok(None);
        };
        let n: usize = raw
            .parse()
            .map_err(|_| format!("invalid index {raw:?} (expected positive integer)"))?;
        if n == 0 {
            return Err("index must be 1 or greater".to_string());
        }
        Ok(Some(n))
    }
}

impl Default for BacklogCommandHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CommandHandler for BacklogCommandHandler {
    fn name(&self) -> &str {
        "/backlog"
    }

    fn description(&self) -> &str {
        "Save and replay user messages (push|list|pop|rm)"
    }

    /// `/backlog push` is followed by a free-form multi-line description.
    /// After parsing, `args[1]` holds the space-joined first-line content
    /// (empty string if none was provided) and `args[2..]` holds
    /// continuation lines, one per element.
    fn collect_subsequent_lines(&self) -> bool {
        true
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        // Concurrency: this handler holds no shared state. Each call does
        // a read-modify-write via atomic tempfile-rename, so two
        // simultaneous calls could race — the last writer wins, and
        // earlier items would be lost. Topic state is single-user, so
        // simultaneous push/pop is extremely unlikely. If races are ever
        // observed, add `fs2` file locking around the read-modify-write.
        let args = &context.args;
        let path = Self::backlog_path(&context.topic_path);

        if args.is_empty() {
            return Ok(CommandResult {
                success: false,
                message: "Usage: /backlog <push|list|pop|rm> ...".to_string(),
                error: Some("missing subcommand".to_string()),
                append_body: None,
            });
        }

        match args[0].as_str() {
            "push" => {
                // After registry parsing:
                // - args[0] = "push"
                // - args[1] = space-joined first-line content (or "" if none)
                // - args[2..] = continuation lines, one element per line
                let description = {
                    let first = args.get(1).map(|s| s.as_str()).unwrap_or("");
                    let rest = args[2..].join("\n");
                    match (first.is_empty(), rest.is_empty()) {
                        (true, true) => String::new(),
                        (true, false) => rest,
                        (false, true) => first.to_string(),
                        (false, false) => format!("{first}\n{rest}"),
                    }
                };
                if description.trim().is_empty() {
                    return Ok(CommandResult {
                        success: false,
                        message: "/backlog push: description required".to_string(),
                        error: Some("empty description".to_string()),
                        append_body: None,
                    });
                }
                let mut items = Self::read_items(&path)?;
                items.push(BacklogItem { text: description });
                let new_index = items.len();
                Self::write_items(&path, &items)?;
                Ok(CommandResult {
                    success: true,
                    message: format!("Backlog: pushed item {new_index}"),
                    error: None,
                    append_body: None,
                })
            }

            "list" => {
                let items = Self::read_items(&path)?;
                if items.is_empty() {
                    return Ok(CommandResult {
                        success: true,
                        message: "Backlog: (empty)".to_string(),
                        error: None,
                        append_body: None,
                    });
                }
                let mut msg = String::new();
                for (i, item) in items.iter().enumerate() {
                    msg.push_str(&format!("{}. {}\n", i + 1, item.text));
                }
                let trimmed = msg.trim_end().to_string();
                Ok(CommandResult {
                    success: true,
                    message: trimmed,
                    error: None,
                    append_body: None,
                })
            }

            "pop" => {
                // Default to `1` when no index is given (matches the user
                // spec: "/backlog pop" pops the first item). Propagate
                // parse errors as a friendly CommandResult instead of
                // bubbling them up — otherwise the registry collapses
                // them into a generic "/backlog: error" reply.
                let n = match Self::parse_n(args, 1).map_err(anyhow::Error::msg) {
                    Ok(Some(n)) => n,
                    Ok(None) => 1,
                    Err(e) => {
                        return Ok(CommandResult {
                            success: false,
                            message: format!("/backlog pop: {e}"),
                            error: Some(e.to_string()),
                            append_body: None,
                        });
                    }
                };
                let mut items = Self::read_items(&path)?;
                if n > items.len() {
                    let err = format!("index {n} out of range (backlog has {} items)", items.len());
                    return Ok(CommandResult {
                        success: false,
                        message: format!("/backlog pop: {err}"),
                        error: Some(err),
                        append_body: None,
                    });
                }
                let removed = items.remove(n - 1);
                Self::write_items(&path, &items)?;
                Ok(CommandResult {
                    success: true,
                    message: format!("Backlog: popped item {n}: {}", removed.text),
                    error: None,
                    // Inject the popped text as a user message so the agent
                    // receives it on the next turn. Same pattern used by
                    // `CustomCommandHandler` to forward `user_prompt`.
                    append_body: Some(removed.text),
                })
            }

            "rm" => {
                let n = match Self::parse_n(args, 1).map_err(anyhow::Error::msg) {
                    Ok(Some(n)) => n,
                    Ok(None) => {
                        let err = "missing index (usage: /backlog rm <N>)".to_string();
                        return Ok(CommandResult {
                            success: false,
                            message: format!("/backlog rm: {err}"),
                            error: Some(err),
                            append_body: None,
                        });
                    }
                    Err(e) => {
                        return Ok(CommandResult {
                            success: false,
                            message: format!("/backlog rm: {e}"),
                            error: Some(e.to_string()),
                            append_body: None,
                        });
                    }
                };
                let mut items = Self::read_items(&path)?;
                if n > items.len() {
                    let err = format!("index {n} out of range (backlog has {} items)", items.len());
                    return Ok(CommandResult {
                        success: false,
                        message: format!("/backlog rm: {err}"),
                        error: Some(err),
                        append_body: None,
                    });
                }
                items.remove(n - 1);
                Self::write_items(&path, &items)?;
                Ok(CommandResult {
                    success: true,
                    message: format!("Backlog: removed item {n}"),
                    error: None,
                    append_body: None,
                })
            }

            other => Ok(CommandResult {
                success: false,
                message: format!(
                    "/backlog: unknown subcommand {other:?} (expected push|list|pop|rm)"
                ),
                error: Some(format!("unknown subcommand {other}")),
                append_body: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// Build a `CommandContext` pointing at the given temp dir as the topic path.
    fn ctx_for(path: &Path) -> CommandContext {
        CommandContext {
            args: vec![],
            topic_path: path.to_path_buf(),
            config: Arc::new(
                jyc_types::load_config_from_str(
                    r#"
[general]
[channels.test]
type = "email"
[channels.test.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.test.outbound]
host = "h"
port = 465
username = "u"
password = "p"
[agent]
enabled = true
mode = "agent"
"#,
                )
                .unwrap(),
            ),
            channel: "test".into(),
            channel_type: "websocket".into(),
            agent: None,
            template_dirs: PathBuf::from("/tmp/test/templates").into(),
            config_path: None,
        }
    }

    /// Convenience: build a fresh context whose args are `args`.
    async fn run(handler: &BacklogCommandHandler, topic: &Path, args: &[&str]) -> CommandResult {
        let mut ctx = ctx_for(topic);
        ctx.args = args.iter().map(|s| s.to_string()).collect();
        handler.execute(ctx).await.unwrap()
    }

    fn fresh_topic() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    #[tokio::test]
    async fn push_space_separated_command_line_joins_with_space() {
        let dir = fresh_topic();
        let h = BacklogCommandHandler::new();

        // `/backlog push hello world` (single command line, no continuation):
        // the registry collapses the tokens after `push` into a single
        // first-line string, so the description is the user's literal input.
        // Regression test for the user-reported issue where each token was
        // joined with `\n` instead, rendering as multiple lines.
        let r = run(&h, dir.path(), &["push", "hello", "world"]).await;
        assert!(r.success, "{:?}", r.error);
        assert_eq!(r.message, "Backlog: pushed item 1");

        let items =
            BacklogCommandHandler::read_items(&BacklogCommandHandler::backlog_path(dir.path()))
                .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "hello world");
    }

    #[tokio::test]
    async fn push_multi_line_continuation_lines_join_with_newlines() {
        let dir = fresh_topic();
        let h = BacklogCommandHandler::new();

        // Pure continuation form: args = ["push", "", "line one", ...]
        // (the empty placeholder at args[1] means "no first-line content").
        let r = run(
            &h,
            dir.path(),
            &["push", "", "line one", "line two", "line three"],
        )
        .await;
        assert!(r.success, "{:?}", r.error);

        let items =
            BacklogCommandHandler::read_items(&BacklogCommandHandler::backlog_path(dir.path()))
                .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "line one\nline two\nline three");
    }

    #[tokio::test]
    async fn push_first_line_plus_continuation_joins_with_newlines() {
        let dir = fresh_topic();
        let h = BacklogCommandHandler::new();

        // Mixed form: first-line content on the command line, then
        // continuation lines. The first-line stays space-joined; the
        // boundary between first-line and continuation uses `\n`.
        let r = run(
            &h,
            dir.path(),
            &["push", "first line", "second line", "third line"],
        )
        .await;
        assert!(r.success, "{:?}", r.error);

        let items =
            BacklogCommandHandler::read_items(&BacklogCommandHandler::backlog_path(dir.path()))
                .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "first line\nsecond line\nthird line");
    }

    #[tokio::test]
    async fn push_with_no_description_returns_error() {
        let dir = fresh_topic();
        let h = BacklogCommandHandler::new();

        let r = run(&h, dir.path(), &["push"]).await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("empty description"));
        // Nothing should be persisted.
        assert!(!BacklogCommandHandler::backlog_path(dir.path()).exists());
    }

    #[tokio::test]
    async fn list_empty_backlog_says_empty() {
        let dir = fresh_topic();
        let h = BacklogCommandHandler::new();

        let r = run(&h, dir.path(), &["list"]).await;
        assert!(r.success);
        assert_eq!(r.message, "Backlog: (empty)");
    }

    #[tokio::test]
    async fn list_returns_numbered_items_with_multiline_text() {
        let dir = fresh_topic();
        let h = BacklogCommandHandler::new();

        run(&h, dir.path(), &["push", "alpha"]).await;
        run(&h, dir.path(), &["push", "beta", "continuation"]).await;

        let r = run(&h, dir.path(), &["list"]).await;
        assert!(r.success);
        assert_eq!(r.message, "1. alpha\n2. beta\ncontinuation");
    }

    #[tokio::test]
    async fn pop_default_removes_first_and_returns_append_body() {
        let dir = fresh_topic();
        let h = BacklogCommandHandler::new();

        run(&h, dir.path(), &["push", "first"]).await;
        run(&h, dir.path(), &["push", "second"]).await;

        let r = run(&h, dir.path(), &["pop"]).await;
        assert!(r.success);
        assert_eq!(r.message, "Backlog: popped item 1: first");
        assert_eq!(r.append_body.as_deref(), Some("first"));

        let remaining =
            BacklogCommandHandler::read_items(&BacklogCommandHandler::backlog_path(dir.path()))
                .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].text, "second");
    }

    #[tokio::test]
    async fn pop_nth_removes_correct_item() {
        let dir = fresh_topic();
        let h = BacklogCommandHandler::new();

        run(&h, dir.path(), &["push", "a"]).await;
        run(&h, dir.path(), &["push", "b"]).await;
        run(&h, dir.path(), &["push", "c"]).await;

        let r = run(&h, dir.path(), &["pop", "2"]).await;
        assert!(r.success);
        assert_eq!(r.append_body.as_deref(), Some("b"));

        let remaining =
            BacklogCommandHandler::read_items(&BacklogCommandHandler::backlog_path(dir.path()))
                .unwrap();
        assert_eq!(
            remaining
                .iter()
                .map(|i| i.text.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "c"]
        );
    }

    #[tokio::test]
    async fn pop_out_of_range_returns_error() {
        let dir = fresh_topic();
        let h = BacklogCommandHandler::new();

        run(&h, dir.path(), &["push", "only"]).await;
        let r = run(&h, dir.path(), &["pop", "5"]).await;
        assert!(!r.success);
        assert!(r.message.contains("out of range"));
        assert!(r.append_body.is_none());

        // File unchanged.
        let items =
            BacklogCommandHandler::read_items(&BacklogCommandHandler::backlog_path(dir.path()))
                .unwrap();
        assert_eq!(items.len(), 1);
    }

    #[tokio::test]
    async fn rm_removes_item_without_append_body() {
        let dir = fresh_topic();
        let h = BacklogCommandHandler::new();

        run(&h, dir.path(), &["push", "x"]).await;
        run(&h, dir.path(), &["push", "y"]).await;

        let r = run(&h, dir.path(), &["rm", "1"]).await;
        assert!(r.success);
        assert_eq!(r.message, "Backlog: removed item 1");
        assert!(r.append_body.is_none());

        let remaining =
            BacklogCommandHandler::read_items(&BacklogCommandHandler::backlog_path(dir.path()))
                .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].text, "y");
    }

    #[tokio::test]
    async fn rm_missing_index_returns_error() {
        let dir = fresh_topic();
        let h = BacklogCommandHandler::new();
        let r = run(&h, dir.path(), &["rm"]).await;
        assert!(!r.success);
        assert!(r.message.contains("missing index"));
    }

    #[tokio::test]
    async fn rm_out_of_range_returns_error() {
        let dir = fresh_topic();
        let h = BacklogCommandHandler::new();
        run(&h, dir.path(), &["push", "only"]).await;
        let r = run(&h, dir.path(), &["rm", "9"]).await;
        assert!(!r.success);
        assert!(r.message.contains("out of range"));
    }

    #[tokio::test]
    async fn unknown_subcommand_returns_error() {
        let dir = fresh_topic();
        let h = BacklogCommandHandler::new();
        let r = run(&h, dir.path(), &["frobnicate"]).await;
        assert!(!r.success);
        assert!(r.message.contains("unknown subcommand"));
    }

    #[tokio::test]
    async fn no_subcommand_returns_error() {
        let dir = fresh_topic();
        let h = BacklogCommandHandler::new();
        let r = run(&h, dir.path(), &[]).await;
        assert!(!r.success);
        // The handler puts the actionable reason in `error` because that's
        // what `results_summary()` prepends with "Error: " when success=false.
        assert!(
            r.error
                .as_deref()
                .unwrap_or("")
                .contains("missing subcommand"),
            "expected error to mention 'missing subcommand', got {:?}",
            r.error
        );
    }

    #[tokio::test]
    async fn persistence_survives_across_handler_instances() {
        let dir = fresh_topic();
        let h1 = BacklogCommandHandler::new();
        let h2 = BacklogCommandHandler::new();

        run(&h1, dir.path(), &["push", "persisted"]).await;
        let r = run(&h2, dir.path(), &["list"]).await;
        assert!(r.success);
        assert_eq!(r.message, "1. persisted");
    }

    #[tokio::test]
    async fn pop_zero_index_is_rejected() {
        let dir = fresh_topic();
        let h = BacklogCommandHandler::new();
        run(&h, dir.path(), &["push", "x"]).await;
        let r = run(&h, dir.path(), &["pop", "0"]).await;
        assert!(!r.success);
        assert!(r.message.contains("1 or greater"));
    }

    #[tokio::test]
    async fn pop_invalid_index_is_rejected() {
        let dir = fresh_topic();
        let h = BacklogCommandHandler::new();
        run(&h, dir.path(), &["push", "x"]).await;
        let r = run(&h, dir.path(), &["pop", "abc"]).await;
        assert!(!r.success);
        assert!(r.message.contains("invalid index"));
    }

    /// Atomic write should leave a stale tmp file alone if a previous
    /// attempt was interrupted, and never leave a half-written .jsonl.
    #[tokio::test]
    async fn write_items_does_not_leave_tmp_on_success() {
        let dir = fresh_topic();
        let h = BacklogCommandHandler::new();

        run(&h, dir.path(), &["push", "x"]).await;

        let path = BacklogCommandHandler::backlog_path(dir.path());
        assert!(path.exists(), "backlog.jsonl should exist after push");
        let tmp = path.with_extension("jsonl.tmp");
        assert!(
            !tmp.exists(),
            "tmp file should be renamed away after write, but {} still exists",
            tmp.display()
        );
    }

    #[tokio::test]
    async fn write_items_recovers_when_stale_tmp_is_present() {
        let dir = fresh_topic();
        let h = BacklogCommandHandler::new();

        // Pre-plant a stale tmp from a previous interrupted write.
        let path = BacklogCommandHandler::backlog_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let tmp = path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, b"stale garbage\n").unwrap();

        // The new push must succeed — overwriting the stale tmp is correct.
        let r = run(&h, dir.path(), &["push", "fresh"]).await;
        assert!(r.success, "{:?}", r.error);

        // Tmp file no longer exists (it was the rename target and got moved).
        assert!(!tmp.exists());
        // Real file contains only the new item, not the stale garbage.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("fresh"));
        assert!(!raw.contains("stale garbage"));
    }
}
