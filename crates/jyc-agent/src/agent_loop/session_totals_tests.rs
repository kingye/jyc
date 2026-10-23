//! The loop's own wiring of the session's token totals.
//!
//! The session's totals are the base the loop adds this turn's calls to, so
//! a turn continues the session's counts instead of restarting them. That
//! wiring is the loop's own — the reader seeds the running sums, and every
//! call adds its own figures to them — so it is tested here, against a
//! session file that already holds an earlier turn's totals. The reader and
//! the writer are covered by their own unit tests; what is missing between
//! them is the loop, and this is it.

use super::event_test_helpers::scripted::ScriptedProvider;
use super::event_test_helpers::test_config;
use super::*;
use crate::types::StreamEvent;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;

/// The usage the one call reports: the figures the loop adds to the
/// session's running totals, and to the turn's own pair.
const CALL_INPUT: u64 = 700;
const CALL_OUTPUT: u64 = 50;
const CALL_CACHE_HIT: u64 = 300;

/// The totals the session has banked before this turn — the base the loop's
/// running sums start from.
const SEED_INPUT: u64 = 1_000;
const SEED_OUTPUT: u64 = 200;
const SEED_CACHE_HIT: u64 = 500;

/// A turn continues the session's counts: the loop adds this turn's call to
/// the totals the session already holds, while the turn's own pair covers
/// this turn's call alone.
#[tokio::test]
async fn the_session_totals_continue_the_earlier_turns() {
    let topic_name = "session-totals-test";
    let tmp = TempDir::new().unwrap();
    let working_dir = tmp.path().to_path_buf();
    let tools = crate::tools::builtin::create_builtin_registry();
    let provider = ScriptedProvider {
        rounds: vec![vec![
            StreamEvent::Usage {
                input_tokens: CALL_INPUT,
                output_tokens: CALL_OUTPUT,
                cache_hit_tokens: CALL_CACHE_HIT,
                cache_creation_tokens: 0,
                reasoning_tokens: 0,
            },
            StreamEvent::TextDelta("done.".to_string()),
            StreamEvent::Done,
        ]],
        calls: AtomicUsize::new(0),
        seen_tools: Mutex::new(vec![]),
    };
    // `test_config` registers the topic's state dir, so the session file
    // below lands where the loop will look for it.
    let config = test_config(
        &provider,
        &tools,
        &working_dir,
        CancellationToken::new(),
        topic_name,
    );

    // A session that has banked an earlier turn: its totals, and the turn
    // pair that turn left behind.
    crate::session::ensure_session_file(topic_name, &working_dir, None).await;
    crate::session::persist_tokens(
        topic_name,
        &working_dir,
        100,
        20,
        SEED_INPUT,
        SEED_OUTPUT,
        SEED_CACHE_HIT,
        0,
        0,
        None,
        0.0,
        10,
        60,
        70,
        0,
        0,
    )
    .await;

    // The turn starts: the turn-scoped pair is zeroed, the session's totals
    // are not — they answer a different question and outlive the turn.
    crate::session::begin_turn_counters(topic_name, &working_dir).await;

    run(config)
        .await
        .expect("the agent loop should run to completion");

    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        1,
        "the scripted text-only response ends the turn after one call"
    );

    let state = jyc_core::session_state::read_token_state(topic_name, &working_dir).await;
    assert_eq!(
        state.total_input_tokens,
        Some(SEED_INPUT + CALL_INPUT),
        "the session's input total continues the earlier turn's"
    );
    assert_eq!(
        state.total_output_tokens,
        Some(SEED_OUTPUT + CALL_OUTPUT),
        "the session's output total continues the earlier turn's"
    );
    assert_eq!(
        state.total_cache_hit_tokens,
        Some(SEED_CACHE_HIT + CALL_CACHE_HIT),
        "the session's cache total continues the earlier turn's"
    );
    assert_eq!(
        state.turn_input_tokens,
        Some(CALL_INPUT),
        "the turn's own pair covers this turn's call alone"
    );
    assert_eq!(
        state.turn_cache_hit_tokens,
        Some(CALL_CACHE_HIT),
        "the turn's own cache pair covers this turn's call alone"
    );
    assert_eq!(
        state.turn_output_tokens,
        Some(CALL_OUTPUT),
        "the turn's own output covers this turn's call alone"
    );
    assert_eq!(
        state.context_input_tokens,
        Some(CALL_INPUT),
        "the context figure is the last call's own, not the session's sum"
    );
}
