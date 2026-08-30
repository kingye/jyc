use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use std::path::Path;

/// Arguments for the `jyc token` command.
#[derive(Debug, Args)]
pub struct TokenArgs {
    #[command(subcommand)]
    pub action: TokenAction,
}

/// Actions supported by the `jyc token` command.
#[derive(Debug, Subcommand)]
pub enum TokenAction {
    /// Print the dashboard authorization token.
    Show,
    /// Generate a new random token and write it to the workdir, replacing
    /// the existing one. Run this when a server restart should also rotate
    /// the token (e.g. after a suspected leak), then restart `jyc serve`.
    Reset,
}

/// Run a token management command.
pub fn run(args: &TokenArgs, workdir: &Path) -> Result<()> {
    match args.action {
        TokenAction::Show => {
            let path = jyc_utils::auth_token::token_path(workdir);
            let token = jyc_utils::auth_token::read_token(&path).with_context(|| {
                format!(
                    "run `jyc serve` with workdir {} to generate the token",
                    workdir.display()
                )
            })?;
            println!("{token}");
            Ok(())
        }
        TokenAction::Reset => {
            let path = jyc_utils::auth_token::token_path(workdir);
            let token = jyc_utils::auth_token::generate_token();
            jyc_utils::auth_token::write_token(&path, &token).with_context(|| {
                format!("failed to write authorization token to {}", path.display())
            })?;
            println!("{token}");
            eprintln!(
                "Token written to {}. Restart `jyc serve` (workdir {}) for it to take effect; \
                 existing dashboards must reconnect with `jyc dashboard`.",
                path.display(),
                workdir.display()
            );
            Ok(())
        }
    }
}
