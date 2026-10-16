//! Finding a topic in a list of topics — one rule, one place.
//!
//! A topic is addressed by its **name**, which is unique process-wide. The
//! channel narrows the match when the caller knows it (the wire address is
//! `(channel, topic)`, and a caller that opened a chat without one — the
//! ad-hoc CLI launch passes `None` — still finds its topic by name alone).
//!
//! The `topic_path` is NOT an identity: a subagent that shares its parent's
//! directory (`share_topic_path`) carries the same path as its parent, and so
//! does every other child of that parent. A lookup by path can therefore only
//! return whichever of them the scan happened to hit first — which is how
//! `/info` came to report a child topic instead of the one that asked.
//!
//! Both shapes of topic data (`TopicInfo` from a full resolve, `TopicSummary`
//! from the polled overview) implement [`TopicRef`], so every lookup in the
//! workspace goes through the same functions below.

use crate::inspect::{TopicInfo, TopicSummary};

/// What identifies a topic in a list: its name, and the channel it is on.
pub trait TopicRef {
    /// Channel the topic belongs to.
    fn topic_channel(&self) -> &str;
    /// Topic name — unique process-wide.
    fn topic_name(&self) -> &str;
}

impl TopicRef for TopicInfo {
    fn topic_channel(&self) -> &str {
        &self.channel
    }
    fn topic_name(&self) -> &str {
        &self.name
    }
}

impl TopicRef for TopicSummary {
    fn topic_channel(&self) -> &str {
        &self.channel
    }
    fn topic_name(&self) -> &str {
        &self.name
    }
}

/// How one entry matches: the name must be equal, and the channel too when
/// the caller knows which channel it is asking about.
fn matches<T: TopicRef>(entry: &T, name: &str, channel: Option<&str>) -> bool {
    entry.topic_name() == name && channel.is_none_or(|c| entry.topic_channel() == c)
}

/// Position of the topic `name` addresses in `topics`, if this list holds it.
pub fn topic_index<T: TopicRef>(topics: &[T], name: &str, channel: Option<&str>) -> Option<usize> {
    topics.iter().position(|t| matches(t, name, channel))
}

/// The topic `name` addresses in `topics`, if this list holds it.
pub fn find_topic<'a, T: TopicRef>(
    topics: &'a [T],
    name: &str,
    channel: Option<&str>,
) -> Option<&'a T> {
    topic_index(topics, name, channel).map(|i| &topics[i])
}

/// Whether `topics` holds the topic `name` addresses.
pub fn has_topic<T: TopicRef>(topics: &[T], name: &str, channel: Option<&str>) -> bool {
    topic_index(topics, name, channel).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Just the identity fields — enough to exercise the lookups, and the
    /// reason they are generic over [`TopicRef`].
    struct Row {
        channel: &'static str,
        name: &'static str,
        topic_path: &'static str,
    }

    impl TopicRef for Row {
        fn topic_channel(&self) -> &str {
            self.channel
        }
        fn topic_name(&self) -> &str {
            self.name
        }
    }

    /// The shape `share_topic_path` produces: a parent and two of its
    /// children, all working in the parent's directory.
    fn shared_dir_rows() -> Vec<Row> {
        vec![
            Row {
                channel: "agents",
                name: "ci-watch-925",
                topic_path: "/home/jiny/projects/jyc",
            },
            Row {
                channel: "agents",
                name: "jyc",
                topic_path: "/home/jiny/projects/jyc",
            },
            Row {
                channel: "agents",
                name: "review-prompt-names",
                topic_path: "/home/jiny/projects/jyc",
            },
        ]
    }

    /// Three topics share one path; each name must still find its own topic.
    /// Picking by path is what returned the child `ci-watch-925` for a
    /// question asked in `jyc`.
    #[test]
    fn same_topic_path_still_resolves_each_name() {
        let rows = shared_dir_rows();
        // The premise of the regression: one path, three topics.
        assert!(rows.windows(2).all(|w| w[0].topic_path == w[1].topic_path));
        assert_eq!(find_topic(&rows, "jyc", None).map(|t| t.name), Some("jyc"));
        assert_eq!(
            find_topic(&rows, "ci-watch-925", None).map(|t| t.name),
            Some("ci-watch-925")
        );
        assert_eq!(topic_index(&rows, "review-prompt-names", None), Some(2));
    }

    /// A known channel is checked; an unknown one is not required — a chat
    /// opened by the ad-hoc CLI launch carries no channel and must still find
    /// its topic.
    #[test]
    fn a_known_channel_narrows_but_is_not_required() {
        let rows = shared_dir_rows();
        assert_eq!(
            find_topic(&rows, "jyc", Some("agents")).map(|t| t.name),
            Some("jyc")
        );
        assert!(find_topic(&rows, "jyc", Some("feishu")).is_none());
        assert!(has_topic(&rows, "jyc", None));
        assert!(!has_topic(&rows, "nobody", None));
    }
}
