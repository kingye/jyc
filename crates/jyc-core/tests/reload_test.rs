use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// Mock agent service for testing.
struct MockAgent;

#[async_trait::async_trait]
impl jyc_core::agent::AgentService for MockAgent {
    async fn base_url(&self) -> anyhow::Result<String> {
        Ok("".to_string())
    }

    async fn process(
        &self,
        _message: &jyc_types::InboundMessage,
        _thread_name: &str,
        _thread_path: &Path,
        _message_dir: &str,
        _pending_rx: &mut tokio::sync::mpsc::Receiver<jyc_types::QueueItem>,
        _thread_cancel: CancellationToken,
    ) -> anyhow::Result<jyc_core::agent::AgentResult> {
        Ok(jyc_core::agent::AgentResult {
            reply_sent_by_tool: false,
            reply_text: None,
        })
    }

    async fn reset_session(
        &self,
        _thread_path: &Path,
        _thread_name: &str,
        _config: &jyc_types::channel::ResetCompressionConfig,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Mock outbound adapter for testing.
struct MockOutbound;

#[async_trait::async_trait]
impl jyc_types::OutboundAdapter for MockOutbound {
    fn channel_type(&self) -> &str {
        "mock"
    }

    async fn connect(&self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn disconnect(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn clean_body(&self, raw_body: &str) -> String {
        raw_body.to_string()
    }

    async fn send_reply(
        &self,
        _original: &jyc_types::InboundMessage,
        _reply_text: &str,
        _thread_path: &Path,
        _message_dir: &str,
        _attachments: Option<&[jyc_types::OutboundAttachment]>,
    ) -> anyhow::Result<jyc_types::SendResult> {
        Ok(jyc_types::SendResult {
            message_id: "".to_string(),
        })
    }

    async fn send_message(
        &self,
        _recipient: &str,
        _subject: &str,
        _body: &str,
    ) -> anyhow::Result<jyc_types::SendResult> {
        Ok(jyc_types::SendResult {
            message_id: "".to_string(),
        })
    }
}

fn create_test_config(pattern_names: Vec<&str>) -> jyc_types::AppConfig {
    let mut patterns = Vec::new();
    for name in pattern_names {
        patterns.push(jyc_types::ChannelPattern {
            name: name.to_string(),
            enabled: true,
            rules: jyc_types::PatternRules::default(),
            ..Default::default()
        });
    }

    let mut channels = HashMap::new();
    channels.insert(
        "test_channel".to_string(),
        jyc_types::ChannelConfig {
            channel_type: "email".to_string(),
            inbound: None,
            outbound: None,
            feishu: None,
            gitee: None,
            github: None,
            wechat: None,
            wecom: None,
            wecom_kf: None,
            wecom_bot: None,
            monitor: None,
            patterns: Some(patterns),
            agent: None,
            model: None,
            small_model: None,
            footer: None,
            skills: None,
            disabled_skills: None,
            disabled_tools: None,
            disabled_mcp_servers: None,
            mcps: None,
        },
    );

    jyc_types::AppConfig {
        general: jyc_types::GeneralConfig::default(),
        channels,
        agent: jyc_types::AgentConfig {
            enabled: false,
            mode: "static".to_string(),
            model: None,
            plan_model: None,
            build_model: None,
            small_model: None,
            system_prompt: None,
            max_iterations: 200,
            sse_read_timeout_secs: 120,
            text: None,
            attachments: None,
            providers: HashMap::new(),
            vision: None,
            reset_compression: None,
            auto_reset_threshold: 0.95,
        },
        inspect: None,
        attachments: None,
        wecom: None,
        mcps: Vec::new(),
        scheduler: jyc_types::SchedulerConfig::default(),
    }
}

#[tokio::test]
async fn test_dynamic_pattern_reload() {
    let tmpdir = TempDir::new().unwrap();

    // Start metrics collector
    let cancel = CancellationToken::new();
    let metrics_collector = jyc_core::metrics::MetricsCollector::new(cancel.clone());
    let (metrics_handle, _shared_stats, _metrics_task) = metrics_collector.start();

    // Initial config with 1 pattern
    let config1 = create_test_config(vec!["pattern1"]);
    let config_swap = Arc::new(ArcSwap::from_pointee(config1));

    // Create MessageStorage
    let storage = Arc::new(jyc_core::message_storage::MessageStorage::new(
        tmpdir.path(),
    ));

    // Create ThreadManager with mocks
    let thread_manager = Arc::new(jyc_core::thread_manager::ThreadManager::new(
        1,
        10,
        storage.clone(),
        Arc::new(MockOutbound),
        Arc::new(MockAgent),
        cancel.clone(),
        tmpdir.path().join("templates"),
        config_swap.clone(),
        "test_channel".to_string(),
        "email".to_string(),
        tmpdir.path().to_path_buf(),
        metrics_handle,
    ));

    // Create MessageRouter
    let router = jyc_core::message_router::MessageRouter::new(
        thread_manager,
        storage,
        config_swap.clone(),
        "test_channel".to_string(),
    );

    // Verify initial patterns
    let patterns = router.patterns();
    assert_eq!(patterns.len(), 1);
    assert_eq!(patterns[0].name, "pattern1");

    // Update config with 2 patterns
    let config2 = create_test_config(vec!["pattern1", "pattern2"]);
    config_swap.store(Arc::new(config2));

    // Verify new patterns are read dynamically
    let patterns = router.patterns();
    assert_eq!(patterns.len(), 2);
    assert_eq!(patterns[0].name, "pattern1");
    assert_eq!(patterns[1].name, "pattern2");
}

#[tokio::test]
async fn test_channel_orchestrator_reload() {
    let tmpdir = TempDir::new().unwrap();

    let config1 = create_test_config(vec!["pattern1"]);
    let config_swap = Arc::new(ArcSwap::from_pointee(config1));

    let orchestrator = jyc_core::channel_orchestrator::ChannelOrchestrator::new(
        config_swap.clone(),
        tmpdir.path(),
    );

    // Initially empty
    let tms = orchestrator.thread_managers().load();
    assert!(tms.is_empty());

    // Update config: remove the channel
    let config2 = jyc_types::AppConfig {
        channels: HashMap::new(),
        ..create_test_config(vec![])
    };
    config_swap.store(Arc::new(config2));

    // Reload
    orchestrator.reload().await.unwrap();

    // Still empty (no channels registered, so nothing to remove)
    let tms = orchestrator.thread_managers().load();
    assert!(tms.is_empty());
}

#[tokio::test]
async fn test_channel_orchestrator_register_and_remove() {
    let tmpdir = TempDir::new().unwrap();

    let config1 = create_test_config(vec!["pattern1"]);
    let config_swap = Arc::new(ArcSwap::from_pointee(config1));

    let orchestrator = jyc_core::channel_orchestrator::ChannelOrchestrator::new(
        config_swap.clone(),
        tmpdir.path(),
    );

    // Create a mock thread manager
    let cancel = CancellationToken::new();
    let metrics_collector = jyc_core::metrics::MetricsCollector::new(cancel.clone());
    let (metrics_handle, _shared_stats, _metrics_task) = metrics_collector.start();

    let storage = Arc::new(jyc_core::message_storage::MessageStorage::new(
        tmpdir.path(),
    ));
    let thread_manager = Arc::new(jyc_core::thread_manager::ThreadManager::new(
        1,
        10,
        storage,
        Arc::new(MockOutbound),
        Arc::new(MockAgent),
        cancel.clone(),
        tmpdir.path().join("templates"),
        config_swap.clone(),
        "test_channel".to_string(),
        "email".to_string(),
        tmpdir.path().to_path_buf(),
        metrics_handle,
    ));

    // Register a channel
    let channel_info = jyc_types::ChannelInfo {
        name: "test_channel".to_string(),
        channel_type: "email".to_string(),
        active_workers: 0,
        max_concurrent: 1,
    };

    orchestrator
        .register_channel(
            "test_channel".to_string(),
            jyc_core::channel_orchestrator::ChannelHandle {
                cancel: cancel.clone(),
                thread_manager,
                channel_info,
                workspace_dir: tmpdir.path().to_path_buf(),
            },
        )
        .await;

    // Verify the channel is registered
    let tms = orchestrator.thread_managers().load();
    assert_eq!(tms.len(), 1);
    assert_eq!(tms[0].channel_name(), "test_channel");

    let infos = orchestrator.channel_infos().load();
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].name, "test_channel");

    // Update config: remove the channel
    let config2 = jyc_types::AppConfig {
        channels: HashMap::new(),
        ..create_test_config(vec![])
    };
    config_swap.store(Arc::new(config2));

    // Reload should detect the removed channel and update state
    orchestrator.reload().await.unwrap();

    // Verify the channel is removed from shared state
    let tms = orchestrator.thread_managers().load();
    assert!(tms.is_empty());

    let infos = orchestrator.channel_infos().load();
    assert!(infos.is_empty());

    // Verify the cancel token was fired (channel should be shutting down)
    assert!(cancel.is_cancelled());
}
