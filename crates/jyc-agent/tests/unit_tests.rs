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
    fn drops_assistant_with_tool_calls_missing_results() {
        // Assistant tool_calls without matching tool results are dangling and
        // cannot be replayed; they are dropped by filter_valid_messages.
        let messages = vec![json!({"role": "assistant", "content": null, "tool_calls": [
            {"id": "1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
        ]})];
        let result = filter_valid_messages(&messages);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn drops_assistant_with_reasoning_and_tool_calls_missing_results() {
        // reasoning_content is preserved on valid assistant turns, but a tool
        // call with no matching result is still dangling and is dropped.
        let messages = vec![
            json!({"role": "assistant", "content": null, "reasoning_content": "thinking...", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
            ]}),
        ];
        let result = filter_valid_messages(&messages);
        assert_eq!(result.len(), 0);
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
    fn drops_anthropic_assistant_with_tool_use_block_missing_results() {
        // Anthropic tool_use blocks without matching tool_result blocks are
        // dangling and are dropped.
        let messages = vec![json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "1", "name": "bash", "input": {}}
        ]})];
        let result = filter_valid_messages(&messages);
        assert_eq!(result.len(), 0);
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
    use jyc_types::channel::ResetCompressionConfig;

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
            r#"{"created_at":"2026-01-01T00:00:00Z","context_input_tokens":0,"total_output_tokens":0,"max_input_tokens":0}"#,
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
            r#"{"created_at":"2026-01-01T00:00:00Z","context_input_tokens":0,"total_output_tokens":0,"max_input_tokens":0}"#,
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
            r#"{"created_at":"2026-01-01T00:00:00Z","context_input_tokens":0,"total_output_tokens":0,"max_input_tokens":0}"#,
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
        session::update_tokens(
            tmp.path(),
            1000,
            1000,
            200,
            0,
            0,
            Some(100000),
            &StubProvider,
            0.95,
            &ResetCompressionConfig::default(),
            None,
        )
        .await;
        assert!(jyc_dir.join("agent-session.json").exists());

        // Verify content
        let content = tokio::fs::read_to_string(jyc_dir.join("agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(state["context_input_tokens"], 1000);
        assert_eq!(state["total_input_tokens"], 1000);
        assert_eq!(state["total_output_tokens"], 200);
        assert_eq!(state["total_cache_hit_tokens"], 0);
        assert_eq!(state["max_input_tokens"], 95000); // 95% of 100000
    }

    /// `context_input_tokens` and `total_input_tokens` are stored as passed
    /// in (= latest call's value, since each call already includes full
    /// context). `total_output_tokens` is also stored as passed in (= the
    /// running total accumulated by the caller). No += accumulation
    /// happens inside `update_tokens` itself.
    #[tokio::test]
    async fn update_tokens_stores_latest_not_accumulated() {
        let tmp = tempfile::tempdir().unwrap();

        // First call
        session::update_tokens(
            tmp.path(),
            1000,
            1000,
            100,
            0,
            0,
            Some(100000),
            &StubProvider,
            0.95,
            &ResetCompressionConfig::default(),
            None,
        )
        .await;
        // Second call — caller has accumulated locally. For output, the
        // running total after call 2 is 100 + 150 = 250, which is what
        // gets passed in (not just the per-call delta of 150).
        session::update_tokens(
            tmp.path(),
            2000,
            2000,
            250,
            0,
            0,
            Some(100000),
            &StubProvider,
            0.95,
            &ResetCompressionConfig::default(),
            None,
        )
        .await;

        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(state["context_input_tokens"], 2000); // Latest, not 3000
        assert_eq!(state["total_input_tokens"], 2000); // Latest, not 3000
        assert_eq!(state["total_output_tokens"], 250); // Caller's running sum
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

        use jyc_types::channel::{CompressionMode, ResetCompressionConfig};
        let config = ResetCompressionConfig {
            mode: CompressionMode::Heuristic,
            keep_pairs: 3,
        };
        session::reset_session(tmp.path(), &config, None, None).await;

        assert!(!jyc_dir.join("agent-session.json").exists());
        // Context should be summarized (empty in this case = deleted)
        assert!(!jyc_dir.join("agent-context.json").exists());
    }

    /// `update_tokens` post-loop auto-reset now honors `reset_compression.mode = None`.
    /// Previously it inlined an LLM call; now it goes through `reset_session`.
    #[tokio::test]
    async fn update_tokens_auto_reset_with_none_mode_deletes_files() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();

        // Session with tokens already over the (small) max — will trigger
        // auto-reset on the very next update_tokens call.
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"created_at":"2026-01-01","context_input_tokens":5000,"total_output_tokens":100,"max_input_tokens":1000}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(jyc_dir.join("agent-context.json"), "[]")
            .await
            .unwrap();

        let config = ResetCompressionConfig {
            mode: jyc_types::channel::CompressionMode::None,
            keep_pairs: 3,
        };
        session::update_tokens(
            tmp.path(),
            6000, // still over 1000 → auto-reset fires
            6000,
            50,
            0,
            0,
            Some(1000),
            &StubProvider,
            0.95,
            &config,
            None,
        )
        .await;

        // With mode=None, the session file is deleted by reset_session
        // then recreated by update_tokens with zeroed counters. The
        // `output_tokens` from THIS call is also discarded because the
        // reset path explicitly zeros all counters before save.
        let content = tokio::fs::read_to_string(jyc_dir.join("agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(state["context_input_tokens"], 0);
        assert_eq!(state["total_input_tokens"], 0);
        assert_eq!(state["total_output_tokens"], 0);
        assert_eq!(state["total_cache_hit_tokens"], 0);
        assert_eq!(state["max_input_tokens"], 950); // 0.95 * 1000 from the call's context_window
    }

    /// Pre-check compaction: when the loaded session's tokens exceed the new
    /// (smaller) context window, the session is reset using the configured
    /// compression strategy before the agent loop runs.
    #[tokio::test]
    async fn maybe_reset_for_new_context_resets_when_oversized() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();

        // Loaded session: 600k tokens, max_input_tokens irrelevant here
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"created_at":"2026-01-01","context_input_tokens":600000,"total_output_tokens":0,"max_input_tokens":950000}"#,
        )
        .await
        .unwrap();
        // Some context to compact
        tokio::fs::write(jyc_dir.join("agent-context.json"), "[]")
            .await
            .unwrap();

        let config = ResetCompressionConfig {
            mode: jyc_types::channel::CompressionMode::None,
            keep_pairs: 3,
        };
        let reset = session::maybe_reset_for_new_context(
            tmp.path(),
            250_000, // new max for build model (256k * ~0.95 ≈ 243k; 250k close enough)
            &config,
            None,
            None,
        )
        .await;
        assert!(reset, "should have triggered reset");

        // With mode=None the session file is deleted, then re-created by
        // the next call to update_tokens. Here we just assert that the
        // reset_session path ran (context deleted, session gone or reset).
        // The exact on-disk state depends on whether anyone re-saves the
        // session; reset_session itself deletes it.
        assert!(!jyc_dir.join("agent-context.json").exists());
    }

    /// Pre-check is a no-op when the loaded session fits the new window.
    #[tokio::test]
    async fn maybe_reset_for_new_context_is_noop_when_under_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();

        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"created_at":"2026-01-01","context_input_tokens":100000,"total_output_tokens":0,"max_input_tokens":950000}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(jyc_dir.join("agent-context.json"), "[]")
            .await
            .unwrap();

        let config = ResetCompressionConfig::default();
        let reset =
            session::maybe_reset_for_new_context(tmp.path(), 250_000, &config, None, None).await;
        assert!(!reset, "should not have triggered reset");

        // Both files unchanged
        let session = tokio::fs::read_to_string(jyc_dir.join("agent-session.json"))
            .await
            .unwrap();
        let context = tokio::fs::read_to_string(jyc_dir.join("agent-context.json"))
            .await
            .unwrap();
        assert!(session.contains("\"context_input_tokens\":100000"));
        assert_eq!(context, "[]");
    }

    /// Pre-check is a no-op when `new_max_input_tokens == 0` (caller didn't
    /// pass a context_window — fall back to the post-loop auto-reset).
    #[tokio::test]
    async fn maybe_reset_for_new_context_zero_max_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();

        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"created_at":"2026-01-01","context_input_tokens":999999,"total_output_tokens":0,"max_input_tokens":0}"#,
        )
        .await
        .unwrap();

        let reset = session::maybe_reset_for_new_context(
            tmp.path(),
            0,
            &ResetCompressionConfig::default(),
            None,
            None,
        )
        .await;
        assert!(!reset);
        assert!(jyc_dir.join("agent-session.json").exists());
    }

    /// `persist_tokens` must write the latest input/output counts but NEVER
    /// trigger `reset_session`, even when input crosses the auto-reset
    /// threshold. Mid-loop the in-memory `raw_context` is the source of
    /// truth; the on-disk `agent-context.json` must be left alone until the
    /// post-loop `update_tokens` runs.
    #[tokio::test]
    async fn persist_tokens_does_not_trigger_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");

        // Seed a context file the reset would otherwise delete.
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        let context_before = r#"[{"role":"user","content":"seed"}]"#;
        tokio::fs::write(jyc_dir.join("agent-context.json"), context_before)
            .await
            .unwrap();

        // Call persist_tokens with input well above the 95% threshold.
        session::persist_tokens(
            tmp.path(),
            100_000,
            100_000,
            200,
            0,
            0,
            Some(10_000),
            0.95,
            0.0,
        )
        .await;

        // Context file untouched.
        let context_after = tokio::fs::read_to_string(jyc_dir.join("agent-context.json"))
            .await
            .unwrap();
        assert_eq!(context_after, context_before);

        // Session file got the new values.
        let session = tokio::fs::read_to_string(jyc_dir.join("agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&session).unwrap();
        assert_eq!(state["context_input_tokens"], 100_000);
        assert_eq!(state["total_output_tokens"], 200);
        assert_eq!(state["max_input_tokens"], 9500);
    }

    /// Output tokens accumulate upstream in the agent_loop accumulator,
    /// which sums each call's `output_tokens` and passes the running
    /// total here. `persist_tokens` stores it as-is (assignment).
    /// This test verifies the data flow: the on-disk value reflects the
    /// running total passed in (= 330 across three calls of 100+150+80).
    #[tokio::test]
    async fn persist_tokens_stores_total_output_as_passed() {
        let tmp = tempfile::tempdir().unwrap();

        // Simulate three LLM calls with per-call output 100, 150, 80.
        // agent_loop accumulates locally: 100, 250, 330. Each running
        // total is passed into persist_tokens.
        session::persist_tokens(tmp.path(), 1000, 1000, 100, 0, 0, None, 0.95, 0.0).await;
        session::persist_tokens(tmp.path(), 1500, 1500, 250, 0, 0, None, 0.95, 0.0).await;
        session::persist_tokens(tmp.path(), 2000, 2000, 330, 0, 0, None, 0.95, 0.0).await;

        let session = tokio::fs::read_to_string(tmp.path().join(".jyc/agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&session).unwrap();
        assert_eq!(state["context_input_tokens"], 2000);
        assert_eq!(state["total_input_tokens"], 2000);
        assert_eq!(state["total_output_tokens"], 330);
    }

    /// `total_input_tokens` is stored as passed in (assignment, not `+=`).
    /// The accumulation happens upstream in the agent_loop accumulator,
    /// which sums each call's `input_tokens` (= full context size) and
    /// passes the running total here. This test verifies that the
    /// passed-in sum is what ends up on disk across multiple calls.
    #[tokio::test]
    async fn persist_tokens_stores_total_input_as_passed() {
        let tmp = tempfile::tempdir().unwrap();

        // Simulate three LLM calls with per-call `input_tokens` of
        // 1000, 2000, 3000 — agent_loop sums them to running totals of
        // 1000, 3000, 6000 and passes each running total to persist_tokens.
        // The on-disk value reflects the latest passed-in sum (= 6000).
        session::persist_tokens(tmp.path(), 1000, 1000, 0, 0, 0, None, 0.95, 0.0).await;
        session::persist_tokens(tmp.path(), 2000, 3000, 0, 0, 0, None, 0.95, 0.0).await;
        session::persist_tokens(tmp.path(), 3000, 6000, 0, 0, 0, None, 0.95, 0.0).await;

        let session = tokio::fs::read_to_string(tmp.path().join(".jyc/agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&session).unwrap();
        assert_eq!(state["context_input_tokens"], 3000);
        assert_eq!(state["total_input_tokens"], 6000);
    }

    /// Mirrors the actual agent_loop accumulation pattern: per call,
    /// `total_input_tokens += response.input_tokens` and
    /// `total_output_tokens += response.output_tokens`. Verifies that
    /// the += accumulation is what produces the value that ends up in
    /// agent-session.json after a round of multiple LLM calls.
    #[tokio::test]
    async fn agent_loop_token_accumulation_pattern() {
        let tmp = tempfile::tempdir().unwrap();

        // Simulate three LLM calls as agent_loop would accumulate them.
        let mut context_input_tokens: u64 = 0;
        let mut total_input_tokens: u64 = 0;
        let mut total_output_tokens: u64 = 0;

        for (per_call_input, per_call_output) in [(1000, 100), (1500, 150), (2000, 80)] {
            if per_call_input > 0 {
                context_input_tokens = per_call_input;
            }
            total_input_tokens += per_call_input;
            total_output_tokens += per_call_output;

            session::persist_tokens(
                tmp.path(),
                context_input_tokens,
                total_input_tokens,
                total_output_tokens,
                0, // no cache hits exercised in this loop-pattern test
                0, // no cache writes exercised in this loop-pattern test
                None,
                0.95,
                0.0, // cost not exercised here — see session_cost tests
            )
            .await;
        }

        let session = tokio::fs::read_to_string(tmp.path().join(".jyc/agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&session).unwrap();

        // context_input_tokens = latest call's input (current context size)
        assert_eq!(state["context_input_tokens"], 2000);
        // total_input_tokens = sum across all three calls: 1000 + 1500 + 2000
        assert_eq!(state["total_input_tokens"], 4500);
        // total_output_tokens = sum across all three calls: 100 + 150 + 80
        assert_eq!(state["total_output_tokens"], 330);
    }

    /// `session_cost` is the ONE accumulating field: unlike the token
    /// fields (which are assigned from the caller's running total), each
    /// call's cost is added to the previous value.
    #[tokio::test]
    async fn session_cost_accumulates_across_calls() {
        let tmp = tempfile::tempdir().unwrap();

        session::persist_tokens(tmp.path(), 1000, 1000, 100, 0, 0, None, 0.95, 0.25).await;
        session::persist_tokens(tmp.path(), 1500, 2500, 200, 0, 0, None, 0.95, 0.10).await;
        session::persist_tokens(tmp.path(), 2000, 4500, 300, 0, 0, None, 0.95, 0.05).await;

        let session = tokio::fs::read_to_string(tmp.path().join(".jyc/agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&session).unwrap();

        // 0.25 + 0.10 + 0.05 = 0.40 (accumulated, not replaced by 0.05)
        let cost = state["session_cost"].as_f64().unwrap();
        assert!((cost - 0.40).abs() < 1e-9, "got {cost}");
    }

    /// A zero `call_cost` (unpriced model) leaves `session_cost` untouched
    /// rather than resetting it.
    #[tokio::test]
    async fn zero_call_cost_preserves_existing_session_cost() {
        let tmp = tempfile::tempdir().unwrap();

        session::persist_tokens(tmp.path(), 1000, 1000, 100, 0, 0, None, 0.95, 0.75).await;
        session::persist_tokens(tmp.path(), 1500, 2500, 200, 0, 0, None, 0.95, 0.0).await;

        let session = tokio::fs::read_to_string(tmp.path().join(".jyc/agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&session).unwrap();

        let cost = state["session_cost"].as_f64().unwrap();
        assert!((cost - 0.75).abs() < 1e-9, "got {cost}");
    }

    /// Session files written before this feature have no `session_cost`
    /// key. `serde(default)` must make them load as 0.0 rather than
    /// failing to deserialize (which would silently drop token history).
    #[tokio::test]
    async fn legacy_session_file_without_cost_field_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc).await.unwrap();
        tokio::fs::write(
            jyc.join("agent-session.json"),
            r#"{"created_at":"2026-01-01T00:00:00+00:00","context_input_tokens":500,
                "total_input_tokens":500,"total_output_tokens":50,
                "total_cache_hit_tokens":0,"max_input_tokens":95000}"#,
        )
        .await
        .unwrap();

        // Adding cost to a legacy file starts from 0.0.
        session::persist_tokens(tmp.path(), 600, 1100, 90, 0, 0, None, 0.95, 0.30).await;

        let session = tokio::fs::read_to_string(jyc.join("agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&session).unwrap();

        let cost = state["session_cost"].as_f64().unwrap();
        assert!((cost - 0.30).abs() < 1e-9, "got {cost}");
        // Pre-existing token data survived the load.
        assert_eq!(state["total_input_tokens"], 1100);
    }

    /// `ensure_session_file` must create `agent-session.json` for a brand-new
    /// topic that has none, with `max_input_tokens = context_window *
    /// auto_reset_threshold`, zeroed counters, and a populated `created_at`.
    #[tokio::test]
    async fn ensure_session_file_creates_file_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = tmp.path().join(".jyc/agent-session.json");

        assert!(!session_path.exists());

        session::ensure_session_file(tmp.path(), Some(100_000), 0.95).await;

        assert!(session_path.exists());

        let content = tokio::fs::read_to_string(&session_path).await.unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        // Zeroed counters so the dashboard shows "0 / 95000 (0%)" instead
        // of (None, None, None).
        assert_eq!(state["context_input_tokens"], 0);
        assert_eq!(state["total_output_tokens"], 0);
        // max_input_tokens = 100000 * 0.95 = 95000
        assert_eq!(state["max_input_tokens"], 95_000);
        // created_at must be populated so the dashboard can show session age.
        assert!(
            state["created_at"].as_str().is_some_and(|s| !s.is_empty()),
            "created_at should be set when the file is freshly created"
        );
    }

    /// `ensure_session_file` must NOT touch an existing session file — its
    /// only job is to create the file when missing. Existing token counts,
    /// `max_input_tokens`, and `created_at` must be preserved verbatim.
    #[tokio::test]
    async fn ensure_session_file_skips_when_file_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();

        let original = r#"{"created_at":"2026-01-01T00:00:00Z","context_input_tokens":42000,"total_output_tokens":7000,"max_input_tokens":123000}"#;
        let session_path = jyc_dir.join("agent-session.json");
        tokio::fs::write(&session_path, original).await.unwrap();

        // Even with a different context_window / threshold, the file must
        // remain untouched.
        session::ensure_session_file(tmp.path(), Some(999_999), 0.5).await;

        let after = tokio::fs::read_to_string(&session_path).await.unwrap();
        assert_eq!(after, original);
    }

    /// When `context_window` is `None`, `ensure_session_file` still creates
    /// the file but leaves `max_input_tokens` at 0 (the post-loop
    /// `update_tokens` will fill it in on the next turn).
    #[tokio::test]
    async fn ensure_session_file_creates_with_no_context_window() {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = tmp.path().join(".jyc/agent-session.json");

        assert!(!session_path.exists());

        session::ensure_session_file(tmp.path(), None, 0.95).await;

        assert!(session_path.exists());

        let content = tokio::fs::read_to_string(&session_path).await.unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(state["context_input_tokens"], 0);
        assert_eq!(state["total_output_tokens"], 0);
        assert_eq!(state["max_input_tokens"], 0);
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
        assert!(registry.has_tool("job_list"));
        assert!(registry.has_tool("job_create"));
        assert!(registry.has_tool("job_delete"));
        assert!(registry.has_tool("job_toggle"));
        assert!(registry.has_tool("jyc_send_to_topic"));
        assert_eq!(registry.len(), 12);
    }

    #[test]
    fn registry_produces_definitions() {
        let registry = create_builtin_registry();
        let definitions = registry.definitions();
        assert_eq!(definitions.len(), 12);

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

    #[tokio::test]
    async fn send_to_topic_attachment_has_content() {
        // Verify that MessageAttachment produced by the send_to_topic
        // pattern (std::fs::read → content: Some(bytes)) has non-None
        // content, so save_attachments_to_dir will not skip it.
        let tmp = tempfile::tempdir().unwrap();

        // Write a test file the same way send_to_topic's execute() does
        let test_data = b"hello attachment world";
        let file_path = tmp.path().join("test.pdf");
        std::fs::write(&file_path, test_data).unwrap();

        let size = std::fs::metadata(&file_path)
            .map(|m| m.len() as usize)
            .unwrap_or(0);
        let file_bytes = std::fs::read(&file_path).ok();

        let attachment = jyc_types::MessageAttachment {
            filename: "test.pdf".to_string(),
            content_type: "application/octet-stream".to_string(),
            size,
            content: file_bytes,
            saved_path: None,
        };

        assert!(
            attachment.content.is_some(),
            "Attachment content should be Some(bytes), not None"
        );
        assert_eq!(attachment.content.as_deref().unwrap(), test_data);
    }

    #[test]
    fn send_to_topic_schema_includes_require_reply() {
        let tool = builtin::send_to_topic::SendToThreadTool;
        let schema = tool.input_schema();
        let props = schema["properties"].as_object().unwrap();
        assert!(
            props.contains_key("require_reply"),
            "schema should include require_reply parameter"
        );
        assert_eq!(props["require_reply"]["type"], "boolean");
    }

    #[test]
    fn send_to_topic_description_mentions_require_reply() {
        let tool = builtin::send_to_topic::SendToThreadTool;
        assert!(
            tool.description().contains("require_reply"),
            "description should mention require_reply"
        );
    }
}

mod mcp_bridge {
    use jyc_agent::tools::mcp_bridge::{ReplyMessageTool, SendMessageTool};
    use jyc_agent::tools::{Tool, ToolContext};
    use jyc_types::{InboundMessage, OutboundAdapter, OutboundAttachment, SendResult};
    use serde_json::json;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

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

    /// Mock outbound adapter that records send_message and send_message_with_attachments calls.
    #[allow(clippy::type_complexity)]
    struct MockOutbound {
        calls: Arc<Mutex<Vec<(String, String, String)>>>,
        attachment_calls: Arc<Mutex<Vec<(String, String, String, Vec<String>)>>>,
    }

    impl MockOutbound {
        #[allow(clippy::type_complexity)]
        fn new() -> (
            Self,
            Arc<Mutex<Vec<(String, String, String)>>>,
            Arc<Mutex<Vec<(String, String, String, Vec<String>)>>>,
        ) {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let attachment_calls = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    calls: calls.clone(),
                    attachment_calls: attachment_calls.clone(),
                },
                calls,
                attachment_calls,
            )
        }
    }

    #[async_trait::async_trait]
    impl OutboundAdapter for MockOutbound {
        fn channel_type(&self) -> &str {
            "mock"
        }

        async fn connect(&self) -> anyhow::Result<()> {
            Ok(())
        }

        async fn disconnect(&self) -> anyhow::Result<()> {
            Ok(())
        }

        fn clean_body(&self, body: &str) -> String {
            body.to_string()
        }

        async fn send_reply(
            &self,
            _original: &InboundMessage,
            _reply_text: &str,
            _topic_path: &Path,
            _message_dir: &str,
            _attachments: Option<&[OutboundAttachment]>,
        ) -> anyhow::Result<SendResult> {
            Ok(SendResult {
                message_id: "mock-reply".to_string(),
            })
        }

        async fn send_message(
            &self,
            recipient: &str,
            subject: &str,
            body: &str,
        ) -> anyhow::Result<SendResult> {
            self.calls.lock().unwrap().push((
                recipient.to_string(),
                subject.to_string(),
                body.to_string(),
            ));
            Ok(SendResult {
                message_id: "mock-msg".to_string(),
            })
        }

        async fn send_message_with_attachments(
            &self,
            recipient: &str,
            subject: &str,
            body: &str,
            attachments: Option<&[OutboundAttachment]>,
        ) -> anyhow::Result<SendResult> {
            let att_filenames: Vec<String> = attachments
                .unwrap_or_default()
                .iter()
                .map(|a| a.filename.clone())
                .collect();
            self.attachment_calls.lock().unwrap().push((
                recipient.to_string(),
                subject.to_string(),
                body.to_string(),
                att_filenames,
            ));
            Ok(SendResult {
                message_id: "mock-attachment-msg".to_string(),
            })
        }
    }

    #[tokio::test]
    async fn send_message_rejects_empty_recipient() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SendMessageTool;
        let ctx = ToolContext::new(tmp.path());
        let result = tool
            .execute(json!({"recipient": "", "message": "hi"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("Recipient cannot be empty"));
    }

    #[tokio::test]
    async fn send_message_rejects_empty_message() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SendMessageTool;
        let ctx = ToolContext::new(tmp.path());
        let result = tool
            .execute(
                json!({"recipient": "user@example.com", "message": ""}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("Message cannot be empty"));
    }

    #[tokio::test]
    async fn send_message_requires_outbound() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SendMessageTool;
        let ctx = ToolContext::new(tmp.path());
        let result = tool
            .execute(
                json!({"recipient": "user@example.com", "message": "hello"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("No outbound adapter available"));
    }

    #[tokio::test]
    async fn send_message_sends_via_outbound() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SendMessageTool;
        let (mock, calls, _attachment_calls) = MockOutbound::new();
        let mut ctx = ToolContext::new(tmp.path());
        ctx.outbound = Some(Arc::new(mock));

        let result = tool
            .execute(
                json!({
                    "recipient": "wecomkf:kf001:user123",
                    "subject": "Alert",
                    "message": "System is down"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("wecomkf:kf001:user123"));
        assert!(result.content.contains("mock-msg"));

        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, "wecomkf:kf001:user123");
        assert_eq!(recorded[0].1, "Alert");
        assert_eq!(recorded[0].2, "System is down");
    }

    #[tokio::test]
    async fn send_message_with_channel_cross_channel() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SendMessageTool;
        let (mock, calls, _attachment_calls) = MockOutbound::new();
        let mut ctx = ToolContext::new(tmp.path());
        // Set up an outbounds map with the mock for channel "email"
        let mut outbounds_map: std::collections::HashMap<String, Arc<dyn OutboundAdapter>> =
            std::collections::HashMap::new();
        outbounds_map.insert("email".to_string(), Arc::new(mock));
        ctx.outbounds = Some(Arc::new(tokio::sync::Mutex::new(outbounds_map)));

        let result = tool
            .execute(
                json!({
                    "channel": "email",
                    "recipient": "user@example.com",
                    "subject": "Cross-channel alert",
                    "message": "Hello from another channel"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("user@example.com"));
        assert!(result.content.contains("mock-msg"));

        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, "user@example.com");
        assert_eq!(recorded[0].1, "Cross-channel alert");
    }

    #[tokio::test]
    async fn send_message_rejects_unknown_channel() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SendMessageTool;
        let (mock, _calls, _attachment_calls) = MockOutbound::new();
        let mut ctx = ToolContext::new(tmp.path());
        let mut outbounds_map: std::collections::HashMap<String, Arc<dyn OutboundAdapter>> =
            std::collections::HashMap::new();
        outbounds_map.insert("email".to_string(), Arc::new(mock));
        ctx.outbounds = Some(Arc::new(tokio::sync::Mutex::new(outbounds_map)));

        let result = tool
            .execute(
                json!({
                    "channel": "nonexistent",
                    "recipient": "user@example.com",
                    "message": "hello"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("Unknown channel"));
        assert!(result.content.contains("nonexistent"));
    }

    #[tokio::test]
    async fn send_message_rejects_missing_outbounds_map() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SendMessageTool;
        let mut ctx = ToolContext::new(tmp.path());
        // outbounds is None (not configured)
        ctx.outbounds = None;

        let result = tool
            .execute(
                json!({
                    "channel": "email",
                    "recipient": "user@example.com",
                    "message": "hello"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("Cross-channel messaging is not available")
        );
    }

    #[tokio::test]
    async fn send_message_with_attachments_success() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SendMessageTool;
        let (mock, _calls, attachment_calls) = MockOutbound::new();
        let mut ctx = ToolContext::new(tmp.path());
        ctx.outbound = Some(Arc::new(mock));

        // Create a test attachment file
        let file_path = tmp.path().join("report.pdf");
        tokio::fs::write(&file_path, b"pdf content").await.unwrap();

        let result = tool
            .execute(
                json!({
                    "recipient": "user@example.com",
                    "subject": "Report",
                    "message": "Here is your report",
                    "attachments": ["report.pdf"]
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("user@example.com"));
        assert!(result.content.contains("mock-attachment-msg"));
        assert!(result.content.contains("1 attachment(s)"));

        let recorded = attachment_calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, "user@example.com");
        assert_eq!(recorded[0].1, "Report");
        assert_eq!(recorded[0].3, vec!["report.pdf"]);
    }

    #[tokio::test]
    async fn send_message_attachment_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SendMessageTool;
        let (mock, _calls, _attachment_calls) = MockOutbound::new();
        let mut ctx = ToolContext::new(tmp.path());
        ctx.outbound = Some(Arc::new(mock));

        let result = tool
            .execute(
                json!({
                    "recipient": "user@example.com",
                    "message": "Here is your report",
                    "attachments": ["nonexistent.pdf"]
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("Attachment not found"));
        assert!(result.content.contains("nonexistent.pdf"));
    }

    #[tokio::test]
    async fn send_message_cross_channel_with_attachments() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SendMessageTool;
        let (mock, _calls, attachment_calls) = MockOutbound::new();
        let mut ctx = ToolContext::new(tmp.path());
        // Set up outbounds map for cross-channel
        let mut outbounds_map: std::collections::HashMap<String, Arc<dyn OutboundAdapter>> =
            std::collections::HashMap::new();
        outbounds_map.insert("email".to_string(), Arc::new(mock));
        ctx.outbounds = Some(Arc::new(tokio::sync::Mutex::new(outbounds_map)));

        // Create a test attachment file
        let file_path = tmp.path().join("data.csv");
        tokio::fs::write(&file_path, b"a,b,c\n1,2,3").await.unwrap();

        let result = tool
            .execute(
                json!({
                    "channel": "email",
                    "recipient": "external@example.com",
                    "subject": "CSV Export",
                    "message": "Here is your export",
                    "attachments": ["data.csv"]
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("external@example.com"));
        assert!(result.content.contains("mock-attachment-msg"));
        assert!(result.content.contains("1 attachment(s)"));

        let recorded = attachment_calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, "external@example.com");
        assert_eq!(recorded[0].3, vec!["data.csv"]);
    }
}

mod skills {
    use arc_swap::ArcSwap;
    use jyc_agent::JycAgentService;
    use jyc_agent::service::{SkillMeta, format_skills_section, parse_skill_frontmatter};
    use jyc_types::AppConfig;
    use std::sync::Arc;

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
        let app = AppConfig {
            general: jyc_types::GeneralConfig::default(),
            channels: std::collections::HashMap::new(),
            ai: jyc_types::AiConfig {
                enabled: true,
                mode: "agent".to_string(),
                model: None,
                plan_model: None,
                build_model: None,
                small_model: None,
                system_prompt: None,
                max_iterations: 500,
                sse_read_timeout_secs: 120,
                text: None,
                attachments: None,
                providers: std::collections::HashMap::new(),
                vision: None,
                reset_compression: None,
                auto_reset_threshold: 0.95,
            },
            inspect: None,
            attachments: None,
            wecom: None,
            mcps: Vec::new(),
            scheduler: jyc_types::SchedulerConfig::default(),
            commands: Vec::new(),
        };
        JycAgentService::new(
            Arc::new(ArcSwap::from_pointee(app)),
            workdir,
            vec![],
            None,
            vec![],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            "test".to_string(),
        )
    }

    #[test]
    fn no_skills_dir_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        with_temp_home(tmp.path(), || {
            let svc = make_service(tmp.path().to_path_buf());
            let skills = svc.discover_skills(tmp.path(), None, None);
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
            let skills = svc.discover_skills(tmp.path(), None, None);
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
            let skills = svc.discover_skills(tmp.path(), None, None);
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
            let skills = svc.discover_skills(tmp.path(), None, None);
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
            let skills = svc.discover_skills(tmp.path(), None, None);
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
            let skills = svc.discover_skills(tmp.path(), None, None);
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

/// End-to-end billing behaviour: the per-call pattern the agent loop
/// applies on every LLM response — compute cost from that call's own
/// usage, append it to the durable ledger, and add it to `session_cost`.
///
/// Exercised here rather than by driving the whole loop because the loop
/// needs a live provider; this covers the exact sequence of calls the
/// loop makes per response.
mod billing_integration {
    use jyc_core::billing_log_store::{BillingEntry, BillingLogStore};
    use jyc_types::config::ModelPricing;
    use jyc_types::pricing::compute_cost;

    fn pricing() -> ModelPricing {
        // Claude-Opus-like rates: $15/M in, $75/M out, $1.50/M cache.
        // `currency` is explicit because DEFAULT_CURRENCY is CNY, and these
        // are USD rates.
        ModelPricing {
            input_per_million: 15.0,
            output_per_million: 75.0,
            cache_hit_per_million: 1.5,
            cache_creation_per_million: None,
            currency: Some("USD".to_string()),
        }
    }

    /// Simulate one LLM call the way `agent_loop` does.
    async fn bank_one_call(
        topic_path: &std::path::Path,
        input: u64,
        output: u64,
        cache_hit: u64,
    ) -> f64 {
        let p = pricing();
        let cost = compute_cost(&p, input, output, cache_hit);
        BillingLogStore::append(
            topic_path,
            &BillingEntry {
                ts: chrono::Utc::now().to_rfc3339(),
                model: "anthropic/claude-opus-4-7".to_string(),
                input_tokens: input,
                output_tokens: output,
                cache_hit_tokens: cache_hit,
                cache_creation_tokens: 0,
                cost,
                currency: p.currency_label().to_string(),
                kind: jyc_core::billing_log_store::KIND_CALL.to_string(),
            },
        )
        .unwrap();
        jyc_agent::session::persist_tokens(
            topic_path,
            input,
            input,
            output,
            cache_hit,
            0,
            Some(200_000),
            0.95,
            cost,
        )
        .await;
        cost
    }

    /// Three calls in one round: the ledger gets exactly three lines and
    /// `session_cost` equals their sum. This is the invariant that makes
    /// per-call banking correct — no call is dropped or double-counted.
    #[tokio::test]
    async fn three_calls_produce_three_ledger_lines_and_summed_session_cost() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path();

        let mut expected = 0.0;
        for (input, output, cache) in [(1000, 100, 0), (2500, 250, 800), (4000, 90, 3200)] {
            expected += bank_one_call(path, input, output, cache).await;
        }

        // Ledger: one line per call.
        let entries =
            BillingLogStore::load_date(path, &chrono::Utc::now().format("%Y-%m-%d").to_string());
        assert_eq!(entries.len(), 3, "one ledger line per LLM call");

        // Ledger total and session_cost agree with the computed sum.
        let (ledger_total, currency) = BillingLogStore::today_total(path).unwrap();
        assert!(
            (ledger_total - expected).abs() < 1e-9,
            "ledger {ledger_total} vs {expected}"
        );
        assert_eq!(currency, "USD");

        let state: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(path.join(".jyc/agent-session.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        let session_cost = state["session_cost"].as_f64().unwrap();
        assert!(
            (session_cost - expected).abs() < 1e-9,
            "session_cost {session_cost} vs {expected}"
        );
    }

    /// The durability guarantee: `session_cost` resets with the session,
    /// but the ledger does not. After a reset the ledger still holds the
    /// full day, which is why "today" is read from it and not from
    /// `agent-session.json`.
    #[tokio::test]
    async fn ledger_survives_session_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path();

        let first = bank_one_call(path, 5000, 400, 1000).await;

        // Reset semantics, matching `reset_session`: the session file is
        // deleted, then rebuilt with zeroed counters by the auto-reset
        // path's follow-up `persist_tokens(.., 0.0)` call.
        tokio::fs::remove_file(path.join(".jyc/agent-session.json"))
            .await
            .unwrap();
        jyc_agent::session::persist_tokens(path, 0, 0, 0, 0, 0, Some(200_000), 0.95, 0.0).await;

        let after: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(path.join(".jyc/agent-session.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            after["session_cost"].as_f64().unwrap(),
            0.0,
            "session_cost must zero with the session"
        );

        // ...but the ledger still has the spend.
        let (ledger_total, _) = BillingLogStore::today_total(path).unwrap();
        assert!(
            (ledger_total - first).abs() < 1e-9,
            "ledger must survive the reset: {ledger_total} vs {first}"
        );
    }
    /// Regression for the review finding: ancillary summarization calls
    /// used to discard their usage entirely, so `today` silently
    /// undercounted. They must now land in the ledger, tagged `summary`
    /// so overhead is separable from user-facing spend.
    #[tokio::test]
    async fn summary_calls_are_billed_and_tagged() {
        use jyc_core::billing_log_store::{KIND_CALL, KIND_SUMMARY};

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path();
        let p = pricing();

        // One normal call...
        bank_one_call(path, 1000, 100, 0).await;

        // ...and one summary call, billed the way the agent loop does it.
        let summary_cost = jyc_types::pricing::compute_cost(&p, 40_000, 300, 0);
        BillingLogStore::append(
            path,
            &BillingEntry {
                ts: chrono::Utc::now().to_rfc3339(),
                model: "anthropic/claude-opus-4-7".to_string(),
                input_tokens: 40_000,
                output_tokens: 300,
                cache_hit_tokens: 0,
                cache_creation_tokens: 0,
                cost: summary_cost,
                currency: p.currency_label().to_string(),
                kind: KIND_SUMMARY.to_string(),
            },
        )
        .unwrap();
        jyc_agent::session::add_session_cost(path, summary_cost).await;

        let entries =
            BillingLogStore::load_date(path, &chrono::Utc::now().format("%Y-%m-%d").to_string());
        assert_eq!(entries.len(), 2, "both the call and the summary are billed");

        let kinds: Vec<&str> = entries.iter().map(|e| e.kind.as_str()).collect();
        assert!(kinds.contains(&KIND_CALL), "main call tagged 'call'");
        assert!(kinds.contains(&KIND_SUMMARY), "summary tagged 'summary'");

        // A 40K-token summary is not rounding error -- it dominates here,
        // which is exactly why omitting it undercounted.
        assert!(
            summary_cost > 0.0,
            "summary must carry real cost, not be silently free"
        );

        // today_total covers both kinds; session_cost includes the summary.
        let (today, _) = BillingLogStore::today_total(path).unwrap();
        let expected: f64 = entries.iter().map(|e| e.cost).sum();
        assert!((today - expected).abs() < 1e-9);

        let state: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(path.join(".jyc/agent-session.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        let session_cost = state["session_cost"].as_f64().unwrap();
        assert!(
            (session_cost - expected).abs() < 1e-9,
            "session_cost {session_cost} must include the summary, expected {expected}"
        );
    }

    /// `add_session_cost` must not disturb the token counters -- summary
    /// tokens are real spend but are NOT part of the main loop's context
    /// accounting, and folding them in would corrupt the auto-reset math.
    #[tokio::test]
    async fn add_session_cost_leaves_token_counters_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path();

        bank_one_call(path, 5000, 400, 1000).await;
        let before: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(path.join(".jyc/agent-session.json"))
                .await
                .unwrap(),
        )
        .unwrap();

        jyc_agent::session::add_session_cost(path, 0.25).await;

        let after: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(path.join(".jyc/agent-session.json"))
                .await
                .unwrap(),
        )
        .unwrap();

        for field in [
            "context_input_tokens",
            "total_input_tokens",
            "total_output_tokens",
            "total_cache_hit_tokens",
            "max_input_tokens",
        ] {
            assert_eq!(before[field], after[field], "{field} must not change");
        }

        let delta =
            after["session_cost"].as_f64().unwrap() - before["session_cost"].as_f64().unwrap();
        assert!((delta - 0.25).abs() < 1e-9, "only session_cost moves");
    }

    /// A zero / failed summary call must not write a ledger line.
    #[tokio::test]
    async fn zero_cost_summary_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path();
        jyc_agent::session::add_session_cost(path, 0.0).await;
        assert!(
            BillingLogStore::today_total(path).is_none(),
            "no ledger entry for a zero-cost call"
        );
    }
}
