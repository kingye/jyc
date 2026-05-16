use std::path::PathBuf;
use std::sync::Arc;
use arc_swap::ArcSwap;
use tempfile::TempDir;

use jyc_core::thread_manager::ThreadManager;
use jyc_core::static_agent::StaticAgentService;
use jyc_core::command::close_handler::CloseCommandHandler;
use jyc_core::command::handler::{CommandContext, CommandHandler};
use jyc_core::message_storage::MessageStorage;
use jyc_core::metrics::MetricsHandle;
use jyc_channels::email::outbound::EmailOutboundAdapter;
use jyc_types::{AppConfig, HeartbeatConfig, SmtpConfig, load_config_from_str};

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

fn test_context(thread_path: &std::path::Path) -> CommandContext {
    CommandContext {
        args: vec![],
        thread_path: thread_path.to_path_buf(),
        config: test_config(),
        channel: "test".into(),
        agent: None,
        template_dir: PathBuf::from("/tmp/test/templates"),
    }
}

#[tokio::test]
async fn test_close_command_deletes_thread_directory() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let thread_dir = workspace.join("test_thread");
    std::fs::create_dir_all(&thread_dir).unwrap();
    std::fs::write(thread_dir.join("test.txt"), "content").unwrap();

    let storage = Arc::new(MessageStorage::new(&workspace));

    let thread_manager = Arc::new(ThreadManager::new(
        3,
        10,
        storage.clone(),
        Arc::new(EmailOutboundAdapter::new(
            &SmtpConfig {
                host: "smtp.example.com".into(),
                port: 465,
                secure: true,
                username: "test".into(),
                password: "test".into(),
                from_address: Some("test@example.com".into()),
                from_name: None,
            },
            storage,
        )),
        Arc::new(StaticAgentService::new("test reply")),
        tokio_util::sync::CancellationToken::new(),
        HeartbeatConfig::default(),
        "".into(),
        PathBuf::from("/tmp/templates"),
        test_config_swap(),
        "test".to_string(),
        workspace.clone(),
        MetricsHandle::noop(),
    ));

    let handler = CloseCommandHandler::new(thread_manager);
    let ctx = test_context(&thread_dir);

    let result = handler.execute(ctx).await.unwrap();
    assert!(result.success);
    assert!(!thread_dir.exists());
    assert!(result.message.contains("test_thread"));
}

#[tokio::test]
async fn test_close_command_nonexistent_thread_succeeds() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let thread_dir = workspace.join("nonexistent_thread");

    let storage = Arc::new(MessageStorage::new(&workspace));

    let thread_manager = Arc::new(ThreadManager::new(
        3,
        10,
        storage.clone(),
        Arc::new(EmailOutboundAdapter::new(
            &SmtpConfig {
                host: "smtp.example.com".into(),
                port: 465,
                secure: true,
                username: "test".into(),
                password: "test".into(),
                from_address: Some("test@example.com".into()),
                from_name: None,
            },
            storage,
        )),
        Arc::new(StaticAgentService::new("test reply")),
        tokio_util::sync::CancellationToken::new(),
        HeartbeatConfig::default(),
        "".into(),
        PathBuf::from("/tmp/templates"),
        test_config_swap(),
        "test".to_string(),
        workspace.clone(),
        MetricsHandle::noop(),
    ));

    let handler = CloseCommandHandler::new(thread_manager);
    let ctx = test_context(&thread_dir);

    let result = handler.execute(ctx).await.unwrap();
    assert!(result.success);
}

#[tokio::test]
async fn test_close_command_invalid_thread_path() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let storage = Arc::new(MessageStorage::new(&workspace));

    let thread_manager = Arc::new(ThreadManager::new(
        3,
        10,
        storage.clone(),
        Arc::new(EmailOutboundAdapter::new(
            &SmtpConfig {
                host: "smtp.example.com".into(),
                port: 465,
                secure: true,
                username: "test".into(),
                password: "test".into(),
                from_address: Some("test@example.com".into()),
                from_name: None,
            },
            storage,
        )),
        Arc::new(StaticAgentService::new("test reply")),
        tokio_util::sync::CancellationToken::new(),
        HeartbeatConfig::default(),
        "".into(),
        PathBuf::from("/tmp/templates"),
        test_config_swap(),
        "test".to_string(),
        workspace.clone(),
        MetricsHandle::noop(),
    ));

    let handler = CloseCommandHandler::new(thread_manager);

    let ctx = CommandContext {
        args: vec![],
        thread_path: PathBuf::from("/"),
        config: test_config(),
        channel: "test".into(),
        agent: None,
        template_dir: PathBuf::from("/tmp/test/templates"),
    };

    let result = handler.execute(ctx).await.unwrap();
    assert!(!result.success);
    assert!(result.error.is_some());
}
