//! End-to-end `/exchange` tests that need a real `ThreadManager`.
//!
//! The unit tests in `jyc-core` cover formatting; these cover thread-name
//! resolution, which is the part that can silently emit a 403 link.

use arc_swap::ArcSwap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

use jyc_channels::email::outbound::EmailOutboundAdapter;
use jyc_core::command::exchange_handler::ExchangeCommandHandler;
use jyc_core::command::handler::{CommandContext, CommandHandler};
use jyc_core::message_storage::MessageStorage;
use jyc_core::metrics::MetricsHandle;
use jyc_core::static_agent::StaticAgentService;
use jyc_core::thread_manager::ThreadManager;
use jyc_types::{AppConfig, SmtpConfig, load_config_from_str};

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
[inspect]
enabled = true
exchange_base_url = "https://jyc.example.com"
"#,
        )
        .unwrap(),
    )
}

fn test_context(thread_path: &Path, args: &[&str]) -> CommandContext {
    CommandContext {
        args: args.iter().map(|s| s.to_string()).collect(),
        thread_path: thread_path.to_path_buf(),
        config: test_config(),
        channel: "test".into(),
        channel_type: "email".to_string(),
        agent: None,
        template_dirs: PathBuf::from("/tmp/test/templates").into(),
        config_path: None,
    }
}

fn make_thread_manager(tmp: &TempDir, workspace: &Path) -> Arc<ThreadManager> {
    let storage = Arc::new(MessageStorage::new(workspace));
    Arc::new(ThreadManager::new(
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
        PathBuf::from("/tmp/templates"),
        Arc::new(ArcSwap::new(test_config())),
        "test".to_string(),
        "email".to_string(),
        tmp.path().to_path_buf(),
        workspace.to_path_buf(),
        MetricsHandle::noop(),
    ))
}

/// Seed a published file plus the token that guards it.
async fn seed_published(thread_dir: &Path, name: &str, token: &str) {
    let exchange = thread_dir.join(".jyc").join("exchange");
    tokio::fs::create_dir_all(&exchange).await.unwrap();
    tokio::fs::write(exchange.join(name), b"bytes")
        .await
        .unwrap();
    tokio::fs::write(thread_dir.join(".jyc").join("exchange-token"), token)
        .await
        .unwrap();
}

/// The regression this test exists for: a thread whose directory basename
/// differs from its thread name (shared-repo layout — thread `issue-197`
/// lives in `.../repos/jin-197`). The URL must carry the registered thread
/// name, otherwise the link 403s.
#[tokio::test]
async fn url_uses_registered_thread_name_not_directory_basename() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let thread_dir = workspace.join("repos").join("jin-197");

    let tm = make_thread_manager(&tmp, &workspace);
    tm.set_thread_path("issue-197", thread_dir.clone())
        .await
        .unwrap();
    seed_published(&thread_dir, "report.pdf", "tok123").await;

    let result = ExchangeCommandHandler::new(tm)
        .execute(test_context(&thread_dir, &[]))
        .await
        .unwrap();

    assert!(result.success);
    assert_eq!(
        result.message,
        "report.pdf: https://jyc.example.com/exchange/test/issue-197/report.pdf?token=tok123"
    );
    assert!(
        !result.message.contains("jin-197"),
        "directory basename must not appear in the URL: {}",
        result.message
    );
}

/// Plain workspace thread: basename *is* the thread name, and multiple files
/// list one per line, sorted.
#[tokio::test]
async fn lists_all_published_files_sorted() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    let thread_dir = workspace.join("weather");
    std::fs::create_dir_all(&thread_dir).unwrap();

    let tm = make_thread_manager(&tmp, &workspace);
    seed_published(&thread_dir, "b.txt", "tok123").await;
    seed_published(&thread_dir, "a.txt", "tok123").await;

    let result = ExchangeCommandHandler::new(tm)
        .execute(test_context(&thread_dir, &[]))
        .await
        .unwrap();

    assert_eq!(
        result.message,
        "a.txt: https://jyc.example.com/exchange/test/weather/a.txt?token=tok123\n\
         b.txt: https://jyc.example.com/exchange/test/weather/b.txt?token=tok123"
    );
}

#[tokio::test]
async fn argument_narrows_output_to_one_file() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    let thread_dir = workspace.join("weather");
    std::fs::create_dir_all(&thread_dir).unwrap();

    let tm = make_thread_manager(&tmp, &workspace);
    seed_published(&thread_dir, "a.txt", "tok123").await;
    seed_published(&thread_dir, "report.pdf", "tok123").await;

    let result = ExchangeCommandHandler::new(tm)
        .execute(test_context(&thread_dir, &["report.pdf"]))
        .await
        .unwrap();

    assert_eq!(
        result.message,
        "report.pdf: https://jyc.example.com/exchange/test/weather/report.pdf?token=tok123"
    );
}

#[tokio::test]
async fn unknown_filename_reports_what_is_published() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    let thread_dir = workspace.join("weather");
    std::fs::create_dir_all(&thread_dir).unwrap();

    let tm = make_thread_manager(&tmp, &workspace);
    seed_published(&thread_dir, "a.txt", "tok123").await;

    let result = ExchangeCommandHandler::new(tm)
        .execute(test_context(&thread_dir, &["missing.pdf"]))
        .await
        .unwrap();

    assert!(!result.success);
    assert!(result.error.unwrap().contains("a.txt"));
}

/// No token means nothing was published (or `/reset` rotated it). The command
/// must report that rather than minting a token, which would hand out access.
#[tokio::test]
async fn no_token_reports_nothing_published_and_creates_no_token() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    let thread_dir = workspace.join("weather");
    std::fs::create_dir_all(thread_dir.join(".jyc")).unwrap();

    let tm = make_thread_manager(&tmp, &workspace);

    let result = ExchangeCommandHandler::new(tm)
        .execute(test_context(&thread_dir, &[]))
        .await
        .unwrap();

    assert!(result.success);
    assert!(result.message.contains("no published files"));
    assert!(
        !thread_dir.join(".jyc").join("exchange-token").exists(),
        "/exchange must never create a token"
    );
}
