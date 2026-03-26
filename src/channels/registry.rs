use std::collections::HashMap;
use std::sync::Arc;

use super::types::{InboundAdapter, OutboundAdapter};

/// Registry for looking up channel adapters by name.
///
/// Each channel name (e.g., "work", "personal") maps to its
/// inbound and outbound adapters.
pub struct ChannelRegistry {
    inbound: HashMap<String, Arc<dyn InboundAdapter>>,
    outbound: HashMap<String, Arc<dyn OutboundAdapter>>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self {
            inbound: HashMap::new(),
            outbound: HashMap::new(),
        }
    }

    pub fn register_inbound(&mut self, name: String, adapter: Arc<dyn InboundAdapter>) {
        self.inbound.insert(name, adapter);
    }

    pub fn register_outbound(&mut self, name: String, adapter: Arc<dyn OutboundAdapter>) {
        self.outbound.insert(name, adapter);
    }

    pub fn get_inbound(&self, name: &str) -> Option<&Arc<dyn InboundAdapter>> {
        self.inbound.get(name)
    }

    pub fn get_outbound(&self, name: &str) -> Option<&Arc<dyn OutboundAdapter>> {
        self.outbound.get(name)
    }

    pub fn inbound_names(&self) -> Vec<&String> {
        self.inbound.keys().collect()
    }

    pub fn outbound_names(&self) -> Vec<&String> {
        self.outbound.keys().collect()
    }
}

impl Default for ChannelRegistry {
    fn default() -> Self {
        Self::new()
    }
}
