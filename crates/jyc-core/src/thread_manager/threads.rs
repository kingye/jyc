//! `ThreadManager` impl block: threads.rs methods.
//!
//! Extracted from the monolithic `thread_manager.rs`.

use std::collections::HashMap;
use std::path::PathBuf;

use jyc_types::{ThreadCost, ThreadInfo, ThreadStatus};

use super::ThreadManager;
/// Per-thread queue stats.
use super::git::{branch_for_thread_path, changed_files_for_thread_path};
use super::worker::read_skills;

impl ThreadManager {
    pub async fn thread_path(&self, thread_name: &str) -> Option<PathBuf> {
        let paths = self.thread_paths.lock().await;
        if let Some(path) = paths.get(thread_name) {
            return Some(path.clone());
        }
        // Fallback: try the default workspace path
        let default_path = self.workspace_dir.join(thread_name);
        if tokio::fs::metadata(&default_path).await.is_ok() {
            return Some(default_path);
        }
        None
    }

    /// Get all custom thread paths (from pattern `thread_path` overrides).
    pub async fn custom_thread_paths(&self) -> HashMap<String, PathBuf> {
        self.thread_paths.lock().await.clone()
    }

    /// Register a custom thread path (e.g. from `jyc dashboard open <path>`
    /// or REST `create_thread`).
    ///
    /// Subsequent calls to `thread_path(thread_name)` will return this path
    /// instead of the default `<workspace>/<thread_name>/`. The directory
    /// is created if it does not already exist. `.jyc/thread-name` is also
    /// written so `list_threads` recognises the entry — without it, the
    /// `path.join(".jyc").is_dir()` filter in `list_threads` drops the
    /// entry and `wait_for_thread` times out for fresh ad-hoc threads.
    pub async fn set_thread_path(&self, thread_name: &str, path: PathBuf) -> std::io::Result<()> {
        tokio::fs::create_dir_all(&path).await?;
        let jyc_dir = path.join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await?;
        tokio::fs::write(jyc_dir.join("thread-name"), thread_name)
            .await
            .ok();
        let mut paths = self.thread_paths.lock().await;
        paths.insert(thread_name.to_string(), path);
        Ok(())
    }

    /// Resolve the enabled pattern whose name matches `thread_name`.
    ///
    /// Used by cross-thread injection (`jyc_send_to_thread`) so injected
    /// messages carry the same pattern identity — name, template/role
    /// metadata, live_injection, custom `thread_path` — as router-matched
    /// messages (#542).
    pub fn pattern_for_thread(&self, thread_name: &str) -> Option<jyc_types::ChannelPattern> {
        let cfg = self.config.load();
        cfg.channels
            .get(&self.channel_name)
            .and_then(|c| c.patterns.as_ref())
            .and_then(|pats| pats.iter().find(|p| p.enabled && p.name == thread_name))
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

    /// Restore custom `thread_path` mappings from disk.
    ///
    /// Scans this ThreadManager's channel patterns for `thread_path` overrides.
    /// For each, checks if the directory exists and contains a `.jyc/thread-name`
    /// file. If so, restores the mapping into the in-memory `thread_paths` map
    /// and pre-creates an event bus so the ActivityTracker can subscribe before
    /// any messages arrive.
    ///
    /// Only patterns belonging to this TM's channel are scanned — each TM only
    /// restores its own threads to avoid cross-channel contamination.
    ///
    /// This is called at startup so that threads with custom paths survive
    /// process restarts (e.g. Docker container restart).
    pub async fn restore_custom_thread_paths(&self) {
        let config = self.config.load();
        let Some(channel_cfg) = config.channels.get(&self.channel_name) else {
            return;
        };
        let Some(patterns) = &channel_cfg.patterns else {
            return;
        };
        for pattern in patterns {
            let Some(tp) = &pattern.thread_path else {
                continue;
            };
            let resolved = crate::thread_path::resolve_thread_path(tp, self.data_root());
            let thread_name_file = resolved.join(".jyc").join("thread-name");
            match tokio::fs::read_to_string(&thread_name_file).await {
                Ok(name) => {
                    let name = name.trim().to_string();
                    if name.is_empty() {
                        continue;
                    }
                    let mut paths = self.thread_paths.lock().await;
                    paths.entry(name.clone()).or_insert_with(|| {
                        tracing::info!(
                            path = %resolved.display(),
                            "Restored custom thread_path from disk"
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
                    // Directory exists but no thread-name file — not yet
                    // initialized, or pre-this-feature thread. Skip.
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        path = %thread_name_file.display(),
                        "Failed to read thread-name file during restore"
                    );
                }
            }
        }
    }

    /// List all open threads with their info, reading state from disk.
    ///
    /// Scans the workspace directory for thread directories containing `.jyc/pattern`.
    /// This includes both actively queued threads and idle threads that have been
    /// created but have no messages pending.
    pub async fn list_threads(&self) -> Vec<ThreadInfo> {
        use crate::session_state::{read_mode_override, read_session_cost, read_token_state};

        // Collect names of actively queued threads
        let queues = self.thread_queues.lock().await;
        let active_names: std::collections::HashSet<String> = queues.keys().cloned().collect();
        drop(queues);

        // Scan workspace for all thread directories with .jyc/ subdirectory
        let mut thread_names: Vec<String> = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir(&self.workspace_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.is_dir()
                    && path.join(".jyc").is_dir()
                    && let Some(name) = entry.file_name().to_str()
                {
                    thread_names.push(name.to_string());
                }
            }
        }

        // Also include threads with custom `thread_path` overrides that live
        // outside the workspace directory (e.g. `~/projects/jyc`).
        // Clean up entries whose directory has been deleted (e.g. thread closed).
        {
            let mut paths = self.thread_paths.lock().await;
            paths.retain(|name, path| {
                let exists = path.join(".jyc").is_dir();
                if !exists {
                    tracing::info!(
                        thread = %name,
                        path = %path.display(),
                        "Removed stale thread_path entry (directory no longer exists)"
                    );
                }
                exists
            });
            for name in paths.keys() {
                if !thread_names.contains(name) {
                    thread_names.push(name.clone());
                }
            }
        }

        thread_names.sort();

        let mut threads = Vec::with_capacity(thread_names.len());

        for name in thread_names {
            // Check for custom thread_path from pattern override first
            let paths = self.thread_paths.lock().await;
            let thread_path = paths
                .get(&name)
                .cloned()
                .unwrap_or_else(|| self.workspace_dir.join(&name));
            drop(paths);

            // Read pattern from .jyc/pattern
            let pattern = tokio::fs::read_to_string(thread_path.join(".jyc").join("pattern"))
                .await
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());

            // Read session state
            let (
                input_tokens,
                max_tokens,
                output_tokens,
                total_input_tokens,
                total_cache_hit_tokens,
                total_cache_creation_tokens,
            ) = read_token_state(&thread_path).await;

            // Read mode first — needed to resolve mode-specific model overrides.
            let mode = read_mode_override(&thread_path).await;

            // Read mode-specific override file first, fallback to legacy.
            let file_override = {
                async fn read_trimmed(path: &std::path::Path) -> Option<String> {
                    tokio::fs::read_to_string(path)
                        .await
                        .ok()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                }
                let plan_path = thread_path.join(".jyc").join("plan-model-override");
                let build_path = thread_path.join(".jyc").join("build-model-override");
                let legacy_path = thread_path.join(".jyc").join("model-override");

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
                    Some("plan") => cfg.agent.plan_model.clone(),
                    _ => cfg.agent.build_model.clone(), // None = build mode
                }
            })
            .or_else(|| self.config.load().agent.model.clone());

            // Read skills from .jyc/skills.json
            let skills = read_skills(&thread_path).await;

            // Determine status
            let status = if thread_path.join(".jyc").join("question-sent.flag").exists() {
                ThreadStatus::WaitingForAnswer
            } else if active_names.contains(&name) {
                // Thread has an active queue — it's either processing or waiting for messages
                ThreadStatus::Idle
            } else {
                // Thread exists on disk but has no active queue — it's dormant
                ThreadStatus::Idle
            };

            // Fallback: read .jyc directory mtime if no activity tracker data
            let last_active_at = match tokio::fs::metadata(thread_path.join(".jyc")).await {
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
                let session = read_session_cost(&thread_path).await;
                let today = crate::billing_log_store::BillingLogStore::today_total(&thread_path);
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
                        // reports `mixed` — a thread that switched between
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
                        Some(ThreadCost {
                            session: session.unwrap_or(0.0),
                            today: today.map(|(amount, _)| amount).unwrap_or(0.0),
                            currency,
                        })
                    }
                }
            };

            threads.push(ThreadInfo {
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
                thread_path: Some(thread_path.clone()),
                branch: branch_for_thread_path(&thread_path),
                changed_files: changed_files_for_thread_path(&thread_path),
                cost,
            });
        }

        threads
    }
}
