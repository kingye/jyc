
use jyc_core::topic_event::TopicEvent;

/// Drain a receiver synchronously to a Vec, with a small grace timeout
/// so any in-flight publishes complete.
pub(super) async fn drain_events(
    rx: &mut tokio::sync::mpsc::Receiver<TopicEvent>,
) -> Vec<TopicEvent> {
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await {
            Ok(Some(e)) => out.push(e),
            Ok(None) => break, // sender closed
            Err(_) => break,   // timeout — no more events
        }
    }
    out
}
