//! Live injection at the worker: a message that arrives while a turn is
//! running joins that turn when the pattern's `live_injection` is on,
//! and waits for its own turn when it is off.

use super::*;
use crate::agent::{AgentResult, AgentService};
use crate::message_storage::MessageStorage;
use crate::metrics::MetricsCollector;
use jyc_types::{InboundMessage, PatternMatch};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
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
            chat_seq: 0,
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
            chat_seq: 0,
        })
    }
}

/// An agent that records the messages it was asked to process (turns) and
/// the ones it received live (injected).
///
/// The turn stays open until the test lets it end, so the test decides
/// when the follow-up arrives and when the drain sees it. Nothing here
/// sleeps past a guess: the tests wait for what they assert.
struct FakeAgent {
    turn_log: Mutex<Vec<String>>,
    injected_log: Mutex<Vec<String>>,
    /// Whether the agent drains the live queue the way the in-process
    /// agent does. An agent that does not drain leaves the message in the
    /// channel, and the worker has to re-enqueue it.
    drains: bool,
    /// Set by the test to let the turn end.
    finish: AtomicBool,
    /// Why the turn ended: the test released it, or the topic was
    /// cancelled under it.
    exit: Mutex<Option<&'static str>>,
    /// How many turns ended.
    finished: AtomicUsize,
    /// One signal per turn, so the test knows a turn is running.
    started_tx: mpsc::UnboundedSender<()>,
}

impl FakeAgent {
    fn new(drains: bool, started_tx: mpsc::UnboundedSender<()>) -> Self {
        Self {
            turn_log: Mutex::new(Vec::new()),
            injected_log: Mutex::new(Vec::new()),
            drains,
            finish: AtomicBool::new(false),
            exit: Mutex::new(None),
            finished: AtomicUsize::new(0),
            started_tx,
        }
    }

    /// The bodies of the turns the agent processed, in order.
    fn turns(&self) -> Vec<String> {
        self.turn_log.lock().expect("turns lock").clone()
    }

    /// The bodies of the messages the agent received live, in order.
    fn injected(&self) -> Vec<String> {
        self.injected_log.lock().expect("injected lock").clone()
    }

    /// How many turns ended.
    fn finished(&self) -> usize {
        self.finished.load(Ordering::SeqCst)
    }

    /// Why the last turn ended: the test released it ("finished") or the
    /// topic was cancelled under it ("cancelled").
    fn exit(&self) -> Option<&'static str> {
        *self.exit.lock().expect("exit lock")
    }

    /// Let the turn end.
    fn finish(&self) {
        self.finish.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl AgentService for FakeAgent {
    async fn base_url(&self) -> Result<String> {
        Ok(String::new())
    }

    async fn process(
        &self,
        message: &InboundMessage,
        _topic_name: &str,
        _topic_path: &Path,
        _message_dir: &str,
        pending_rx: &mut mpsc::Receiver<jyc_types::QueueItem>,
        topic_cancel: CancellationToken,
    ) -> Result<AgentResult> {
        self.turn_log
            .lock()
            .expect("turns lock")
            .push(message.content.text.clone().unwrap_or_default());
        let _ = self.started_tx.send(());

        loop {
            if self.finish.load(Ordering::SeqCst) {
                *self.exit.lock().expect("exit lock") = Some("finished");
                break;
            }
            if self.drains {
                while let Ok(item) = pending_rx.try_recv() {
                    self.injected_log
                        .lock()
                        .expect("injected lock")
                        .push(item.message.content.text.unwrap_or_default());
                }
            }
            tokio::select! {
                _ = topic_cancel.cancelled() => {
                    *self.exit.lock().expect("exit lock") = Some("cancelled");
                    break;
                }
                _ = tokio::time::sleep(Duration::from_millis(2)) => {}
            }
        }
        self.finished.fetch_add(1, Ordering::SeqCst);

        Ok(AgentResult {
            reply_delivered: true,
            reply_text: Some("ok".to_string()),
        })
    }

    async fn reset_session(&self, _topic_path: &Path, _topic_name: &str) -> Result<()> {
        Ok(())
    }
}

/// Wait for `check` to hold, up to `timeout`. Says whether it held.
async fn wait_for(mut check: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if check() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Wait until `check` holds. The tests wait for what they assert instead
/// of sleeping past a guess.
async fn wait_until(what: &str, check: impl FnMut() -> bool) {
    assert!(
        wait_for(check, Duration::from_secs(10)).await,
        "timed out waiting for {what}"
    );
}

/// The `message_received` hook payloads a recording hook wrote, in order.
///
/// The hook sees every message that reaches the model — the one that
/// starts a turn and the one that joins a running turn — so the list
/// says which path a message took.
fn hook_payloads(workspace: &Path, topic: &str) -> Vec<String> {
    let text = std::fs::read_to_string(workspace.join(topic).join("hook-payloads.log"))
        .unwrap_or_default();
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.to_string())
        .collect()
}

/// A config that records every `message_received` payload in the topic
/// directory, so a test can see which path a message took.
fn config_with_recording_hook() -> String {
    format!(
        "{BASE_CONFIG}\n\
         [[hooks]]\n\
         event = \"message_received\"\n\
         shell = [\"sh\", \"-c\", \"cat >> hook-payloads.log; echo >> hook-payloads.log; exit 0\"]\n"
    )
}

/// The bodies the chat log holds for a topic, in the order they landed.
fn chat_log_bodies(workspace: &Path, topic: &str) -> Vec<String> {
    let state_dir = workspace.join(topic).join(".jyc");
    let mut bodies = Vec::new();
    let entries = match std::fs::read_dir(&state_dir) {
        Ok(entries) => entries,
        Err(_) => return bodies,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if !name.starts_with("chat_history_") || !name.ends_with(".jsonl") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("chat log file");
        for line in text.lines() {
            let record: serde_json::Value = match serde_json::from_str(line) {
                Ok(record) => record,
                Err(_) => continue,
            };
            if record.get("type").and_then(|t| t.as_str()) != Some("received") {
                continue;
            }
            if let Some(content) = record.get("content").and_then(|c| c.as_str()) {
                bodies.push(content.to_string());
            }
        }
    }
    bodies
}

const BASE_CONFIG: &str = r#"
[ai]
enabled = true
mode = "agent"

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
"#;

fn make_test_tm(workspace: &std::path::Path, agent: Arc<dyn AgentService>) -> Arc<TopicManager> {
    make_test_tm_with_config(workspace, agent, BASE_CONFIG)
}

fn make_test_tm_with_config(
    workspace: &std::path::Path,
    agent: Arc<dyn AgentService>,
    config: &str,
) -> Arc<TopicManager> {
    let storage = Arc::new(MessageStorage::new(workspace));
    let cancel = CancellationToken::new();
    let metrics_cancel = CancellationToken::new();
    let (metrics, _stats, _metrics_task) = MetricsCollector::new(metrics_cancel).start();
    let config = Arc::new(ArcSwap::from_pointee(
        jyc_types::load_config_from_str(config).unwrap(),
    ));

    Arc::new(TopicManager::new_with_options(
        1,
        10,
        storage,
        Arc::new(NoopOutbound) as Arc<dyn jyc_types::OutboundAdapter>,
        agent,
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

fn message(id: &str, body: &str, topic: &str) -> InboundMessage {
    InboundMessage {
        id: id.to_string(),
        channel: "test-channel".to_string(),
        channel_uid: "test".to_string(),
        sender: "user".to_string(),
        sender_address: "user".to_string(),
        recipients: vec![],
        topic: topic.to_string(),
        content: jyc_types::MessageContent {
            text: Some(body.to_string()),
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
    }
}

fn pattern_match() -> PatternMatch {
    PatternMatch {
        pattern_name: "test".to_string(),
        channel: "websocket".to_string(),
        matches: HashMap::new(),
    }
}

/// With `live_injection` on, the follow-up joins the running turn: the
/// agent receives it live and it never becomes a turn of its own.
#[tokio::test]
async fn a_follow_up_joins_the_running_turn() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tname = "live_injection_joins";
    jyc_types::state_dir::register_if_absent(tname, &workspace.join(tname).join(".jyc"));

    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let agent = Arc::new(FakeAgent::new(true, started_tx));
    let agent_service: Arc<dyn AgentService> = agent.clone();
    let tm = make_test_tm_with_config(&workspace, agent_service, &config_with_recording_hook());

    tm.enqueue(
        message("first", "do the thing", tname),
        tname.to_string(),
        pattern_match(),
        None,
        true,
        None,
    )
    .await;

    // The first turn is running now — the follow-up arrives during it.
    started_rx.recv().await.expect("the first turn started");
    // The worker registers the topic's directory the first time it stores
    // a message, so from here on every message carries a topic path. The
    // follow-up must still join the running turn: a path that names the
    // topic's own directory is not a reason to wait.
    assert!(
        tm.custom_topic_paths().await.contains_key(tname),
        "the topic's directory is registered by now"
    );
    tm.enqueue(
        message("second", "extra info", tname),
        tname.to_string(),
        pattern_match(),
        None,
        true,
        None,
    )
    .await;

    let injected = wait_for(|| !agent.injected().is_empty(), Duration::from_secs(10)).await;
    agent.finish();
    // The turn has to end for the evidence below to be complete: a
    // follow-up that did not join it waits for a turn of its own, and
    // that turn is where the hook sees it a second time.
    let ended = wait_for(|| agent.finished() >= 1, Duration::from_secs(10)).await;
    let _ = wait_for(|| agent.turns().len() >= 2, Duration::from_secs(2)).await;

    let turns = agent.turns();
    let payloads = hook_payloads(&workspace, tname);
    let log = chat_log_bodies(&workspace, tname);
    let exit = agent.exit();
    tm.shutdown().await;

    assert!(
        injected && ended,
        "the follow-up never reached the running turn: \
         injected={:?} turn_ended={} turns={:?} turn_exit={:?} chat_log={:?} \
         message_received payloads={:?}",
        agent.injected(),
        ended,
        turns,
        exit,
        log,
        payloads
    );
    assert_eq!(
        agent.injected(),
        vec!["extra info".to_string()],
        "the follow-up reached the running turn"
    );
    assert_eq!(
        agent.turns(),
        vec!["do the thing".to_string()],
        "the follow-up did not become a turn of its own"
    );
    assert!(
        payloads
            .iter()
            .any(|payload| payload.contains("extra info")),
        "the injected message passed the message_received hook: {payloads:?}"
    );
}

/// With `live_injection` off, the follow-up waits for its own turn — the
/// behavior the flag used to have.
#[tokio::test]
async fn a_follow_up_waits_for_its_own_turn_when_injection_is_off() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tname = "live_injection_off";
    jyc_types::state_dir::register_if_absent(tname, &workspace.join(tname).join(".jyc"));

    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let agent = Arc::new(FakeAgent::new(true, started_tx));
    let agent_service: Arc<dyn AgentService> = agent.clone();
    let tm = make_test_tm(&workspace, agent_service);

    tm.enqueue(
        message("first", "do the thing", tname),
        tname.to_string(),
        pattern_match(),
        None,
        false,
        None,
    )
    .await;

    started_rx.recv().await.expect("the first turn started");
    tm.enqueue(
        message("second", "extra info", tname),
        tname.to_string(),
        pattern_match(),
        None,
        false,
        None,
    )
    .await;

    agent.finish();
    wait_until("the follow-up to become a turn of its own", || {
        agent.turns().len() == 2
    })
    .await;
    tm.shutdown().await;

    assert!(
        agent.injected().is_empty(),
        "nothing is injected when live_injection is off: {:?}",
        agent.injected()
    );
    assert_eq!(
        agent.turns(),
        vec!["do the thing".to_string(), "extra info".to_string()],
        "the follow-up was processed as a turn of its own"
    );
}

/// A follow-up the running turn never folded — the agent ended without
/// draining the live queue — waits for its own turn instead of being
/// lost, and it lands in the chat log exactly once.
#[tokio::test]
async fn a_follow_up_the_turn_never_folded_waits_for_its_own_turn() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tname = "live_injection_leftover";
    jyc_types::state_dir::register_if_absent(tname, &workspace.join(tname).join(".jyc"));

    // An agent that never drains the live queue: the message stays in the
    // channel when the turn ends.
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let agent = Arc::new(FakeAgent::new(false, started_tx));
    let agent_service: Arc<dyn AgentService> = agent.clone();
    let tm = make_test_tm(&workspace, agent_service);

    tm.enqueue(
        message("first", "do the thing", tname),
        tname.to_string(),
        pattern_match(),
        None,
        true,
        None,
    )
    .await;

    started_rx.recv().await.expect("the first turn started");
    tm.enqueue(
        message("second", "extra info", tname),
        tname.to_string(),
        pattern_match(),
        None,
        true,
        None,
    )
    .await;

    agent.finish();
    wait_until("the follow-up to become a turn of its own", || {
        agent.turns().len() == 2
    })
    .await;
    tm.shutdown().await;

    assert!(
        agent.injected().is_empty(),
        "the agent never drained the live queue"
    );
    assert_eq!(
        agent.turns(),
        vec!["do the thing".to_string(), "extra info".to_string()],
        "the follow-up the turn never folded waited for its own turn"
    );
    assert_eq!(
        chat_log_bodies(&workspace, tname),
        vec!["do the thing".to_string(), "extra info".to_string()],
        "both messages are in the chat log exactly once"
    );
}

/// A `message_received` hook that blocks the follow-up keeps it out of the
/// running turn. The message waits for a turn of its own, where the same
/// hook blocks it again — and the chat log still records it.
#[tokio::test]
async fn a_blocked_hook_keeps_the_follow_up_out_of_the_running_turn() {
    let tmp = tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let tname = "live_injection_blocked";
    jyc_types::state_dir::register_if_absent(tname, &workspace.join(tname).join(".jyc"));

    // The hook blocks the follow-up only: the payload carries the body,
    // and exit code 2 is a block.
    let config = format!(
        "{BASE_CONFIG}\n\
         [[hooks]]\n\
         event = \"message_received\"\n\
         shell = [\"sh\", \"-c\", \"grep -q 'extra info' >/dev/null && exit 2; exit 0\"]\n"
    );

    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let agent = Arc::new(FakeAgent::new(true, started_tx));
    let agent_service: Arc<dyn AgentService> = agent.clone();
    let tm = make_test_tm_with_config(&workspace, agent_service, &config);

    tm.enqueue(
        message("first", "do the thing", tname),
        tname.to_string(),
        pattern_match(),
        None,
        true,
        None,
    )
    .await;

    started_rx.recv().await.expect("the first turn started");
    tm.enqueue(
        message("second", "extra info", tname),
        tname.to_string(),
        pattern_match(),
        None,
        true,
        None,
    )
    .await;

    agent.finish();
    // The blocked message waits for its own turn, and that turn stores it
    // in the chat log before the hook blocks it again.
    wait_until("the blocked follow-up to reach the chat log", || {
        chat_log_bodies(&workspace, tname)
            .iter()
            .any(|body| body == "extra info")
    })
    .await;
    tm.shutdown().await;

    assert!(
        agent.injected().is_empty(),
        "the blocked follow-up never joined the running turn: {:?}",
        agent.injected()
    );
    assert_eq!(
        agent.turns(),
        vec!["do the thing".to_string()],
        "the blocked follow-up never reached the model"
    );
    assert_eq!(
        chat_log_bodies(&workspace, tname),
        vec!["do the thing".to_string(), "extra info".to_string()],
        "the blocked follow-up is still in the chat log, exactly once"
    );
}
