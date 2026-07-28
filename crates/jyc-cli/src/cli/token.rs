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
    }
}
