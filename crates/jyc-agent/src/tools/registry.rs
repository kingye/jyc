//! Tool registry — collects all available tools and provides definitions to the LLM.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use jyc_types::config::HookEvent;
use jyc_utils::hooks::{HookCtx, HookOutcome, HookSet};

use super::{Tool, ToolContext, ToolOutput};
use crate::types::ToolDefinition;

/// Registry of available tools.
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
    /// Compiled global + per-agent hook set for tool events
    /// (`pre_tool_use`, `post_tool_use`, `post_tool_use_failure`);
    /// `None` (the default) → zero hook overhead.
    hooks: Option<Arc<HookSet>>,
}

impl ToolRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            hooks: None,
        }
    }

    /// Attach agent hooks. Called by `build_tool_registry` when the agent
    /// (or global config) declares hooks; the registry cache key includes
    /// the config snapshot pointer, so a reload rebuilds with fresh hooks.
    pub fn set_hooks(&mut self, hooks: Arc<HookSet>) {
        self.hooks = Some(hooks);
    }

    /// Register a tool.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Remove a tool by name. No-op if the tool is not registered.
    pub fn remove(&mut self, name: &str) {
        self.tools.remove(name);
    }

    /// Get all tool definitions for the LLM, sorted by name.
    ///
    /// The sort is required for prompt caching: providers cache on an exact
    /// prefix match, and `HashMap` iteration order is randomized per process.
    /// Without a stable order the serialized `tools` array differs on every
    /// restart, so the cache breakpoint placed on the last tool would never
    /// produce a hit (see `provider::anthropic::apply_cache_breakpoints`).
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut defs: Vec<ToolDefinition> =
            self.tools.values().map(|t| t.to_definition()).collect();
        // Unstable sort: tool names are unique (they're `HashMap` keys), so
        // there are no equal elements whose relative order could matter, and
        // it avoids the scratch allocation a stable sort needs.
        defs.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        defs
    }

    /// Execute a tool by name, applying the agent's external hooks.
    ///
    /// Exit-code semantics mirror Claude Code: a `pre_tool_use` hook that
    /// exits 2 fails the call with the hook's stderr as the error (visible
    /// to the model); a `post_tool_use` hook that exits 2 marks the
    /// (already executed) result as error with the stderr appended.
    /// `post_tool_use_failure` is notification-only and fires whenever the
    /// TOOL itself failed. A `jyc_reply_message` call that delivers
    /// synchronously also fires `reply_send` just before the send. Without
    /// hooks configured this is the same zero-cost pass-through it always
    /// was.
    pub async fn execute(
        &self,
        name: &str,
        input: Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput> {
        let tool = self.tools.get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "Tool '{}' not found. Available: {:?}",
                name,
                self.tools.keys().collect::<Vec<_>>()
            )
        })?;

        let Some(hooks) = self.hooks.clone() else {
            return tool.execute(input, ctx).await;
        };

        // Clone payload data only when hooks are configured.
        let input_for_hook = input.clone();
        let hook_ctx = |response: Option<String>| HookCtx {
            topic: ctx.current_topic.clone().unwrap_or_default(),
            cwd: ctx.working_dir.display().to_string(),
            channel: ctx.current_channel.clone(),
            tool_name: Some(name.to_string()),
            tool_input: Some(input_for_hook.clone()),
            tool_response: response,
            ..Default::default()
        };

        // ── pre_tool_use ───────────────────────────────────────────────
        if let HookOutcome::Block(reason) = hooks
            .run(HookEvent::PreToolUse, Some(name), &hook_ctx(None))
            .await
        {
            return Err(anyhow::anyhow!("blocked by pre_tool_use hook: {reason}"));
        }

        // ── reply_send (synchronous tool delivery) ─────────────────────
        // When this call WILL deliver through the adapter directly — the
        // exact condition `mcp_bridge` uses for its synchronous send — the
        // real delivery point is here, so `reply_send` gates it now. File
        // relay deliveries (no reply_target, or the post-failure relay retry)
        // are gated at their send sites in `jyc-core` instead, so the event
        // fires exactly once per delivery attempt either way. Silent replies
        // send nothing and are not gated.
        if name == "jyc_reply_message"
            && ctx.outbound.is_some()
            && ctx.reply_target.is_some()
            && !input
                .get("silent")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            if let Some(text) = input.get("message").and_then(Value::as_str) {
                let mut rctx = hook_ctx(None);
                rctx.message_content = None; // this is an output, not an intake
                rctx.reply_text = Some(text.to_string());
                if let HookOutcome::Block(reason) = hooks
                    .run(HookEvent::ReplySend, ctx.current_topic.as_deref(), &rctx)
                    .await
                {
                    // Suppress the send and end the turn (`error` defaults to
                    // stop_after), so the model can't loop retrying it.
                    return Ok(ToolOutput::error(format!(
                        "reply suppressed by reply_send hook: {reason}"
                    )));
                }
            }
        }

        let mut output = match tool.execute(input, ctx).await {
            Ok(output) => output,
            Err(e) => {
                // Failure notification (does not alter the outcome).
                hooks
                    .run(
                        HookEvent::PostToolUseFailure,
                        Some(name),
                        &hook_ctx(Some(e.to_string())),
                    )
                    .await;
                return Err(e);
            }
        };

        // ── post_tool_use ──────────────────────────────────────────────
        // "Failure" means the TOOL failed — captured before the post hook
        // so a post-hook's own block is not misreported as a tool failure.
        let tool_failed = output.is_error;
        if let HookOutcome::Block(reason) = hooks
            .run(
                HookEvent::PostToolUse,
                Some(name),
                &hook_ctx(Some(output.content.clone())),
            )
            .await
        {
            // The tool already ran; surface the hook's reason to the model
            // as an error result while preserving the output's other
            // semantics (`stop_after`, delivery flags).
            output.is_error = true;
            output.content.push_str("\n\n[post_tool_use hook: ");
            output.content.push_str(reason.trim());
            output.content.push(']');
        }
        // ── post_tool_use_failure (notification-only) ──────────────────
        if tool_failed {
            hooks
                .run(
                    HookEvent::PostToolUseFailure,
                    Some(name),
                    &hook_ctx(Some(output.content.clone())),
                )
                .await;
        }
        Ok(output)
    }

    /// Check if a tool exists.
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct MockTool {
        name: String,
    }

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "mock tool"
        }

        fn input_schema(&self) -> Value {
            Value::Null
        }

        async fn execute(&self, _input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput> {
            Ok(ToolOutput::success("executed"))
        }
    }

    #[test]
    fn new_registry_is_empty() {
        let reg = ToolRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn default_registry_is_empty() {
        let reg = ToolRegistry::default();
        assert!(reg.is_empty());
    }

    #[test]
    fn register_and_has_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool {
            name: "test".to_string(),
        }));
        assert!(reg.has_tool("test"));
        assert!(!reg.has_tool("missing"));
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_empty());
    }

    #[test]
    fn remove_existing_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool {
            name: "a".to_string(),
        }));
        reg.remove("a");
        assert!(!reg.has_tool("a"));
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool {
            name: "a".to_string(),
        }));
        reg.remove("nonexistent");
        assert!(reg.has_tool("a"));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn definitions_returns_all_tools() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool {
            name: "tool1".to_string(),
        }));
        reg.register(Box::new(MockTool {
            name: "tool2".to_string(),
        }));
        let defs = reg.definitions();
        assert_eq!(defs.len(), 2);
        assert!(defs.iter().any(|d| d.name == "tool1"));
        assert!(defs.iter().any(|d| d.name == "tool2"));
    }

    /// Tool order must be deterministic regardless of registration order,
    /// because prompt caching needs a byte-identical `tools` prefix across
    /// requests (and `HashMap` iteration is randomized per process).
    #[test]
    fn definitions_are_sorted_by_name() {
        let mut reg = ToolRegistry::new();
        for name in ["write", "bash", "read", "edit"] {
            reg.register(Box::new(MockTool {
                name: name.to_string(),
            }));
        }
        let names: Vec<String> = reg.definitions().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["bash", "edit", "read", "write"]);
    }

    #[tokio::test]
    async fn execute_existing_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool {
            name: "mock".to_string(),
        }));
        let ctx = ToolContext::new(std::path::Path::new("/tmp"));
        let result = reg.execute("mock", Value::Null, &ctx).await.unwrap();
        assert!(!result.is_error);
        assert_eq!(result.content, "executed");
    }

    #[tokio::test]
    async fn execute_missing_tool_returns_error() {
        let reg = ToolRegistry::new();
        let ctx = ToolContext::new(std::path::Path::new("/tmp"));
        let result = reg.execute("missing", Value::Null, &ctx).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Tool 'missing' not found"));
    }

    fn hook_cfg(event: &str, script: &str) -> jyc_types::config::HookConfig {
        jyc_types::config::HookConfig {
            event: event.to_string(),
            matcher: None,
            shell: vec!["sh".into(), "-c".into(), script.into()],
            timeout: None,
        }
    }

    fn hook(event: &str, script: &str) -> Arc<HookSet> {
        Arc::new(HookSet::for_agent(&[hook_cfg(event, script)], "test"))
    }

    fn reg_with_mock() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool {
            name: "mock".to_string(),
        }));
        reg
    }

    #[tokio::test]
    async fn pre_tool_hook_exit2_blocks_call() {
        let mut reg = reg_with_mock();
        reg.set_hooks(hook("pre_tool_use", "echo nope >&2; exit 2"));
        let ctx = ToolContext::new(std::path::Path::new("/tmp"));
        let err = reg.execute("mock", Value::Null, &ctx).await.unwrap_err();
        assert!(err.to_string().contains("nope"), "got: {err}");
    }

    #[tokio::test]
    async fn pre_tool_hook_exit0_allows_call() {
        let mut reg = reg_with_mock();
        reg.set_hooks(hook("pre_tool_use", "exit 0"));
        let ctx = ToolContext::new(std::path::Path::new("/tmp"));
        let out = reg.execute("mock", Value::Null, &ctx).await.unwrap();
        assert_eq!(out.content, "executed");
    }

    #[tokio::test]
    async fn post_tool_hook_exit2_marks_error_appends_reason() {
        let mut reg = reg_with_mock();
        reg.set_hooks(hook("post_tool_use", "echo review this >&2; exit 2"));
        let ctx = ToolContext::new(std::path::Path::new("/tmp"));
        let out = reg.execute("mock", Value::Null, &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("executed"));
        assert!(out.content.contains("review this"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn hook_matcher_scopes_by_tool_name() {
        let mut reg = reg_with_mock();
        // matcher only fires for a tool literally named "other", not "mock".
        reg.set_hooks(Arc::new(HookSet::for_agent(
            &[jyc_types::config::HookConfig {
                event: "pre_tool_use".into(),
                matcher: Some("^other$".into()),
                shell: vec!["sh".into(), "-c".into(), "exit 2".into()],
                timeout: None,
            }],
            "test",
        )));
        let ctx = ToolContext::new(std::path::Path::new("/tmp"));
        // "mock" doesn't match "^other$" → proceeds unblocked.
        let out = reg.execute("mock", Value::Null, &ctx).await.unwrap();
        assert_eq!(out.content, "executed");
    }

    /// Outbound adapter that does nothing (sync-delivery presence check only).
    struct NoopOut;

    #[async_trait]
    impl jyc_types::OutboundAdapter for NoopOut {
        fn channel_type(&self) -> &str {
            "test"
        }
        async fn connect(&self) -> Result<()> {
            Ok(())
        }
        async fn disconnect(&self) -> Result<()> {
            Ok(())
        }
        fn clean_body(&self, raw_body: &str) -> String {
            raw_body.to_string()
        }
        async fn send_reply(
            &self,
            _original: &jyc_types::InboundMessage,
            _reply_text: &str,
            _topic_path: &std::path::Path,
            _message_dir: &str,
            _attachments: Option<&[jyc_types::OutboundAttachment]>,
        ) -> Result<jyc_types::SendResult> {
            Ok(jyc_types::SendResult {
                message_id: "test".to_string(),
            })
        }
        async fn send_message(
            &self,
            _recipient: &str,
            _subject: &str,
            _body: &str,
        ) -> Result<jyc_types::SendResult> {
            Ok(jyc_types::SendResult {
                message_id: "test".to_string(),
            })
        }
    }

    /// Wire the synchronous-delivery fields (the exact pair `mcp_bridge`
    /// checks before sending inline instead of relaying via file).
    fn sync_reply_ctx(ctx: &mut ToolContext<'_>) {
        ctx.outbound = Some(Arc::new(NoopOut));
        ctx.reply_target = Some(crate::tools::ReplyTarget {
            original: jyc_types::InboundMessage {
                id: "m1".into(),
                channel: "test".into(),
                channel_uid: "t".into(),
                sender: "s".into(),
                sender_address: "s@x".into(),
                recipients: vec![],
                topic: "rt".into(),
                content: jyc_types::MessageContent {
                    text: Some("q".into()),
                    html: None,
                    markdown: None,
                },
                timestamp: chrono::Utc::now(),
                references: None,
                reply_to_id: None,
                external_id: None,
                attachments: vec![],
                metadata: Default::default(),
                matched_pattern: None,
            },
            message_dir: "d".into(),
        });
    }

    #[tokio::test]
    async fn reply_send_gates_synchronous_tool_delivery() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool {
            name: "jyc_reply_message".into(),
        }));
        reg.set_hooks(hook(
            "reply_send",
            "grep -q reply_text - && echo rs >> markers; exit 2",
        ));
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::new(tmp.path());
        sync_reply_ctx(&mut ctx);
        ctx.current_topic = Some("rt".into());
        let out = reg
            .execute(
                "jyc_reply_message",
                serde_json::json!({"message": "secret reply"}),
                &ctx,
            )
            .await
            .unwrap();
        // Suppressed: error result (ends the turn), tool never ran.
        assert!(out.is_error);
        assert!(out.content.contains("suppressed"), "got: {}", out.content);
        let markers = std::fs::read_to_string(tmp.path().join("markers")).unwrap();
        assert!(markers.contains("rs"), "hook must see reply_text");
    }

    #[tokio::test]
    async fn silent_reply_is_not_gated_by_reply_send() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool {
            name: "jyc_reply_message".into(),
        }));
        reg.set_hooks(hook("reply_send", "exit 2"));
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::new(tmp.path());
        sync_reply_ctx(&mut ctx);
        let out = reg
            .execute(
                "jyc_reply_message",
                serde_json::json!({"message": "x", "silent": true}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.content, "executed", "silent reply sends nothing");
    }

    #[tokio::test]
    async fn relay_reply_is_not_gated_at_tool_level() {
        // Without reply_target/outbound the tool writes the file relay;
        // reply_send must fire at the delivery sites in jyc-core instead.
        let mut reg = reg_with_mock();
        reg.register(Box::new(MockTool {
            name: "jyc_reply_message".into(),
        }));
        reg.set_hooks(hook("reply_send", "exit 2"));
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::new(tmp.path());
        ctx.current_topic = Some("rt".into());
        let out = reg
            .execute(
                "jyc_reply_message",
                serde_json::json!({"message": "x"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.content, "executed");
    }

    #[tokio::test]
    async fn post_hook_block_does_not_emit_failure_event() {
        let mut reg = reg_with_mock();
        // Tool SUCCEEDS; post hook blocks. The failure hook must not fire.
        reg.set_hooks(Arc::new(HookSet::for_agent(
            &[
                hook_cfg("post_tool_use", "echo p >> markers; exit 2"),
                hook_cfg("post_tool_use_failure", "echo f >> markers"),
            ],
            "test",
        )));
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(tmp.path());
        let out = reg.execute("mock", Value::Null, &ctx).await.unwrap();
        assert!(out.is_error, "post block must mark error");
        let markers = std::fs::read_to_string(tmp.path().join("markers")).unwrap();
        assert_eq!(
            markers.trim(),
            "p",
            "failure event fired for a hook-marked error"
        );
    }

    #[tokio::test]
    async fn tool_error_output_emits_failure_event() {
        struct ErrorTool;
        #[async_trait]
        impl Tool for ErrorTool {
            fn name(&self) -> &str {
                "mock"
            }
            fn description(&self) -> &str {
                "e"
            }
            fn input_schema(&self) -> Value {
                Value::Null
            }
            async fn execute(&self, _input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput> {
                Ok(ToolOutput::error("boom"))
            }
        }
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(ErrorTool));
        reg.set_hooks(Arc::new(HookSet::for_agent(
            &[hook_cfg(
                "post_tool_use_failure",
                "grep -q boom - && echo f >> markers",
            )],
            "test",
        )));
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(tmp.path());
        let out = reg.execute("mock", Value::Null, &ctx).await.unwrap();
        assert!(out.is_error);
        let markers = std::fs::read_to_string(tmp.path().join("markers")).unwrap();
        assert_eq!(markers.trim(), "f");
    }
}
