use arc_swap::ArcSwap;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

use jyc_channels::websocket::outbound::WebsocketOutboundAdapter;
use jyc_core::command::close_handler::CloseCommandHandler;
use jyc_core::command::handler::{CommandContext, CommandHandler};
use jyc_core::message_storage::MessageStorage;
use jyc_core::metrics::MetricsHandle;
use jyc_core::static_agent::StaticAgentService;
use jyc_core::topic_manager::TopicManager;
use jyc_types::{AppConfig, load_config_from_str};

fn test_config() -> Arc<AppConfig> {
    Arc::new(
        load_config_from_str(
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
        .unwrap(),
    )
}

fn test_config_swap() -> Arc<ArcSwap<AppConfig>> {
    Arc::new(ArcSwap::new(test_config()))
}

fn test_context(topic_path: &std::path::Path) -> CommandContext {
    test_context_with_args(topic_path, &["--confirm"])
}

fn test_context_with_args(topic_path: &std::path::Path, args: &[&str]) -> CommandContext {
    CommandContext {
        args: args.iter().map(|s| s.to_string()).collect(),
        topic_path: topic_path.to_path_buf(),
        config: test_config(),
        channel: "test".into(),
        channel_type: "websocket".to_string(),
        agent: None,
        template_dirs: PathBuf::from("/tmp/test/templates").into(),
        config_path: None,
        per_agent_commands: vec![],
    }
}

#[tokio::test]
async fn test_close_command_deletes_topic_directory() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let topic_dir = workspace.join("test_topic");
    std::fs::create_dir_all(&topic_dir).unwrap();
    std::fs::write(topic_dir.join("test.txt"), "content").unwrap();

    let storage = Arc::new(MessageStorage::new(&workspace));

    let topic_manager = Arc::new(TopicManager::new(
        3,
        10,
        storage.clone(),
        Arc::new(WebsocketOutboundAdapter::new(
            tokio::sync::broadcast::channel(4).0,
            storage,
        )),
        Arc::new(StaticAgentService::new("test reply")),
        tokio_util::sync::CancellationToken::new(),
        PathBuf::from("/tmp/templates"),
        test_config_swap(),
        "test".to_string(),
        "email".to_string(),
        tmp.path().to_path_buf(),
        workspace.clone(),
        MetricsHandle::noop(),
    ));

    let handler = CloseCommandHandler::new(topic_manager);
    let ctx = test_context(&topic_dir);

    let result = handler.execute(ctx).await.unwrap();
    assert!(result.success);
    assert!(!topic_dir.exists());
    assert!(result.message.contains("test_topic"));
}

#[tokio::test]
async fn test_close_command_nonexistent_topic_succeeds() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let topic_dir = workspace.join("nonexistent_topic");

    let storage = Arc::new(MessageStorage::new(&workspace));

    let topic_manager = Arc::new(TopicManager::new(
        3,
        10,
        storage.clone(),
        Arc::new(WebsocketOutboundAdapter::new(
            tokio::sync::broadcast::channel(4).0,
            storage,
        )),
        Arc::new(StaticAgentService::new("test reply")),
        tokio_util::sync::CancellationToken::new(),
        PathBuf::from("/tmp/templates"),
        test_config_swap(),
        "test".to_string(),
        "email".to_string(),
        tmp.path().to_path_buf(),
        workspace.clone(),
        MetricsHandle::noop(),
    ));

    let handler = CloseCommandHandler::new(topic_manager);
    let ctx = test_context(&topic_dir);

    let result = handler.execute(ctx).await.unwrap();
    assert!(result.success);
}

#[tokio::test]
async fn test_close_command_invalid_topic_path() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let storage = Arc::new(MessageStorage::new(&workspace));

    let topic_manager = Arc::new(TopicManager::new(
        3,
        10,
        storage.clone(),
        Arc::new(WebsocketOutboundAdapter::new(
            tokio::sync::broadcast::channel(4).0,
            storage,
        )),
        Arc::new(StaticAgentService::new("test reply")),
        tokio_util::sync::CancellationToken::new(),
        PathBuf::from("/tmp/templates"),
        test_config_swap(),
        "test".to_string(),
        "email".to_string(),
        tmp.path().to_path_buf(),
        workspace.clone(),
        MetricsHandle::noop(),
    ));

    let handler = CloseCommandHandler::new(topic_manager);

    let ctx = CommandContext {
        args: vec!["--confirm".into()],
        topic_path: PathBuf::from("/"),
        config: test_config(),
        channel: "test".into(),
        channel_type: "websocket".to_string(),
        agent: None,
        template_dirs: PathBuf::from("/tmp/test/templates").into(),
        config_path: None,
        per_agent_commands: vec![],
    };

    let result = handler.execute(ctx).await.unwrap();
    assert!(!result.success);
    assert!(result.error.is_some());
}

#[tokio::test]
async fn test_close_command_without_confirm_keeps_directory() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let topic_dir = workspace.join("test_topic");
    std::fs::create_dir_all(&topic_dir).unwrap();
    std::fs::write(topic_dir.join("test.txt"), "content").unwrap();

    let storage = Arc::new(MessageStorage::new(&workspace));

    let topic_manager = Arc::new(TopicManager::new(
        3,
        10,
        storage.clone(),
        Arc::new(WebsocketOutboundAdapter::new(
            tokio::sync::broadcast::channel(4).0,
            storage,
        )),
        Arc::new(StaticAgentService::new("test reply")),
        tokio_util::sync::CancellationToken::new(),
        PathBuf::from("/tmp/templates"),
        test_config_swap(),
        "test".to_string(),
        "email".to_string(),
        tmp.path().to_path_buf(),
        workspace.clone(),
        MetricsHandle::noop(),
    ));

    let handler = CloseCommandHandler::new(topic_manager);
    // Plain `/close` (no args) — must NOT delete
    let ctx = test_context_with_args(&topic_dir, &[]);

    let result = handler.execute(ctx).await.unwrap();
    assert!(
        result.success,
        "warning path should be informational success"
    );
    assert!(
        result.message.contains("/close -y"),
        "message should mention the confirm syntax, got: {}",
        result.message
    );
    assert!(
        topic_dir.exists(),
        "topic dir must still exist after plain /close"
    );
    assert!(
        topic_dir.join("test.txt").exists(),
        "topic contents must be preserved"
    );
}
