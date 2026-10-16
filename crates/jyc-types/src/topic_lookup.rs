//! Finding a topic in a list of topics — one rule, one place.
//!
//! A topic's identity is `(channel, name)`. The name is unique
//! process-wide, so that pair addresses exactly one topic. The
//! `topic_path` is NOT an identity: a subagent that shares its parent's
//! directory (`share_topic_path`) carries the same path as its parent, and
//! so does every other child of that parent. A lookup by path can therefore
//! only return whichever of them the scan happened to hit first — which is
//! how `/info` came to report a child topic instead of the one that asked.
//!
//! Both shapes of topic data (`TopicInfo` from a full resolve, `TopicSummary`
//! from the polled overview) implement [`TopicRef`], so every lookup in the
//! workspace goes through the same functions below.

use crate::inspect::{TopicInfo, TopicSummary};

/// What identifies a topic in a list: its channel and its name.
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

/// Position of `(channel, name)` in `topics`, if this list holds it.
pub fn topic_index<T: TopicRef>(topics: &[T], channel: &str, name: &str) -> Option<usize> {
    topics
        .iter()
        .position(|t| t.topic_channel() == channel && t.topic_name() == name)
}

/// The topic `(channel, name)` names in `topics`, if this list holds it.
pub fn find_topic<'a, T: TopicRef>(topics: &'a [T], channel: &str, name: &str) -> Option<&'a T> {
    topic_index(topics, channel, name).map(|i| &topics[i])
}

/// Whether `topics` holds the topic `(channel, name)` names.
pub fn has_topic<T: TopicRef>(topics: &[T], channel: &str, name: &str) -> bool {
    topic_index(topics, channel, name).is_some()
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
        assert_eq!(
            find_topic(&rows, "agents", "jyc").map(|t| t.name),
            Some("jyc")
        );
        assert_eq!(
            find_topic(&rows, "agents", "ci-watch-925").map(|t| t.name),
            Some("ci-watch-925")
        );
        assert_eq!(topic_index(&rows, "agents", "review-prompt-names"), Some(2));
    }

    /// The channel is part of the identity.
    #[test]
    fn channel_is_part_of_the_identity() {
        let rows = shared_dir_rows();
        assert!(find_topic(&rows, "feishu", "jyc").is_none());
        assert!(has_topic(&rows, "agents", "jyc"));
        assert!(!has_topic(&rows, "agents", "nobody"));
    }
}
