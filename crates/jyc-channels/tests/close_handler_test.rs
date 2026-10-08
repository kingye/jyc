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

fn make_tm(tmp: &TempDir, workspace: &std::path::Path) -> Arc<TopicManager> {
    let storage = Arc::new(MessageStorage::new(workspace));
    Arc::new(TopicManager::new(
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
        workspace.to_path_buf(),
        MetricsHandle::noop(),
    ))
}

fn test_context(topic_path: &std::path::Path) -> CommandContext {
    test_context_with(topic_path, &["--force"], "test-topic")
}

fn test_context_with_args(topic_path: &std::path::Path, args: &[&str]) -> CommandContext {
    test_context_with(topic_path, args, "test-topic")
}

fn test_context_with(
    topic_path: &std::path::Path,
    args: &[&str],
    topic_name: &str,
) -> CommandContext {
    CommandContext {
        topic_name: topic_name.to_string(),
        args: args.iter().map(|s| s.to_string()).collect(),
        topic_path: topic_path.to_path_buf(),
        config: test_config(),
        channel: "test".into(),
        channel_type: "websocket".to_string(),
        agent: None,
        template_dirs: PathBuf::from("/tmp/test/templates").into(),
        config_path: None,
        per_agent_commands: vec![],
        hooks: Default::default(),
    }
}

#[tokio::test]
async fn test_close_command_uses_topic_name_not_workspace_dir() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    // A pinned/shared workspace dir whose name deliberately differs from the
    // topic name — the shape a forked topic has.
    let topic_dir = workspace.join("test_topic");
    std::fs::create_dir_all(&topic_dir).unwrap();
    std::fs::write(topic_dir.join("test.txt"), "content").unwrap();
    // Name-keyed state dir under the agents root (tmp.path()).
    let state_dir = tmp.path().join("close-fork-test");
    std::fs::create_dir_all(&state_dir).unwrap();
    jyc_types::state_dir::register("close-fork-test", &state_dir);

    let topic_manager = make_tm(&tmp, &workspace);

    let handler = CloseCommandHandler::new(topic_manager);
    let ctx = test_context_with(&topic_dir, &["--force"], "close-fork-test");

    let result = handler.execute(ctx).await.unwrap();
    assert!(result.success);
    assert!(
        !state_dir.exists(),
        "the named topic's state must be deleted"
    );
    assert!(
        topic_dir.join("test.txt").exists(),
        "the shared workspace dir must be kept"
    );
    assert!(result.message.contains("close-fork-test"));
}

#[tokio::test]
async fn test_close_command_nonexistent_topic_succeeds() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let topic_dir = workspace.join("nonexistent_topic");

    let topic_manager = make_tm(&tmp, &workspace);

    let handler = CloseCommandHandler::new(topic_manager);
    let ctx = test_context(&topic_dir);

    let result = handler.execute(ctx).await.unwrap();
    assert!(result.success);
}

#[tokio::test]
async fn test_close_command_empty_topic_name() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let topic_manager = make_tm(&tmp, &workspace);

    let handler = CloseCommandHandler::new(topic_manager);

    let ctx = CommandContext {
        topic_name: String::new(),
        args: vec!["--force".into()],
        topic_path: PathBuf::from("/"),
        config: test_config(),
        channel: "test".into(),
        channel_type: "websocket".to_string(),
        agent: None,
        template_dirs: PathBuf::from("/tmp/test/templates").into(),
        config_path: None,
        per_agent_commands: vec![],
        hooks: Default::default(),
    };

    let result = handler.execute(ctx).await.unwrap();
    assert!(!result.success);
    assert!(result.error.is_some());
}

#[tokio::test]
async fn test_close_command_without_force_keeps_directory() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let topic_dir = workspace.join("test_topic");
    std::fs::create_dir_all(&topic_dir).unwrap();
    std::fs::write(topic_dir.join("test.txt"), "content").unwrap();

    let topic_manager = make_tm(&tmp, &workspace);

    let handler = CloseCommandHandler::new(topic_manager);
    // Plain `/close` (no args) — must NOT delete
    let ctx = test_context_with_args(&topic_dir, &[]);

    let result = handler.execute(ctx).await.unwrap();
    assert!(
        result.success,
        "warning path should be informational success"
    );
    assert!(
        result.message.contains("/close --force"),
        "message should mention the --force syntax, got: {}",
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

#[tokio::test]
async fn test_close_command_refused_purge_still_closes_topic() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    // A repo two topics co-pin — like a fork sharing its parent's dir.
    let repo = workspace.join("shared_repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("main.rs"), "fn main() {}").unwrap();
    let state_dir = tmp.path().join("close-purge-test");
    std::fs::create_dir_all(&state_dir).unwrap();
    jyc_types::state_dir::register("close-purge-test", &state_dir);

    let topic_manager = make_tm(&tmp, &workspace);
    topic_manager
        .set_topic_path("close-purge-test", repo.clone())
        .await
        .unwrap();
    topic_manager
        .set_topic_path("close-purge-other", repo.clone())
        .await
        .unwrap();

    let handler = CloseCommandHandler::new(topic_manager);
    let ctx = test_context_with(&repo, &["--force", "--purge"], "close-purge-test");

    let result = handler.execute(ctx).await.unwrap();
    assert!(
        result.success,
        "a refused purge must not block the close: {}",
        result.message
    );
    assert!(
        !state_dir.exists(),
        "topic state deleted even though purge was refused"
    );
    assert!(
        repo.join("main.rs").exists(),
        "the shared workspace dir must survive"
    );
    assert!(
        result.message.contains("close-purge-other"),
        "the refusal reason must be reported: {}",
        result.message
    );
}
