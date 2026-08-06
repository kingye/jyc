//! Builtin tool: `jyc_publish_file` — publish a thread-local file over HTTP.
//!
//! Copies (or moves) a file into `<thread>/.jyc/public/` and returns a
//! shareable URL served by the inspect server at
//! `/public/<channel>/<thread>/<name>?token=<per-thread-token>`.
//! The token lives in `<thread>/.jyc/public-token`, is created on first
//! publish, and is deleted by `/reset` (killing previously shared links).

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::tools::{Tool, ToolContext, ToolOutput};

/// Tool for publishing files from the thread directory to a public URL.
pub struct PublishFileTool {
    /// Base URL prepended to `/public/...` links (from
    /// `[inspect] public_base_url`, falling back to `http://<bind>`).
    base_url: String,
}

impl PublishFileTool {
    pub fn new(base_url: String) -> Self {
        Self { base_url }
    }
}

#[async_trait]
impl Tool for PublishFileTool {
    fn name(&self) -> &str {
        "jyc_publish_file"
    }

    fn description(&self) -> &str {
        "Publish a file from the thread directory to make it accessible via a \
         public HTTP link. Copies the file by default; set move=true to move \
         it (the source file disappears). Returns a shareable URL protected \
         by an access token. Use when the user needs to download or view a \
         generated file (report, PDF, image) via a link."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path of the file to publish, relative to the thread directory"
                },
                "name": {
                    "type": "string",
                    "description": "Optional published filename. Defaults to the source file's basename."
                },
                "move": {
                    "type": "boolean",
                    "description": "Whether to move the file instead of copying it. Default: false (copy, source kept)."
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let path = input
            .get("path")
            .and_then(|p| p.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'path' parameter"))?;

        let do_move = input
            .get("move")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Resolve and validate the source file (same boundary check as
        // send_to_thread attachments): must exist, be a file, and lie
        // inside the working directory.
        let src = ctx.working_dir.join(path);
        let src_canonical = match src.canonicalize() {
            Ok(c) if c.is_file() => c,
            Ok(_) => return Ok(ToolOutput::error(format!("'{path}' is not a file"))),
            Err(_) => return Ok(ToolOutput::error(format!("File not found: '{path}'"))),
        };
        let working_canonical = ctx
            .working_dir
            .canonicalize()
            .unwrap_or_else(|_| ctx.working_dir.to_path_buf());
        if !src_canonical.starts_with(&working_canonical) {
            return Ok(ToolOutput::error(format!(
                "File '{path}' is outside the working directory"
            )));
        }

        // Published name: explicit `name` or the source basename.
        let default_name = src_canonical
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let name = input
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or(&default_name);
        if name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || name == "."
            || name == ".."
        {
            return Ok(ToolOutput::error(format!(
                "Invalid published name: '{name}'"
            )));
        }

        let (channel, thread) = match (&ctx.current_channel, &ctx.current_thread) {
            (Some(c), Some(t)) => (c.clone(), t.clone()),
            _ => {
                return Ok(ToolOutput::error(
                    "Channel/thread context unavailable; cannot build public URL",
                ));
            }
        };

        let jyc_dir = ctx.working_dir.join(".jyc");
        let public_dir = jyc_dir.join(jyc_core::PUBLIC_DIR_NAME);
        tokio::fs::create_dir_all(&public_dir)
            .await
            .context("failed to create public directory")?;

        let target = public_dir.join(name);
        if do_move {
            tokio::fs::rename(&src_canonical, &target)
                .await
                .context("failed to move file into public directory")?;
        } else {
            tokio::fs::copy(&src_canonical, &target)
                .await
                .context("failed to copy file into public directory")?;
        }

        let token = load_or_create_token(&jyc_dir)?;
        let url = format!(
            "{}/public/{}/{}/{}?token={}",
            self.base_url, channel, thread, name, token
        );

        tracing::info!(
            channel = %channel,
            thread = %thread,
            name = %name,
            moved = do_move,
            "File published to public URL"
        );

        Ok(ToolOutput::success(format!(
            "File published. Shareable URL:\n{url}"
        )))
    }
}

/// Read the per-thread public-access token, generating and persisting it
/// (owner-only permissions) on first use.
fn load_or_create_token(jyc_dir: &Path) -> Result<String> {
    let token_path: PathBuf = jyc_dir.join(jyc_core::PUBLIC_TOKEN_FILENAME);
    if let Ok(token) = jyc_utils::auth_token::read_token(&token_path) {
        return Ok(token);
    }
    let token = jyc_utils::auth_token::generate_token();
    jyc_utils::auth_token::write_token(&token_path, &token)
        .context("failed to write public token")?;
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx(working_dir: &Path) -> ToolContext<'_> {
        let mut ctx = ToolContext::new(working_dir);
        ctx.current_channel = Some("email".into());
        ctx.current_thread = Some("weather".into());
        ctx
    }

    fn tool() -> PublishFileTool {
        PublishFileTool::new("https://jyc.example.com".into())
    }

    #[tokio::test]
    async fn test_publish_copies_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("report.pdf"), b"pdf-bytes").unwrap();

        let out = tool()
            .execute(json!({"path": "report.pdf"}), &test_ctx(tmp.path()))
            .await
            .unwrap();

        // Source kept (copy), published file exists, URL returned with token.
        assert!(tmp.path().join("report.pdf").exists());
        let published = tmp.path().join(".jyc/public/report.pdf");
        assert_eq!(std::fs::read(&published).unwrap(), b"pdf-bytes");
        let token = std::fs::read_to_string(tmp.path().join(".jyc/public-token")).unwrap();
        let token = token.trim();
        assert!(!out.content.contains("error"), "{}", out.content);
        assert!(out.content.contains(&format!(
            "https://jyc.example.com/public/email/weather/report.pdf?token={token}"
        )));
    }

    #[tokio::test]
    async fn test_publish_move_removes_source() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("chart.png"), b"png").unwrap();

        tool()
            .execute(
                json!({"path": "chart.png", "move": true}),
                &test_ctx(tmp.path()),
            )
            .await
            .unwrap();

        assert!(!tmp.path().join("chart.png").exists());
        assert!(tmp.path().join(".jyc/public/chart.png").exists());
    }

    #[tokio::test]
    async fn test_token_reused_across_publishes() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"a").unwrap();
        std::fs::write(tmp.path().join("b.txt"), b"b").unwrap();

        let out1 = tool()
            .execute(json!({"path": "a.txt"}), &test_ctx(tmp.path()))
            .await
            .unwrap();
        let out2 = tool()
            .execute(json!({"path": "b.txt"}), &test_ctx(tmp.path()))
            .await
            .unwrap();

        let token1 = out1.content.rsplit("token=").next().unwrap().to_string();
        let token2 = out2.content.rsplit("token=").next().unwrap().to_string();
        assert_eq!(token1, token2);
        assert_eq!(token1.trim().len(), 64);
    }

    #[tokio::test]
    async fn test_publish_rejects_traversal() {
        // Self-contained: <outer>/secret.txt is outside <outer>/work/.
        let outer = tempfile::tempdir().unwrap();
        let work = outer.path().join("work");
        std::fs::create_dir(&work).unwrap();
        std::fs::write(outer.path().join("secret.txt"), b"secret").unwrap();

        let out = tool()
            .execute(json!({"path": "../secret.txt"}), &test_ctx(&work))
            .await
            .unwrap();

        assert!(out.is_error);
        assert!(!work.join(".jyc/public").exists());
    }

    #[tokio::test]
    async fn test_publish_rejects_bad_name() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"a").unwrap();

        let out = tool()
            .execute(
                json!({"path": "a.txt", "name": "sub/dir.txt"}),
                &test_ctx(tmp.path()),
            )
            .await
            .unwrap();

        assert!(out.is_error);
        assert!(!tmp.path().join(".jyc/public").exists());
    }
}
