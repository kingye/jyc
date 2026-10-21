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

/// An agent that works for a while and then drains the live queue the
/// way the in-process agent does. It records the messages it was asked
/// to process (turns) apart from the ones it received live (injected).
#[derive(Default)]
struct DrainingAgent {
    turns: Mutex<Vec<String>>,
    injected: Mutex<Vec<String>>,
    /// How long a turn works before it drains the queue — long enough
    /// for the test to enqueue a follow-up while the turn runs.
    work_ms: u64,
}

#[async_trait::async_trait]
impl AgentService for DrainingAgent {
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
        _topic_cancel: CancellationToken,
    ) -> Result<AgentResult> {
        self.turns
            .lock()
            .expect("turns lock")
            .push(message.content.text.clone().unwrap_or_default());
        tokio::time::sleep(std::time::Duration::from_millis(self.work_ms)).await;
        while let Ok(item) = pending_rx.try_recv() {
            self.injected
                .lock()
                .expect("injected lock")
                .push(item.message.content.text.unwrap_or_default());
        }
        Ok(AgentResult {
            reply_delivered: true,
            reply_text: Some("ok".to_string()),
        })
    }

    async fn reset_session(&self, _topic_path: &Path, _topic_name: &str) -> Result<()> {
        Ok(())
    }
}

fn make_test_tm(workspace: &std::path::Path, agent: Arc<dyn AgentService>) -> Arc<TopicManager> {
    let storage = Arc::new(MessageStorage::new(workspace));
    let cancel = CancellationToken::new();
    let metrics_cancel = CancellationToken::new();
    let (metrics, _stats, _metrics_task) = MetricsCollector::new(metrics_cancel).start();
    let config = Arc::new(ArcSwap::from_pointee(
        jyc_types::load_config_from_str(
            r#"
[ai]
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
[agent]
enabled = true
mode = "agent"
"#,
        )
        .unwrap(),
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

    let agent = Arc::new(DrainingAgent {
        work_ms: 400,
        ..Default::default()
    });
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

    // The first turn is running now — the follow-up arrives during it.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    tm.enqueue(
        message("second", "extra info", tname),
        tname.to_string(),
        pattern_match(),
        None,
        true,
        None,
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    let injected = agent.injected.lock().expect("injected lock").clone();
    assert_eq!(
        injected,
        vec!["extra info".to_string()],
        "the follow-up reached the running turn"
    );
    let turns = agent.turns.lock().expect("turns lock").clone();
    assert_eq!(
        turns,
        vec!["do the thing".to_string()],
        "the follow-up did not become a turn of its own"
    );

    tm.shutdown().await;
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

    let agent = Arc::new(DrainingAgent {
        work_ms: 200,
        ..Default::default()
    });
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

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    tm.enqueue(
        message("second", "extra info", tname),
        tname.to_string(),
        pattern_match(),
        None,
        false,
        None,
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    let injected = agent.injected.lock().expect("injected lock").clone();
    assert!(
        injected.is_empty(),
        "nothing is injected when live_injection is off: {injected:?}"
    );
    let turns = agent.turns.lock().expect("turns lock").clone();
    assert_eq!(
        turns,
        vec!["do the thing".to_string(), "extra info".to_string()],
        "the follow-up was processed as a turn of its own"
    );

    tm.shutdown().await;
}
