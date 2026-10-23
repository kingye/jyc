//! Monotonic per-topic sequence numbers for chat history rows.
//!
//! One owner, allocated once per message: a chat row's id is assigned before
//! either consumer sees the row — at [`TopicManager::enqueue`] for an inbound
//! message, at delivery time for a reply — carried on the dashboard event,
//! and written verbatim into the chat log by [`ChatLogStore`]. Because both
//! the live row and the persisted row carry the same number, a client can
//! dedup on that number alone; no surface has to compare message text.
//!
//! Rows logged before this module existed have no id (they deserialize as
//! `0`) and are not part of the sequence.
//!
//! [`TopicManager::enqueue`]: crate::topic_manager::TopicManager::enqueue
//! [`ChatLogStore`]: crate::chat_log_store::ChatLogStore

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::{LazyLock, Mutex};

use jyc_types::InboundMessage;

/// Metadata key carrying an inbound message's chat sequence number.
///
/// Set by the enqueue funnel so the chat-log writer can store the same number
/// the dashboard event carried.
pub const METADATA_CHAT_SEQ: &str = "jyc_chat_seq";

/// Metadata key carrying the dashboard client's own id for a message it sent.
///
/// The client puts it on the wire; it comes back on the row the server
/// broadcasts, which is how the client recognizes its own optimistic echo.
pub const METADATA_CLIENT_MSG_ID: &str = "jyc_client_msg_id";

/// Last allocated number per topic name.
///
/// Keyed by name (process-wide unique) rather than by path: a topic's log
/// lives in its state dir, which may be re-created, and the sequence must not
/// go backwards when that happens.
static NEXT: LazyLock<Mutex<HashMap<String, u64>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Allocate the next sequence number for `topic_name`.
///
/// Seeds from the newest chat log file the first time a topic is seen, so a
/// restart continues the sequence instead of reusing numbers.
pub fn next(topic_name: &str) -> u64 {
    let mut map = NEXT.lock().unwrap_or_else(|e| e.into_inner());
    let counter = map
        .entry(topic_name.to_string())
        .or_insert_with(|| max_on_disk(topic_name));
    *counter += 1;
    *counter
}

/// The highest id already written to the topic's chat log, or `0` when none.
///
/// Reads the tail of the newest `chat_history_*.jsonl` file: allocation is
/// per topic and files roll over by date, so the newest file's last record
/// holds the highest number.
fn max_on_disk(topic_name: &str) -> u64 {
    // Only the state dir can hold numbered rows: the legacy topic-root
    // location is where pre-sequence rows live, and they carry no id.
    let Some(state_dir) = jyc_types::state_dir::registered_state(topic_name) else {
        return 0;
    };
    let Some(newest) = crate::chat_log_store::chat_history_files_in(&state_dir)
        .last()
        .cloned()
    else {
        return 0;
    };
    let Ok(len) = std::fs::metadata(&newest).map(|m| m.len()) else {
        return 0;
    };

    // The last record can be longer than the window (a long message); widen
    // until its opening line is inside the window or the whole file is.
    let mut window = 64 * 1024;
    loop {
        let start = len.saturating_sub(window);
        let Ok(mut file) = std::fs::File::open(&newest) else {
            return 0;
        };
        if file.seek(SeekFrom::Start(start)).is_err() {
            return 0;
        }
        let mut buf = String::new();
        if file.read_to_string(&mut buf).is_err() {
            return 0;
        }
        if let Some(record) = buf.split('\n').rev().find(|l| !l.trim().is_empty())
            && let Some(id) = serde_json::from_str::<serde_json::Value>(record)
                .ok()
                .and_then(|v| v.get("id").and_then(|i| i.as_u64()))
        {
            return id;
        }
        if start == 0 {
            return 0;
        }
        window = window.saturating_mul(2);
    }
}

/// The chat sequence number an inbound message was tagged with, if any.
pub fn of_message(message: &InboundMessage) -> Option<u64> {
    message
        .metadata
        .get(METADATA_CHAT_SEQ)
        .and_then(|v| v.as_u64())
}

/// The dashboard client's own id for an inbound message, if it sent one.
pub fn client_msg_id(message: &InboundMessage) -> Option<String> {
    message
        .metadata
        .get(METADATA_CLIENT_MSG_ID)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jyc_types::state_dir;
    use std::io::Write;
    use std::path::Path;

    fn write_log(dir: &Path, lines: &[&str]) {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("chat_history_2026-10-23.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    #[test]
    fn seeds_from_the_newest_log_then_increments() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "chat-seq-seed";
        let state_dir_path = tmp.path().join(".jyc");
        state_dir::register(topic, &state_dir_path);
        write_log(
            &state_dir_path,
            &[
                r#"{"id":7,"type":"received","content":"a"}"#,
                r#"{"id":8,"type":"reply","content":"b"}"#,
            ],
        );

        assert_eq!(next(topic), 9);
        assert_eq!(next(topic), 10);
    }

    #[test]
    fn starts_at_one_without_a_log() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "chat-seq-empty";
        state_dir::register(topic, &tmp.path().join(".jyc"));

        assert_eq!(next(topic), 1);
    }

    #[test]
    fn skips_records_without_an_id() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "chat-seq-legacy";
        let state_dir_path = tmp.path().join(".jyc");
        state_dir::register(topic, &state_dir_path);
        write_log(
            &state_dir_path,
            &[
                r#"{"ts":"t","type":"received","content":"old"}"#,
                r#"{"id":41,"type":"reply","content":"new"}"#,
            ],
        );

        assert_eq!(next(topic), 42);
    }

    #[test]
    fn reads_a_record_longer_than_the_first_window() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "chat-seq-long-record";
        let state_dir_path = tmp.path().join(".jyc");
        state_dir::register(topic, &state_dir_path);
        let long = "x".repeat(200 * 1024);
        write_log(
            &state_dir_path,
            &[&format!(
                r#"{{"id":12345,"type":"received","content":"{long}"}}"#
            )],
        );

        assert_eq!(next(topic), 12346);
    }

    #[test]
    fn reads_the_client_message_id_from_metadata() {
        let mut message = InboundMessage {
            id: "m1".to_string(),
            channel: "websocket".to_string(),
            channel_uid: "u1".to_string(),
            sender: "user".to_string(),
            sender_address: "user".to_string(),
            recipients: vec![],
            topic: "t".to_string(),
            content: jyc_types::MessageContent {
                text: Some("hi".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            references: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: HashMap::new(),
            matched_pattern: None,
        };
        message
            .metadata
            .insert(METADATA_CLIENT_MSG_ID.to_string(), "c-1".into());

        assert_eq!(client_msg_id(&message).as_deref(), Some("c-1"));
        assert_eq!(of_message(&message), None);
    }
}
