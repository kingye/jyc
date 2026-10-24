//! Shared fixtures for the crate's unit tests.
//!
//! One owner for the pieces the handler and topic-manager tests used to define
//! each for itself: the no-op outbound adapter, the standard email-channel
//! config, a `TopicManager` built on a given config, and a `CommandContext`.
//! Only compiled under `#[cfg(test)]`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use arc_swap::ArcSwap;

use crate::command::handler::CommandContext;
use crate::message_storage::MessageStorage;
use crate::metrics::MetricsCollector;
use crate::static_agent::StaticAgentService;
use crate::topic_manager::TopicManager;

/// The config fixture most tests use: one email channel named `test`, agent
/// enabled. Tests that need providers, patterns or rules of their own pass
/// their own TOML to `test_context_with_config` / `make_topic_manager`.
pub(crate) const TEST_CONFIG_STR: &str = r#"
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
"#;

/// Minimal outbound adapter that does nothing: no connection, no send, and
/// every send reports the fixed message id `noop`.
pub(crate) struct NoopOutbound;

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
        _original: &jyc_types::InboundMessage,
        _reply_text: &str,
        _topic_path: &Path,
        _message_dir: &str,
        _attachments: Option<&[jyc_types::OutboundAttachment]>,
    ) -> Result<jyc_types::SendResult> {
        Ok(noop_send_result())
    }
    async fn send_message(
        &self,
        _recipient: &str,
        _subject: &str,
        _body: &str,
    ) -> Result<jyc_types::SendResult> {
        Ok(noop_send_result())
    }
}

/// The fixed result every no-op send reports.
fn noop_send_result() -> jyc_types::SendResult {
    jyc_types::SendResult {
        message_id: "noop".to_string(),
        chat_seq: 0,
    }
}

/// Build a `TopicManager` on `topic_path` with `config_str` as its loaded
/// configuration, a no-op outbound adapter and a static agent.
///
/// `topic_path` is the topic's working directory; its parent stands in for
/// the instance's data dir, exactly as the tests' own managers did.
pub(crate) fn make_topic_manager(topic_path: &Path, config_str: &str) -> Arc<TopicManager> {
    let storage = Arc::new(MessageStorage::new(topic_path));
    let cancel = tokio_util::sync::CancellationToken::new();
    let metrics_cancel = tokio_util::sync::CancellationToken::new();
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
        false,
        topic_path.join("templates"),
        config,
        "test".to_string(),
        "websocket".to_string(),
        topic_path.parent().unwrap_or(topic_path).to_path_buf(),
        topic_path.to_path_buf(),
        metrics,
        None,
    ))
}

/// A `CommandContext` on the standard config — see `test_context_with_config`.
pub(crate) fn test_context(topic_name: &str, topic_path: &Path) -> CommandContext {
    test_context_with_config(topic_name, topic_path, TEST_CONFIG_STR)
}

/// Build a `CommandContext` for a command-handler test.
///
/// Channel `test` over websocket, no agent, no args, templates under
/// `/tmp/test/templates`. The topic is **not** registered in the state-dir
/// registry: tests that resolve state dirs register it themselves, since some
/// deliberately reuse one topic name across temp dirs.
pub(crate) fn test_context_with_config(
    topic_name: &str,
    topic_path: &Path,
    config_str: &str,
) -> CommandContext {
    CommandContext {
        topic_name: topic_name.to_string(),
        args: vec![],
        topic_path: topic_path.to_path_buf(),
        config: Arc::new(jyc_types::load_config_from_str(config_str).unwrap()),
        channel: "test".into(),
        agent: None,
        template_dirs: PathBuf::from("/tmp/test/templates").into(),
        channel_type: "websocket".to_string(),
        config_path: None,
        per_agent_commands: vec![],
        hooks: Default::default(),
    }
}
