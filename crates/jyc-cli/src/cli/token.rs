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
    /// Generate a new random token and write it to the data_dir, replacing
    /// the existing one. Run this when a server restart should also rotate
    /// the token (e.g. after a suspected leak), then restart `jyc serve`.
    Reset,
}

/// Run a token management command.
pub fn run(args: &TokenArgs, data_dir: &Path) -> Result<()> {
    match args.action {
        TokenAction::Show => {
            let path = jyc_utils::auth_token::token_path(data_dir);
            let token = jyc_utils::auth_token::read_token(&path).with_context(|| {
                format!(
                    "run `jyc serve` with data_dir {} to generate the token",
                    data_dir.display()
                )
            })?;
            println!("{token}");
            Ok(())
        }
        TokenAction::Reset => {
            let token = jyc_utils::auth_token::generate_and_write_token(data_dir)?;
            println!("{token}");
            eprintln!(
                "Token written to {}. Restart `jyc serve` (data_dir {}) for it to take effect; \
                 existing dashboards must reconnect with `jyc dashboard`.",
                jyc_utils::auth_token::token_path(data_dir).display(),
                data_dir.display()
            );
            Ok(())
        }
    }
}
