//! Every running [`TopicManager`], in one place.
//!
//! jyc keeps one manager per channel, each owning the queues of the topics its
//! channel serves. A topic is addressed by **name** — the identity the state
//! dir registry keys on — so anything that needs "the manager that serves topic
//! X" needs the whole set, not just the one it happens to run in. That set
//! lives here: the
//! [`ChannelOrchestrator`](crate::channel_orchestrator::ChannelOrchestrator)
//! owns it, each manager holds a `Weak` back-pointer into it (a strong one
//! would make an `Arc` cycle and leak the managers), and command handlers, the
//! tools and the inspect server all read the same instance — so "where does
//! this topic live" has one answer in the process.

use std::sync::Arc;

use arc_swap::ArcSwap;

use super::TopicManager;

/// The running topic managers, addressable by channel or by the topic they
/// serve.
pub struct TopicManagers {
    managers: ArcSwap<Vec<Arc<TopicManager>>>,
}

impl Default for TopicManagers {
    fn default() -> Self {
        Self::new()
    }
}

impl TopicManagers {
    pub fn new() -> Self {
        Self {
            managers: ArcSwap::from_pointee(Vec::new()),
        }
    }

    /// Publish the current set. The orchestrator calls this whenever its
    /// channel set changes; managers hold only a `Weak` pointer to it.
    pub fn set(&self, managers: Vec<Arc<TopicManager>>) {
        self.managers.store(Arc::new(managers));
    }

    /// Every running manager, as of the last [`set`](Self::set).
    pub fn all(&self) -> Arc<Vec<Arc<TopicManager>>> {
        self.managers.load_full()
    }

    /// The manager running `channel` (e.g. `"agents"`).
    pub fn by_channel(&self, channel: &str) -> Option<Arc<TopicManager>> {
        self.all()
            .iter()
            .find(|tm| tm.channel_name() == channel)
            .cloned()
    }

    /// The manager that already serves `topic`, found by asking whose queue map
    /// holds it. A topic exists because a channel routed to it (which enqueued)
    /// or because `/spawn` made an agent, so a live queue is what "this topic
    /// is mine" actually means. A topic that has been idle since a restart has
    /// no queue anywhere and resolves to `None`.
    pub async fn serving(&self, topic: &str) -> Option<Arc<TopicManager>> {
        let managers = self.all();
        for tm in managers.iter() {
            if tm.serves_topic(topic).await {
                return Some(tm.clone());
            }
        }
        None
    }
}
