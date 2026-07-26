//! Shared test helpers for the inspect server/client unit tests.
//!
//! These helpers are `#[cfg(test)]` so they don't appear in release
//! builds. They live at the crate root so both `server::tests` and
//! `client::tests` can use the same `test_context` + `nonexistent_token_home_path`
//! without duplicating the body.

#![cfg(test)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use jyc_types::ChannelInfo;
use tokio::sync::Mutex;

use crate::server::InspectContext;

/// Return a unique, never-on-disk path under the system temp dir.
///
/// Used to point `InspectContext::token_data_home` at a directory that
/// provably has no `inspect-token` file in it — this guarantees the
/// auth middleware's `read_at` call returns `Ok(None)` and the test is
/// hermetic regardless of whether the developer's machine (or CI) has
/// a real token file at the platform data home.
///
/// The nonce includes the process ID and a nanosecond timestamp so
/// concurrent test binaries don't collide.
pub fn nonexistent_token_home_path() -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "jyc-inspect-test-nonexistent-{}-{}",
        std::process::id(),
        nonce
    ))
}

/// Build a default `InspectContext` for tests.
///
/// `token_data_home` is set to a unique nonexistent path so the auth
/// middleware sees no token file and tests are hermetic regardless
/// of the dev environment. Override with `test_context_with_token_home`
/// for tests that exercise the file-present path.
pub fn test_context() -> Arc<InspectContext> {
    Arc::new(InspectContext {
        thread_managers: Arc::new(ArcSwap::from_pointee(vec![])),
        channels: Arc::new(ArcSwap::from_pointee(vec![ChannelInfo {
            name: "emf".to_string(),
            channel_type: "github".to_string(),
            active_workers: 0,
            max_concurrent: 0,
        }])),
        health_stats: Arc::new(Mutex::new(jyc_core::metrics::HealthStats::default())),
        activity_map: Arc::new(Mutex::new(std::collections::HashMap::new())),
        start_time: Instant::now(),
        config_path: None,
        global_config_path: None,
        config: None,
        workspace_dirs: Arc::new(ArcSwap::from_pointee(vec![])),
        websocket_handlers: None,
        reload_callback: None,
        token_data_home: Some(nonexistent_token_home_path()),
    })
}
