use super::*;
use crate::message_storage::MessageStorage;
use crate::metrics::MetricsCollector;
use crate::static_agent::StaticAgentService;
use jyc_types::{ChangeKind, ChangedFileEntry, PatternMatch};
use std::collections::HashMap;
use tempfile::tempdir;

/// Minimal outbound adapter that does nothing.
struct NoopOutbound;

#[async_trait::async_trait]
impl jyc_types::OutboundAdapter for NoopOutbound {
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
        _original: &InboundMessage,
        _reply_text: &str,
        _topic_path: &Path,
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

fn make_test_tm(workspace: &std::path::Path) -> Arc<TopicManager> {
    make_test_tm_with_config(
        workspace,
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
}

fn make_test_tm_with_config(workspace: &std::path::Path, config_str: &str) -> Arc<TopicManager> {
    let storage = Arc::new(MessageStorage::new(workspace));
    let cancel = CancellationToken::new();
    let metrics_cancel = CancellationToken::new();
    let (metrics, _stats, _metrics_task) = MetricsCollector::new(metrics_cancel).start();
    let config = Arc::new(ArcSwap::from_pointee(
        jyc_types::load_config_from_str(config_str).unwrap(),
    ));

    Arc::new(TopicManager::new_with_options(
        1,
        10,
        storage,
        Arc::new(NoopOutbound),
        Arc::new(StaticAgentService::new("ok")),
        cancel,
        true,
        workspace.join("templates"),
        config,
        "test-channel".to_string(),
        "websocket".to_string(),
        workspace.parent().unwrap_or(workspace).to_path_buf(),
        workspace.to_path_buf(),
        metrics,
        None,
    ))
}

#[tokio::test]
async fn test_has_active_queue_false_for_unknown_topic() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);
    assert!(!tm.has_active_queue("nonexistent").await);
}

#[tokio::test]
async fn test_has_active_queue_true_after_enqueue() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    // Create a topic directory so list_topics finds it
    let topic_path = workspace.join("test-topic");
    tokio::fs::create_dir_all(topic_path.join(".jyc"))
        .await
        .unwrap();

    // Enqueue a dummy message — this creates an mpsc queue
    let msg = InboundMessage {
        id: "test".to_string(),
        channel: "test-channel".to_string(),
        channel_uid: "test".to_string(),
        sender: "user".to_string(),
        sender_address: "user".to_string(),
        recipients: vec![],
        topic: "test".to_string(),
        content: jyc_types::MessageContent {
            text: Some("hello".to_string()),
            html: None,
            markdown: None,
        },
        timestamp: chrono::Utc::now(),
        references: None,
        reply_to_id: None,
        external_id: None,
        attachments: vec![],
        metadata: HashMap::new(),
        matched_pattern: None,
    };
    let pattern_match = PatternMatch {
        pattern_name: "test".to_string(),
        channel: "websocket".to_string(),
        matches: HashMap::new(),
    };
    tm.enqueue(
        msg,
        "test-topic".to_string(),
        pattern_match,
        None,
        false,
        None,
    )
    .await;

    // Give the worker a moment to start
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert!(
        tm.has_active_queue("test-topic").await,
        "Topic should have an active queue after enqueue"
    );

    // Clean up
    tm.shutdown().await;
}

/// Regression test (#542): injected messages (jyc_send_to_topic,
/// dashboard topic proxy) carry an empty pattern_name. The worker must
/// not overwrite `.jyc/pattern` with it, or the dashboard loses the
/// topic's pattern identity until a router-matched message rewrites it.
#[tokio::test]
async fn test_empty_pattern_name_does_not_clobber_pattern_file() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    let make_msg = || InboundMessage {
        id: uuid::Uuid::new_v4().to_string(),
        channel: "test-channel".to_string(),
        channel_uid: "test".to_string(),
        sender: "user".to_string(),
        sender_address: "user".to_string(),
        recipients: vec![],
        topic: "test".to_string(),
        content: jyc_types::MessageContent {
            text: Some("hello".to_string()),
            html: None,
            markdown: None,
        },
        timestamp: chrono::Utc::now(),
        references: None,
        reply_to_id: None,
        external_id: None,
        attachments: vec![],
        metadata: HashMap::new(),
        matched_pattern: None,
    };
    let make_pm = |name: &str| PatternMatch {
        pattern_name: name.to_string(),
        channel: "websocket".to_string(),
        matches: HashMap::new(),
    };

    // Router-matched message writes the real pattern name.
    tm.enqueue(
        make_msg(),
        "test-topic".to_string(),
        make_pm("jyc"),
        None,
        false,
        None,
    )
    .await;
    let topic_path = workspace.join("test-topic");
    assert!(
        wait_for_history_lines(&topic_path, 1).await,
        "worker did not process the first message in time"
    );
    let pattern_file = topic_path.join(".jyc").join("pattern");
    assert_eq!(
        tokio::fs::read_to_string(&pattern_file).await.unwrap(),
        "jyc"
    );

    // Injected message with empty pattern_name must leave the file
    // alone. Wait until the worker provably processed it (second chat
    // history line) before asserting — otherwise a slow worker would
    // let the test pass even without the guard.
    tm.enqueue(
        make_msg(),
        "test-topic".to_string(),
        make_pm(""),
        None,
        false,
        None,
    )
    .await;
    assert!(
        wait_for_history_lines(&topic_path, 2).await,
        "worker did not process the injected message in time"
    );
    assert_eq!(
        tokio::fs::read_to_string(&pattern_file).await.unwrap(),
        "jyc"
    );

    tm.shutdown().await;
}

/// Poll until the topic's chat history holds at least `n` lines
/// (i.e. the worker processed `n` messages). ~2s timeout.
async fn wait_for_history_lines(topic_path: &std::path::Path, n: usize) -> bool {
    for _ in 0..40 {
        let (files, _) = crate::chat_log_store::list_chat_history_files(topic_path);
        let mut count = 0;
        for f in files {
            if let Ok(content) = tokio::fs::read_to_string(&f).await {
                count += content.lines().filter(|l| !l.trim().is_empty()).count();
            }
        }
        if count >= n {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    false
}

/// `pattern_for_topic` (#542): resolves the enabled pattern named after
/// the topic, including its template/role/custom `topic_path`; returns
/// None for unknown/disabled names so injection falls back to an empty
/// pattern.
#[tokio::test]
async fn test_pattern_for_topic() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let custom_path = tmp.path().join("custom-jyc");

    let config_str = format!(
        r#"
[general]
[channels.test-channel]
type = "websocket"
[[channels.test-channel.patterns]]
name = "jyc"
enabled = true
topic_path = "{}"
template = "dev"
role = "Developer"
[channels.test-channel.patterns.rules]
[[channels.test-channel.patterns]]
name = "disabled"
enabled = false
topic_path = "/nowhere"
[channels.test-channel.patterns.rules]
[agent]
enabled = true
mode = "agent"
"#,
        custom_path.display()
    );
    let tm = make_test_tm_with_config(&workspace, &config_str);

    let p = tm.pattern_for_topic("jyc").expect("pattern should resolve");
    assert_eq!(p.name, "jyc");
    assert_eq!(p.topic_path.as_deref(), Some(custom_path.to_str().unwrap()));
    assert_eq!(p.template.as_deref(), Some("dev"));
    assert_eq!(p.role.as_deref(), Some("Developer"));
    assert!(p.live_injection);
    assert!(tm.pattern_for_topic("disabled").is_none());
    assert!(tm.pattern_for_topic("unknown").is_none());
}

/// Regression test: the per-worker clone must share the parent's
/// `topic_cancels` map. Previously the clone got a fresh empty map, so
/// `cancel_topic` (invoked via /cancel through the command registry,
/// which holds the clone) never found the running worker's token — the
/// user got a success reply but the agent kept running.
#[tokio::test]
async fn test_cancel_topic_via_worker_clone_really_cancels() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    // Simulate an active worker token registered in the shared map
    let token = CancellationToken::new();
    {
        let mut cancels = tm.topic_cancels.lock().await;
        cancels.insert("test-topic".to_string(), token.clone());
    }

    // Cancel through the worker clone — the path /cancel actually takes
    let clone = tm.worker_clone();
    assert!(
        clone.cancel_topic("test-topic").await,
        "cancel_topic via worker clone must find the shared token"
    );
    assert!(token.is_cancelled());

    // Unknown topic must report "nothing cancelled"
    assert!(!clone.cancel_topic("no-such-topic").await);
}

#[tokio::test]
async fn test_publish_incoming_message_on_event_bus() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    // Create event bus manually so we can subscribe
    let bus = tm.get_or_create_event_bus("test-topic").await.unwrap();
    let mut rx = bus.subscribe().await.unwrap();

    // Publish incoming message event
    tm.publish_incoming_message("test-topic", "user", "hello world")
        .await;

    let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("should receive event within timeout")
        .expect("should have an event");

    match event {
        crate::topic_event::TopicEvent::IncomingMessage {
            topic_name,
            sender,
            text,
            ..
        } => {
            assert_eq!(topic_name, "test-topic");
            assert_eq!(sender, "user");
            assert_eq!(text, "hello world");
        }
        other => panic!("expected IncomingMessage, got {:?}", other),
    }

    tm.shutdown().await;
}

#[tokio::test]
async fn test_publish_reply_sent_on_event_bus() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    // Create event bus manually so we can subscribe
    let bus = tm.get_or_create_event_bus("test-topic").await.unwrap();
    let mut rx = bus.subscribe().await.unwrap();

    // Publish reply sent event
    tm.publish_reply_sent("test-topic", "AI reply here").await;

    let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("should receive event within timeout")
        .expect("should have an event");

    match event {
        crate::topic_event::TopicEvent::ReplySent {
            topic_name, text, ..
        } => {
            assert_eq!(topic_name, "test-topic");
            assert_eq!(text, "AI reply here");
        }
        other => panic!("expected ReplySent, got {:?}", other),
    }

    tm.shutdown().await;
}

#[tokio::test]
async fn test_publish_incoming_message_noop_without_event_bus() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    // No event bus created — publish should silently succeed (no panic)
    tm.publish_incoming_message("test-topic", "user", "hello")
        .await;

    tm.shutdown().await;
}

#[tokio::test]
async fn test_topic_meta_written_on_first_message() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    let topic_path = workspace.join("test-topic");
    tokio::fs::create_dir_all(topic_path.join(".jyc"))
        .await
        .unwrap();

    let mut metadata = HashMap::new();
    metadata.insert("github_number".to_string(), serde_json::json!(42));

    let msg = InboundMessage {
        id: "test".to_string(),
        channel: "test-channel".to_string(),
        channel_uid: "test-uid".to_string(),
        sender: "user".to_string(),
        sender_address: "user".to_string(),
        recipients: vec![],
        topic: "test".to_string(),
        content: jyc_types::MessageContent {
            text: Some("hello".to_string()),
            html: None,
            markdown: None,
        },
        timestamp: chrono::Utc::now(),
        references: Some(vec!["ref-1".to_string()]),
        reply_to_id: None,
        external_id: Some("ext-123".to_string()),
        attachments: vec![],
        metadata,
        matched_pattern: None,
    };
    let pattern_match = PatternMatch {
        pattern_name: "test".to_string(),
        channel: "websocket".to_string(),
        matches: HashMap::new(),
    };
    tm.enqueue(
        msg,
        "test-topic".to_string(),
        pattern_match,
        None,
        false,
        None,
    )
    .await;

    // Give the worker a moment to process
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Check topic-meta.json was written
    let meta_path = topic_path.join(".jyc").join("topic-meta.json");
    assert!(meta_path.exists(), "topic-meta.json should be written");

    let content = std::fs::read_to_string(&meta_path).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(meta["channel_uid"], "test-uid");
    assert_eq!(meta["external_id"], "ext-123");
    assert_eq!(meta["references"], serde_json::json!(["ref-1"]));
    assert_eq!(meta["metadata"]["github_number"], 42);

    tm.shutdown().await;
}

#[tokio::test]
async fn test_topic_meta_not_overwritten_on_second_message() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    let topic_path = workspace.join("test-topic");
    tokio::fs::create_dir_all(topic_path.join(".jyc"))
        .await
        .unwrap();

    // Pre-write a topic-meta.json with a known value
    let meta_path = topic_path.join(".jyc").join("topic-meta.json");
    std::fs::write(
        &meta_path,
        r#"{"channel_uid":"original-uid","metadata":{"github_number":99}}"#,
    )
    .unwrap();

    let msg = InboundMessage {
        id: "test".to_string(),
        channel: "test-channel".to_string(),
        channel_uid: "new-uid".to_string(),
        sender: "user".to_string(),
        sender_address: "user".to_string(),
        recipients: vec![],
        topic: "test".to_string(),
        content: jyc_types::MessageContent {
            text: Some("hello".to_string()),
            html: None,
            markdown: None,
        },
        timestamp: chrono::Utc::now(),
        references: None,
        reply_to_id: None,
        external_id: None,
        attachments: vec![],
        metadata: HashMap::new(),
        matched_pattern: None,
    };
    let pattern_match = PatternMatch {
        pattern_name: "test".to_string(),
        channel: "websocket".to_string(),
        matches: HashMap::new(),
    };
    tm.enqueue(
        msg,
        "test-topic".to_string(),
        pattern_match,
        None,
        false,
        None,
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Should NOT be overwritten — still has original values
    let content = std::fs::read_to_string(&meta_path).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(meta["channel_uid"], "original-uid");
    assert_eq!(meta["metadata"]["github_number"], 99);

    tm.shutdown().await;
}

#[tokio::test]
async fn test_topic_meta_not_written_for_dashboard_channel_uid() {
    // Dashboard-injected messages have channel_uid == "dashboard" and empty
    // metadata. Writing topic-meta.json for these would poison subsequent
    // injections — the empty metadata would be re-used and real routing data
    // (e.g. github_number) would be lost, causing 404 errors on replies.
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    let topic_path = workspace.join("test-topic");
    let mut metadata = HashMap::new();
    metadata.insert("github_number".to_string(), serde_json::json!(42));

    let msg = InboundMessage {
        id: "test".to_string(),
        channel: "test-channel".to_string(),
        channel_uid: "dashboard".to_string(),
        sender: "user".to_string(),
        sender_address: "user".to_string(),
        recipients: vec![],
        topic: "test".to_string(),
        content: jyc_types::MessageContent {
            text: Some("hello".to_string()),
            html: None,
            markdown: None,
        },
        timestamp: chrono::Utc::now(),
        references: Some(vec!["ref-1".to_string()]),
        reply_to_id: None,
        external_id: Some("ext-123".to_string()),
        attachments: vec![],
        metadata,
        matched_pattern: None,
    };
    let pattern_match = PatternMatch {
        pattern_name: "test".to_string(),
        channel: "websocket".to_string(),
        matches: HashMap::new(),
    };
    tm.enqueue(
        msg,
        "test-topic".to_string(),
        pattern_match,
        None,
        false,
        None,
    )
    .await;

    // Give the worker a moment to process
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // topic-meta.json must NOT be written for dashboard messages
    let meta_path = topic_path.join(".jyc").join("topic-meta.json");
    assert!(
        !meta_path.exists(),
        "topic-meta.json should NOT be written for dashboard channel_uid"
    );

    tm.shutdown().await;
}

#[tokio::test]
async fn test_topic_path_returns_custom_override() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    let custom_path = tmp.path().join("custom-topics").join("my-topic");
    tm.topic_paths
        .lock()
        .await
        .insert("my-topic".to_string(), custom_path.clone());

    let resolved = tm.topic_path("my-topic").await;
    assert_eq!(resolved, Some(custom_path));

    tm.shutdown().await;
}

#[tokio::test]
async fn test_topic_path_falls_back_to_default() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    // Create topic dir at default location
    let default_path = workspace.join("default-topic");
    tokio::fs::create_dir_all(&default_path).await.unwrap();

    // No custom path stored — should fall back to workspace/topic_name
    let resolved = tm.topic_path("default-topic").await;
    assert_eq!(resolved, Some(default_path));

    tm.shutdown().await;
}

#[tokio::test]
async fn test_topic_path_returns_none_for_nonexistent() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    let resolved = tm.topic_path("nonexistent").await;
    assert_eq!(resolved, None);

    tm.shutdown().await;
}

#[tokio::test]
async fn test_custom_topic_paths_empty_initially() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    let paths = tm.custom_topic_paths().await;
    assert!(paths.is_empty());

    tm.shutdown().await;
}

/// Regression test for the `jyc open` ad-hoc topic timeout:
///
/// `set_topic_path` must create `.jyc/topic-name` so that the
/// `path.join(".jyc").is_dir()` filter in `list_topics` keeps the
/// entry. Without it, a freshly-registered ad-hoc topic is dropped
/// from the overview and `wait_for_topic` in `run_open` times out
/// with "Timeout waiting for topic ... to be created".
#[tokio::test]
async fn test_set_topic_path_creates_jyc_dir_and_appears_in_list() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    let custom_path = tmp.path().join("adhoc-projects");
    tm.set_topic_path("projects", custom_path.clone())
        .await
        .unwrap();

    // `.jyc/` and `.jyc/topic-name` are written by set_topic_path so
    // list_topics doesn't filter the entry out.
    assert!(
        custom_path.join(".jyc").is_dir(),
        "set_topic_path must create .jyc/"
    );
    assert_eq!(
        tokio::fs::read_to_string(custom_path.join(".jyc").join("topic-name"))
            .await
            .unwrap()
            .trim(),
        "projects",
        "set_topic_path must write .jyc/topic-name"
    );

    // The new ad-hoc topic appears in list_topics so the dashboard
    // overview reports it within the 5s wait_for_topic window.
    let topics = tm.list_topics().await;
    assert!(
        topics.iter().any(|t| t.name == "projects"),
        "ad-hoc topic should appear in list_topics"
    );

    tm.shutdown().await;
}

#[tokio::test]
async fn test_restore_custom_topic_paths_from_disk() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    // Custom topic path outside workspace
    let custom_path = tmp.path().join("external-project");
    tokio::fs::create_dir_all(custom_path.join(".jyc"))
        .await
        .unwrap();
    // Simulate a previously initialized topic
    tokio::fs::write(
        custom_path.join(".jyc").join("topic-name"),
        "my-custom-topic",
    )
    .await
    .unwrap();

    // Config with topic_path override — channel name must match TM's channel_name
    let config_str = format!(
        r#"
[general]
[channels.test-channel]
type = "email"
[channels.test-channel.inbound]
host = "h"
port = 998
username = "u"
password = "p"
[channels.test-channel.outbound]
host = "h"
port = 465
username = "u"
password = "p"
[[channels.test-channel.patterns]]
name = "test-pattern"
topic_path = "{}"
[channels.test-channel.patterns.rules]
[agent]
enabled = true
mode = "agent"
"#,
        custom_path.display()
    );

    let config = Arc::new(ArcSwap::from_pointee(
        jyc_types::load_config_from_str(&config_str).unwrap(),
    ));

    let storage = Arc::new(MessageStorage::new(&workspace));
    let cancel = CancellationToken::new();
    let metrics_cancel = CancellationToken::new();
    let (metrics, _stats, _metrics_task) = MetricsCollector::new(metrics_cancel).start();

    let tm = Arc::new(TopicManager::new_with_options(
        1,
        10,
        storage,
        Arc::new(NoopOutbound),
        Arc::new(StaticAgentService::new("ok")),
        cancel,
        true,
        workspace.join("templates"),
        config,
        "test-channel".to_string(),
        "websocket".to_string(),
        workspace.parent().unwrap_or(&workspace).to_path_buf(),
        workspace.to_path_buf(),
        metrics,
        None,
    ));

    // Before restore: empty
    let paths = tm.custom_topic_paths().await;
    assert!(paths.is_empty());

    // Restore from disk
    tm.restore_custom_topic_paths().await;

    // After restore: mapping exists
    let paths = tm.custom_topic_paths().await;
    assert_eq!(
        paths.get("my-custom-topic"),
        Some(&custom_path),
        "restore_custom_topic_paths should rediscover the topic"
    );

    // list_topics should now include the restored topic
    let topics = tm.list_topics().await;
    let names: Vec<&str> = topics.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names.contains(&"my-custom-topic"),
        "list_topics should include restored custom-path topic"
    );

    // Event bus should be pre-created so ActivityTracker can subscribe
    // before the first message arrives (avoids lost first-message events).
    let bus = tm.get_event_bus("my-custom-topic").await;
    assert!(
        bus.is_some(),
        "restore_custom_topic_paths should pre-create event bus"
    );

    tm.shutdown().await;
}

#[tokio::test]
async fn test_restore_skips_missing_topic_name_file() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    // Custom path exists but has no .jyc/topic-name file
    let custom_path = tmp.path().join("uninitialized");
    tokio::fs::create_dir_all(&custom_path).await.unwrap();

    let config_str = format!(
        r#"
[general]
[channels.test-channel]
type = "email"
[channels.test-channel.inbound]
host = "h"
port = 998
username = "u"
password = "p"
[channels.test-channel.outbound]
host = "h"
port = 465
username = "u"
password = "p"
[[channels.test-channel.patterns]]
name = "test-pattern"
topic_path = "{}"
[channels.test-channel.patterns.rules]
[agent]
enabled = true
mode = "agent"
"#,
        custom_path.display()
    );

    let config = Arc::new(ArcSwap::from_pointee(
        jyc_types::load_config_from_str(&config_str).unwrap(),
    ));

    let storage = Arc::new(MessageStorage::new(&workspace));
    let cancel = CancellationToken::new();
    let metrics_cancel = CancellationToken::new();
    let (metrics, _stats, _metrics_task) = MetricsCollector::new(metrics_cancel).start();

    let tm = Arc::new(TopicManager::new_with_options(
        1,
        10,
        storage,
        Arc::new(NoopOutbound),
        Arc::new(StaticAgentService::new("ok")),
        cancel,
        true,
        workspace.join("templates"),
        config,
        "test-channel".to_string(),
        "websocket".to_string(),
        workspace.parent().unwrap_or(&workspace).to_path_buf(),
        workspace.to_path_buf(),
        metrics,
        None,
    ));

    tm.restore_custom_topic_paths().await;

    // Should be empty — no topic-name file
    let paths = tm.custom_topic_paths().await;
    assert!(
        paths.is_empty(),
        "Should skip paths without topic-name file"
    );

    tm.shutdown().await;
}

#[tokio::test]
async fn test_list_topics_cleans_stale_custom_path() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    // Insert a custom path that doesn't exist on disk
    let ghost_path = tmp.path().join("deleted-topic");
    tokio::fs::create_dir_all(ghost_path.join(".jyc"))
        .await
        .unwrap();
    tm.topic_paths
        .lock()
        .await
        .insert("ghost".to_string(), ghost_path.clone());

    // list_topics should include it while dir exists
    let topics = tm.list_topics().await;
    assert!(
        topics.iter().any(|t| t.name == "ghost"),
        "Should list topic while directory exists"
    );

    // Delete the directory
    tokio::fs::remove_dir_all(&ghost_path).await.unwrap();

    // list_topics should now clean it up
    let topics = tm.list_topics().await;
    assert!(
        !topics.iter().any(|t| t.name == "ghost"),
        "Should not list topic after directory deleted"
    );

    // topic_paths map should no longer contain the entry
    let paths = tm.custom_topic_paths().await;
    assert!(
        !paths.contains_key("ghost"),
        "Stale entry should be removed from topic_paths"
    );

    tm.shutdown().await;
}

/// Multi-topic-per-agent restore: when `pattern.topic_path` is the
/// agent root, each subdirectory `<agent>/<topic>/.jyc/topic-name`
/// must be rediscovered on startup. This is the layout used by the
/// synthesized "agents" channel.
#[tokio::test]
async fn test_restore_multi_topic_per_agent_layout() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    // Agent root sits OUTSIDE the workspace (it's the
    // `<data_home>/agents/<agent_name>/` dir).
    let agent_root = tmp.path().join("agents").join("jyc");
    let topic_a = agent_root.join("topic-a");
    let topic_b = agent_root.join("topic-b");
    for t in [&topic_a, &topic_b] {
        tokio::fs::create_dir_all(t.join(".jyc")).await.unwrap();
        tokio::fs::write(
            t.join(".jyc").join("topic-name"),
            t.file_name().unwrap().to_str().unwrap(),
        )
        .await
        .unwrap();
    }

    // Config: pattern's topic_path points at the agent root, with
    // no legacy single-topic `.jyc/topic-name` at the root itself.
    let config_str = format!(
        r#"
[general]
[channels.test-channel]
type = "websocket"
[[channels.test-channel.patterns]]
name = "jyc"
topic_path = "{}"
[ai]
enabled = true
mode = "agent"
"#,
        agent_root.display()
    );

    let config = Arc::new(ArcSwap::from_pointee(
        jyc_types::load_config_from_str(&config_str).unwrap(),
    ));

    let storage = Arc::new(MessageStorage::new(&workspace));
    let cancel = CancellationToken::new();
    let metrics_cancel = CancellationToken::new();
    let (metrics, _stats, _metrics_task) = MetricsCollector::new(metrics_cancel).start();

    let tm = Arc::new(TopicManager::new_with_options(
        1,
        10,
        storage,
        Arc::new(NoopOutbound),
        Arc::new(StaticAgentService::new("ok")),
        cancel,
        true,
        workspace.join("templates"),
        config,
        "test-channel".to_string(),
        "websocket".to_string(),
        workspace.parent().unwrap_or(&workspace).to_path_buf(),
        workspace.to_path_buf(),
        metrics,
        None,
    ));

    tm.restore_custom_topic_paths().await;

    let paths = tm.custom_topic_paths().await;
    assert_eq!(
        paths.get("topic-a"),
        Some(&topic_a),
        "multi-topic restore: topic-a should be rediscovered"
    );
    assert_eq!(
        paths.get("topic-b"),
        Some(&topic_b),
        "multi-topic restore: topic-b should be rediscovered"
    );

    // list_topics must include both rediscovered topics.
    let topics = tm.list_topics().await;
    let names: Vec<&str> = topics.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"topic-a"), "list_topics: {names:?}");
    assert!(names.contains(&"topic-b"), "list_topics: {names:?}");

    // Event bus pre-create for both.
    assert!(tm.get_event_bus("topic-a").await.is_some());
    assert!(tm.get_event_bus("topic-b").await.is_some());

    tm.shutdown().await;
}

/// Same multi-topic-per-agent restore, but for an agent with **no**
/// configured `topic_path`: its root is `<agents-workspace>/<agent>/`,
/// derived rather than configured. Without this the nested topics are
/// invisible after a restart.
#[tokio::test]
async fn test_restore_agent_default_root_without_topic_path() {
    let tmp = tempdir().unwrap();
    // The agents channel's workspace is `<data_home>/agents/`.
    let workspace = tmp.path().join("agents");
    let agent_root = workspace.join("planner");
    let topic = agent_root.join("plan-197");
    tokio::fs::create_dir_all(topic.join(".jyc")).await.unwrap();
    tokio::fs::write(topic.join(".jyc").join("topic-name"), "plan-197")
        .await
        .unwrap();

    let config_str = r#"
[general]
[channels.agents]
type = "websocket"
[[channels.agents.patterns]]
name = "planner"
[channels.agents.patterns.rules]
[ai]
enabled = true
mode = "agent"
"#;

    let config = Arc::new(ArcSwap::from_pointee(
        jyc_types::load_config_from_str(config_str).unwrap(),
    ));
    let storage = Arc::new(MessageStorage::new(&workspace));
    let cancel = CancellationToken::new();
    let metrics_cancel = CancellationToken::new();
    let (metrics, _stats, _metrics_task) = MetricsCollector::new(metrics_cancel).start();

    let tm = Arc::new(TopicManager::new_with_options(
        1,
        10,
        storage,
        Arc::new(NoopOutbound),
        Arc::new(StaticAgentService::new("ok")),
        cancel,
        true,
        workspace.join("templates"),
        config,
        "agents".to_string(),
        "websocket".to_string(),
        tmp.path().to_path_buf(),
        workspace.clone(),
        metrics,
        None,
    ));

    tm.restore_custom_topic_paths().await;

    assert_eq!(
        tm.custom_topic_paths().await.get("plan-197"),
        Some(&topic),
        "agent without topic_path: nested topic must survive restart"
    );

    tm.shutdown().await;
}

/// Build a TM whose config prices `cnprov/m1` in CNY, so
/// `list_topics` has a real pricing entry to resolve a currency from.
fn make_priced_tm(workspace: &std::path::Path) -> Arc<TopicManager> {
    let storage = Arc::new(MessageStorage::new(workspace));
    let cancel = CancellationToken::new();
    let metrics_cancel = CancellationToken::new();
    let (metrics, _stats, _metrics_task) = MetricsCollector::new(metrics_cancel).start();
    let config = Arc::new(ArcSwap::from_pointee(
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
model = "cnprov/m1"
[agent.providers.cnprov]
type = "openai-compatible"
[agent.providers.cnprov.models.m1]
pricing = { input_per_million = 3.0, output_per_million = 4.0, cache_hit_per_million = 0.5, currency = "CNY" }
"#,
            )
            .unwrap(),
        ));

    Arc::new(TopicManager::new_with_options(
        1,
        10,
        storage,
        Arc::new(NoopOutbound),
        Arc::new(StaticAgentService::new("ok")),
        cancel,
        true,
        workspace.join("templates"),
        config,
        "test-channel".to_string(),
        "websocket".to_string(),
        workspace.parent().unwrap_or(workspace).to_path_buf(),
        workspace.to_path_buf(),
        metrics,
        None,
    ))
}

/// Regression: a session carrying spend across UTC midnight has a
/// non-zero `session_cost` but an empty ledger for the new day. The
/// currency must still come from the model's configured pricing —
/// previously it fell back to DEFAULT_CURRENCY, labelling a CNY
/// amount with the wrong unit.
#[tokio::test]
async fn list_topics_currency_from_config_when_ledger_empty() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    let topic = workspace.join("t1");
    std::fs::create_dir_all(topic.join(".jyc")).unwrap();
    // Session has spend; no bill-<today>.jsonl exists at all.
    std::fs::write(
        topic.join(".jyc/agent-session.json"),
        r#"{"session_cost":0.05,"context_input_tokens":10}"#,
    )
    .unwrap();

    let tm = make_priced_tm(&workspace);
    let topics = tm.list_topics().await;
    let t = topics
        .iter()
        .find(|t| t.name == "t1")
        .expect("topic listed");
    let cost = t.cost.as_ref().expect("cost present when session_cost > 0");

    assert_eq!(
        cost.currency, "CNY",
        "currency must come from pricing config"
    );
    assert!((cost.session - 0.05).abs() < 1e-9);
    assert_eq!(cost.today, 0.0, "no ledger entries today");
    tm.shutdown().await;
}

/// A multi-currency day is the one case where the ledger's own label
/// wins: config can only name one currency, but the ledger knows the
/// topic actually spent in two.
#[tokio::test]
async fn list_topics_preserves_mixed_currency_from_ledger() {
    use crate::billing_log_store::{BillingEntry, BillingLogStore, MIXED_CURRENCY};

    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    let topic = workspace.join("t1");
    std::fs::create_dir_all(topic.join(".jyc")).unwrap();

    for (cost, currency) in [(1.0, "CNY"), (2.0, "USD")] {
        BillingLogStore::append(
            &topic,
            &BillingEntry {
                ts: chrono::Utc::now().to_rfc3339(),
                model: "cnprov/m1".to_string(),
                input_tokens: 100,
                output_tokens: 10,
                cache_hit_tokens: 0,
                cache_creation_tokens: 0,
                cost,
                currency: currency.to_string(),
                kind: crate::billing_log_store::KIND_CALL.to_string(),
                input_rate_per_million: 0.0,
                output_rate_per_million: 0.0,
                cache_hit_rate_per_million: 0.0,
                time_window: None,
                utc_offset: String::new(),
            },
        )
        .unwrap();
    }

    let tm = make_priced_tm(&workspace);
    let topics = tm.list_topics().await;
    let t = topics
        .iter()
        .find(|t| t.name == "t1")
        .expect("topic listed");
    let cost = t.cost.as_ref().expect("cost present");

    assert_eq!(
        cost.currency, MIXED_CURRENCY,
        "ledger's mixed marker must survive, not be replaced by config"
    );
    assert!((cost.today - 3.0).abs() < 1e-9);
    tm.shutdown().await;
}

/// Regression for #512: `TopicManager::list_topics` must populate
/// `TopicInfo::branch` by reading `.git/HEAD` under each topic's
/// path. Without this test, a future refactor that drops the call to
/// `branch_for_topic_path` at the `topics.push(...)` site would
/// silently leave `branch == None` on every payload.
#[tokio::test]
async fn list_topics_populates_branch_from_dot_git_head() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");

    // Topic "main-test" with a symbolic-ref HEAD pointing at main.
    let t1 = workspace.join("main-test");
    std::fs::create_dir_all(t1.join(".jyc")).unwrap();
    std::fs::create_dir_all(t1.join(".git")).unwrap();
    std::fs::write(t1.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

    // Topic "detached-test" with a raw 40-char SHA — should appear
    // as "(detached)" rather than as `None`.
    let t2 = workspace.join("detached-test");
    std::fs::create_dir_all(t2.join(".jyc")).unwrap();
    std::fs::create_dir_all(t2.join(".git")).unwrap();
    std::fs::write(
        t2.join(".git/HEAD"),
        "0123456789abcdef0123456789abcdef01234567",
    )
    .unwrap();

    // Topic "no-git" — `.jyc` exists but no `.git/HEAD`. Branch
    // should be `None` (renderer skips the row).
    let t3 = workspace.join("no-git");
    std::fs::create_dir_all(t3.join(".jyc")).unwrap();

    let tm = make_test_tm(&workspace);
    let topics = tm.list_topics().await;

    let by_name = |n: &str| {
        topics
            .iter()
            .find(|t| t.name == n)
            .unwrap_or_else(|| panic!("topic {n} missing from list_topics"))
    };

    assert_eq!(
        by_name("main-test").branch.as_deref(),
        Some("main"),
        "symbolic-ref branch must be resolved"
    );
    assert_eq!(
        by_name("detached-test").branch.as_deref(),
        Some("(detached)"),
        "raw SHA must surface as (detached)"
    );
    assert!(
        by_name("no-git").branch.is_none(),
        "non-git topic must have branch=None"
    );

    tm.shutdown().await;
}

/// Regression for #220: `TopicManager::list_topics` must populate
/// `TopicInfo::changed_files` by running `git diff --name-only
/// main...HEAD` under each topic's path. Without this test, a
/// future refactor that drops the call to
/// `changed_files_for_topic_path` at the `topics.push(...)` site
/// would silently leave `changed_files == None` on every payload.
#[tokio::test]
async fn list_topics_populates_changed_files_from_git_diff() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");

    // Topic "clean": real git repo on `main` with no commits ahead.
    // Expect `Some(vec![])`.
    let clean = workspace.join("clean");
    std::fs::create_dir_all(clean.join(".jyc")).unwrap();
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&clean)
            .output()
            .expect("git failed")
    };
    run(&["init", "-q", "-b", "main"]);
    run(&[
        "-c",
        "user.email=t@e",
        "-c",
        "user.name=t",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "init",
    ]);

    // Topic "ahead": feature branch with one commit adding "x.rs".
    let ahead = workspace.join("ahead");
    std::fs::create_dir_all(ahead.join(".jyc")).unwrap();
    let run_ahead = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&ahead)
            .output()
            .expect("git failed")
    };
    run_ahead(&["init", "-q", "-b", "main"]);
    run_ahead(&[
        "-c",
        "user.email=t@e",
        "-c",
        "user.name=t",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "init",
    ]);
    run_ahead(&["checkout", "-q", "-b", "feature"]);
    std::fs::write(ahead.join("x.rs"), "fn x() {}").unwrap();
    run_ahead(&["add", "x.rs"]);
    run_ahead(&[
        "-c",
        "user.email=t@e",
        "-c",
        "user.name=t",
        "commit",
        "-q",
        "-m",
        "x",
    ]);

    // Topic "no-git": no `.git` at all → `changed_files == None`.
    let no_git = workspace.join("no-git");
    std::fs::create_dir_all(no_git.join(".jyc")).unwrap();

    let tm = make_test_tm(&workspace);
    let topics = tm.list_topics().await;

    let by_name = |n: &str| {
        topics
            .iter()
            .find(|t| t.name == n)
            .unwrap_or_else(|| panic!("topic {n} missing from list_topics"))
    };

    assert_eq!(
        by_name("clean").changed_files.as_deref(),
        Some(&[][..]),
        "branch == main must surface as Some(vec![])"
    );
    assert_eq!(
        by_name("ahead").changed_files.as_deref(),
        Some(
            &[ChangedFileEntry {
                path: "x.rs".into(),
                uncommitted: false,
                change: ChangeKind::Added,
            }][..]
        ),
        "feature branch with one new file must list it"
    );
    assert!(
        by_name("no-git").changed_files.is_none(),
        "non-git topic must have changed_files=None"
    );

    tm.shutdown().await;
}
