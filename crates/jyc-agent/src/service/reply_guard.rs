//! The reply guard: a `require_reply` request that nobody answers must not
//! leave the requester waiting forever.
//!
//! `require_reply` is a request, not a promise: it tells the target that its
//! answer is wanted, and the target is the one that has to send it. Nothing
//! checked that it did — a target that ended its turn without replying (a
//! small model that forgot, a turn that died on a provider error) left the
//! requester with silence, and silence is indistinguishable from "still
//! working".
//!
//! So the target checks at the end of its turn: when a message it handled
//! asked for a reply, and nothing this turn went to the asking topic, the
//! target's own last words are sent there in its place. The requester then
//! hears *something* — the answer, or the fact that the answer never came —
//! instead of nothing at all.
//!
//! This module owns the reading of the metadata that says where a message
//! came from and whether it wants an answer, so the prompt that renders the
//! header and the guard that forwards the answer cannot drift apart.

use std::collections::HashMap;

use serde_json::Value;

/// The topic a message came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplySource {
    /// The channel the message came from.
    pub channel: String,
    /// The topic the message came from — its name is its address.
    pub topic: String,
}

/// Where a message came from, if it says.
///
/// A message that names no source came from wherever it arrived, and there
/// is nobody to answer to.
pub fn source_of(metadata: &HashMap<String, Value>) -> Option<ReplySource> {
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

/// Whether the message asks for an answer.
pub fn asks_for_reply(metadata: &HashMap<String, Value>) -> bool {
    metadata
        .get("require_reply")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The reply a message asked for, if it asked for one.
///
/// `None` when the message asked for no reply, or when it does not say where
/// a reply would go: a request that cannot be answered where it was made
/// has nowhere to be forwarded to.
pub fn reply_source(metadata: &HashMap<String, Value>) -> Option<ReplySource> {
    if !asks_for_reply(metadata) {
        return None;
    }
    source_of(metadata)
}

/// A reply request the turn owes an answer to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyRequest {
    /// Where the answer goes.
    pub source: ReplySource,
    /// How many outbound sends the turn had made when it folded this
    /// request into the conversation.
    ///
    /// A send made before this mark cannot be the answer to this request:
    /// the model had not been shown the request yet, so it could not have
    /// answered it. The turn's own request — the one it started with —
    /// carries the mark of zero, since nothing had been sent yet.
    pub sent_before: usize,
}

/// The reply requests a turn has to answer: the one the turn started with,
/// plus the ones that arrived while it ran.
pub fn reply_requests(
    metadata: &HashMap<String, Value>,
    arrived: &[ReplyRequest],
) -> Vec<ReplyRequest> {
    let mut requests: Vec<ReplyRequest> = reply_source(metadata)
        .into_iter()
        .map(|source| ReplyRequest {
            source,
            sent_before: 0,
        })
        .collect();
    requests.extend(arrived.iter().cloned());
    requests
}

/// Whether this turn already answered `request`.
///
/// Only the sends that came after the request was folded in count: the
/// model cannot have answered a request it had not been shown yet, so a
/// send made before that is no answer to it.
///
/// The channel is part of the pair because the pair is what the send
/// recorded: the guard compares the send's own record with the request's
/// own record, like with like. A topic name is unique process wide, so the
/// pair names one topic either way.
pub fn answered(request: &ReplyRequest, outbound_targets: &[(String, String)]) -> bool {
    let from = request.sent_before.min(outbound_targets.len());
    outbound_targets[from..].iter().any(|(channel, topic)| {
        channel == &request.source.channel && topic == &request.source.topic
    })
}

/// The requests this turn left unanswered.
///
/// A request the turn already answered is not in here, and a request that
/// arrived twice is in here once: the requester gets one answer, not two.
pub fn unanswered_requests(
    requests: &[ReplyRequest],
    outbound_targets: &[(String, String)],
) -> Vec<ReplySource> {
    let mut unanswered: Vec<ReplySource> = Vec::new();
    for request in requests {
        if answered(request, outbound_targets) {
            continue;
        }
        if unanswered.contains(&request.source) {
            continue;
        }
        unanswered.push(request.source.clone());
    }
    unanswered
}

/// How a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEnding<'a> {
    /// The turn finished; the text is its last message, empty when it had
    /// none.
    Finished(&'a str),
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
    match ending {
        TurnEnding::Finished(text) if !text.trim().is_empty() => {
            out.push_str("\n\nIts last message follows.\n\n");
            out.push_str(text);
        }
        TurnEnding::Finished(_) => out.push_str("\n\nIt left no message."),
        TurnEnding::Failed(failure) => out.push_str(&format!("\n\nThe turn failed: {failure}")),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(require_reply: bool, channel: &str, topic: &str) -> HashMap<String, Value> {
        let mut map = HashMap::new();
        map.insert("require_reply".to_string(), Value::Bool(require_reply));
        map.insert("source_channel".to_string(), Value::String(channel.into()));
        map.insert("source_topic".to_string(), Value::String(topic.into()));
        map
    }

    #[test]
    fn a_request_names_where_its_answer_belongs() {
        let source = reply_source(&metadata(true, "agents", "jyc")).unwrap();
        assert_eq!(source.channel, "agents");
        assert_eq!(source.topic, "jyc");
    }

    #[test]
    fn a_message_that_asks_for_nothing_has_no_request() {
        assert!(reply_source(&metadata(false, "agents", "jyc")).is_none());
        // It still knows where it came from — the prompt renders that header
        // whether or not an answer was asked for.
        assert_eq!(
            source_of(&metadata(false, "agents", "jyc")).unwrap().topic,
            "jyc"
        );
    }

    #[test]
    fn a_request_without_an_address_has_nowhere_to_be_answered() {
        let mut map = metadata(true, "agents", "jyc");
        map.remove("source_topic");
        assert!(reply_source(&map).is_none());
    }

    /// A request for the answer to go to topic "jyc" on the agents channel.
    fn a_request(sent_before: usize) -> ReplyRequest {
        ReplyRequest {
            source: reply_source(&metadata(true, "agents", "jyc")).unwrap(),
            sent_before,
        }
    }

    /// The turn's own request, plus the ones that arrived while it ran.
    #[test]
    fn the_requests_are_the_one_the_turn_started_with_and_the_ones_that_arrived() {
        let own = reply_source(&metadata(true, "agents", "jyc")).unwrap();
        let arrived = ReplyRequest {
            source: reply_source(&metadata(true, "email", "boss")).unwrap(),
            sent_before: 2,
        };

        let requests = reply_requests(
            &metadata(true, "agents", "jyc"),
            std::slice::from_ref(&arrived),
        );
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].source, own);
        assert_eq!(
            requests[0].sent_before, 0,
            "nothing was sent before the turn started"
        );
        assert_eq!(
            requests[1], arrived,
            "the request that arrived keeps the mark it came with"
        );

        // A turn that was not asked anything owes nothing.
        let requests = reply_requests(&metadata(false, "agents", "jyc"), &[]);
        assert!(requests.is_empty());
    }

    #[test]
    fn a_reply_to_the_asking_topic_answers_it() {
        assert!(answered(
            &a_request(0),
            &[("agents".to_string(), "jyc".to_string())]
        ));
    }

    #[test]
    fn a_reply_elsewhere_leaves_the_request_unanswered() {
        assert!(!answered(
            &a_request(0),
            &[("agents".to_string(), "other".to_string())]
        ));
        assert!(!answered(
            &a_request(0),
            &[("email".to_string(), "jyc".to_string())]
        ));
        assert!(!answered(&a_request(0), &[]));
    }

    /// A send made before the request arrived cannot be its answer: it went
    /// to that topic before anybody asked.
    #[test]
    fn a_reply_sent_before_the_request_arrived_is_not_its_answer() {
        let request = a_request(1);
        let sent_earlier = vec![("agents".to_string(), "jyc".to_string())];
        assert!(
            !answered(&request, &sent_earlier),
            "the send predates the request"
        );

        let request = a_request(1);
        let sent_after = vec![
            ("agents".to_string(), "other".to_string()),
            ("agents".to_string(), "jyc".to_string()),
        ];
        assert!(answered(&request, &sent_after), "the send came after it");
    }

    #[test]
    fn only_the_unanswered_requests_are_forwarded_to() {
        let request = a_request(0);
        let unanswered = unanswered_requests(
            &[request.clone(), request.clone()],
            &[("agents".to_string(), "jyc".to_string())],
        );
        assert!(unanswered.is_empty());

        let request = a_request(0);
        let unanswered = unanswered_requests(&[request.clone(), request.clone()], &[]);
        assert_eq!(unanswered.len(), 1, "the same request twice is one request");
    }

    #[test]
    fn the_forward_carries_the_turns_last_words() {
        let source = reply_source(&metadata(true, "agents", "jyc")).unwrap();
        let text = forward_message(
            &source,
            "ci-watch-pr977",
            TurnEnding::Finished("CI is green."),
        );
        assert!(text.contains("ci-watch-pr977"));
        assert!(text.contains("CI is green."));
        assert!(!text.contains("left no message"));
    }

    #[test]
    fn the_forward_says_so_when_there_were_no_last_words() {
        let source = reply_source(&metadata(true, "agents", "jyc")).unwrap();
        let text = forward_message(&source, "ci-watch-pr977", TurnEnding::Finished("   "));
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
        assert!(!text.contains("left no message"));
    }
}
