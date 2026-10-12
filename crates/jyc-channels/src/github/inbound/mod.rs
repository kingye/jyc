use std::path::PathBuf;

use jyc_types::GithubConfig;

/// GitHub channel matcher — stateless pattern matching for GitHub events.
pub struct GithubMatcher;

pub struct GithubInboundAdapter {
    config: GithubConfig,
    channel_name: String,
    /// Directory for persistent state: <data dir>/channels/<channel>/.github/
    state_dir: PathBuf,
}

mod matcher;
mod poll;
mod state;
mod tests;

#[cfg(test)]
pub(crate) use poll::*;
