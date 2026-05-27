//! Unit tests for jyc-agent crate.

mod filter_valid_messages {
    use jyc_agent::provider::filter_valid_messages;
    use serde_json::json;

    #[test]
    fn keeps_user_messages() {
        let messages = vec![json!({"role": "user", "content": "hello"})];
        let result = filter_valid_messages(&messages);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn keeps_tool_messages() {
        let messages = vec![json!({"role": "tool", "tool_call_id": "123", "content": "result"})];
        let result = filter_valid_messages(&messages);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn keeps_assistant_with_content() {
        let messages = vec![json!({"role": "assistant", "content": "Hello!"})];
        let result = filter_valid_messages(&messages);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn keeps_assistant_with_tool_calls() {
        let messages = vec![json!({"role": "assistant", "content": null, "tool_calls": [
            {"id": "1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
        ]})];
        let result = filter_valid_messages(&messages);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn keeps_assistant_with_reasoning_and_tool_calls() {
        let messages = vec![
            json!({"role": "assistant", "content": null, "reasoning_content": "thinking...", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
            ]}),
        ];
        let result = filter_valid_messages(&messages);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn removes_assistant_with_null_content_no_tool_calls() {
        let messages = vec![json!({"role": "assistant", "content": null})];
        let result = filter_valid_messages(&messages);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn removes_assistant_with_empty_content_no_tool_calls() {
        let messages = vec![json!({"role": "assistant", "content": ""})];
        let result = filter_valid_messages(&messages);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn removes_assistant_with_only_reasoning_content() {
        // DeepSeek sends this but rejects it on replay
        let messages = vec![
            json!({"role": "assistant", "content": null, "reasoning_content": "I'm thinking..."}),
        ];
        let result = filter_valid_messages(&messages);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn removes_assistant_with_empty_tool_calls() {
        let messages = vec![json!({"role": "assistant", "content": null, "tool_calls": []})];
        let result = filter_valid_messages(&messages);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn keeps_anthropic_assistant_with_text_block() {
        let messages = vec![json!({"role": "assistant", "content": [
            {"type": "text", "text": "Hello!"}
        ]})];
        let result = filter_valid_messages(&messages);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn keeps_anthropic_assistant_with_tool_use_block() {
        let messages = vec![json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "1", "name": "bash", "input": {}}
        ]})];
        let result = filter_valid_messages(&messages);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn removes_anthropic_assistant_with_empty_content_array() {
        let messages = vec![json!({"role": "assistant", "content": []})];
        let result = filter_valid_messages(&messages);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn removes_anthropic_assistant_with_empty_text_block() {
        let messages = vec![json!({"role": "assistant", "content": [
            {"type": "text", "text": ""}
        ]})];
        let result = filter_valid_messages(&messages);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn mixed_conversation_filters_correctly() {
        let messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": null, "reasoning_content": "thinking"}), // invalid
            json!({"role": "assistant", "content": null, "tool_calls": [{"id":"1","type":"function","function":{"name":"bash","arguments":"{}"}}]}), // valid
            json!({"role": "tool", "tool_call_id": "1", "content": "done"}),
            json!({"role": "assistant", "content": "Here's the result."}), // valid
            json!({"role": "user", "content": "thanks"}),
            json!({"role": "assistant", "content": null}), // invalid
        ];
        let result = filter_valid_messages(&messages);
        assert_eq!(result.len(), 5); // user, assistant+tool_calls, tool, assistant+content, user
    }

    /// Regression test for v0.3.7. The v0.3.6 release introduced
    /// `reasoning_content` stripping on non-final assistant turns, which broke
    /// DeepSeek `thinking = enabled` mode with HTTP 400:
    ///   "The reasoning_content in the thinking mode must be passed back to the API."
    /// v0.3.7 reverted the strip; this test pins the contract: every assistant
    /// turn that already carries `reasoning_content` must still carry it after
    /// `filter_valid_messages` returns.
    #[test]
    fn preserves_reasoning_content_on_all_assistant_turns() {
        let messages = vec![
            json!({"role": "user", "content": "task"}),
            json!({
                "role": "assistant",
                "content": "step 1",
                "reasoning_content": "thinking 1",
                "tool_calls": [{"id":"1","type":"function","function":{"name":"bash","arguments":"{}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "1", "content": "ok"}),
            json!({
                "role": "assistant",
                "content": "step 2",
                "reasoning_content": "thinking 2"
            }),
        ];
        let result = filter_valid_messages(&messages);
        assert_eq!(result.len(), 4);
        assert_eq!(result[1]["reasoning_content"], "thinking 1");
        assert_eq!(result[3]["reasoning_content"], "thinking 2");
    }
}

mod parse_openai_chunk {
    use jyc_agent::types::StreamEvent;

    // Helper to parse a chunk and collect events
    #[allow(dead_code)]
    fn parse_chunk(_data: &str) -> Vec<StreamEvent> {
        // We need access to the internal parse function.
        // Since it's private, we test via the public stream interface indirectly.
        // For now, test the stream behavior through type checking.
        vec![] // TODO: expose parse_openai_chunk for testing or test via integration
    }

    // These tests verify the StreamEvent types are correct
    #[test]
    fn stream_event_text_delta() {
        let event = StreamEvent::TextDelta("hello".to_string());
        match event {
            StreamEvent::TextDelta(t) => assert_eq!(t, "hello"),
            _ => panic!("Expected TextDelta"),
        }
    }

    #[test]
    fn stream_event_reasoning_delta() {
        let event = StreamEvent::ReasoningDelta("thinking".to_string());
        match event {
            StreamEvent::ReasoningDelta(t) => assert_eq!(t, "thinking"),
            _ => panic!("Expected ReasoningDelta"),
        }
    }

    #[test]
    fn stream_event_tool_use() {
        let event = StreamEvent::ToolUseStart {
            id: "call_123".to_string(),
            name: "bash".to_string(),
        };
        match event {
            StreamEvent::ToolUseStart { id, name } => {
                assert_eq!(id, "call_123");
                assert_eq!(name, "bash");
            }
            _ => panic!("Expected ToolUseStart"),
        }
    }
}

mod session {
    use jyc_agent::session;

    /// Minimal stub provider that panics if any LLM method is invoked.
    /// Used by `update_tokens` tests where the auto-reset threshold is NOT
    /// crossed, so the provider's LLM is never actually called.
    struct StubProvider;

    #[async_trait::async_trait]
    impl jyc_agent::provider::Provider for StubProvider {
        fn name(&self) -> &str {
            "stub"
        }
        fn model(&self) -> &str {
            "stub"
        }

        async fn complete(
            &self,
            _messages: &[jyc_agent::types::Message],
            _tools: &[jyc_agent::types::ToolDefinition],
            _system: &str,
        ) -> anyhow::Result<jyc_agent::provider::EventStream> {
            panic!("stub provider should not be invoked in these tests")
        }

        fn format_user_message(
            &self,
            _blocks: &[jyc_agent::types::ContentBlock],
        ) -> serde_json::Value {
            panic!("stub")
        }

        fn format_tool_result(
            &self,
            _id: &str,
            _content: &str,
            _is_error: bool,
        ) -> serde_json::Value {
            panic!("stub")
        }

        fn build_raw_assistant_message(
            &self,
            _text: &str,
            _reasoning: &str,
            _tool_calls: &[(String, String, String)],
        ) -> serde_json::Value {
            panic!("stub")
        }

        async fn complete_raw(
            &self,
            _raw_messages: &[serde_json::Value],
            _tools: &[jyc_agent::types::ToolDefinition],
            _system: &str,
        ) -> anyhow::Result<jyc_agent::provider::EventStream> {
            panic!("stub provider should not be invoked in these tests")
        }
    }

    #[tokio::test]
    async fn load_context_returns_empty_when_no_session_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (messages, raw_context) = session::load_context(tmp.path()).await;
        assert!(messages.is_empty());
        assert!(raw_context.is_empty());
    }

    #[tokio::test]
    async fn save_and_load_raw_context() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();

        // Create session file (needed for load_context to proceed)
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"created_at":"2026-01-01T00:00:00Z","total_input_tokens":0,"total_output_tokens":0,"max_input_tokens":0}"#,
        ).await.unwrap();

        // Save raw context
        let context = vec![
            serde_json::json!({"role": "user", "content": "hello"}),
            serde_json::json!({"role": "assistant", "content": "Hi there!"}),
        ];
        session::save_raw_context(tmp.path(), &context).await;

        // Load it back
        let (messages, raw_context) = session::load_context(tmp.path()).await;
        assert_eq!(raw_context.len(), 2);
        assert_eq!(messages.len(), 2);
    }

    #[tokio::test]
    async fn load_context_filters_invalid_assistant_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();

        // Create session file
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"created_at":"2026-01-01T00:00:00Z","total_input_tokens":0,"total_output_tokens":0,"max_input_tokens":0}"#,
        ).await.unwrap();

        // Save context with an invalid assistant message (null content, no tool_calls)
        let context = vec![
            serde_json::json!({"role": "user", "content": "hello"}),
            serde_json::json!({"role": "assistant", "content": null, "reasoning_content": "thinking..."}),
            serde_json::json!({"role": "assistant", "content": "Valid reply"}),
        ];
        tokio::fs::write(
            jyc_dir.join("agent-context.json"),
            serde_json::to_string(&context).unwrap(),
        )
        .await
        .unwrap();

        // Load — should filter out the invalid message
        let (messages, raw_context) = session::load_context(tmp.path()).await;
        assert_eq!(raw_context.len(), 2); // user + valid assistant
        assert_eq!(messages.len(), 2);
    }

    #[tokio::test]
    async fn load_context_discards_all_user_only_context() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();

        // Create session file
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"created_at":"2026-01-01T00:00:00Z","total_input_tokens":0,"total_output_tokens":0,"max_input_tokens":0}"#,
        ).await.unwrap();

        // Save context with only user messages (corrupted)
        let context = vec![
            serde_json::json!({"role": "user", "content": "hello"}),
            serde_json::json!({"role": "user", "content": "hello again"}),
        ];
        tokio::fs::write(
            jyc_dir.join("agent-context.json"),
            serde_json::to_string(&context).unwrap(),
        )
        .await
        .unwrap();

        // Load — should return empty (no valid assistant messages)
        let (messages, raw_context) = session::load_context(tmp.path()).await;
        assert!(messages.is_empty());
        assert!(raw_context.is_empty());
    }

    #[tokio::test]
    async fn update_tokens_creates_session_file() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");

        assert!(!jyc_dir.join("agent-session.json").exists());
        session::update_tokens(tmp.path(), 1000, 200, Some(100000), &StubProvider).await;
        assert!(jyc_dir.join("agent-session.json").exists());

        // Verify content
        let content = tokio::fs::read_to_string(jyc_dir.join("agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(state["total_input_tokens"], 1000);
        assert_eq!(state["total_output_tokens"], 200);
        assert_eq!(state["max_input_tokens"], 95000); // 95% of 100000
    }

    #[tokio::test]
    async fn update_tokens_stores_latest_not_accumulated() {
        let tmp = tempfile::tempdir().unwrap();

        // First call
        session::update_tokens(tmp.path(), 1000, 100, Some(100000), &StubProvider).await;
        // Second call — input_tokens should be latest, not accumulated
        session::update_tokens(tmp.path(), 2000, 150, Some(100000), &StubProvider).await;

        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(state["total_input_tokens"], 2000); // Latest, not 3000
        assert_eq!(state["total_output_tokens"], 250); // Accumulated: 100 + 150
    }

    #[tokio::test]
    async fn reset_session_deletes_session_file() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();

        // Create session and context files
        tokio::fs::write(jyc_dir.join("agent-session.json"), "{}")
            .await
            .unwrap();
        tokio::fs::write(jyc_dir.join("agent-context.json"), "[]")
            .await
            .unwrap();

        session::reset_session(tmp.path()).await;

        assert!(!jyc_dir.join("agent-session.json").exists());
        // Context should be summarized (empty in this case = deleted)
        assert!(!jyc_dir.join("agent-context.json").exists());
    }
}

mod tool_registry {
    use jyc_agent::tools::builtin::create_builtin_registry;

    #[test]
    fn builtin_registry_has_all_tools() {
        let registry = create_builtin_registry();
        assert!(registry.has_tool("bash"));
        assert!(registry.has_tool("read"));
        assert!(registry.has_tool("write"));
        assert!(registry.has_tool("edit"));
        assert!(registry.has_tool("glob"));
        assert!(registry.has_tool("grep"));
        assert!(registry.has_tool("webfetch"));
        assert_eq!(registry.len(), 7);
    }

    #[test]
    fn registry_produces_definitions() {
        let registry = create_builtin_registry();
        let definitions = registry.definitions();
        assert_eq!(definitions.len(), 7);

        // Each definition should have name, description, and input_schema
        for def in &definitions {
            assert!(!def.name.is_empty());
            assert!(!def.description.is_empty());
            assert!(def.input_schema.is_object());
        }
    }

    #[test]
    fn registry_unknown_tool_returns_error() {
        let registry = create_builtin_registry();
        assert!(!registry.has_tool("nonexistent"));
    }
}

mod tools {
    use jyc_agent::tools::{Tool, ToolContext, builtin};
    use serde_json::json;
    use std::path::Path;

    fn ctx(path: &Path) -> ToolContext<'_> {
        ToolContext::new(path)
    }

    #[tokio::test]
    async fn bash_requires_command_param() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = builtin::bash::BashTool;
        let result = tool.execute(json!({}), &ctx(tmp.path())).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn bash_executes_simple_command() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = builtin::bash::BashTool;
        let result = tool
            .execute(json!({"command": "echo hello"}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("hello"));
    }

    #[tokio::test]
    async fn bash_reports_error_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = builtin::bash::BashTool;
        let result = tool
            .execute(json!({"command": "false"}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn read_requires_file_path() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = builtin::read::ReadTool;
        let result = tool.execute(json!({}), &ctx(tmp.path())).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn read_file_with_content() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "line1\nline2\nline3").unwrap();
        let tool = builtin::read::ReadTool;
        let result = tool
            .execute(json!({"file_path": "test.txt"}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("1: line1"));
        assert!(result.content.contains("2: line2"));
    }

    #[tokio::test]
    async fn write_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = builtin::write::WriteTool;
        let result = tool
            .execute(
                json!({"file_path": "new.txt", "content": "hello world"}),
                &ctx(tmp.path()),
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("new.txt")).unwrap(),
            "hello world"
        );
    }

    #[tokio::test]
    async fn edit_replaces_text() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file.txt"), "hello world").unwrap();
        let tool = builtin::edit::EditTool;
        let result = tool
            .execute(
                json!({"file_path": "file.txt", "old_string": "hello", "new_string": "goodbye"}),
                &ctx(tmp.path()),
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("file.txt")).unwrap(),
            "goodbye world"
        );
    }

    #[tokio::test]
    async fn edit_fails_when_old_string_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file.txt"), "hello world").unwrap();
        let tool = builtin::edit::EditTool;
        let result = tool
            .execute(
                json!({"file_path": "file.txt", "old_string": "xyz", "new_string": "abc"}),
                &ctx(tmp.path()),
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("not found"));
    }

    #[tokio::test]
    async fn edit_fails_on_multiple_matches() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file.txt"), "aaa bbb aaa").unwrap();
        let tool = builtin::edit::EditTool;
        let result = tool
            .execute(
                json!({"file_path": "file.txt", "old_string": "aaa", "new_string": "ccc"}),
                &ctx(tmp.path()),
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("2 matches"));
    }

    #[tokio::test]
    async fn glob_finds_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "").unwrap();
        std::fs::write(tmp.path().join("b.rs"), "").unwrap();
        std::fs::write(tmp.path().join("c.txt"), "").unwrap();
        let tool = builtin::glob_tool::GlobTool;
        let result = tool
            .execute(json!({"pattern": "*.rs"}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("2 file(s)"));
        assert!(result.content.contains("a.rs"));
        assert!(result.content.contains("b.rs"));
    }

    #[tokio::test]
    async fn grep_finds_matches() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file.rs"), "fn main() {}\nfn helper() {}").unwrap();
        let tool = builtin::grep::GrepTool;
        let result = tool
            .execute(json!({"pattern": "fn \\w+"}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("fn main"));
        assert!(result.content.contains("fn helper"));
    }
}

mod mcp_bridge {
    use jyc_agent::tools::mcp_bridge::ReplyMessageTool;
    use jyc_agent::tools::{Tool, ToolContext};
    use serde_json::json;

    #[tokio::test]
    async fn reply_tool_rejects_empty_message() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();

        let tool = ReplyMessageTool;
        let ctx = ToolContext::new(tmp.path());
        let result = tool.execute(json!({"message": ""}), &ctx).await.unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("empty"));
    }

    #[tokio::test]
    async fn reply_tool_writes_signal_files() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();

        let tool = ReplyMessageTool;
        let ctx = ToolContext::new(tmp.path());
        let result = tool
            .execute(json!({"message": "Hello user!"}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);

        // Verify signal files
        assert!(jyc_dir.join("reply-sent.flag").exists());
        assert!(jyc_dir.join("reply.md").exists());
        assert_eq!(
            std::fs::read_to_string(jyc_dir.join("reply.md")).unwrap(),
            "Hello user!"
        );
    }
}

mod skills {
    use jyc_agent::JycAgentService;
    use jyc_agent::service::{SkillMeta, format_skills_section, parse_skill_frontmatter};
    use jyc_agent::types::AgentConfig;

    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Ensure HOME is set to a temp dir so system-level skills don't leak into tests.
    /// Returns the guard that keeps the override alive.
    static HOME_LOCK: Mutex<()> = Mutex::new(());

    fn with_temp_home(tmp: &std::path::Path, f: impl FnOnce()) {
        let _lock = HOME_LOCK.lock().unwrap();
        let old_home = std::env::var("HOME").ok();
        // Create .config/opencode/skills and .claude/skills in the temp dir
        // but leave them empty so no system skills leak
        std::fs::create_dir_all(tmp.join(".config/opencode/skills")).ok();
        std::fs::create_dir_all(tmp.join(".claude/skills")).ok();
        // SAFETY: guarded by HOME_LOCK mutex and restored after f() returns
        unsafe {
            std::env::set_var("HOME", tmp.as_os_str());
        }
        f();
        if let Some(old) = old_home {
            // SAFETY: restoring original value within same lock scope
            unsafe {
                std::env::set_var("HOME", old);
            }
        }
    }

    /// Helper: create a JycAgentService with a specific workdir.
    fn make_service(workdir: PathBuf) -> JycAgentService {
        JycAgentService::new(AgentConfig::default(), workdir, vec![], vec![], None, None)
    }

    #[test]
    fn no_skills_dir_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        with_temp_home(tmp.path(), || {
            let svc = make_service(tmp.path().to_path_buf());
            let skills = svc.discover_skills(tmp.path());
            assert!(skills.is_empty());
        });
    }

    #[test]
    fn single_skill_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        // Create .jyc/skills/test-skill/SKILL.md
        let skill_dir = tmp.path().join(".jyc/skills/test-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: test-skill\ndescription: A test skill\n---\n\n# Full content here\n",
        )
        .unwrap();

        with_temp_home(tmp.path(), || {
            let svc = make_service(tmp.path().to_path_buf());
            let skills = svc.discover_skills(tmp.path());
            assert_eq!(skills.len(), 1);
            assert_eq!(skills[0].name, "test-skill");
            assert_eq!(skills[0].description, "A test skill");
            assert!(skills[0].source_path.ends_with("test-skill"));
        });
    }

    #[test]
    fn empty_skills_dir_handled() {
        let tmp = tempfile::tempdir().unwrap();
        // Create the directory but leave it empty
        std::fs::create_dir_all(tmp.path().join(".jyc/skills")).unwrap();

        with_temp_home(tmp.path(), || {
            let svc = make_service(tmp.path().to_path_buf());
            let skills = svc.discover_skills(tmp.path());
            assert!(skills.is_empty());
        });
    }

    #[test]
    fn malformed_skill_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        // Create a valid skill
        let good_dir = tmp.path().join(".jyc/skills/good-skill");
        std::fs::create_dir_all(&good_dir).unwrap();
        std::fs::write(
            good_dir.join("SKILL.md"),
            "---\nname: good-skill\ndescription: Valid\n---\n",
        )
        .unwrap();

        // Create a malformed skill (no frontmatter)
        let bad_dir = tmp.path().join(".jyc/skills/bad-skill");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(bad_dir.join("SKILL.md"), "Just some text, no frontmatter").unwrap();

        with_temp_home(tmp.path(), || {
            let svc = make_service(tmp.path().to_path_buf());
            let skills = svc.discover_skills(tmp.path());
            assert_eq!(skills.len(), 1);
            assert_eq!(skills[0].name, "good-skill");
        });
    }

    #[test]
    fn same_name_priority() {
        let tmp = tempfile::tempdir().unwrap();
        // Create .claude/skills/my-skill/ (lower priority — scanned earlier)
        let claude_dir = tmp.path().join(".claude/skills/my-skill");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(
            claude_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: From claude\n---\n",
        )
        .unwrap();

        // Create .jyc/skills/my-skill/ (higher priority — scanned later, overwrites)
        let jyc_dir = tmp.path().join(".jyc/skills/my-skill");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(
            jyc_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: From JYC (overrides)\n---\n",
        )
        .unwrap();

        with_temp_home(tmp.path(), || {
            let svc = make_service(tmp.path().to_path_buf());
            let skills = svc.discover_skills(tmp.path());
            assert_eq!(skills.len(), 1);
            // Should take the .jyc version (higher priority)
            assert_eq!(skills[0].description, "From JYC (overrides)");
        });
    }

    #[test]
    fn multi_path_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        // Skill 1 in .jyc/skills/
        let d1 = tmp.path().join(".jyc/skills/skill-one");
        std::fs::create_dir_all(&d1).unwrap();
        std::fs::write(
            d1.join("SKILL.md"),
            "---\nname: skill-one\ndescription: One\n---\n",
        )
        .unwrap();

        // Skill 2 in repo/.opencode/skills/
        let d2 = tmp.path().join("repo/.opencode/skills/skill-two");
        std::fs::create_dir_all(&d2).unwrap();
        std::fs::write(
            d2.join("SKILL.md"),
            "---\nname: skill-two\ndescription: Two\n---\n",
        )
        .unwrap();

        with_temp_home(tmp.path(), || {
            let svc = make_service(tmp.path().to_path_buf());
            let skills = svc.discover_skills(tmp.path());
            assert_eq!(skills.len(), 2);
            let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
            assert!(names.contains(&"skill-one"));
            assert!(names.contains(&"skill-two"));
        });
    }

    #[test]
    fn format_includes_path() {
        let meta = SkillMeta {
            name: "test-skill".to_string(),
            description: "A test skill".to_string(),
            source_path: PathBuf::from("/some/path/to/test-skill"),
        };
        let section = format_skills_section(&[meta]);
        assert!(section.contains("(at /some/path/to/test-skill)"));
        assert!(section.contains("**test-skill**"));
        assert!(section.contains("A test skill"));
        assert!(section.contains("## Available Skills"));
        assert!(section.contains("read <skill-path>/SKILL.md"));
    }

    #[test]
    fn format_section_empty_returns_empty_string() {
        let section = format_skills_section(&[]);
        assert!(section.is_empty());
    }

    #[test]
    fn parse_frontmatter_valid() {
        let content =
            "---\nname: my-skill\ndescription: Does something useful\n---\n\nBody text here";
        let meta = parse_skill_frontmatter(content).unwrap();
        assert_eq!(meta.name, "my-skill");
        assert_eq!(meta.description, "Does something useful");
    }

    #[test]
    fn parse_frontmatter_no_delimiter_returns_none() {
        assert!(parse_skill_frontmatter("no frontmatter here").is_none());
    }

    #[test]
    fn parse_frontmatter_missing_name_returns_none() {
        assert!(parse_skill_frontmatter("---\ndescription: desc\n---\n").is_none());
    }

    #[test]
    fn parse_frontmatter_missing_description_returns_none() {
        assert!(parse_skill_frontmatter("---\nname: n\n---\n").is_none());
    }

    #[test]
    fn parse_frontmatter_empty_values_returns_none() {
        assert!(parse_skill_frontmatter("---\nname: \ndescription: d\n---\n").is_none());
        assert!(parse_skill_frontmatter("---\nname: n\ndescription: \n---\n").is_none());
    }

    #[test]
    fn parse_frontmatter_block_scalar_pipe() {
        // Multi-line description using YAML block scalar |
        let content = "---\nname: my-skill\ndescription: |\n  Line one\n  Line two\n---\n\nBody";
        let meta = parse_skill_frontmatter(content).unwrap();
        assert_eq!(meta.name, "my-skill");
        assert_eq!(meta.description, "Line one Line two");
    }

    #[test]
    fn parse_frontmatter_block_scalar_greater_than() {
        // Folded block scalar >
        let content = "---\nname: fs\ndescription: >\n  Folded line one\n  Folded line two\n---\n";
        let meta = parse_skill_frontmatter(content).unwrap();
        assert_eq!(meta.name, "fs");
        assert_eq!(meta.description, "Folded line one Folded line two");
    }

    #[test]
    fn parse_frontmatter_block_scalar_empty_returns_none() {
        // Block scalar with no content lines → empty description → None
        let content = "---\nname: n\ndescription: |\n---\n";
        assert!(parse_skill_frontmatter(content).is_none());
    }
}

mod max_iterations {
    use jyc_agent::types::AgentConfig;

    #[test]
    fn default_is_500() {
        let cfg = AgentConfig::default();
        assert_eq!(cfg.max_iterations, 500);
    }

    #[test]
    fn deserializes_from_toml_default() {
        // No max_iterations in TOML → default 500 (raised from 200 in v0.3.6
        // because in-loop summarization at the cycle boundary now keeps the
        // request size bounded regardless of iteration count).
        let toml = r#"
            model = "anthropic/claude-3"
        "#;
        let cfg: AgentConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.max_iterations, 500);
    }

    #[test]
    fn deserializes_explicit_value() {
        let toml = r#"
            model = "anthropic/claude-3"
            max_iterations = 1000
        "#;
        let cfg: AgentConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.max_iterations, 1000);
    }
}

mod small_model {
    use jyc_agent::types::AgentConfig;

    #[test]
    fn default_is_none() {
        let cfg = AgentConfig::default();
        assert!(cfg.small_model.is_none());
    }

    #[test]
    fn deserializes_when_absent() {
        // No small_model in TOML → None.
        let toml = r#"
            model = "anthropic/claude-3"
        "#;
        let cfg: AgentConfig = toml::from_str(toml).unwrap();
        assert!(cfg.small_model.is_none());
    }

    #[test]
    fn deserializes_when_present() {
        let toml = r#"
            model = "deepseek/deepseek-v4-pro"
            small_model = "deepseek/deepseek-v4-flash"
        "#;
        let cfg: AgentConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.small_model.as_deref(),
            Some("deepseek/deepseek-v4-flash"),
        );
    }
}
