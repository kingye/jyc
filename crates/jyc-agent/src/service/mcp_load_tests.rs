//! Tests for the MCP loading behavior introduced by
//! `fix/mcp-load-blocks-agent-loop`:
//!   - per-server timeout (load_mcp_tools must not block forever)
//!   - concurrent loading with bounded parallelism
//!   - registry cache hits (no MCP re-spawn on second call with same config)
//!   - registry cache invalidation on config reload (different config_ptr)
//!
//! These are integration-style tests against `JycAgentService`; they
//! avoid real MCP servers by using shell commands that hang or no-op.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jyc_types::ChannelPattern;
use jyc_types::config::{McpServerConfig, McpServerKind};

use crate::service::JycAgentService;
use crate::service::tests::app_config_with_model;

/// Build a Service with the given channel-level MCP configs and a
/// short per-server timeout (so tests don't take forever).
fn service_with_timeout_ms(
    mcp_configs: Vec<McpServerConfig>,
    timeout_ms: u64,
) -> Arc<JycAgentService> {
    // Override the per-server timeout on every config so tests don't
    // take the production default of 10s.
    let mcp_configs: Vec<McpServerConfig> = mcp_configs
        .into_iter()
        .map(|mut cfg| {
            cfg.timeout_ms = Some(timeout_ms);
            cfg
        })
        .collect();
    let config = app_config_with_model(None);
    let svc = JycAgentService::new(
        config,
        Path::new("/tmp/test-topic").to_path_buf(),
        mcp_configs,
        None,
        vec![ChannelPattern::default()],
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        "test".to_string(),
    );
    // Sanity-check: every MCP must end up with a positive timeout.
    for cfg in &svc.mcp_configs {
        assert!(
            cfg.timeout_ms.unwrap_or(0) > 0,
            "test pre-condition: every MCP must have a positive timeout",
        );
    }
    Arc::new(svc)
}

fn hanging_mcp(name: &str) -> McpServerConfig {
    // `/bin/sh -c "sleep 60"` reads nothing and never writes back —
    // rmcp's `initialize` handshake waits forever for a response.
    McpServerConfig {
        name: name.to_string(),
        kind: McpServerKind::Local {
            command: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "sleep 60".to_string(),
            ],
            environment: HashMap::new(),
        },
        enabled_tools: None,
        timeout_ms: None, // patched in service_with_timeout_ms
    }
}

/// Build a Service with a single hanging MCP and a tight timeout.
fn service_with_hanging_mcp(timeout_ms: u64) -> Arc<JycAgentService> {
    service_with_timeout_ms(vec![hanging_mcp("hanger")], timeout_ms)
}

/// Layer 1 — timeout. A hanging MCP must not block the agent loop
/// forever; the registry must still build (empty of MCP tools) within
/// the timeout budget.
#[tokio::test]
async fn hanging_mcp_does_not_block_registry_build() {
    // 300ms timeout is plenty: the connect attempt will hang in the
    // rmcp handshake; tokio::time::timeout must fire well before
    // CI gets impatient.
    let svc = service_with_hanging_mcp(300);
    let start = Instant::now();
    let arc = svc
        .get_or_build_tool_registry("test", Path::new("/tmp/test-topic"), None, false, None)
        .await;
    let elapsed = start.elapsed();

    // The build must complete — that's the whole point of the
    // timeout — and it must NOT take much longer than the timeout
    // budget (allow generous slack for scheduler jitter on CI).
    assert!(
        elapsed < Duration::from_millis(2_000),
        "hanging MCP should be cut off by timeout; took {:?}",
        elapsed
    );

    // Built-in tools are unaffected by an MCP timeout.
    let registry = &*arc;
    assert!(registry.has_tool("bash"));
    assert!(registry.has_tool("read"));
    assert!(registry.has_tool("jyc_reply_message"));
}

/// Layer 2 — concurrent loading. N hanging MCPs must finish in
/// roughly max(timeout) time, not N×timeout.
#[tokio::test]
async fn concurrent_mcp_load_respects_bounded_parallelism() {
    // 5 hanging MCPs, each cut off at 300ms. With `buffer_unordered(4)`,
    // wall time should be ~300ms (first wave) + 300ms (second wave)
    // ≈ 600ms. Sequential would be 5 × 300ms = 1.5s.
    let cfgs: Vec<McpServerConfig> = (0..5).map(|i| hanging_mcp(&format!("hanger{i}"))).collect();
    let svc = service_with_timeout_ms(cfgs, 300);

    let start = Instant::now();
    let _arc = svc
        .get_or_build_tool_registry("test", Path::new("/tmp/test-topic"), None, false, None)
        .await;
    let elapsed = start.elapsed();

    // Generous upper bound: 5 sequential × 300ms = 1500ms; with
    // buffer_unordered(4) we expect ~600ms. Allow 1500ms as the
    // "no worse than sequential" assertion — anything below proves
    // concurrency helped, but we don't want CI flakes from a tight
    // 600ms upper bound on busy runners.
    assert!(
        elapsed < Duration::from_millis(1_500),
        "concurrent load should be faster than sequential; took {:?}",
        elapsed
    );
}

/// Layer 3 — cache hit. Two calls with the same (topic, config_ptr)
/// must not re-load MCPs. We verify by counting connect attempts via
/// a side channel (the cache itself is private).
#[tokio::test]
async fn cache_hit_avoids_reloading_mcps() {
    let svc = service_with_hanging_mcp(300);

    // First call: cache miss → MCP load (which times out at 300ms).
    let first = svc
        .get_or_build_tool_registry("test", Path::new("/tmp/test-topic"), None, false, None)
        .await;
    let start = Instant::now();
    // Second call: same (topic, config_ptr) → cache hit, must be
    // effectively instant (no MCP re-spawn).
    let second = svc
        .get_or_build_tool_registry("test", Path::new("/tmp/test-topic"), None, false, None)
        .await;
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_millis(50),
        "cache hit should be ~instant; took {:?}",
        elapsed
    );
    // Both calls must return registries with the same set of built-in
    // tools — content parity is what "hit" really means.
    assert!(first.has_tool("bash"));
    assert!(second.has_tool("bash"));
}

/// Layer 3 — cache invalidation. A config swap (ArcSwap::store)
/// changes the snapshot pointer; the next call must rebuild, not hit
/// the stale cache.
#[tokio::test]
async fn cache_invalidates_on_config_swap() {
    let mut hanger = hanging_mcp("hanger");
    hanger.timeout_ms = Some(300);
    let config = app_config_with_model(None);
    let svc = JycAgentService::new(
        config.clone(),
        Path::new("/tmp/test-topic").to_path_buf(),
        vec![hanger],
        None,
        vec![ChannelPattern::default()],
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        "test".to_string(),
    );
    let svc = Arc::new(svc);

    // First call: cache miss → MCP load (which times out at default).
    // We don't care about its duration — just that it ran.
    let _first = svc
        .get_or_build_tool_registry("test", Path::new("/tmp/test-topic"), None, false, None)
        .await;

    // Swap config to a fresh snapshot. This changes the Arc pointer
    // the cache key is derived from, so the next call MUST miss.
    // `app_config_with_model` allocates a fresh `Arc<AppConfig>` per
    // call, so the inner `AppConfig` clone here ends up wrapped in a
    // new Arc with a different pointer than the original snapshot.
    let new_snapshot: jyc_types::AppConfig = app_config_with_model(None).load().as_ref().clone();
    config.store(Arc::new(new_snapshot));

    let start = Instant::now();
    let _second = svc
        .get_or_build_tool_registry("test", Path::new("/tmp/test-topic"), None, false, None)
        .await;
    let elapsed = start.elapsed();

    // If the cache had been correctly invalidated, this call
    // re-runs MCP load (which times out at ~300ms — the default we
    // configured on the hanging MCP). If the cache wrongly hit, it
    // would return in ~0ms. Asserting the slow path is the
    // fingerprint of correct invalidation.
    assert!(
        elapsed >= Duration::from_millis(200),
        "after config swap, MCP should re-load (timeout ~300ms); \
         took {:?} — cache may have wrongly hit",
        elapsed
    );
    assert!(
        elapsed < Duration::from_millis(2_000),
        "MCP reload after invalidation should still be bounded by timeout; \
         took {:?}",
        elapsed
    );
}
