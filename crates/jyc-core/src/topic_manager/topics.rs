//! `TopicManager` impl block: topics.rs methods.
//!
//! Extracted from the monolithic `topic_manager.rs`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use jyc_types::{TopicCost, TopicInfo, TopicStatus};

use super::TopicManager;
/// Per-topic queue stats.
use super::git::{branch_for_topic_path, changed_files_for_topic_path};
use super::worker::read_skills;

/// Display-relevant state for one topic (mode, model, context usage).
/// Produced by [`TopicManager::topic_display_state`]; all fields are
/// `None` when the topic has no recorded state yet.
#[derive(Debug, Default, Clone)]
pub struct TopicDisplayState {
    /// Effective mode ("plan"/"build") after the override chain.
    pub mode: Option<String>,
    /// Effective model after the full override chain.
    pub model: Option<String>,
    /// Current context input tokens from the session state file.
    pub input_tokens: Option<u64>,
    /// Context window size.
    pub max_tokens: Option<u64>,
}

impl TopicDisplayState {
    /// Input-token percentage of the context window. `None` when either
    /// bound is missing or `max_tokens` is zero (mirrors the dashboard's
    /// `input_token_pct`).
    pub fn context_pct(&self) -> Option<u32> {
        let cur = self.input_tokens?;
        let max = self.max_tokens?;
        if max == 0 {
            return None;
        }
        Some(
            cur.checked_mul(100)
                .and_then(|v| v.checked_div(max))
                .unwrap_or(0) as u32,
        )
    }
}

impl TopicManager {
    pub async fn topic_path(&self, topic_name: &str) -> Option<PathBuf> {
        let paths = self.topic_paths.lock().await;
        if let Some(path) = paths.get(topic_name) {
            return Some(path.clone());
        }
        // Fallback: try the default workspace path
        let default_path = self.workspace_dir.join(topic_name);
        if tokio::fs::metadata(&default_path).await.is_ok() {
            return Some(default_path);
        }
        None
    }

    /// Resolve display-relevant per-topic state: pattern, session token
    /// usage, mode override, and the model override chain. Shared by
    /// `list_topics()` and `topic_display_state()` so the dashboard and
    /// the Feishu status card can never drift apart. Small state-file
    /// reads only — no workspace scan, no git calls.
    ///
    /// Returns `(pattern, token_state, mode, model)` where `token_state`
    /// is the raw tuple from `read_token_state()`.
    #[allow(clippy::type_complexity)]
    async fn resolve_display_state(
        &self,
        topic_path: &Path,
    ) -> (
        Option<String>,
        (
            Option<u64>,
            Option<u64>,
            Option<u64>,
            Option<u64>,
            Option<u64>,
            Option<u64>,
        ),
        Option<String>,
        Option<String>,
    ) {
        // Read pattern from .jyc/pattern
        let pattern = tokio::fs::read_to_string(topic_path.join(".jyc").join("pattern"))
            .await
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        // Read session state
        let token_state = crate::session_state::read_token_state(topic_path).await;

        // Read mode first — needed to resolve mode-specific model overrides.
        // Chain: .jyc/mode-override > pattern mode from config > build default.
        let mode = crate::session_state::resolve_effective_mode(
            topic_path,
            &self.config.load(),
            &self.channel_name,
        )
        .await;

        // Read mode-specific override file first, fallback to legacy.
        let file_override = {
            async fn read_trimmed(path: &std::path::Path) -> Option<String> {
                tokio::fs::read_to_string(path)
                    .await
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            }
            let plan_path = topic_path.join(".jyc").join("plan-model-override");
            let build_path = topic_path.join(".jyc").join("build-model-override");
            let legacy_path = topic_path.join(".jyc").join("model-override");

            let mode_specific = match mode.as_deref() {
                Some("plan") => read_trimmed(&plan_path).await,
                _ => read_trimmed(&build_path).await, // None = build mode
            };
            if mode_specific.is_some() {
                mode_specific
            } else {
                read_trimmed(&legacy_path).await
            }
        };

        // Resolve effective model with priority:
        // 1. .jyc/<mode>-model-override (mode-specific runtime override)
        // 2. .jyc/model-override (legacy generic override)
        // 3. Pattern-level plan_model / build_model (mode-specific)
        // 4. Pattern-level model (generic)
        // 5. Channel-level model (generic)
        // 6. Global plan_model / build_model (mode-specific)
        // 7. Global model (generic)

        let model = if let Some(ref m) = file_override {
            Some(m.clone())
        } else if let Some(ref pattern_name) = pattern {
            let cfg = self.config.load();
            let channel_cfg = cfg.channels.get(&self.channel_name);
            let pattern_override = channel_cfg
                .and_then(|c| c.patterns.as_ref())
                .and_then(|pats| pats.iter().find(|p| p.name == *pattern_name));
            pattern_override
                .and_then(|p| match mode.as_deref() {
                    Some("plan") => p.plan_model.clone(),
                    _ => p.build_model.clone(), // None = build mode
                })
                .or_else(|| pattern_override.and_then(|p| p.model.clone()))
        } else {
            None
        }
        .or_else(|| {
            let cfg = self.config.load();
            let channel_cfg = cfg.channels.get(&self.channel_name)?;
            channel_cfg.model.clone()
        })
        .or_else(|| {
            let cfg = self.config.load();
            match mode.as_deref() {
                Some("plan") => cfg.ai.plan_model.clone(),
                _ => cfg.ai.build_model.clone(), // None = build mode
            }
        })
        .or_else(|| self.config.load().ai.model.clone());

        (pattern, token_state, mode, model)
    }

    /// Lightweight display state for one topic (mode, model, context
    /// usage) — same resolution as `list_topics()` via the shared helper,
    /// but without the workspace scan or git calls, so the Feishu
    /// status-card watcher can poll it every few seconds.
    pub async fn topic_display_state(&self, topic_name: &str) -> TopicDisplayState {
        let Some(topic_path) = self.topic_path(topic_name).await else {
            return TopicDisplayState::default();
        };
        let (_pattern, token_state, mode, model) = self.resolve_display_state(&topic_path).await;
        let (input_tokens, max_tokens, ..) = token_state;
        TopicDisplayState {
            mode,
            model,
            input_tokens,
            max_tokens,
        }
    }

    /// Get all custom topic paths (from pattern `topic_path` overrides).
    pub async fn custom_topic_paths(&self) -> HashMap<String, PathBuf> {
        self.topic_paths.lock().await.clone()
    }

    /// Register a custom topic path (e.g. from `jyc dashboard open <path>`
    /// or REST `create_topic`).
    ///
    /// Subsequent calls to `topic_path(topic_name)` will return this path
    /// instead of the default `<workspace>/<topic_name>/`. The directory
    /// is created if it does not already exist. `.jyc/topic-name` is also
    /// written so `list_topics` recognises the entry — without it, the
    /// `path.join(".jyc").is_dir()` filter in `list_topics` drops the
    /// entry and `wait_for_topic` times out for fresh ad-hoc topics.
    pub async fn set_topic_path(&self, topic_name: &str, path: PathBuf) -> std::io::Result<()> {
        tokio::fs::create_dir_all(&path).await?;
        let jyc_dir = path.join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await?;
        tokio::fs::write(jyc_dir.join("topic-name"), topic_name)
            .await
            .ok();
        let mut paths = self.topic_paths.lock().await;
        paths.insert(topic_name.to_string(), path);
        Ok(())
    }

    /// Resolve the enabled pattern whose name matches `topic_name`.
    ///
    /// Used by cross-topic injection (`jyc_send_to_topic`) so injected
    /// messages carry the same pattern identity — name, template/role
    /// metadata, live_injection, custom `topic_path` — as router-matched
    /// messages (#542).
    pub fn pattern_for_topic(&self, topic_name: &str) -> Option<jyc_types::ChannelPattern> {
        let cfg = self.config.load();
        cfg.channels
            .get(&self.channel_name)
            .and_then(|c| c.patterns.as_ref())
            .and_then(|pats| pats.iter().find(|p| p.enabled && p.name == topic_name))
            .cloned()
    }

    /// Names of enabled patterns for this channel.
    ///
    /// Used by the inspect server's `list_patterns` REST handler to
    /// populate the dashboard's pattern-select UI.
    pub async fn pattern_names(&self) -> Vec<String> {
        let cfg = self.config.load();
        cfg.channels
            .get(&self.channel_name)
            .and_then(|c| c.patterns.as_ref())
            .map(|pats| {
                pats.iter()
                    .filter(|p| p.enabled)
                    .map(|p| p.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Restore custom `topic_path` mappings from disk.
    ///
    /// Scans this TopicManager's channel patterns for `topic_path` overrides.
    /// For each, checks if the directory exists and contains a `.jyc/topic-name`
    /// file. If so, restores the mapping into the in-memory `topic_paths` map
    /// and pre-creates an event bus so the ActivityTracker can subscribe before
    /// any messages arrive.
    ///
    /// Only patterns belonging to this TM's channel are scanned — each TM only
    /// restores its own topics to avoid cross-channel contamination.
    ///
    /// This is called at startup so that topics with custom paths survive
    /// process restarts (e.g. Docker container restart).
    pub async fn restore_custom_topic_paths(&self) {
        let config = self.config.load();
        let Some(channel_cfg) = config.channels.get(&self.channel_name) else {
            return;
        };
        let Some(patterns) = &channel_cfg.patterns else {
            return;
        };
        for pattern in patterns {
            // A pinned `topic_path` holds exactly one topic (the self-named
            // one); an agent's default root holds one subdirectory per
            // topic. Scan whichever this pattern has — for the agents
            // channel that is always the default root, since a pinned path
            // never contains sibling topics.
            let resolved = match &pattern.topic_path {
                Some(tp) => crate::topic_path::resolve_topic_path(tp, self.data_root()),
                None if self.channel_name == "agents" => self.workspace_dir.join(&pattern.name),
                None => continue,
            };
            let jyc_dir = resolved.join(".jyc");
            // One-time migration for the topic → topic rename.
            crate::topic_path::migrate_topic_name_file(&jyc_dir);
            let topic_name_file = jyc_dir.join("topic-name");
            match tokio::fs::read_to_string(&topic_name_file).await {
                Ok(name) => {
                    let name = name.trim().to_string();
                    if name.is_empty() {
                        continue;
                    }
                    let mut paths = self.topic_paths.lock().await;
                    paths.entry(name.clone()).or_insert_with(|| {
                        tracing::info!(
                            path = %resolved.display(),
                            "Restored custom topic_path from disk"
                        );
                        resolved
                    });
                    drop(paths);
                    // Pre-create the event bus so the ActivityTracker can
                    // subscribe before the first message arrives. Without this,
                    // events from the first message are lost because the
                    // ActivityTracker's 2s poll hasn't discovered the bus yet.
                    if self.enable_events {
                        self.get_or_create_event_bus(&name).await;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Legacy single-topic file missing — fall through
                    // to the multi-topic-per-agent scan underneath.
                    self.scan_subdirs_for_topics(&resolved).await;
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        path = %topic_name_file.display(),
                        "Failed to read topic-name file during restore"
                    );
                }
            }
        }
    }

    /// Register one custom-path entry (with event-bus pre-create).
    ///
    /// Shared by the legacy single-topic-per-pattern restore and the new
    /// multi-topic-per-agent scan; extracted so both branches share the
    /// same locking + tracing.
    async fn register_custom_path(&self, topic_name: &str, resolved: PathBuf) {
        let mut paths = self.topic_paths.lock().await;
        paths.entry(topic_name.to_string()).or_insert_with(|| {
            tracing::info!(
                topic = %topic_name,
                path = %resolved.display(),
                "Restored custom topic_path from disk"
            );
            resolved.clone()
        });
        drop(paths);
        if self.enable_events {
            self.get_or_create_event_bus(topic_name).await;
        }
    }

    /// Scan a directory for entries whose `.jyc/topic-name` exists, and
    /// register each one in `topic_paths`. Used by the multi-topic-per-agent
    /// restore path: the synthesized pattern's `topic_path` points at the
    /// agent root, and each sub-topic lives at
    /// `<agent_root>/<topic>/.jyc/topic-name`.
    async fn scan_subdirs_for_topics(&self, agent_root: &std::path::Path) {
        let Ok(mut entries) = tokio::fs::read_dir(agent_root).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let jyc_dir = path.join(".jyc");
            if !jyc_dir.is_dir() {
                continue;
            }
            crate::topic_path::migrate_topic_name_file(&jyc_dir);
            let topic_name_file = jyc_dir.join("topic-name");
            match tokio::fs::read_to_string(&topic_name_file).await {
                Ok(name) => {
                    let name = name.trim().to_string();
                    if name.is_empty() {
                        continue;
                    }
                    self.register_custom_path(&name, path.clone()).await;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Empty `.jyc/` (template init not finished yet) — skip.
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        path = %topic_name_file.display(),
                        "Failed to read topic-name file during restore"
                    );
                }
            }
        }
    }

    /// List all open topics with their info, reading state from disk.
    ///
    /// Scans the workspace directory for topic directories containing `.jyc/pattern`.
    /// This includes both actively queued topics and idle topics that have been
    /// created but have no messages pending.
    pub async fn list_topics(&self) -> Vec<TopicInfo> {
        use crate::session_state::read_session_cost;

        // Collect names of actively queued topics
        let queues = self.topic_queues.lock().await;
        let active_names: std::collections::HashSet<String> = queues.keys().cloned().collect();
        drop(queues);

        // Scan workspace for all topic directories with .jyc/ subdirectory
        let mut topic_names: Vec<String> = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir(&self.workspace_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.is_dir()
                    && path.join(".jyc").is_dir()
                    && let Some(name) = entry.file_name().to_str()
                {
                    topic_names.push(name.to_string());
                }
            }
        }

        // Also include topics with custom `topic_path` overrides that live
        // outside the workspace directory (e.g. `~/projects/jyc`).
        // Clean up entries whose directory has been deleted (e.g. topic closed).
        {
            let mut paths = self.topic_paths.lock().await;
            paths.retain(|name, path| {
                let exists = path.join(".jyc").is_dir();
                if !exists {
                    tracing::info!(
                        topic = %name,
                        path = %path.display(),
                        "Removed stale topic_path entry (directory no longer exists)"
                    );
                }
                exists
            });
            for name in paths.keys() {
                if !topic_names.contains(name) {
                    topic_names.push(name.clone());
                }
            }
        }

        topic_names.sort();

        let mut topics = Vec::with_capacity(topic_names.len());

        for name in topic_names {
            // Check for custom topic_path from pattern override first
            let paths = self.topic_paths.lock().await;
            let topic_path = paths
                .get(&name)
                .cloned()
                .unwrap_or_else(|| self.workspace_dir.join(&name));
            drop(paths);

            // Resolve display state (pattern, mode, model, token usage) —
            // shared with `topic_display_state()` so consumers can never
            // drift apart.
            let (pattern, token_state, mode, model) = self.resolve_display_state(&topic_path).await;
            let (
                input_tokens,
                max_tokens,
                output_tokens,
                total_input_tokens,
                total_cache_hit_tokens,
                total_cache_creation_tokens,
            ) = token_state;

            // Read skills from .jyc/skills.json
            let skills = read_skills(&topic_path).await;

            // Determine status
            let status = if topic_path.join(".jyc").join("question-sent.flag").exists() {
                TopicStatus::WaitingForAnswer
            } else if active_names.contains(&name) {
                // Topic has an active queue — it's either processing or waiting for messages
                TopicStatus::Idle
            } else {
                // Topic exists on disk but has no active queue — it's dormant
                TopicStatus::Idle
            };

            // Fallback: read .jyc directory mtime if no activity tracker data
            let last_active_at = match tokio::fs::metadata(topic_path.join(".jyc")).await {
                Ok(meta) => match meta.modified() {
                    Ok(mtime) => {
                        let dt: chrono::DateTime<chrono::Utc> = mtime.into();
                        Some(dt.to_rfc3339())
                    }
                    Err(_) => None,
                },
                Err(_) => None,
            };

            // Resolve accumulated cost: session-scoped from the session
            // file, today's durable total from the billing ledger. Both are
            // absent when the model has no configured pricing, in which case
            // `cost` stays `None` and the dashboard omits the row.
            let cost = {
                let session = read_session_cost(&topic_path).await;
                let today = crate::billing_log_store::BillingLogStore::today_total(&topic_path);
                match (session, today) {
                    (None, None) => None,
                    (session, today) => {
                        // Currency comes from the model's configured pricing,
                        // NOT from the ledger. The ledger is empty on a fresh
                        // UTC day while `session_cost` is still carrying spend
                        // from before midnight, and falling back to a constant
                        // there would label a CNY amount with `$` (or the
                        // reverse). A correct amount under the wrong unit is
                        // worse than showing nothing.
                        //
                        // The ledger's own label is still preferred when it
                        // reports `mixed` — a topic that switched between
                        // differently-priced models in one day is a real
                        // signal that config alone cannot express.
                        let ledger_currency = today.as_ref().map(|(_, c)| c.clone());
                        let mixed = ledger_currency.as_deref()
                            == Some(crate::billing_log_store::MIXED_CURRENCY);
                        let currency = if mixed {
                            ledger_currency.unwrap_or_default()
                        } else {
                            model
                                .as_deref()
                                .and_then(|m| {
                                    jyc_types::pricing::lookup_pricing(&self.config.load(), m)
                                })
                                .map(|p| p.currency_label().to_string())
                                .or(ledger_currency)
                                .unwrap_or_else(|| jyc_types::DEFAULT_CURRENCY.to_string())
                        };
                        Some(TopicCost {
                            session: session.unwrap_or(0.0),
                            today: today.map(|(amount, _)| amount).unwrap_or(0.0),
                            currency,
                        })
                    }
                }
            };

            topics.push(TopicInfo {
                name,
                channel: self.channel_name.clone(),
                pattern,
                status,
                model,
                mode,
                context_input_tokens: input_tokens,
                max_tokens,
                output_tokens,
                total_input_tokens,
                total_cache_hit_tokens,
                total_cache_creation_tokens,
                activity: vec![], // Filled by InspectServer from event bus
                last_active_at,   // Filled by activity tracker; falls back to .jyc mtime
                skills,
                recent_messages: vec![], // Filled by InspectServer from event bus
                thinking_text: None,     // Filled by InspectServer from event bus
                thinking_blocks: vec![], // Filled by InspectServer from event bus
                topic_path: Some(topic_path.clone()),
                branch: branch_for_topic_path(&topic_path),
                changed_files: changed_files_for_topic_path(&topic_path),
                cost,
            });
        }

        topics
    }
}

#[cfg(test)]
mod list_topics_tests {
    use super::*;
    use crate::message_storage::MessageStorage;
    use crate::metrics::MetricsCollector;
    use crate::static_agent::StaticAgentService;
    use anyhow::Result;
    use jyc_types::{InboundMessage, OutboundAdapter};
    use std::path::Path;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    /// Minimal outbound adapter that does nothing.
    struct NoopOutbound;

    #[async_trait::async_trait]
    impl OutboundAdapter for NoopOutbound {
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

    /// Config with channel `test` and one pattern `p1` whose mode is "plan"
    /// and which carries distinct plan/build models.
    fn test_config_str() -> &'static str {
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
[[channels.test.patterns]]
name = "p1"
mode = "plan"
plan_model = "deepseek/deepseek-reasoner"
build_model = "deepseek/deepseek-chat"
[agent]
enabled = true
mode = "agent"
"#
    }

    fn make_tm(workspace: &Path) -> Arc<TopicManager> {
        let storage = Arc::new(MessageStorage::new(workspace));
        let cancel = CancellationToken::new();
        let metrics_cancel = CancellationToken::new();
        let (metrics, _stats, _metrics_task) = MetricsCollector::new(metrics_cancel).start();
        let config = Arc::new(arc_swap::ArcSwap::from_pointee(
            jyc_types::load_config_from_str(test_config_str()).unwrap(),
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
            "test".to_string(),
            "websocket".to_string(),
            workspace.parent().unwrap_or(workspace).to_path_buf(),
            workspace.to_path_buf(),
            metrics,
            None,
        ))
    }

    /// #615: a topic whose mode comes from pattern config (no
    /// `.jyc/mode-override` file) must display that mode and resolve the
    /// mode-specific model chain accordingly.
    #[tokio::test]
    async fn list_topics_resolves_mode_from_pattern_config() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let topic_path = workspace.join("plan-615");
        tokio::fs::create_dir_all(topic_path.join(".jyc"))
            .await
            .unwrap();
        tokio::fs::write(topic_path.join(".jyc").join("pattern"), "p1\n")
            .await
            .unwrap();
        let tm = make_tm(&workspace);

        let topics = tm.list_topics().await;
        let info = topics
            .iter()
            .find(|t| t.name == "plan-615")
            .expect("topic should be listed");
        assert_eq!(info.mode.as_deref(), Some("plan"));
        assert_eq!(info.model.as_deref(), Some("deepseek/deepseek-reasoner"));
    }

    /// The explicit `.jyc/mode-override` file still wins over the pattern
    /// config mode.
    #[tokio::test]
    async fn list_topics_mode_override_wins_over_pattern_config() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let topic_path = workspace.join("plan-615");
        tokio::fs::create_dir_all(topic_path.join(".jyc"))
            .await
            .unwrap();
        tokio::fs::write(topic_path.join(".jyc").join("pattern"), "p1\n")
            .await
            .unwrap();
        tokio::fs::write(topic_path.join(".jyc").join("mode-override"), "build\n")
            .await
            .unwrap();
        let tm = make_tm(&workspace);

        let topics = tm.list_topics().await;
        let info = topics
            .iter()
            .find(|t| t.name == "plan-615")
            .expect("topic should be listed");
        assert_eq!(info.mode.as_deref(), Some("build"));
        assert_eq!(info.model.as_deref(), Some("deepseek/deepseek-chat"));
    }

    /// `topic_display_state` resolves the same mode/model chain as
    /// `list_topics` plus context token usage — without a workspace scan
    /// or git calls (polled by the Feishu status-card watcher).
    #[tokio::test]
    async fn topic_display_state_resolves_mode_model_and_context() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let topic_path = workspace.join("plan-615");
        tokio::fs::create_dir_all(topic_path.join(".jyc"))
            .await
            .unwrap();
        tokio::fs::write(topic_path.join(".jyc").join("pattern"), "p1\n")
            .await
            .unwrap();
        tokio::fs::write(
            topic_path.join(".jyc").join("agent-session.json"),
            r#"{"context_input_tokens":1500,"max_input_tokens":10000}"#,
        )
        .await
        .unwrap();
        let tm = make_tm(&workspace);

        let state = tm.topic_display_state("plan-615").await;
        assert_eq!(state.mode.as_deref(), Some("plan"));
        assert_eq!(state.model.as_deref(), Some("deepseek/deepseek-reasoner"));
        assert_eq!(state.input_tokens, Some(1500));
        assert_eq!(state.max_tokens, Some(10000));
        assert_eq!(state.context_pct(), Some(15));

        // Mode override file flips mode and the mode-specific model.
        tokio::fs::write(topic_path.join(".jyc").join("mode-override"), "build\n")
            .await
            .unwrap();
        let state = tm.topic_display_state("plan-615").await;
        assert_eq!(state.mode.as_deref(), Some("build"));
        assert_eq!(state.model.as_deref(), Some("deepseek/deepseek-chat"));

        // Unknown topic → all fields None (no directory, no state files).
        let state = tm.topic_display_state("no-such-topic").await;
        assert!(state.mode.is_none() && state.model.is_none());
        assert_eq!(state.context_pct(), None);
    }
}
