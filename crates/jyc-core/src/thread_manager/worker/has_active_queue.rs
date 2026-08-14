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
        _thread_path: &Path,
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

fn make_test_tm(workspace: &std::path::Path) -> Arc<ThreadManager> {
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

fn make_test_tm_with_config(workspace: &std::path::Path, config_str: &str) -> Arc<ThreadManager> {
    let storage = Arc::new(MessageStorage::new(workspace));
    let cancel = CancellationToken::new();
    let metrics_cancel = CancellationToken::new();
    let (metrics, _stats, _metrics_task) = MetricsCollector::new(metrics_cancel).start();
    let config = Arc::new(ArcSwap::from_pointee(
        jyc_types::load_config_from_str(config_str).unwrap(),
    ));

    Arc::new(ThreadManager::new_with_options(
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
async fn test_has_active_queue_false_for_unknown_thread() {
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

    // Create a thread directory so list_threads finds it
    let thread_path = workspace.join("test-thread");
    tokio::fs::create_dir_all(thread_path.join(".jyc"))
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
        thread_refs: None,
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
        "test-thread".to_string(),
        pattern_match,
        None,
        false,
        None,
    )
    .await;

    // Give the worker a moment to start
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert!(
        tm.has_active_queue("test-thread").await,
        "Thread should have an active queue after enqueue"
    );

    // Clean up
    tm.shutdown().await;
}

/// Regression test (#542): injected messages (jyc_send_to_thread,
/// dashboard thread proxy) carry an empty pattern_name. The worker must
/// not overwrite `.jyc/pattern` with it, or the dashboard loses the
/// thread's pattern identity until a router-matched message rewrites it.
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
        thread_refs: None,
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
        "test-thread".to_string(),
        make_pm("jyc"),
        None,
        false,
        None,
    )
    .await;
    let thread_path = workspace.join("test-thread");
    assert!(
        wait_for_history_lines(&thread_path, 1).await,
        "worker did not process the first message in time"
    );
    let pattern_file = thread_path.join(".jyc").join("pattern");
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
        "test-thread".to_string(),
        make_pm(""),
        None,
        false,
        None,
    )
    .await;
    assert!(
        wait_for_history_lines(&thread_path, 2).await,
        "worker did not process the injected message in time"
    );
    assert_eq!(
        tokio::fs::read_to_string(&pattern_file).await.unwrap(),
        "jyc"
    );

    tm.shutdown().await;
}

/// Poll until the thread's chat history holds at least `n` lines
/// (i.e. the worker processed `n` messages). ~2s timeout.
async fn wait_for_history_lines(thread_path: &std::path::Path, n: usize) -> bool {
    for _ in 0..40 {
        let (files, _) = crate::chat_log_store::list_chat_history_files(thread_path);
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

/// `pattern_for_thread` (#542): resolves the enabled pattern named after
/// the thread, including its template/role/custom `thread_path`; returns
/// None for unknown/disabled names so injection falls back to an empty
/// pattern.
#[tokio::test]
async fn test_pattern_for_thread() {
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
thread_path = "{}"
template = "dev"
role = "Developer"
[channels.test-channel.patterns.rules]
[[channels.test-channel.patterns]]
name = "disabled"
enabled = false
thread_path = "/nowhere"
[channels.test-channel.patterns.rules]
[agent]
enabled = true
mode = "agent"
"#,
        custom_path.display()
    );
    let tm = make_test_tm_with_config(&workspace, &config_str);

    let p = tm
        .pattern_for_thread("jyc")
        .expect("pattern should resolve");
    assert_eq!(p.name, "jyc");
    assert_eq!(
        p.thread_path.as_deref(),
        Some(custom_path.to_str().unwrap())
    );
    assert_eq!(p.template.as_deref(), Some("dev"));
    assert_eq!(p.role.as_deref(), Some("Developer"));
    assert!(p.live_injection);
    assert!(tm.pattern_for_thread("disabled").is_none());
    assert!(tm.pattern_for_thread("unknown").is_none());
}

/// Regression test: the per-worker clone must share the parent's
/// `thread_cancels` map. Previously the clone got a fresh empty map, so
/// `cancel_thread` (invoked via /cancel through the command registry,
/// which holds the clone) never found the running worker's token — the
/// user got a success reply but the agent kept running.
#[tokio::test]
async fn test_cancel_thread_via_worker_clone_really_cancels() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    // Simulate an active worker token registered in the shared map
    let token = CancellationToken::new();
    {
        let mut cancels = tm.thread_cancels.lock().await;
        cancels.insert("test-thread".to_string(), token.clone());
    }

    // Cancel through the worker clone — the path /cancel actually takes
    let clone = tm.worker_clone();
    assert!(
        clone.cancel_thread("test-thread").await,
        "cancel_thread via worker clone must find the shared token"
    );
    assert!(token.is_cancelled());

    // Unknown thread must report "nothing cancelled"
    assert!(!clone.cancel_thread("no-such-thread").await);
}

#[tokio::test]
async fn test_publish_incoming_message_on_event_bus() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    // Create event bus manually so we can subscribe
    let bus = tm.get_or_create_event_bus("test-thread").await.unwrap();
    let mut rx = bus.subscribe().await.unwrap();

    // Publish incoming message event
    tm.publish_incoming_message("test-thread", "user", "hello world")
        .await;

    let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("should receive event within timeout")
        .expect("should have an event");

    match event {
        crate::thread_event::ThreadEvent::IncomingMessage {
            thread_name,
            sender,
            text,
            ..
        } => {
            assert_eq!(thread_name, "test-thread");
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
    let bus = tm.get_or_create_event_bus("test-thread").await.unwrap();
    let mut rx = bus.subscribe().await.unwrap();

    // Publish reply sent event
    tm.publish_reply_sent("test-thread", "AI reply here").await;

    let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("should receive event within timeout")
        .expect("should have an event");

    match event {
        crate::thread_event::ThreadEvent::ReplySent {
            thread_name, text, ..
        } => {
            assert_eq!(thread_name, "test-thread");
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
    tm.publish_incoming_message("test-thread", "user", "hello")
        .await;

    tm.shutdown().await;
}

#[tokio::test]
async fn test_thread_meta_written_on_first_message() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    let thread_path = workspace.join("test-thread");
    tokio::fs::create_dir_all(thread_path.join(".jyc"))
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
        thread_refs: Some(vec!["ref-1".to_string()]),
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
        "test-thread".to_string(),
        pattern_match,
        None,
        false,
        None,
    )
    .await;

    // Give the worker a moment to process
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Check thread-meta.json was written
    let meta_path = thread_path.join(".jyc").join("thread-meta.json");
    assert!(meta_path.exists(), "thread-meta.json should be written");

    let content = std::fs::read_to_string(&meta_path).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(meta["channel_uid"], "test-uid");
    assert_eq!(meta["external_id"], "ext-123");
    assert_eq!(meta["thread_refs"], serde_json::json!(["ref-1"]));
    assert_eq!(meta["metadata"]["github_number"], 42);

    tm.shutdown().await;
}

#[tokio::test]
async fn test_thread_meta_not_overwritten_on_second_message() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    let thread_path = workspace.join("test-thread");
    tokio::fs::create_dir_all(thread_path.join(".jyc"))
        .await
        .unwrap();

    // Pre-write a thread-meta.json with a known value
    let meta_path = thread_path.join(".jyc").join("thread-meta.json");
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
        thread_refs: None,
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
        "test-thread".to_string(),
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
async fn test_thread_meta_not_written_for_dashboard_channel_uid() {
    // Dashboard-injected messages have channel_uid == "dashboard" and empty
    // metadata. Writing thread-meta.json for these would poison subsequent
    // injections — the empty metadata would be re-used and real routing data
    // (e.g. github_number) would be lost, causing 404 errors on replies.
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    let thread_path = workspace.join("test-thread");
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
        thread_refs: Some(vec!["ref-1".to_string()]),
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
        "test-thread".to_string(),
        pattern_match,
        None,
        false,
        None,
    )
    .await;

    // Give the worker a moment to process
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // thread-meta.json must NOT be written for dashboard messages
    let meta_path = thread_path.join(".jyc").join("thread-meta.json");
    assert!(
        !meta_path.exists(),
        "thread-meta.json should NOT be written for dashboard channel_uid"
    );

    tm.shutdown().await;
}

#[tokio::test]
async fn test_thread_path_returns_custom_override() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    let custom_path = tmp.path().join("custom-threads").join("my-thread");
    tm.thread_paths
        .lock()
        .await
        .insert("my-thread".to_string(), custom_path.clone());

    let resolved = tm.thread_path("my-thread").await;
    assert_eq!(resolved, Some(custom_path));

    tm.shutdown().await;
}

#[tokio::test]
async fn test_thread_path_falls_back_to_default() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    // Create thread dir at default location
    let default_path = workspace.join("default-thread");
    tokio::fs::create_dir_all(&default_path).await.unwrap();

    // No custom path stored — should fall back to workspace/thread_name
    let resolved = tm.thread_path("default-thread").await;
    assert_eq!(resolved, Some(default_path));

    tm.shutdown().await;
}

#[tokio::test]
async fn test_thread_path_returns_none_for_nonexistent() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    let resolved = tm.thread_path("nonexistent").await;
    assert_eq!(resolved, None);

    tm.shutdown().await;
}

#[tokio::test]
async fn test_custom_thread_paths_empty_initially() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    let paths = tm.custom_thread_paths().await;
    assert!(paths.is_empty());

    tm.shutdown().await;
}

/// Regression test for the `jyc open` ad-hoc thread timeout:
///
/// `set_thread_path` must create `.jyc/thread-name` so that the
/// `path.join(".jyc").is_dir()` filter in `list_threads` keeps the
/// entry. Without it, a freshly-registered ad-hoc thread is dropped
/// from the overview and `wait_for_thread` in `run_open` times out
/// with "Timeout waiting for thread ... to be created".
#[tokio::test]
async fn test_set_thread_path_creates_jyc_dir_and_appears_in_list() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    let custom_path = tmp.path().join("adhoc-projects");
    tm.set_thread_path("projects", custom_path.clone())
        .await
        .unwrap();

    // `.jyc/` and `.jyc/thread-name` are written by set_thread_path so
    // list_threads doesn't filter the entry out.
    assert!(
        custom_path.join(".jyc").is_dir(),
        "set_thread_path must create .jyc/"
    );
    assert_eq!(
        tokio::fs::read_to_string(custom_path.join(".jyc").join("thread-name"))
            .await
            .unwrap()
            .trim(),
        "projects",
        "set_thread_path must write .jyc/thread-name"
    );

    // The new ad-hoc thread appears in list_threads so the dashboard
    // overview reports it within the 5s wait_for_thread window.
    let threads = tm.list_threads().await;
    assert!(
        threads.iter().any(|t| t.name == "projects"),
        "ad-hoc thread should appear in list_threads"
    );

    tm.shutdown().await;
}

#[tokio::test]
async fn test_restore_custom_thread_paths_from_disk() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    // Custom thread path outside workspace
    let custom_path = tmp.path().join("external-project");
    tokio::fs::create_dir_all(custom_path.join(".jyc"))
        .await
        .unwrap();
    // Simulate a previously initialized thread
    tokio::fs::write(
        custom_path.join(".jyc").join("thread-name"),
        "my-custom-thread",
    )
    .await
    .unwrap();

    // Config with thread_path override — channel name must match TM's channel_name
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
thread_path = "{}"
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

    let tm = Arc::new(ThreadManager::new_with_options(
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
    let paths = tm.custom_thread_paths().await;
    assert!(paths.is_empty());

    // Restore from disk
    tm.restore_custom_thread_paths().await;

    // After restore: mapping exists
    let paths = tm.custom_thread_paths().await;
    assert_eq!(
        paths.get("my-custom-thread"),
        Some(&custom_path),
        "restore_custom_thread_paths should rediscover the thread"
    );

    // list_threads should now include the restored thread
    let threads = tm.list_threads().await;
    let names: Vec<&str> = threads.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names.contains(&"my-custom-thread"),
        "list_threads should include restored custom-path thread"
    );

    // Event bus should be pre-created so ActivityTracker can subscribe
    // before the first message arrives (avoids lost first-message events).
    let bus = tm.get_event_bus("my-custom-thread").await;
    assert!(
        bus.is_some(),
        "restore_custom_thread_paths should pre-create event bus"
    );

    tm.shutdown().await;
}

#[tokio::test]
async fn test_restore_skips_missing_thread_name_file() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    // Custom path exists but has no .jyc/thread-name file
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
thread_path = "{}"
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

    let tm = Arc::new(ThreadManager::new_with_options(
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

    tm.restore_custom_thread_paths().await;

    // Should be empty — no thread-name file
    let paths = tm.custom_thread_paths().await;
    assert!(
        paths.is_empty(),
        "Should skip paths without thread-name file"
    );

    tm.shutdown().await;
}

#[tokio::test]
async fn test_list_threads_cleans_stale_custom_path() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tm = make_test_tm(&workspace);

    // Insert a custom path that doesn't exist on disk
    let ghost_path = tmp.path().join("deleted-thread");
    tokio::fs::create_dir_all(ghost_path.join(".jyc"))
        .await
        .unwrap();
    tm.thread_paths
        .lock()
        .await
        .insert("ghost".to_string(), ghost_path.clone());

    // list_threads should include it while dir exists
    let threads = tm.list_threads().await;
    assert!(
        threads.iter().any(|t| t.name == "ghost"),
        "Should list thread while directory exists"
    );

    // Delete the directory
    tokio::fs::remove_dir_all(&ghost_path).await.unwrap();

    // list_threads should now clean it up
    let threads = tm.list_threads().await;
    assert!(
        !threads.iter().any(|t| t.name == "ghost"),
        "Should not list thread after directory deleted"
    );

    // thread_paths map should no longer contain the entry
    let paths = tm.custom_thread_paths().await;
    assert!(
        !paths.contains_key("ghost"),
        "Stale entry should be removed from thread_paths"
    );

    tm.shutdown().await;
}
/// Build a TM whose config prices `cnprov/m1` in CNY, so
/// `list_threads` has a real pricing entry to resolve a currency from.
fn make_priced_tm(workspace: &std::path::Path) -> Arc<ThreadManager> {
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

    Arc::new(ThreadManager::new_with_options(
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
async fn list_threads_currency_from_config_when_ledger_empty() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    let thread = workspace.join("t1");
    std::fs::create_dir_all(thread.join(".jyc")).unwrap();
    // Session has spend; no bill-<today>.jsonl exists at all.
    std::fs::write(
        thread.join(".jyc/agent-session.json"),
        r#"{"session_cost":0.05,"context_input_tokens":10}"#,
    )
    .unwrap();

    let tm = make_priced_tm(&workspace);
    let threads = tm.list_threads().await;
    let t = threads
        .iter()
        .find(|t| t.name == "t1")
        .expect("thread listed");
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
/// thread actually spent in two.
#[tokio::test]
async fn list_threads_preserves_mixed_currency_from_ledger() {
    use crate::billing_log_store::{BillingEntry, BillingLogStore, MIXED_CURRENCY};

    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    let thread = workspace.join("t1");
    std::fs::create_dir_all(thread.join(".jyc")).unwrap();

    for (cost, currency) in [(1.0, "CNY"), (2.0, "USD")] {
        BillingLogStore::append(
            &thread,
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
            },
        )
        .unwrap();
    }

    let tm = make_priced_tm(&workspace);
    let threads = tm.list_threads().await;
    let t = threads
        .iter()
        .find(|t| t.name == "t1")
        .expect("thread listed");
    let cost = t.cost.as_ref().expect("cost present");

    assert_eq!(
        cost.currency, MIXED_CURRENCY,
        "ledger's mixed marker must survive, not be replaced by config"
    );
    assert!((cost.today - 3.0).abs() < 1e-9);
    tm.shutdown().await;
}

/// Regression for #512: `ThreadManager::list_threads` must populate
/// `ThreadInfo::branch` by reading `.git/HEAD` under each thread's
/// path. Without this test, a future refactor that drops the call to
/// `branch_for_thread_path` at the `threads.push(...)` site would
/// silently leave `branch == None` on every payload.
#[tokio::test]
async fn list_threads_populates_branch_from_dot_git_head() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");

    // Thread "main-test" with a symbolic-ref HEAD pointing at main.
    let t1 = workspace.join("main-test");
    std::fs::create_dir_all(t1.join(".jyc")).unwrap();
    std::fs::create_dir_all(t1.join(".git")).unwrap();
    std::fs::write(t1.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

    // Thread "detached-test" with a raw 40-char SHA — should appear
    // as "(detached)" rather than as `None`.
    let t2 = workspace.join("detached-test");
    std::fs::create_dir_all(t2.join(".jyc")).unwrap();
    std::fs::create_dir_all(t2.join(".git")).unwrap();
    std::fs::write(
        t2.join(".git/HEAD"),
        "0123456789abcdef0123456789abcdef01234567",
    )
    .unwrap();

    // Thread "no-git" — `.jyc` exists but no `.git/HEAD`. Branch
    // should be `None` (renderer skips the row).
    let t3 = workspace.join("no-git");
    std::fs::create_dir_all(t3.join(".jyc")).unwrap();

    let tm = make_test_tm(&workspace);
    let threads = tm.list_threads().await;

    let by_name = |n: &str| {
        threads
            .iter()
            .find(|t| t.name == n)
            .unwrap_or_else(|| panic!("thread {n} missing from list_threads"))
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
        "non-git thread must have branch=None"
    );

    tm.shutdown().await;
}

/// Regression for #220: `ThreadManager::list_threads` must populate
/// `ThreadInfo::changed_files` by running `git diff --name-only
/// main...HEAD` under each thread's path. Without this test, a
/// future refactor that drops the call to
/// `changed_files_for_thread_path` at the `threads.push(...)` site
/// would silently leave `changed_files == None` on every payload.
#[tokio::test]
async fn list_threads_populates_changed_files_from_git_diff() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");

    // Thread "clean": real git repo on `main` with no commits ahead.
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

    // Thread "ahead": feature branch with one commit adding "x.rs".
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

    // Thread "no-git": no `.git` at all → `changed_files == None`.
    let no_git = workspace.join("no-git");
    std::fs::create_dir_all(no_git.join(".jyc")).unwrap();

    let tm = make_test_tm(&workspace);
    let threads = tm.list_threads().await;

    let by_name = |n: &str| {
        threads
            .iter()
            .find(|t| t.name == n)
            .unwrap_or_else(|| panic!("thread {n} missing from list_threads"))
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
        "non-git thread must have changed_files=None"
    );

    tm.shutdown().await;
}
