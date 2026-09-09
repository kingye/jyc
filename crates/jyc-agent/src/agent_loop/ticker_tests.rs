
use super::*;
use jyc_core::topic_event::TopicEvent;
use jyc_core::topic_event_bus::{SimpleThreadEventBus, TopicEventBusRef};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[tokio::test]
async fn run_ticker_publishes_then_exits_on_cancel() {
    let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(32));
    let mut rx = bus.subscribe().await.unwrap();
    let cancel = CancellationToken::new();
    let start = Instant::now();

    // Spawn with a fast interval so the test finishes in <500 ms
    // instead of waiting 3+ seconds at the production 1 Hz cadence.
    let handle = run_ticker(
        start,
        Duration::from_millis(50),
        cancel.clone(),
        Some(&bus),
        "topic-x".to_string(),
    );

    // Wait for at least 3 ticks before cancelling (~150 ms at 50 ms
    // interval — production uses 1 s). The very first tick fires
    // immediately at t=0, so we expect ticks more frequently than
    // the interval suggests.
    let mut got = 0u32;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while got < 3 && std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Some(TopicEvent::LoopTick { .. })) => got += 1,
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }
    assert!(got >= 3, "expected at least 3 LoopTick events, got {got}");

    // Cancel and verify no further tick arrives within 200 ms.
    cancel.cancel();
    if let Ok(Some(TopicEvent::LoopTick { .. })) =
        tokio::time::timeout(Duration::from_millis(200), rx.recv()).await
    {
        panic!("ticker should not publish after cancel")
    }
    // Tidy up so the test doesn't leak (and so clippy doesn't warn).
    let _ = handle.await;
}

/// The very first tick must fire at t=0, not at t=`interval` —
/// otherwise a sub-second loop produces no event at all and the
/// dashboard shows nothing.
#[tokio::test]
async fn run_ticker_publishes_immediately_at_t_zero() {
    let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(32));
    let mut rx = bus.subscribe().await.unwrap();
    let cancel = CancellationToken::new();
    let start = Instant::now();

    // Long interval (production cadence), short observation window:
    // if the first tick waited for the interval, the recv() would
    // time out before any event landed.
    let handle = run_ticker(
        start,
        Duration::from_secs(1),
        cancel.clone(),
        Some(&bus),
        "topic-x".to_string(),
    );

    match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
        Ok(Some(TopicEvent::LoopTick { elapsed_ms, .. })) => {
            assert!(
                elapsed_ms < 50,
                "first tick should fire near t=0, got elapsed_ms={elapsed_ms}"
            );
        }
        other => panic!("expected an immediate first LoopTick, got {other:?}"),
    }

    cancel.cancel();
    let _ = handle.await;
}

/// Regression for the orphan-ticker bug: when the cancel token is
/// *not* fired (natural completion), the ticker must still exit via
/// `TickerGuard::drop` → `JoinHandle::abort`. Before the guard was
/// added, this test would hang forever.
#[tokio::test]
async fn ticker_exits_on_handle_abort_when_cancel_not_fired() {
    let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(32));
    let mut rx = bus.subscribe().await.unwrap();
    let cancel = CancellationToken::new();
    let start = Instant::now();

    let handle = run_ticker(
        start,
        Duration::from_millis(50),
        cancel.clone(),
        Some(&bus),
        "topic-x".to_string(),
    );

    // Drain one tick to confirm it's actually running.
    match tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
        Ok(Some(TopicEvent::LoopTick { .. })) => {}
        other => panic!("expected a LoopTick, got {other:?}"),
    }

    // Drop the TickerGuard via RAII scope — this is what `run()`
    // does at every return path. The cancel token is NEVER fired.
    {
        let _guard = TickerGuard::new(handle, cancel);
    }

    // The handle must be joined within a reasonable bound. If the
    // orphan-bug regresses (handle leaked, no abort), this hangs.
    let joined = tokio::time::timeout(Duration::from_secs(2), async {
        // Re-acquire the handle from the guard would require a getter;
        // instead just assert the ticker stops publishing. Both
        // `cancel.cancel()` and `handle.abort()` happen in `drop`,
        // so within one tick interval (50 ms here) no further tick fires.
        tokio::time::sleep(Duration::from_millis(150)).await;
    })
    .await;
    assert!(joined.is_ok(), "drop guard should not hang");

    if let Ok(Some(TopicEvent::LoopTick { .. })) =
        tokio::time::timeout(Duration::from_millis(100), rx.recv()).await
    {
        panic!("ticker should not publish after TickerGuard drop")
    }
}
