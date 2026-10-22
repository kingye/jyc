//! The reply guard: a `require_reply` request that nobody answers must not
//! leave the requester waiting forever.
//!
//! `require_reply` is a request, not a promise. It tells the target that its
//! answer is wanted, and the target is the one that has to send it — but
//! nothing checked that it did. A target that ended its turn without
//! replying (a small model that forgot the tool call, a turn that died on a
//! provider error) left the requester with silence, and silence is
//! indistinguishable from "still working".
//!
//! So the target checks at the end of its turn: when the message it just
//! handled asked for a reply, and nothing this turn went to the asking
//! topic, the target's own last words are sent there in its place. The
//! requester then hears *something* — the answer, or the fact that there
//! was none — instead of nothing at all.
//!
//! An explicit reply wins: a turn that sent to the asking topic has
//! answered it, and nothing is forwarded on top of that.

use std::collections::HashMap;

use serde_json::Value;

/// The topic a `require_reply` request came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplySource {
    /// The channel the request came from.
    pub channel: String,
    /// The topic the request came from — where a reply belongs.
    pub topic: String,
}

/// The reply a message asked for, if it asked for one.
///
/// `None` when the message asked for no reply, or when it does not say where
/// a reply would go: a request that cannot be answered where it was made has
/// nowhere to be forwarded to.
pub fn reply_source(metadata: &HashMap<String, Value>) -> Option<ReplySource> {
    if !metadata
        .get("require_reply")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let channel = metadata.get("source_channel")?.as_str()?.trim();
    let topic = metadata.get("source_topic")?.as_str()?.trim();
    if channel.is_empty() || topic.is_empty() {
        return None;
    }
    Some(ReplySource {
        channel: channel.to_string(),
        topic: topic.to_string(),
    })
}

/// Whether this turn already answered `source`.
///
/// The turn's sends are `(channel, topic)` pairs; a pair that names the
/// asking topic in the asking channel is the answer.
pub fn answered(source: &ReplySource, outbound_targets: &[(String, String)]) -> bool {
    outbound_targets
        .iter()
        .any(|(channel, topic)| channel == &source.channel && topic == &source.topic)
}

/// How a turn ended.
///
/// The two shapes are mutually exclusive — a turn either finished or died —
/// so they are one value, not two options that could both be set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEnding<'a> {
    /// The turn finished; the text is its last message, when it had one.
    Finished(Option<&'a str>),
    /// The turn died; the text says why.
    Failed(&'a str),
}

/// The message to send to the requester when the turn did not answer it.
pub fn forward_message(source: &ReplySource, topic_name: &str, ending: TurnEnding<'_>) -> String {
    let mut out = format!(
        "⚠️ Automatic forward — topic '{topic_name}' was asked to reply to topic '{}' \
         and ended its turn without doing so.",
        source.topic
    );
    if let TurnEnding::Failed(failure) = ending {
        out.push_str(&format!("\n\nThe turn failed: {failure}"));
    }
    match ending {
        TurnEnding::Finished(Some(text)) if !text.trim().is_empty() => {
            out.push_str("\n\nIts last message follows.\n\n");
            out.push_str(text);
        }
        _ => out.push_str("\n\nIt left no message."),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn metadata(require_reply: bool, channel: &str, topic: &str) -> HashMap<String, Value> {
        HashMap::from([
            ("require_reply".to_string(), json!(require_reply)),
            ("source_channel".to_string(), json!(channel)),
            ("source_topic".to_string(), json!(topic)),
        ])
    }

    #[test]
    fn a_request_names_its_source() {
        let source = reply_source(&metadata(true, "agents", "jyc")).unwrap();
        assert_eq!(source.channel, "agents");
        assert_eq!(source.topic, "jyc");
    }

    #[test]
    fn a_message_that_asked_for_nothing_has_no_source() {
        assert!(reply_source(&metadata(false, "agents", "jyc")).is_none());
    }

    #[test]
    fn a_request_without_a_destination_has_no_source() {
        let mut meta = metadata(true, "agents", "jyc");
        meta.remove("source_topic");
        assert!(reply_source(&meta).is_none());

        let mut meta = metadata(true, "agents", "  ");
        meta.insert("source_topic".to_string(), json!("  "));
        assert!(reply_source(&meta).is_none());
    }

    #[test]
    fn a_send_to_the_asking_topic_answers_the_request() {
        let source = reply_source(&metadata(true, "agents", "jyc")).unwrap();
        assert!(answered(
            &source,
            &[("agents".to_string(), "jyc".to_string())]
        ));
    }

    #[test]
    fn a_send_elsewhere_does_not_answer_the_request() {
        let source = reply_source(&metadata(true, "agents", "jyc")).unwrap();
        // Another topic of the same channel.
        assert!(!answered(
            &source,
            &[("agents".to_string(), "other".to_string())]
        ));
        // The same topic name in another channel.
        assert!(!answered(
            &source,
            &[("email".to_string(), "jyc".to_string())]
        ));
        assert!(!answered(&source, &[]));
    }

    #[test]
    fn the_forward_carries_the_turns_last_words() {
        let source = reply_source(&metadata(true, "agents", "jyc")).unwrap();
        let text = forward_message(
            &source,
            "ci-watch-pr977",
            TurnEnding::Finished(Some("CI is green.")),
        );
        assert!(text.contains("ci-watch-pr977"));
        assert!(text.contains("CI is green."));
        assert!(!text.contains("left no message"));
    }

    #[test]
    fn the_forward_says_so_when_there_were_no_last_words() {
        let source = reply_source(&metadata(true, "agents", "jyc")).unwrap();
        let text = forward_message(&source, "ci-watch-pr977", TurnEnding::Finished(Some("   ")));
        assert!(text.contains("left no message"));
    }

    #[test]
    fn the_forward_names_the_failure() {
        let source = reply_source(&metadata(true, "agents", "jyc")).unwrap();
        let text = forward_message(
            &source,
            "ci-watch-pr977",
            TurnEnding::Failed("provider timeout"),
        );
        assert!(text.contains("provider timeout"));
    }
}
