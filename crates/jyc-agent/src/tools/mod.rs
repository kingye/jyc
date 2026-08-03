//! Tool system for the agent.
//!
//! Defines the `Tool` trait and built-in tool implementations.

pub mod builtin;
pub mod mcp_bridge;
pub mod mcp_client;
pub mod registry;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::types::{ImageSource, ToolDefinition};
use jyc_core::thread_manager::ThreadManager;
use jyc_types::channel::OutboundAdapter;

/// Shared thread managers map keyed by channel name.
pub type ThreadManagersMap = Arc<tokio::sync::Mutex<HashMap<String, Arc<ThreadManager>>>>;

/// Shared outbound adapters map keyed by channel name.
/// Used by `jyc_send_message` to support the `channel` parameter for
/// cross-channel proactive messaging.
pub type OutboundsMap = Arc<tokio::sync::Mutex<HashMap<String, Arc<dyn OutboundAdapter>>>>;

/// Context provided to tools during execution.
pub struct ToolContext<'a> {
    /// Working directory for the tool.
    pub working_dir: &'a Path,
    /// Additional absolute paths the agent may legitimately read from
    /// outside `working_dir` (currently: a configured absolute
    /// `[attachments.inbound].save_path`). Tools that enforce a path
    /// boundary (e.g. `read_image`) accept paths under any of these
    /// roots in addition to `working_dir`.
    pub additional_read_roots: Vec<PathBuf>,
    /// Additional absolute paths the agent may write to outside
    /// `working_dir` (e.g. configured per-pattern `write` paths).
    /// Write tools (`write`, `edit`, `bash`) accept paths under any of
    /// these roots in addition to `working_dir`.
    pub additional_write_roots: Vec<PathBuf>,
    /// Side-channel for tools (e.g. `read_image`) that need to inject
    /// additional content blocks into the *next* user turn alongside
    /// the textual tool result. The agent loop drains this after each
    /// batch of tool calls and emits a synthetic user turn carrying the
    /// images. `Mutex<Vec<_>>` to allow tools with `&self` execution to
    /// push without requiring `&mut ToolContext`.
    pub pending_images: Mutex<Vec<ImageSource>>,
    /// Whether the active pattern allows image handling. Mirrors the
    /// `inject_inbound_images` flag that `build_user_blocks` checks in
    /// `service.rs`. When `false`, `read_image` should refuse to process
    /// images even if a `VisionClient` is configured, ensuring consistent
    /// behavior between auto-injection and tool-driven image analysis.
    /// Default: `false` (opt-in for safety).
    pub pattern_inject_images: bool,
    /// Optional outbound adapter for proactive messaging tools (e.g.
    /// `jyc_send_message`). Injected by `JycAgentService` when building
    /// the tool registry. `None` when the agent runs in contexts without
    /// a pre-warmed outbound adapter.
    pub outbound: Option<Arc<dyn OutboundAdapter>>,
    /// Cross-channel thread managers keyed by channel name.
    /// Used by `jyc_send_to_thread` tool to inject messages into threads
    /// in other channels. `None` when running in contexts without
    /// cross-channel communication (e.g. unit tests).
    pub thread_managers: Option<ThreadManagersMap>,
    /// Current channel name, for tools that need source context (e.g.
    /// `jyc_send_to_thread` sets `source_channel` metadata from this).
    pub current_channel: Option<String>,
    /// Current thread name, for tools that need source context (e.g.
    /// `jyc_send_to_thread` sets `source_thread` metadata from this).
    pub current_thread: Option<String>,
    /// Cross-channel outbound adapters keyed by channel name.
    /// Used by `jyc_send_message` to support the `channel` parameter for
    /// sending proactive messages through any channel's outbound adapter.
    /// `None` when running in contexts without cross-channel support
    /// (e.g. unit tests).
    pub outbounds: Option<OutboundsMap>,
}

/// Whether `canonical` lies inside the system temp dir `tmp`.
///
/// `tmp` is canonicalized first: on macOS `env::temp_dir()` yields
/// `/var/folders/...` while an already-resolved argument arrives as
/// `/private/var/folders/...`, and a raw comparison would miss the match.
///
/// A `tmp` with no parent (i.e. the filesystem root) is rejected: every
/// absolute path is inside `/`, so honoring it would disable the boundary
/// entirely. `env::temp_dir()` returns `$TMPDIR` unvalidated on Unix, so
/// this is reachable via a stray `export TMPDIR=/` in the service
/// environment rather than being merely theoretical.
///
/// Takes `tmp` as a parameter rather than reading the environment so the
/// degenerate cases stay testable without mutating process state.
fn is_within_temp_dir(canonical: &Path, tmp: &Path) -> bool {
    let tmp_canonical = tmp.canonicalize().unwrap_or_else(|_| tmp.to_path_buf());
    tmp_canonical.parent().is_some() && canonical.starts_with(&tmp_canonical)
}

impl<'a> ToolContext<'a> {
    /// Construct a context with no extra roots and an empty pending-images queue.
    pub fn new(working_dir: &'a Path) -> Self {
        Self {
            working_dir,
            additional_read_roots: Vec::new(),
            additional_write_roots: Vec::new(),
            pending_images: Mutex::new(Vec::new()),
            pattern_inject_images: false,
            outbound: None,
            thread_managers: None,
            current_channel: None,
            current_thread: None,
            outbounds: None,
        }
    }

    /// Construct a context with extra absolute read roots.
    pub fn with_roots(working_dir: &'a Path, additional_read_roots: Vec<PathBuf>) -> Self {
        Self {
            working_dir,
            additional_read_roots,
            additional_write_roots: Vec::new(),
            pending_images: Mutex::new(Vec::new()),
            pattern_inject_images: false,
            outbound: None,
            thread_managers: None,
            current_channel: None,
            current_thread: None,
            outbounds: None,
        }
    }
    /// Drain and return any pending image sources accumulated during the
    /// current tool-execution batch. Called by the agent loop after the
    /// batch completes.
    pub fn take_pending_images(&self) -> Vec<ImageSource> {
        std::mem::take(&mut *self.pending_images.lock().expect("pending_images poisoned"))
    }

    /// Check that `resolved` is within `working_dir` (or one of the
    /// `additional_read_roots`). Returns `Ok(())` when the path is inside
    /// the boundary, or an `Err` with a user-facing access-denied message.
    ///
    /// **Symlink exemption**: when any ancestor component of `resolved`
    /// above `working_dir` is a symlink (e.g. the `repo_group` feature
    /// where `repo/ -> /other/path`), the check is skipped. This lets the
    /// agent work with symlinked repos without false positives.
    ///
    /// **Temp-dir exemption**: paths under `std::env::temp_dir()` are always
    /// accepted, so tools have scratch space without per-pattern `access`
    /// config. Note this makes the shared system temp dir readable. A
    /// degenerate temp dir (the filesystem root) is ignored — see
    /// [`is_within_temp_dir`].
    pub fn check_path_boundary(
        &self,
        display_path: &str,
        resolved: &Path,
    ) -> std::result::Result<(), String> {
        // Symlink exemption: skip the boundary check when a symlink
        // component is found above working_dir. This preserves the
        // repo_group feature where working_dir/repo -> /other/path.
        let has_symlink = resolved
            .ancestors()
            .any(|ancestor| ancestor != self.working_dir && ancestor.is_symlink());

        if has_symlink {
            return Ok(());
        }

        let canonical = resolved
            .canonicalize()
            .unwrap_or_else(|_| resolved.to_path_buf());
        let working_canonical = self
            .working_dir
            .canonicalize()
            .unwrap_or_else(|_| self.working_dir.to_path_buf());

        if canonical.starts_with(&working_canonical) {
            return Ok(());
        }

        // Also check additional_read_roots (e.g. configured attachment
        // save paths outside working_dir).
        for root in &self.additional_read_roots {
            let root_canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
            if canonical.starts_with(&root_canonical) {
                return Ok(());
            }
        }

        // System temp dir is always in-boundary: tools need scratch space.
        if is_within_temp_dir(&canonical, &std::env::temp_dir()) {
            return Ok(());
        }

        Err(format!(
            "Access denied: path '{}' is outside the working directory",
            display_path
        ))
    }

    /// Check that `resolved` is within `working_dir`, `additional_read_roots`,
    /// or `additional_write_roots`. Write paths imply read access.
    ///
    /// Inherits the symlink and temp-dir exemptions from
    /// [`Self::check_path_boundary`], which this delegates to first.
    ///
    /// Used by write/edit/bash tools to enforce the write boundary.
    pub fn check_write_boundary(
        &self,
        display_path: &str,
        resolved: &Path,
    ) -> std::result::Result<(), String> {
        // First check read boundary (working_dir + read_roots)
        if self.check_path_boundary(display_path, resolved).is_ok() {
            return Ok(());
        }
        // Then check write roots
        let canonical = resolved
            .canonicalize()
            .unwrap_or_else(|_| resolved.to_path_buf());
        for root in &self.additional_write_roots {
            let root_canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
            if canonical.starts_with(&root_canonical) {
                return Ok(());
            }
        }
        Err(format!(
            "Access denied: path '{}' is outside the working directory",
            display_path
        ))
    }
}

/// Trait for tools that can be invoked by the agent.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Tool name (used in LLM tool_use).
    fn name(&self) -> &str;

    /// Optional source identifier (e.g. MCP server name).
    /// Returns `None` for built-in and bridge tools.
    fn source(&self) -> Option<&str> {
        None
    }

    /// Tool description (shown to LLM).
    fn description(&self) -> &str;

    /// JSON Schema for the tool's input parameters.
    fn input_schema(&self) -> Value;

    /// Execute the tool with the given input.
    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput>;

    /// Convert to a ToolDefinition for the LLM.
    fn to_definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: self.input_schema(),
        }
    }
}

/// Output from a tool execution.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// The tool's text output.
    pub content: String,
    /// Whether the execution resulted in an error.
    pub is_error: bool,
    /// Whether the agent loop should stop after this tool call.
    ///
    /// Defaults to `true` for backward compatibility. Tools that want the
    /// agent to continue (e.g. progress-update replies with `stop_after:
    /// false`) set this to `false` via [`ToolOutput::success_continue`].
    pub stop_after: bool,
}

impl ToolOutput {
    /// Create a successful output. The agent loop will stop after this
    /// tool call (unless the caller overrides based on tool-specific logic).
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            stop_after: true,
        }
    }

    /// Create a successful output that signals the agent loop to **continue**
    /// working. Used by tools like `jyc_reply_message` with `stop_after: false`
    /// (progress updates).
    pub fn success_continue(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            stop_after: false,
        }
    }

    /// Create an error output.
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            stop_after: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The system temp dir is in-boundary for both read and write, while an
    /// unrelated absolute path is still denied. Uses `/etc/passwd` as the
    /// negative case because a `TempDir` working dir now sits inside an
    /// always-allowed root.
    #[test]
    fn temp_dir_is_within_both_boundaries() {
        let working = tempfile::tempdir().expect("create working dir");
        let ctx = ToolContext::new(working.path());

        let scratch = std::env::temp_dir().join("jyc-boundary-test.txt");
        assert!(ctx.check_path_boundary("scratch", &scratch).is_ok());
        assert!(ctx.check_write_boundary("scratch", &scratch).is_ok());

        let outside = Path::new("/etc/passwd");
        assert!(ctx.check_path_boundary("passwd", outside).is_err());
        assert!(ctx.check_write_boundary("passwd", outside).is_err());
    }

    /// A temp dir of `/` must not be honored: every absolute path is inside
    /// the root, so accepting it would disable the boundary entirely.
    /// `env::temp_dir()` returns `$TMPDIR` unvalidated on Unix, so this is a
    /// reachable misconfiguration.
    #[test]
    fn root_temp_dir_is_rejected() {
        let root = Path::new("/");
        assert!(!is_within_temp_dir(Path::new("/etc/passwd"), root));
        assert!(!is_within_temp_dir(
            Path::new("/home/someone/.ssh/id_rsa"),
            root
        ));

        // A normal temp dir still matches, and only on whole components.
        let tmp = Path::new("/tmp");
        assert!(is_within_temp_dir(Path::new("/tmp/scratch.txt"), tmp));
        assert!(!is_within_temp_dir(Path::new("/tmpfoo/escape.txt"), tmp));
    }
}
