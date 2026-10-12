use arc_swap::ArcSwap;
use std::path::PathBuf;
use std::sync::Arc;

use jyc_types::ChannelPattern;
use jyc_types::GithubConfig;

/// GitHub channel matcher — stateless pattern matching for GitHub events.
pub struct GithubMatcher;

pub struct GithubInboundAdapter {
    config: GithubConfig,
    channel_name: String,
    /// Directory for persistent state: <data dir>/channels/<channel>/.github/
    state_dir: PathBuf,
    /// Channels dir, anchor for workspace resolution: <data dir>/channels/
    channels_dir: PathBuf,
    /// Live application config for dynamic pattern reading.
    app_config: Option<Arc<ArcSwap<jyc_types::AppConfig>>>,
    /// Test-only override for patterns. When set, takes priority over app_config.
    test_patterns: Option<Vec<ChannelPattern>>,
}

mod matcher;
mod poll;
mod state;
mod tests;

#[cfg(test)]
pub(crate) use poll::*;
