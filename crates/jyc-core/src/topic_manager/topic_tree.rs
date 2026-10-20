//! The lineage tree of a channel's topics: who spawned whom, as a tree.
//!
//! The rules live here, once. The explorer's tree, the info pane's tree and
//! the `subagents` a topic's detail carries are all built by these
//! functions, so the three surfaces cannot drift apart.
//!
//! The lineage itself comes from the `spawned-from` breadcrumb each
//! subagent wrote into its own state dir — the same file the inspect server
//! reads to fill `TopicSummary::parent`, so a topic's own tree and the
//! overview's tree are the same data.

use std::collections::{HashMap, HashSet};

use jyc_types::{TopicNode, TopicRef, TopicStatus};

/// A topic that can be placed in a lineage tree: its identity, the topic
/// that spawned it, and its status.
pub trait TopicLineage: TopicRef {
    /// The topic that spawned this one — the name in its `spawned-from`
    /// breadcrumb — or `None` for a topic the user started.
    fn lineage_parent(&self) -> Option<&str>;
    /// The topic's live status.
    fn lineage_status(&self) -> TopicStatus;
}

impl TopicLineage for jyc_types::TopicSummary {
    fn lineage_parent(&self) -> Option<&str> {
        self.parent.as_deref()
    }

    fn lineage_status(&self) -> TopicStatus {
        self.status.clone()
    }
}

/// A topic of a channel, with the topic that spawned it — the lineage a
/// tree is built from.
#[derive(Debug, Clone)]
pub struct LineageEntry {
    /// Topic name
    pub name: String,
    /// Channel the topic belongs to
    pub channel: String,
    /// The topic that spawned this one, per its `spawned-from` breadcrumb
    pub parent: Option<String>,
    /// The topic's live status
    pub status: TopicStatus,
}

impl TopicRef for LineageEntry {
    fn topic_channel(&self) -> &str {
        &self.channel
    }

    fn topic_name(&self) -> &str {
        &self.name
    }
}

impl TopicLineage for LineageEntry {
    fn lineage_parent(&self) -> Option<&str> {
        self.parent.as_deref()
    }

    fn lineage_status(&self) -> TopicStatus {
        self.status.clone()
    }
}

/// Map from `(parent name, channel)` to the topics naming it as their
/// `parent`, in name order. The channel is part of the key because a spawn
/// creates its child in the parent's own channel, so a name alone would
/// confuse two same-named parents on different channels.
fn children_index<T: TopicLineage>(topics: &[T]) -> HashMap<(&str, &str), Vec<usize>> {
    let mut index: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
    for (i, t) in topics.iter().enumerate() {
        if let Some(parent) = t.lineage_parent() {
            index
                .entry((parent, t.topic_channel()))
                .or_default()
                .push(i);
        }
    }
    for kids in index.values_mut() {
        kids.sort_by(|a, b| topics[*a].topic_name().cmp(topics[*b].topic_name()));
    }
    index
}

/// Append the descendants of the topic `name` to `nodes`, depth-first, each
/// nested under its parent. `seen` stops a breadcrumb cycle — only
/// hand-edited state can produce one — from recursing forever.
fn push_subtree<T: TopicLineage>(
    nodes: &mut Vec<TopicNode>,
    topics: &[T],
    index: &HashMap<(&str, &str), Vec<usize>>,
    name: &str,
    channel: &str,
    seen: &mut HashSet<usize>,
) {
    // Drop the kids a cycle already showed *before* picking the last one: a
    // back-edge in the middle of a list would otherwise leave the sibling
    // before it nested as a non-last child with nothing under it.
    let kids: Vec<usize> = index
        .get(&(name, channel))
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .copied()
        .filter(|&kid| seen.insert(kid))
        .collect();
    for &kid in &kids {
        let topic = &topics[kid];
        let mut children = Vec::new();
        push_subtree(
            &mut children,
            topics,
            index,
            topic.topic_name(),
            topic.topic_channel(),
            seen,
        );
        nodes.push(TopicNode {
            name: topic.topic_name().to_string(),
            channel: topic.topic_channel().to_string(),
            status: topic.lineage_status(),
            children,
        });
    }
}

/// Every topic of the list as a forest: the roots, each with the topics it
/// spawned beneath it.
///
/// The roots keep the list's own order. A topic whose parent is not in the
/// list — the parent was closed while the child lived on — is a root too, so
/// no topic disappears from the pane.
pub fn topic_forest<T: TopicLineage>(topics: &[T]) -> Vec<TopicNode> {
    let index = children_index(topics);
    let listed: HashSet<(&str, &str)> = topics
        .iter()
        .map(|t| (t.topic_name(), t.topic_channel()))
        .collect();
    let mut roots = Vec::with_capacity(topics.len());
    let mut seen = HashSet::new();
    for (i, t) in topics.iter().enumerate() {
        // Nesting turns on the parent being *listed* — never on the index key,
        // which holds a topic the moment it names a parent, so asking about it
        // would read as "has a parent" and hide every orphan instead of
        // promoting it to a root.
        if t.lineage_parent()
            .is_some_and(|p| listed.contains(&(p, t.topic_channel())))
        {
            continue;
        }
        push_root(&mut roots, topics, &index, i, &mut seen);
    }
    // A breadcrumb cycle (hand-edited state only) has no root to start from,
    // so the walk above leaves its members unreached — and that is the only
    // case `seen` could not have caught by itself. Anything still unvisited
    // belongs in the pane too rather than blanking it.
    for i in 0..topics.len() {
        push_root(&mut roots, topics, &index, i, &mut seen);
    }
    roots
}

/// Push topic `i` as a root, then its subtree. A topic the walk has already
/// reached is left alone, so a second call is a no-op.
fn push_root<T: TopicLineage>(
    roots: &mut Vec<TopicNode>,
    topics: &[T],
    index: &HashMap<(&str, &str), Vec<usize>>,
    i: usize,
    seen: &mut HashSet<usize>,
) {
    if !seen.insert(i) {
        return;
    }
    let topic = &topics[i];
    let mut children = Vec::new();
    push_subtree(
        &mut children,
        topics,
        index,
        topic.topic_name(),
        topic.topic_channel(),
        seen,
    );
    roots.push(TopicNode {
        name: topic.topic_name().to_string(),
        channel: topic.topic_channel().to_string(),
        status: topic.lineage_status(),
        children,
    });
}

/// The topics the topic `root_name` spawned, as a tree: its descendants, the
/// root itself excluded. Empty when the topic has no subagents.
///
/// The root need not be in `topics` — its children are found by name — but
/// when it is, a breadcrumb cycle leading back to it stops there rather
/// than listing the root as its own descendant.
pub fn subagent_tree<T: TopicLineage>(
    topics: &[T],
    root_name: &str,
    root_channel: &str,
) -> Vec<TopicNode> {
    let index = children_index(topics);
    let mut nodes = Vec::new();
    let mut seen = HashSet::new();
    // The root draws no row of its own, but a breadcrumb cycle leading back to
    // it must stop here rather than list it as its own descendant.
    if let Some(i) = jyc_types::topic_index(topics, root_name, Some(root_channel)) {
        seen.insert(i);
    }
    push_subtree(
        &mut nodes,
        topics,
        &index,
        root_name,
        root_channel,
        &mut seen,
    );
    nodes
}

/// The number of topics in a tree, the roots included.
pub fn node_count(nodes: &[TopicNode]) -> usize {
    nodes.iter().map(|n| 1 + node_count(&n.children)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A topic of the list, with the topic that spawned it.
    fn topic(name: &str, parent: Option<&str>) -> LineageEntry {
        LineageEntry {
            name: name.to_string(),
            channel: "test".to_string(),
            parent: parent.map(str::to_string),
            status: TopicStatus::Idle,
        }
    }

    /// The names of a tree, depth-first, with each node's depth.
    fn shape(nodes: &[TopicNode], depth: usize) -> Vec<(usize, String)> {
        let mut out = Vec::new();
        for node in nodes {
            out.push((depth, node.name.clone()));
            out.extend(shape(&node.children, depth + 1));
        }
        out
    }

    // The explorer walks parent → subagent: a subagent sits under the topic
    // that spawned it, a deeper chain keeps nesting, and a topic whose parent
    // is gone (closed, or spawned by something no longer listed) stays a root
    // so nothing drops out of the pane.
    #[test]
    fn topic_forest_nests_subagents_under_their_parent() {
        let topics = vec![
            topic("a", None),
            topic("b", Some("a")),
            topic("c", Some("b")),
            topic("d", Some("gone")),
        ];

        let forest = topic_forest(&topics);
        assert_eq!(
            shape(&forest, 0),
            [
                (0, "a".to_string()),
                (1, "b".to_string()),
                (2, "c".to_string()),
                (0, "d".to_string()),
            ],
            "tree order"
        );
    }

    // A breadcrumb cycle — hand-edited state only — has no root for the walk
    // to start from, so its members are emitted as the leftovers of that
    // walk. They must not blank the pane, and `seen` is what stops the
    // recursion from running forever.
    #[test]
    fn topic_forest_keeps_a_breadcrumb_cycle_visible() {
        let topics = vec![topic("a", Some("b")), topic("b", Some("a"))];

        let forest = topic_forest(&topics);
        let names: Vec<&str> = forest.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["a", "b"], "both members stay listed, once each");
    }

    // The tree of one topic holds its descendants only (the pane already
    // names the topic itself) and lists them depth-first, in name order.
    #[test]
    fn subagent_tree_lists_descendants_only() {
        let topics = vec![
            topic("a", None),
            topic("b", Some("a")),
            topic("c", Some("b")),
            topic("e", Some("a")),
        ];

        let tree = subagent_tree(&topics, "a", "test");
        assert_eq!(
            shape(&tree, 0),
            [
                (0, "b".to_string()),
                (1, "c".to_string()),
                (0, "e".to_string()),
            ],
            "depth-first, name order, root excluded"
        );

        // A leaf gets no tree, so its pane omits the section entirely.
        assert!(subagent_tree(&topics, "c", "test").is_empty());
    }

    // The count is what the pane's header shows: every descendant, at every
    // depth — not just the direct children.
    #[test]
    fn node_count_covers_every_depth() {
        let topics = vec![
            topic("a", None),
            topic("b", Some("a")),
            topic("c", Some("b")),
            topic("e", Some("a")),
        ];

        assert_eq!(node_count(&subagent_tree(&topics, "a", "test")), 3);
        assert_eq!(node_count(&[]), 0);
    }
}
