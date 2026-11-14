//! /version command — the running build's version line.
//!
//! The version is a compile-time constant of this crate, which the server
//! binary is built from, so the line answers "which build am I talking to"
//! for any channel (email included) without a round-trip to the inspect
//! server. Live figures (uptime, worker stats) stay in the dashboard's
//! overview — they only make sense on a screen that polls them.

use anyhow::Result;
use async_trait::async_trait;

use super::handler::{CommandContext, CommandHandler, CommandResult};

pub struct VersionCommandHandler;

#[async_trait]
impl CommandHandler for VersionCommandHandler {
    async fn execute(&self, _context: CommandContext) -> Result<CommandResult> {
        Ok(CommandResult {
            success: true,
            message: format!("JYC AI v{}", env!("CARGO_PKG_VERSION")),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn version_line_carries_the_pkg_version() {
        let result = VersionCommandHandler
            .execute(CommandContext::default())
            .await
            .expect("execute");
        assert!(result.success);
        assert_eq!(
            result.message,
            format!("JYC AI v{}", env!("CARGO_PKG_VERSION"))
        );
    }
}
