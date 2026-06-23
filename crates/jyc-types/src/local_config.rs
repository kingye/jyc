use serde::{Deserialize, Serialize};

/// Configuration for a local TUI channel.
///
/// Currently empty — placeholder for future TUI-specific settings
/// (e.g., theme, history size, auto-scroll).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LocalConfig {}
