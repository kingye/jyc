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

    fn full_strategy() -> jyc_types::channel::ContextStrategyConfig {
        jyc_types::channel::ContextStrategyConfig {
            mode: jyc_types::channel::ContextStrategy::Full,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn load_context_returns_empty_when_no_session_file() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "load_context_returns_empty_when_no_session_file";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let (messages, raw_context) =
            session::load_context(topic, tmp.path(), &full_strategy()).await;
        assert!(messages.is_empty());
        assert!(raw_context.is_empty());
    }

    #[tokio::test]
    async fn save_and_load_raw_context() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "save_and_load_raw_context";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();

        // Create session file (needed for load_context to proceed)
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"created_at":"2026-01-01T00:00:00Z","context_input_tokens":0,"total_output_tokens":0,"max_input_tokens":0}"#,
        ).await.unwrap();

        // Save raw context
        let context = vec![
            serde_json::json!({"role": "user", "content": "hello"}),
            serde_json::json!({"role": "assistant", "content": "Hi there!"}),
        ];
        session::save_raw_context(topic, tmp.path(), context).await;

        // Load it back — twice: the second load reads the same journal.
        let (messages, raw_context) =
            session::load_context(topic, tmp.path(), &full_strategy()).await;
        assert_eq!(raw_context.len(), 2);
        assert_eq!(messages.len(), 2);
        let (messages2, raw_context2) =
            session::load_context(topic, tmp.path(), &full_strategy()).await;
        assert_eq!(raw_context2.len(), 2);
        assert_eq!(messages2.len(), 2);
        assert_eq!(
            raw_context[1]["content"], raw_context2[1]["content"],
            "cache hit must return identical content"
        );
    }

    /// Write `context` to the journal in its native line format.
    async fn write_context_journal(state_dir: &std::path::Path, context: &[serde_json::Value]) {
        let body: String = context
            .iter()
            .map(|v| serde_json::to_string(v).unwrap() + "\n")
            .collect();
        tokio::fs::write(state_dir.join("agent-context.jsonl"), body)
            .await
            .unwrap();
    }

    /// A pre-journal `agent-context.json` (the whole context as one JSON
    /// array) is neither read nor converted: the journal is the only context
    /// store. The array is left where it is — nothing replaced it, so the
    /// file is the user's data, not garbage to sweep up.
    #[tokio::test]
    async fn load_context_ignores_a_legacy_array_file() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "load_context_ignores_a_legacy_array_file";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"created_at":"2026-01-01T00:00:00Z","context_input_tokens":0,"total_output_tokens":0,"max_input_tokens":0}"#,
        )
        .await
        .unwrap();

        let legacy = vec![
            serde_json::json!({"role": "user", "content": "hello"}),
            serde_json::json!({"role": "assistant", "content": "hi"}),
        ];
        tokio::fs::write(
            state_dir.join("agent-context.json"),
            serde_json::to_string(&legacy).unwrap(),
        )
        .await
        .unwrap();

        let (messages, raw_context) =
            session::load_context(topic, tmp.path(), &full_strategy()).await;
        assert!(messages.is_empty(), "the array is not read");
        assert!(raw_context.is_empty());
        assert!(
            state_dir.join("agent-context.json").exists(),
            "the array is left untouched"
        );
        assert!(
            !state_dir.join("agent-context.jsonl").exists(),
            "no journal is conjured from it"
        );
    }

    #[tokio::test]
    async fn load_context_tolerates_torn_journal_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "load_context_tolerates_torn_journal_tail";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"created_at":"2026-01-01T00:00:00Z","context_input_tokens":0,"total_output_tokens":0,"max_input_tokens":0}"#,
        )
        .await
        .unwrap();

        // One complete line + a torn line from a crash mid-append.
        let good = serde_json::json!({"role": "assistant", "content": "survived"});
        let mut body = serde_json::to_string(&good).unwrap();
        body.push_str("\n{\"role\":\"user\",\"content\":\"tor");
        tokio::fs::write(state_dir.join("agent-context.jsonl"), body)
            .await
            .unwrap();

        let (messages, raw_context) =
            session::load_context(topic, tmp.path(), &full_strategy()).await;
        assert_eq!(raw_context.len(), 1, "torn tail dropped");
        assert_eq!(raw_context[0]["content"], "survived");
        assert_eq!(messages.len(), 1);
    }

    #[tokio::test]
    async fn save_raw_context_appends_the_turn_and_keeps_the_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "save_raw_context_appends_the_turn_and_keeps_the_prefix";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"created_at":"2026-01-01T00:00:00Z","context_input_tokens":0,"total_output_tokens":0,"max_input_tokens":0}"#,
        )
        .await
        .unwrap();

        let seed = vec![
            serde_json::json!({"role": "user", "content": "original"}),
            serde_json::json!({"role": "assistant", "content": "original answer"}),
        ];
        write_context_journal(&state_dir, &seed).await;

        // The journal is append-only: the turn's messages land after the
        // prefix that is already there.
        let turn = vec![
            serde_json::json!({"role": "user", "content": "next question"}),
            serde_json::json!({"role": "assistant", "content": "next answer"}),
        ];
        session::save_raw_context(topic, tmp.path(), turn.clone()).await;
        let journal = tokio::fs::read_to_string(state_dir.join("agent-context.jsonl"))
            .await
            .unwrap();
        assert_eq!(
            journal.lines().count(),
            seed.len() + turn.len(),
            "the turn is appended, the prefix stays"
        );
        let parsed: Vec<serde_json::Value> = journal
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(parsed[0]["content"], "original");
        assert_eq!(parsed[2]["content"], "next question");
    }

    #[tokio::test]
    async fn load_context_detects_external_journal_change() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "load_context_detects_external_journal_change";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"created_at":"2026-01-01T00:00:00Z","context_input_tokens":0,"total_output_tokens":0,"max_input_tokens":0}"#,
        )
        .await
        .unwrap();

        let seed = vec![serde_json::json!({"role": "assistant", "content": "one"})];
        write_context_journal(&state_dir, &seed).await;

        let (_, first) = session::load_context(topic, tmp.path(), &full_strategy()).await;
        assert_eq!(first.len(), 1);

        // An out-of-band append (another writer, a restore) must invalidate
        // the parse cache even though this process never wrote it.
        let extra = serde_json::json!({"role": "assistant", "content": "two"});
        let mut body = String::new();
        for v in &seed {
            body.push_str(&(serde_json::to_string(v).unwrap() + "\n"));
        }
        body.push_str(&(serde_json::to_string(&extra).unwrap() + "\n"));
        tokio::fs::write(state_dir.join("agent-context.jsonl"), body)
            .await
            .unwrap();

        let (_, second) = session::load_context(topic, tmp.path(), &full_strategy()).await;
        assert_eq!(second.len(), 2, "external change must miss the cache");
        assert_eq!(second[1]["content"], "two");
    }

    #[tokio::test]
    async fn load_context_filters_invalid_assistant_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "load_context_filters_invalid_assistant_messages";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();

        // Create session file
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"created_at":"2026-01-01T00:00:00Z","context_input_tokens":0,"total_output_tokens":0,"max_input_tokens":0}"#,
        ).await.unwrap();

        // Save context with an invalid assistant message (null content, no tool_calls)
        let context = vec![
            serde_json::json!({"role": "user", "content": "hello"}),
            serde_json::json!({"role": "assistant", "content": null, "reasoning_content": "thinking..."}),
            serde_json::json!({"role": "assistant", "content": "Valid reply"}),
        ];
        write_context_journal(&state_dir, &context).await;

        // Load — should filter out the invalid message
        let (messages, raw_context) =
            session::load_context(topic, tmp.path(), &full_strategy()).await;
        assert_eq!(raw_context.len(), 2); // user + valid assistant
        assert_eq!(messages.len(), 2);
    }

    #[tokio::test]
    async fn load_context_discards_all_user_only_context() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "load_context_discards_all_user_only_context";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();

        // Create session file
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"created_at":"2026-01-01T00:00:00Z","context_input_tokens":0,"total_output_tokens":0,"max_input_tokens":0}"#,
        ).await.unwrap();

        // Save context with only user messages (corrupted)
        let context = vec![
            serde_json::json!({"role": "user", "content": "hello"}),
            serde_json::json!({"role": "user", "content": "hello again"}),
        ];
        write_context_journal(&state_dir, &context).await;

        // Load — should return empty (no valid assistant messages)
        let (messages, raw_context) =
            session::load_context(topic, tmp.path(), &full_strategy()).await;
        assert!(messages.is_empty());
        assert!(raw_context.is_empty());
    }

    #[tokio::test]
    async fn persist_tokens_creates_session_file() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "persist_tokens_creates_session_file";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");

        assert!(!state_dir.join("agent-session.json").exists());
        session::persist_tokens(
            topic,
            tmp.path(),
            1000,
            250,
            1000,
            200,
            0,
            0,
            0,
            Some(100000),
            0.0,
            0,
            0,
            0,
            0,
            0,
        )
        .await;
        assert!(state_dir.join("agent-session.json").exists());

        // Verify content
        let content = tokio::fs::read_to_string(state_dir.join("agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(state["context_input_tokens"], 1000);
        // The last call's own output — the reply appended to the context.
        assert_eq!(state["context_output_tokens"], 250);
        assert_eq!(state["total_input_tokens"], 1000);
        assert_eq!(state["total_output_tokens"], 200);
        assert_eq!(state["total_cache_hit_tokens"], 0);
        // The raw context window — not a fraction of it.
        assert_eq!(state["context_window"], 100000);
    }

    /// The composition rides the token save in one write, and a later
    /// token-only save must carry it forward instead of dropping it.
    #[tokio::test]
    async fn persist_tokens_with_composition_survives_token_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "persist_tokens_with_composition_survives_token_updates";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        let session_path = state_dir.join("agent-session.json");

        let composition = vec![
            jyc_types::ContextRegionInfo {
                region: jyc_types::ContextRegion::History,
                messages: 3,
                bytes: 512,
                estimated_tokens: 128,
            },
            jyc_types::ContextRegionInfo {
                region: jyc_types::ContextRegion::Current,
                messages: 2,
                bytes: 256,
                estimated_tokens: 64,
            },
        ];
        session::persist_tokens_with_composition(
            topic,
            tmp.path(),
            1000,
            100,
            1000,
            100,
            0,
            0,
            0,
            Some(100_000),
            0.0,
            0,
            0,
            0,
            0,
            0,
            &composition,
        )
        .await;

        let read = || async {
            let content = tokio::fs::read_to_string(&session_path).await.unwrap();
            serde_json::from_str::<serde_json::Value>(&content).unwrap()
        };
        let state = read().await;
        assert_eq!(state["context_composition"][0]["region"], "history");
        assert_eq!(state["context_composition"][0]["messages"], 3);
        assert_eq!(state["context_composition"][1]["region"], "current");
        assert_eq!(state["context_composition"][1]["estimated_tokens"], 64);

        // The next call's token save starts from the file, so it must keep
        // the composition rather than write back an empty vector.
        session::persist_tokens(
            topic,
            tmp.path(),
            2000,
            200,
            2000,
            300,
            0,
            0,
            0,
            Some(100_000),
            0.0,
            0,
            0,
            0,
            0,
            0,
        )
        .await;
        let state = read().await;
        assert_eq!(state["context_input_tokens"], 2000);
        assert_eq!(state["context_composition"][0]["region"], "history");
        assert_eq!(state["context_composition"][1]["region"], "current");
    }

    /// `context_input_tokens` and `total_input_tokens` are stored as passed
    /// in (= latest call's value, since each call already includes full
    /// context). `total_output_tokens` is also stored as passed in (= the
    /// running total accumulated by the caller). No += accumulation
    /// happens inside `persist_tokens` itself.
    #[tokio::test]
    async fn persist_tokens_stores_latest_not_accumulated() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "persist_tokens_stores_latest_not_accumulated";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));

        // First call
        session::persist_tokens(
            topic,
            tmp.path(),
            1000,
            100,
            1000,
            100,
            0,
            0,
            0,
            Some(100000),
            0.0,
            0,
            0,
            0,
            0,
            0,
        )
        .await;
        // Second call — caller has accumulated locally. For output, the
        // running total after call 2 is 100 + 150 = 250, which is what
        // gets passed in (not just the per-call delta of 150).
        session::persist_tokens(
            topic,
            tmp.path(),
            2000,
            150,
            2000,
            250,
            0,
            0,
            0,
            Some(100000),
            0.0,
            0,
            0,
            0,
            0,
            0,
        )
        .await;

        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(state["context_input_tokens"], 2000); // Latest, not 3000
        assert_eq!(state["context_output_tokens"], 150); // The last call's own
        assert_eq!(state["total_input_tokens"], 2000); // Latest, not 3000
        assert_eq!(state["total_output_tokens"], 250); // Caller's running sum
    }

    #[tokio::test]
    async fn reset_session_deletes_session_file() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "reset_session_deletes_session_file";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();

        // Create session and context files
        tokio::fs::write(state_dir.join("agent-session.json"), "{}")
            .await
            .unwrap();
        tokio::fs::write(state_dir.join("agent-context.jsonl"), "[]\n")
            .await
            .unwrap();

        session::reset_session(topic, tmp.path()).await;

        assert!(!state_dir.join("agent-session.json").exists());
        // The context is cleared, not summarized.
        assert!(!state_dir.join("agent-context.jsonl").exists());
    }

    /// `persist_tokens` writes the session file and nothing else: the
    /// on-disk `agent-context.jsonl` is the raw context and only a reset
    /// (or the loop's own save) touches it.
    #[tokio::test]
    async fn persist_tokens_leaves_the_context_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "persist_tokens_leaves_the_context_alone";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");

        // Seed a context file the reset would otherwise delete.
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        let context_before = "{\"role\":\"user\",\"content\":\"seed\"}\n";
        tokio::fs::write(state_dir.join("agent-context.jsonl"), context_before)
            .await
            .unwrap();

        // Call persist_tokens with a large input.
        session::persist_tokens(
            topic,
            tmp.path(),
            100_000,
            0,
            100_000,
            200,
            0,
            0,
            0,
            Some(10_000),
            0.0,
            0,
            0,
            0,
            0,
            0,
        )
        .await;

        // Context file untouched.
        let context_after = tokio::fs::read_to_string(state_dir.join("agent-context.jsonl"))
            .await
            .unwrap();
        assert_eq!(context_after, context_before);

        // Session file got the new values.
        let session = tokio::fs::read_to_string(state_dir.join("agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&session).unwrap();
        assert_eq!(state["context_input_tokens"], 100_000);
        assert_eq!(state["total_output_tokens"], 200);
        assert_eq!(state["context_window"], 10_000);
    }

    /// Output tokens accumulate upstream in the agent_loop accumulator,
    /// which sums each call's `output_tokens` and passes the running
    /// total here. `persist_tokens` stores it as-is (assignment).
    /// This test verifies the data flow: the on-disk value reflects the
    /// running total passed in (= 330 across three calls of 100+150+80).
    #[tokio::test]
    async fn persist_tokens_stores_total_output_as_passed() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "persist_tokens_stores_total_output_as_passed";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));

        // Simulate three LLM calls with per-call output 100, 150, 80.
        // agent_loop accumulates locally: 100, 250, 330. Each running
        // total is passed into persist_tokens.
        session::persist_tokens(
            topic,
            tmp.path(),
            1000,
            0,
            1000,
            100,
            0,
            0,
            0,
            None,
            0.0,
            0,
            0,
            0,
            0,
            0,
        )
        .await;
        session::persist_tokens(
            topic,
            tmp.path(),
            1500,
            0,
            1500,
            250,
            0,
            0,
            0,
            None,
            0.0,
            0,
            0,
            0,
            0,
            0,
        )
        .await;
        session::persist_tokens(
            topic,
            tmp.path(),
            2000,
            0,
            2000,
            330,
            0,
            0,
            0,
            None,
            0.0,
            0,
            0,
            0,
            0,
            0,
        )
        .await;

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
        let topic = "persist_tokens_stores_total_input_as_passed";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));

        // Simulate three LLM calls with per-call `input_tokens` of
        // 1000, 2000, 3000 — agent_loop sums them to running totals of
        // 1000, 3000, 6000 and passes each running total to persist_tokens.
        // The on-disk value reflects the latest passed-in sum (= 6000).
        session::persist_tokens(
            topic,
            tmp.path(),
            1000,
            0,
            1000,
            0,
            0,
            0,
            0,
            None,
            0.0,
            0,
            0,
            0,
            0,
            0,
        )
        .await;
        session::persist_tokens(
            topic,
            tmp.path(),
            2000,
            0,
            3000,
            0,
            0,
            0,
            0,
            None,
            0.0,
            0,
            0,
            0,
            0,
            0,
        )
        .await;
        session::persist_tokens(
            topic,
            tmp.path(),
            3000,
            0,
            6000,
            0,
            0,
            0,
            0,
            None,
            0.0,
            0,
            0,
            0,
            0,
            0,
        )
        .await;

        let session = tokio::fs::read_to_string(tmp.path().join(".jyc/agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&session).unwrap();
        assert_eq!(state["context_input_tokens"], 3000);
        assert_eq!(state["total_input_tokens"], 6000);
    }

    /// The per-call latency counters accumulate per call, like
    /// `session_cost` and unlike the token totals: each call adds its own
    /// duration, and the post-loop write — which has no call to time and
    /// passes `0` — leaves the average intact instead of zeroing or
    /// double-counting it.
    #[tokio::test]
    async fn persist_tokens_accumulates_call_latency() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "persist_tokens_accumulates_call_latency";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));

        // Two measured calls of 800ms and 1200ms ...
        for ms in [800u64, 1200u64] {
            session::persist_tokens(
                topic,
                tmp.path(),
                1000,
                0,
                1000,
                0,
                0,
                0,
                0,
                None,
                0.0,
                ms,
                0,
                0,
                0,
                0,
            )
            .await;
        }
        // ... then the post-loop write, which times nothing.
        session::persist_tokens(
            topic,
            tmp.path(),
            1000,
            0,
            1000,
            0,
            0,
            0,
            0,
            None,
            0.0,
            0,
            0,
            0,
            0,
            0,
        )
        .await;

        let session = tokio::fs::read_to_string(tmp.path().join(".jyc/agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&session).unwrap();
        assert_eq!(state["total_call_duration_ms"], 2000);
        assert_eq!(state["call_count"], 2);

        // The dashboard reads the same pair through the core reader, and
        // the turn-scoped twin accumulated in lockstep (nothing has
        // started a new turn here).
        let latency = jyc_core::session_state::read_call_latency(topic, tmp.path()).await;
        assert_eq!(latency.session, Some((1000, 2)));
        assert_eq!(latency.turn, Some((1000, 2)));
    }

    /// A new turn starts with an empty latency window: the previous turn's
    /// pair is cleared while the session pair stays put, so "latest turn"
    /// can never point at a turn that already ended. A first-ever turn
    /// creates no file (nothing to clear).
    #[tokio::test]
    async fn begin_turn_counters_clears_previous_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "begin_turn_counters_clears_previous_turn";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let path = tmp.path();

        jyc_agent::session::begin_turn_counters(topic, path).await;
        assert_eq!(
            jyc_core::session_state::read_call_latency(topic, path).await,
            jyc_core::session_state::CallLatency::default(),
            "no session file yet: nothing to show"
        );
        assert!(
            !path.join(".jyc/agent-session.json").exists(),
            "a zero-counter turn start must not create the file"
        );

        // Two calls in this turn: 600ms and 400ms.
        for ms in [600u64, 400u64] {
            session::persist_tokens(
                topic, path, 100, 0, 100, 0, 0, 0, 0, None, 0.0, ms, 0, 0, 0, 0,
            )
            .await;
        }
        let latency = jyc_core::session_state::read_call_latency(topic, path).await;
        assert_eq!(latency.turn, Some((500, 2)));

        // The next turn starts: the turn pair clears, the session pair
        // survives.
        jyc_agent::session::begin_turn_counters(topic, path).await;
        let latency = jyc_core::session_state::read_call_latency(topic, path).await;
        assert_eq!(latency.turn, None);
        assert_eq!(latency.session, Some((500, 2)));
    }

    /// The turn's own figures accumulate per call, the way the latency
    /// pair does: each call adds its own cache-hit, input and output
    /// tokens, so the turn's rate and output cover exactly this turn's
    /// calls.
    #[tokio::test]
    async fn persist_tokens_accumulates_the_turns_own_figures() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "persist_tokens_accumulates_the_turns_own_figures";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let path = tmp.path();

        // Two calls of the turn: 300 cache hits over 1000 input tokens,
        // 100 output and 120 reasoning tokens, then 500 over 1500 with
        // 250 out and 380 reasoning.
        session::persist_tokens(
            topic, path, 1000, 0, 1000, 100, 300, 0, 120, None, 0.0, 0, 300, 1000, 100, 120,
        )
        .await;
        session::persist_tokens(
            topic, path, 1500, 0, 2500, 350, 800, 0, 500, None, 0.0, 0, 500, 1500, 250, 380,
        )
        .await;

        let state = jyc_core::session_state::read_token_state(topic, path).await;
        assert_eq!(state.turn_cache_hit_tokens, Some(800));
        assert_eq!(state.turn_input_tokens, Some(2500));
        assert_eq!(state.turn_output_tokens, Some(350));
        assert_eq!(state.total_output_tokens, Some(350));
        assert_eq!(
            state.turn_reasoning_tokens,
            Some(500),
            "each call's reasoning tokens add to the turn's own figure"
        );
        assert_eq!(
            state.total_reasoning_tokens,
            Some(500),
            "the session total is the caller's running sum, which already counts the call"
        );
    }

    /// A new turn clears the turn figures with the latency pair, so the
    /// turn's rate and output never point at a turn that already ended —
    /// while the session's own totals survive.
    #[tokio::test]
    async fn begin_turn_counters_clears_the_turns_own_figures() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "begin_turn_counters_clears_the_turns_own_figures";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let path = tmp.path();

        session::persist_tokens(
            topic, path, 1000, 0, 1000, 900, 300, 0, 60, None, 0.0, 0, 300, 1000, 900, 60,
        )
        .await;
        let state = jyc_core::session_state::read_token_state(topic, path).await;
        assert_eq!(state.turn_cache_hit_tokens, Some(300));
        assert_eq!(state.turn_output_tokens, Some(900));
        assert_eq!(state.turn_reasoning_tokens, Some(60));

        jyc_agent::session::begin_turn_counters(topic, path).await;
        let state = jyc_core::session_state::read_token_state(topic, path).await;
        assert_eq!(state.turn_cache_hit_tokens, None);
        assert_eq!(state.turn_input_tokens, None);
        assert_eq!(state.turn_output_tokens, None);
        assert_eq!(state.turn_reasoning_tokens, None);
        assert_eq!(state.total_cache_hit_tokens, Some(300));
        assert_eq!(state.total_output_tokens, Some(900));
        assert_eq!(state.total_reasoning_tokens, Some(60));
    }

    /// The agent loop's running totals start from the session's own: a
    /// turn continues the session's counts instead of restarting them.
    #[tokio::test]
    async fn session_token_totals_reads_the_sessions_own_totals() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "session_token_totals_reads_the_sessions_own_totals";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let path = tmp.path();

        session::persist_tokens(
            topic, path, 1000, 250, 4000, 900, 300, 40, 60, None, 0.0, 0, 0, 0, 0, 0,
        )
        .await;

        let totals = jyc_agent::session::session_token_totals(topic, path).await;
        assert_eq!(totals.input, 4000);
        assert_eq!(totals.output, 900);
        assert_eq!(totals.cache_hit, 300);
        assert_eq!(totals.cache_creation, 40);
        assert_eq!(totals.reasoning, 60);
    }

    /// The latency counters are session-scoped: the write that follows a
    /// `reset_session` — which deleted the file — must leave them at zero
    /// with the token counters rather than resurrect a pair from a session
    /// the user has forgotten.
    #[tokio::test]
    async fn call_latency_zeroes_with_session_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "call_latency_zeroes_with_session_reset";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let path = tmp.path();

        // One measured call, so there is a pair to lose.
        session::persist_tokens(
            topic, path, 800, 0, 800, 40, 0, 0, 0, None, 0.0, 700, 0, 0, 0, 0,
        )
        .await;
        assert_eq!(
            jyc_core::session_state::read_call_latency(topic, path)
                .await
                .session,
            Some((700, 1))
        );

        // Reset: the file goes, and the next write rebuilds it.
        tokio::fs::remove_file(path.join(".jyc/agent-session.json"))
            .await
            .unwrap();
        jyc_agent::session::persist_tokens(
            topic,
            path,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            Some(200_000),
            0.0,
            0,
            0,
            0,
            0,
            0,
        )
        .await;

        assert_eq!(
            jyc_core::session_state::read_call_latency(topic, path).await,
            jyc_core::session_state::CallLatency::default(),
            "the pair must zero with the session"
        );
    }

    /// Mirrors the actual agent_loop accumulation pattern: per call,
    /// `total_input_tokens += response.input_tokens` and
    /// `total_output_tokens += response.output_tokens`. Verifies that
    /// the += accumulation is what produces the value that ends up in
    /// agent-session.json after a round of multiple LLM calls.
    #[tokio::test]
    async fn agent_loop_token_accumulation_pattern() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "agent_loop_token_accumulation_pattern";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));

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
                topic,
                tmp.path(),
                context_input_tokens,
                0,
                total_input_tokens,
                total_output_tokens,
                0,
                0,
                0,
                None,
                0.0,
                0,
                0,
                0,
                0,
                0,
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

    /// `session_cost` is an accumulating field: unlike the token fields
    /// (which are assigned from the caller's running total), each call's
    /// cost is added to the previous value.
    #[tokio::test]
    async fn session_cost_accumulates_across_calls() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "session_cost_accumulates_across_calls";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));

        session::persist_tokens(
            topic,
            tmp.path(),
            1000,
            0,
            1000,
            100,
            0,
            0,
            0,
            None,
            0.25,
            0,
            0,
            0,
            0,
            0,
        )
        .await;
        session::persist_tokens(
            topic,
            tmp.path(),
            1500,
            0,
            2500,
            200,
            0,
            0,
            0,
            None,
            0.10,
            0,
            0,
            0,
            0,
            0,
        )
        .await;
        session::persist_tokens(
            topic,
            tmp.path(),
            2000,
            0,
            4500,
            300,
            0,
            0,
            0,
            None,
            0.05,
            0,
            0,
            0,
            0,
            0,
        )
        .await;

        let session = tokio::fs::read_to_string(tmp.path().join(".jyc/agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&session).unwrap();

        // 0.25 + 0.10 + 0.05 = 0.40 (accumulated, not replaced by 0.05)
        let cost = state["session_cost"].as_f64().unwrap();
        assert!((cost - 0.40).abs() < 1e-9, "got {cost}");
        // Nothing has started a new turn, so the turn's share is the whole
        // session so far.
        let turn = state["turn_cost"].as_f64().unwrap();
        assert!((turn - 0.40).abs() < 1e-9, "got {turn}");
    }

    /// A turn of many iterations keeps adding to the turn's own cost, and
    /// the dashboard reader hands back both scopes from one read of the
    /// session file. The nesting invariant — a turn's spend is part of its
    /// session's — is what makes "turn ≤ session" safe to show.
    #[tokio::test]
    async fn turn_cost_accumulates_and_never_exceeds_session_cost() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "turn_cost_accumulates_and_never_exceeds_session_cost";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let path = tmp.path();

        // Two turns' worth of calls: the first turn spends 0.30, then the
        // counters reset and the second spends 0.12.
        for cost in [0.10, 0.20] {
            session::persist_tokens(
                topic, path, 1000, 0, 1000, 100, 0, 0, 0, None, cost, 0, 0, 0, 0, 0,
            )
            .await;
        }
        let figures = jyc_core::session_state::read_cost_figures(topic, path).await;
        assert_eq!(figures.session, Some(0.30));
        assert_eq!(figures.turn, Some(0.30), "one turn so far: the two agree");

        jyc_agent::session::begin_turn_counters(topic, path).await;
        for cost in [0.02, 0.10] {
            session::persist_tokens(
                topic, path, 1000, 0, 1000, 100, 0, 0, 0, None, cost, 0, 0, 0, 0, 0,
            )
            .await;
        }

        let figures = jyc_core::session_state::read_cost_figures(topic, path).await;
        let session = figures.session.unwrap();
        let turn = figures.turn.unwrap();
        assert!((session - 0.42).abs() < 1e-9, "got {session}");
        assert!((turn - 0.12).abs() < 1e-9, "got {turn}");
        assert!(
            turn <= session,
            "the turn's spend is part of the session's: {turn} > {session}"
        );
    }

    /// A new turn clears the turn's cost the way it clears the latency
    /// pair, while the session's total survives — "this turn" never points
    /// at a turn that already ended.
    #[tokio::test]
    async fn begin_turn_counters_clears_turn_cost_only() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "begin_turn_counters_clears_turn_cost_only";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let path = tmp.path();

        session::persist_tokens(
            topic, path, 1000, 0, 1000, 100, 0, 0, 0, None, 0.40, 0, 0, 0, 0, 0,
        )
        .await;
        let figures = jyc_core::session_state::read_cost_figures(topic, path).await;
        assert_eq!(figures.turn, Some(0.40));

        jyc_agent::session::begin_turn_counters(topic, path).await;
        let figures = jyc_core::session_state::read_cost_figures(topic, path).await;
        assert_eq!(figures.turn, None, "the new turn has spent nothing yet");
        assert_eq!(figures.session, Some(0.40), "the session total survives");
    }

    /// A zero `call_cost` (unpriced model) leaves `session_cost` untouched
    /// rather than resetting it.
    #[tokio::test]
    async fn zero_call_cost_preserves_existing_session_cost() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "zero_call_cost_preserves_existing_session_cost";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));

        session::persist_tokens(
            topic,
            tmp.path(),
            1000,
            0,
            1000,
            100,
            0,
            0,
            0,
            None,
            0.75,
            0,
            0,
            0,
            0,
            0,
        )
        .await;
        session::persist_tokens(
            topic,
            tmp.path(),
            1500,
            0,
            2500,
            200,
            0,
            0,
            0,
            None,
            0.0,
            0,
            0,
            0,
            0,
            0,
        )
        .await;

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
        let topic = "legacy_session_file_without_cost_field_loads";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
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
        session::persist_tokens(
            topic,
            tmp.path(),
            600,
            0,
            1100,
            90,
            0,
            0,
            0,
            None,
            0.30,
            0,
            0,
            0,
            0,
            0,
        )
        .await;

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
    /// topic that has none, with the context window, zeroed counters, and a
    /// populated `created_at`.
    #[tokio::test]
    async fn ensure_session_file_creates_file_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "ensure_session_file_creates_file_when_missing";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let session_path = tmp.path().join(".jyc/agent-session.json");

        assert!(!session_path.exists());

        session::ensure_session_file(topic, tmp.path(), Some(100_000)).await;

        assert!(session_path.exists());

        let content = tokio::fs::read_to_string(&session_path).await.unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        // Zeroed counters so the dashboard shows "0 / 100000 (0%)" instead
        // of (None, None, None).
        assert_eq!(state["context_input_tokens"], 0);
        assert_eq!(state["total_output_tokens"], 0);
        // The raw context window.
        assert_eq!(state["context_window"], 100_000);
        // created_at must be populated so the dashboard can show session age.
        assert!(
            state["created_at"].as_str().is_some_and(|s| !s.is_empty()),
            "created_at should be set when the file is freshly created"
        );
    }

    /// `ensure_session_file` must NOT touch an existing session file — its
    /// only job is to create the file when missing. Existing token counts,
    /// `context_window`, and `created_at` must be preserved verbatim.
    #[tokio::test]
    async fn ensure_session_file_skips_when_file_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "ensure_session_file_skips_when_file_exists";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();

        let original = r#"{"created_at":"2026-01-01T00:00:00Z","context_input_tokens":42000,"total_output_tokens":7000,"context_window":123000}"#;
        let session_path = state_dir.join("agent-session.json");
        tokio::fs::write(&session_path, original).await.unwrap();

        // Even with a different context_window, the file must remain
        // untouched.
        session::ensure_session_file(topic, tmp.path(), Some(999_999)).await;

        let after = tokio::fs::read_to_string(&session_path).await.unwrap();
        assert_eq!(after, original);
    }

    /// When `context_window` is `None`, `ensure_session_file` still creates
    /// the file but leaves `context_window` at 0 (the agent loop fills it in
    /// on the next turn).
    #[tokio::test]
    async fn ensure_session_file_creates_with_no_context_window() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "ensure_session_file_creates_with_no_context_window";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let session_path = tmp.path().join(".jyc/agent-session.json");

        assert!(!session_path.exists());

        session::ensure_session_file(topic, tmp.path(), None).await;

        assert!(session_path.exists());

        let content = tokio::fs::read_to_string(&session_path).await.unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(state["context_input_tokens"], 0);
        assert_eq!(state["total_output_tokens"], 0);
        assert_eq!(state["context_window"], 0);
    }

    /// A turn can run `/close` on its own topic and then finish: the close has
    /// already deleted the state dir, and the turn's last write must not
    /// recreate it — a dir that exists again is what `list_topic_names` counts
    /// as an open topic, leaving a parentless, config-less topic behind.
    #[tokio::test]
    async fn save_raw_context_writes_nothing_for_a_closed_topic() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "save_raw_context_writes_nothing_for_a_closed_topic";
        let state_dir = tmp.path().join(".jyc");
        jyc_types::state_dir::register(topic, &state_dir);
        jyc_types::state_dir::mark_closed(topic);

        session::save_raw_context(
            topic,
            tmp.path(),
            vec![serde_json::json!({"role": "user", "content": "in flight"})],
        )
        .await;

        assert!(!state_dir.exists(), "a closed topic's state dir stays gone");
        jyc_types::state_dir::clear_closed(topic);
    }

    /// The same for the session file, which the loop writes after every call.
    #[tokio::test]
    async fn persist_tokens_writes_nothing_for_a_closed_topic() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "persist_tokens_writes_nothing_for_a_closed_topic";
        let state_dir = tmp.path().join(".jyc");
        jyc_types::state_dir::register(topic, &state_dir);
        jyc_types::state_dir::mark_closed(topic);

        session::persist_tokens(
            topic,
            tmp.path(),
            9,
            4,
            9,
            4,
            0,
            0,
            0,
            None,
            0.0,
            0,
            0,
            9,
            4,
            0,
        )
        .await;

        assert!(
            !state_dir.join("agent-session.json").exists(),
            "a closed topic gets no session file"
        );
        jyc_types::state_dir::clear_closed(topic);
    }
}

mod tool_registry {
    use jyc_agent::tools::builtin::create_builtin_registry;

    /// The tools the builtin registry must have. The count is derived from
    /// this list, so adding a tool means adding a name — not also
    /// remembering to bump a number somewhere else.
    ///
    /// `list_topics` is in the builtin registry but belongs to the extended
    /// group: it is registered here and kept out of the advertised list
    /// until the model loads the group.
    const EXPECTED_BUILTIN_TOOLS: [&str; 15] = [
        "bash",
        "read",
        "write",
        "edit",
        "grep",
        "webfetch",
        "jyc_send_to_topic",
        "context_browse",
        "ask_user",
        "task_list",
        "task_create",
        "task_update",
        "lesson_add",
        "lesson_read",
        "list_topics",
    ];

    #[test]
    fn builtin_registry_has_all_tools() {
        let registry = create_builtin_registry();
        for name in EXPECTED_BUILTIN_TOOLS {
            assert!(registry.has_tool(name), "registry is missing {name}");
        }
        // Nothing beyond the list: a tool nobody accounted for would go
        // unnoticed in the schema budget and in the model's prompt.
        assert_eq!(registry.len(), EXPECTED_BUILTIN_TOOLS.len());
    }

    #[test]
    fn registry_produces_definitions() {
        let registry = create_builtin_registry();
        let definitions = registry.definitions();
        assert_eq!(definitions.len(), EXPECTED_BUILTIN_TOOLS.len());

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
        let topic = "bash_requires_command_param";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let tool = builtin::bash::BashTool;
        let result = tool.execute(json!({}), &ctx(tmp.path())).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn bash_executes_simple_command() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "bash_executes_simple_command";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
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
        let topic = "bash_reports_error_on_failure";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
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
        let topic = "read_requires_file_path";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let tool = builtin::read::ReadTool;
        let result = tool.execute(json!({}), &ctx(tmp.path())).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn read_file_with_content() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_file_with_content";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
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
        let topic = "write_creates_file";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
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
        let topic = "edit_replaces_text";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
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
        let topic = "edit_fails_when_old_string_not_found";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
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
        let topic = "edit_fails_on_multiple_matches";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
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
    async fn grep_finds_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "grep_finds_matches";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
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
        let topic = "send_to_topic_attachment_has_content";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));

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
            staged_path: None,
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
    fn send_to_topic_require_reply_is_discoverable() {
        // The `require_reply` contract lives in the parameter description, not
        // in the tool description: the tool text rides in every request's
        // cached prefix, and the parameter says the same thing in fewer
        // words. What matters is that the model can still learn what the
        // flag does — so pin the parameter's text, not the tool's.
        let tool = builtin::send_to_topic::SendToThreadTool;
        let schema = tool.input_schema();
        let desc = schema["properties"]["require_reply"]["description"]
            .as_str()
            .expect("require_reply should carry a description");
        assert!(
            desc.contains("send its results back"),
            "the parameter description should say what require_reply does: {desc}"
        );
        assert!(
            desc.contains("Default false"),
            "the parameter description should state the default: {desc}"
        );
    }
}

mod mcp_bridge {
    use jyc_agent::tools::mcp_bridge::SendMessageTool;
    use jyc_agent::tools::{Tool, ToolContext, deliver_reply};
    use jyc_types::{InboundMessage, OutboundAdapter, OutboundAttachment, SendResult};
    use serde_json::json;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    /// Recorded (reply text, attachment filenames) pairs, shared with the test.
    type RecordedReplies = Arc<Mutex<Vec<(String, Vec<String>)>>>;

    struct ReplyMockOutbound {
        fail: bool,
        replies: RecordedReplies,
    }

    impl ReplyMockOutbound {
        fn new(fail: bool) -> (Self, RecordedReplies) {
            let replies = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    fail,
                    replies: replies.clone(),
                },
                replies,
            )
        }
    }

    #[async_trait::async_trait]
    impl OutboundAdapter for ReplyMockOutbound {
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
            reply_text: &str,
            _topic_path: &Path,
            _message_dir: &str,
            attachments: Option<&[OutboundAttachment]>,
        ) -> anyhow::Result<SendResult> {
            if self.fail {
                anyhow::bail!("mock delivery failure");
            }
            self.replies.lock().unwrap().push((
                reply_text.to_string(),
                attachments
                    .unwrap_or_default()
                    .iter()
                    .map(|a| a.filename.clone())
                    .collect(),
            ));
            Ok(SendResult {
                message_id: "direct-1".to_string(),
                chat_seq: 0,
            })
        }
        async fn send_message(
            &self,
            _recipient: &str,
            _subject: &str,
            _body: &str,
        ) -> anyhow::Result<SendResult> {
            Ok(SendResult {
                message_id: "msg-1".to_string(),
                chat_seq: 0,
            })
        }
    }

    fn reply_test_message() -> InboundMessage {
        InboundMessage {
            id: "m1".to_string(),
            channel: "mock".to_string(),
            channel_uid: "u1".to_string(),
            sender: "user".to_string(),
            sender_address: "user@test".to_string(),
            recipients: vec![],
            topic: "t".to_string(),
            content: Default::default(),
            timestamp: chrono::Utc::now(),
            references: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: Default::default(),
            matched_pattern: None,
        }
    }

    /// With a live reply target the reply goes out synchronously through
    /// the outbound adapter — no file relay leftovers.
    #[tokio::test]
    async fn deliver_reply_sends_directly_with_target() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "deliver_reply_sends_directly_with_target";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));

        let (mock, replies) = ReplyMockOutbound::new(false);
        let mut ctx = ToolContext::new(tmp.path());
        ctx.current_topic = Some(topic.into());
        ctx.outbound = Some(Arc::new(mock));
        ctx.reply_target = Some(jyc_agent::tools::ReplyTarget {
            original: reply_test_message(),
            message_dir: "2026-08-22_00-00-00".to_string(),
        });

        let delivery = deliver_reply(&ctx, None, topic, "done!").await.unwrap();

        assert!(delivery.direct);
        assert_eq!(delivery.message_id.as_deref(), Some("direct-1"));
        let recorded = replies.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, "done!");
        // No file relay leftovers.
        let state_dir = tmp.path().join(".jyc");
        assert!(!state_dir.join("reply.md").exists());
        assert!(!state_dir.join("reply-sent.flag").exists());
    }

    /// Without a live reply target (tests, sub-agents) the reply is queued
    /// via the reply.md + reply-sent.flag file relay.
    #[tokio::test]
    async fn deliver_reply_writes_signal_files_without_target() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "deliver_reply_writes_signal_files_without_target";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));

        let ctx = ToolContext::new(tmp.path());
        let delivery = deliver_reply(&ctx, None, topic, "Hello user!")
            .await
            .unwrap();

        assert!(!delivery.direct);
        let state_dir = tmp.path().join(".jyc");
        assert!(state_dir.join("reply-sent.flag").exists());
        assert!(state_dir.join("reply.md").exists());
        assert_eq!(
            std::fs::read_to_string(state_dir.join("reply.md")).unwrap(),
            "Hello user!"
        );
    }

    /// When the direct send fails, the reply falls back to the file relay
    /// so the worker can retry post-loop (and nothing was sent twice).
    #[tokio::test]
    async fn deliver_reply_falls_back_to_file_relay_on_direct_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "deliver_reply_falls_back_to_file_relay_on_direct_failure";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));

        let (mock, replies) = ReplyMockOutbound::new(true);
        let mut ctx = ToolContext::new(tmp.path());
        ctx.current_topic = Some(topic.into());
        ctx.outbound = Some(Arc::new(mock));
        ctx.reply_target = Some(jyc_agent::tools::ReplyTarget {
            original: reply_test_message(),
            message_dir: "2026-08-22_00-00-00".to_string(),
        });

        let delivery = deliver_reply(&ctx, None, topic, "done!").await.unwrap();

        assert!(!delivery.direct);
        assert!(replies.lock().unwrap().is_empty());
        let state_dir = tmp.path().join(".jyc");
        assert_eq!(
            std::fs::read_to_string(state_dir.join("reply.md")).unwrap(),
            "done!"
        );
        assert!(state_dir.join("reply-sent.flag").exists());
    }

    /// The proactive send-message tool still reports a clean tool error
    /// when no outbound adapter is configured.
    #[tokio::test]
    async fn send_message_tool_errors_without_outbound() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(tmp.path());
        let tool = SendMessageTool;
        let result = tool
            .execute(
                json!({"recipient": "wecom:user1", "message": "alert"}),
                &ctx,
            )
            .await;
        assert!(result.is_err() || result.unwrap().is_error);
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

    /// Helper: create a JycAgentService with a specific data_dir.
    fn make_service(data_dir: PathBuf) -> JycAgentService {
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
                models: Default::default(),
                system_prompt: None,
                max_iterations: 500,
                sse_read_timeout_secs: 120,
                text: None,
                attachments: None,
                providers: std::collections::HashMap::new(),
                vision: None,
                context_strategy: None,
            },
            inspect: None,
            attachments: None,
            wecom: None,
            mcps: Vec::new(),
            commands: Vec::new(),
            hooks: Vec::new(),
            agents: std::collections::HashMap::new(),
            subagents: std::collections::HashMap::new(),
        };
        JycAgentService::new(
            Arc::new(ArcSwap::from_pointee(app)),
            data_dir,
            vec![],
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
        let topic = "no_skills_dir_returns_empty";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        with_temp_home(tmp.path(), || {
            let svc = make_service(tmp.path().to_path_buf());
            let skills = svc.discover_skills(topic, tmp.path(), None, None);
            assert!(skills.is_empty());
        });
    }

    #[test]
    fn single_skill_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "single_skill_parsed";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
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
            let skills = svc.discover_skills(topic, tmp.path(), None, None);
            assert_eq!(skills.len(), 1);
            assert_eq!(skills[0].name, "test-skill");
            assert_eq!(skills[0].description, "A test skill");
            assert!(skills[0].source_path.ends_with("test-skill"));
        });
    }

    #[test]
    fn empty_skills_dir_handled() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "empty_skills_dir_handled";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        // Create the directory but leave it empty
        std::fs::create_dir_all(tmp.path().join(".jyc/skills")).unwrap();

        with_temp_home(tmp.path(), || {
            let svc = make_service(tmp.path().to_path_buf());
            let skills = svc.discover_skills(topic, tmp.path(), None, None);
            assert!(skills.is_empty());
        });
    }

    #[test]
    fn malformed_skill_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "malformed_skill_skipped";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
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
            let skills = svc.discover_skills(topic, tmp.path(), None, None);
            assert_eq!(skills.len(), 1);
            assert_eq!(skills[0].name, "good-skill");
        });
    }

    #[test]
    fn same_name_priority() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "same_name_priority";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        // Create .claude/skills/my-skill/ (lower priority — scanned earlier)
        let claude_dir = tmp.path().join(".claude/skills/my-skill");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(
            claude_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: From claude\n---\n",
        )
        .unwrap();

        // Create .jyc/skills/my-skill/ (higher priority — scanned later, overwrites)
        let state_dir = tmp.path().join(".jyc/skills/my-skill");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: From JYC (overrides)\n---\n",
        )
        .unwrap();

        with_temp_home(tmp.path(), || {
            let svc = make_service(tmp.path().to_path_buf());
            let skills = svc.discover_skills(topic, tmp.path(), None, None);
            assert_eq!(skills.len(), 1);
            // Should take the .jyc version (higher priority)
            assert_eq!(skills[0].description, "From JYC (overrides)");
        });
    }

    #[test]
    fn multi_path_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "multi_path_discovery";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
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
            let skills = svc.discover_skills(topic, tmp.path(), None, None);
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
            time_windows: Vec::new(),
            utc_offset: None,
            long_context: None,
        }
    }

    /// Simulate one LLM call the way `agent_loop` does.
    async fn bank_one_call(
        topic: &str,
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
                topic: "t".to_string(),
                model: "anthropic/claude-opus-4-7".to_string(),
                billing: "metered".into(),
                input_tokens: input,
                output_tokens: output,
                cache_hit_tokens: cache_hit,
                cache_creation_tokens: 0,
                cost,
                currency: p.currency_label().to_string(),
                kind: jyc_core::billing_log_store::KIND_CALL.to_string(),
                input_rate_per_million: 0.0,
                output_rate_per_million: 0.0,
                cache_hit_rate_per_million: 0.0,
                cache_creation_rate_per_million: 0.0,
                time_window: None,
                utc_offset: String::new(),
            },
        )
        .unwrap();
        jyc_agent::session::persist_tokens(
            topic,
            topic_path,
            input,
            0,
            input,
            output,
            cache_hit,
            0,
            0,
            Some(200_000),
            cost,
            0,
            0,
            0,
            0,
            0,
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
        let topic = "three_calls_produce_three_ledger_lines_and_summed_session_cost";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let path = tmp.path();

        let mut expected = 0.0;
        for (input, output, cache) in [(1000, 100, 0), (2500, 250, 800), (4000, 90, 3200)] {
            expected += bank_one_call(topic, path, input, output, cache).await;
        }

        // Ledger: one line per call.
        let entries =
            BillingLogStore::load_date(path, &chrono::Utc::now().format("%Y-%m-%d").to_string());
        assert_eq!(entries.len(), 3, "one ledger line per LLM call");

        // Ledger total and session_cost agree with the computed sum.
        let (ledger_total, currency) = BillingLogStore::today_total(path, "t").unwrap();
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
        let topic = "ledger_survives_session_reset";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let path = tmp.path();

        let first = bank_one_call(topic, path, 5000, 400, 1000).await;

        // Reset semantics, matching `reset_session`: the session file is
        // deleted, then rebuilt with zeroed counters by the next
        // `persist_tokens(.., 0.0, 0, 0, 0, 0)` call.
        tokio::fs::remove_file(path.join(".jyc/agent-session.json"))
            .await
            .unwrap();
        jyc_agent::session::persist_tokens(
            topic,
            path,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            Some(200_000),
            0.0,
            0,
            0,
            0,
            0,
            0,
        )
        .await;

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
        let (ledger_total, _) = BillingLogStore::today_total(path, "t").unwrap();
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

        let topic = "summary_calls_are_billed_and_tagged";

        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let path = tmp.path();
        let p = pricing();

        // One normal call...
        bank_one_call(topic, path, 1000, 100, 0).await;

        // ...and one summary call, billed the way the agent loop does it.
        let summary_cost = jyc_types::pricing::compute_cost(&p, 40_000, 300, 0);
        BillingLogStore::append(
            path,
            &BillingEntry {
                ts: chrono::Utc::now().to_rfc3339(),
                topic: "t".to_string(),
                model: "anthropic/claude-opus-4-7".to_string(),
                billing: "metered".into(),
                input_tokens: 40_000,
                output_tokens: 300,
                cache_hit_tokens: 0,
                cache_creation_tokens: 0,
                cost: summary_cost,
                currency: p.currency_label().to_string(),
                kind: KIND_SUMMARY.to_string(),
                input_rate_per_million: 0.0,
                output_rate_per_million: 0.0,
                cache_hit_rate_per_million: 0.0,
                cache_creation_rate_per_million: 0.0,
                time_window: None,
                utc_offset: String::new(),
            },
        )
        .unwrap();
        jyc_agent::session::add_session_cost(topic, path, summary_cost).await;

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
        let (today, _) = BillingLogStore::today_total(path, "t").unwrap();
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

    /// End-to-end billing behaviour with a `time_windows` config: the
    /// call uses the same `compute_cost_split_with_rates` + BillingEntry
    /// construction as `bill_call`, so a real call inside a window must
    /// persist that window's label and rates into the ledger.
    #[tokio::test]
    async fn banked_entry_inside_window_records_window_label_and_rates() {
        use jyc_types::config::TimeWindowPricing;
        use jyc_types::pricing::compute_cost_split_with_rates;

        let tmp = tempfile::tempdir().unwrap();

        let topic = "banked_entry_inside_window_records_window_label_and_rates";

        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let path = tmp.path();

        let p = ModelPricing {
            // Flat: $15/M in, $75/M out, $1.50/M cache.
            input_per_million: 15.0,
            output_per_million: 75.0,
            cache_hit_per_million: 1.5,
            cache_creation_per_million: None,
            currency: Some("USD".to_string()),
            // Off-peak window covers every hour of every day → the call
            // is guaranteed to land inside it without clock mocking.
            time_windows: vec![TimeWindowPricing {
                start: "00:00".to_string(),
                end: "23:59".to_string(),
                days: Vec::new(),
                input_per_million: 1.0,
                output_per_million: 2.0,
                cache_hit_per_million: Some(0.5),
                cache_creation_per_million: None,
            }],
            utc_offset: None,
            long_context: None,
        };

        let (cost, rates) = compute_cost_split_with_rates(&p, 1000, 100, 0, 0);
        let (time_window, utc_offset) = rates.source.billing_fields();
        BillingLogStore::append(
            path,
            &BillingEntry {
                ts: chrono::Utc::now().to_rfc3339(),
                topic: "t".to_string(),
                model: "anthropic/claude-opus-4-7".to_string(),
                billing: "metered".into(),
                input_tokens: 1000,
                output_tokens: 100,
                cache_hit_tokens: 0,
                cache_creation_tokens: 0,
                cost,
                currency: p.currency_label().to_string(),
                kind: jyc_core::billing_log_store::KIND_CALL.to_string(),
                input_rate_per_million: rates.input_per_million,
                output_rate_per_million: rates.output_per_million,
                cache_hit_rate_per_million: rates.cache_hit_per_million,
                cache_creation_rate_per_million: rates
                    .cache_creation_per_million
                    .unwrap_or(rates.cache_hit_per_million),
                time_window,
                utc_offset,
            },
        )
        .unwrap();

        let entries =
            BillingLogStore::load_date(path, &chrono::Utc::now().format("%Y-%m-%d").to_string());
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        // Window rates persisted, not the flat ones.
        assert!((e.input_rate_per_million - 1.0).abs() < 1e-9);
        assert!((e.output_rate_per_million - 2.0).abs() < 1e-9);
        assert!((e.cache_hit_rate_per_million - 0.5).abs() < 1e-9);
        assert_eq!(e.time_window.as_deref(), Some("00:00-23:59"));
        assert_eq!(e.utc_offset, "");
        // Cost matches window rate: 1000 * 1 + 100 * 2 = 1200; / 1e6 = 1.2e-3.
        assert!((e.cost - 1.2e-3).abs() < 1e-9, "got {}", e.cost);
    }

    /// `add_session_cost` must not disturb the token counters -- summary
    /// tokens are real spend but are NOT part of the main loop's context
    /// accounting, and folding them in would inflate the context usage
    /// shown to the user. The cost, however, is the real spend of a call
    /// that happens inside the turn, so it lands in both cost scopes.
    #[tokio::test]
    async fn add_session_cost_leaves_token_counters_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "add_session_cost_leaves_token_counters_untouched";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let path = tmp.path();

        bank_one_call(topic, path, 5000, 400, 1000).await;
        let before: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(path.join(".jyc/agent-session.json"))
                .await
                .unwrap(),
        )
        .unwrap();

        jyc_agent::session::add_session_cost(topic, path, 0.25).await;

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
            "context_window",
        ] {
            assert_eq!(before[field], after[field], "{field} must not change");
        }

        // Both cost fields move: a summary call happens inside the turn,
        // so its spend is the turn's too.
        for field in ["session_cost", "turn_cost"] {
            let delta = after[field].as_f64().unwrap() - before[field].as_f64().unwrap();
            assert!(
                (delta - 0.25).abs() < 1e-9,
                "{field} moves by the call cost"
            );
        }
    }

    /// A zero / failed summary call must not write a ledger line.
    #[tokio::test]
    async fn zero_cost_summary_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "zero_cost_summary_writes_nothing";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let path = tmp.path();
        jyc_agent::session::add_session_cost(topic, path, 0.0).await;
        assert!(
            BillingLogStore::today_total(path, "t").is_none(),
            "no ledger entry for a zero-cost call"
        );
    }
}
