use anyhow::{Context, Result};
use clap::{Args, Subcommand};

/// Manage the inspect-server authentication token.
///
/// The token lives in `<data_dir>/inspect-token` (e.g.
/// `~/.local/share/jyc/inspect-token` on Linux). The inspect server reads
/// it fresh on every non-loopback connection — no in-memory caching, so
/// rotation takes effect immediately for new connections.
#[derive(Debug, Args)]
#[command(about = "Manage the inspect-server authentication token")]
pub struct TokenArgs {
    #[command(subcommand)]
    pub action: TokenAction,
}

#[derive(Debug, Subcommand)]
pub enum TokenAction {
    /// Generate a new token, write it to <data_dir>/inspect-token, and print it.
    ///
    /// Overwrites any existing token file. Already-connected clients keep
    /// their existing authenticated sessions; new connections must use the
    /// new token.
    Generate,

    /// Print the current token from <data_dir>/inspect-token.
    ///
    /// Errors out if no token file exists (run `jyc token generate` first).
    Show,

    /// Delete the existing token file and generate a new one.
    ///
    /// Equivalent to `generate` after a fresh delete — included as a
    /// convenience for scripted rotation flows.
    Rotate,
}

pub fn run(args: &TokenArgs) -> Result<()> {
    match &args.action {
        TokenAction::Generate => {
            let token = jyc_utils::inspect_token::generate().context("failed to generate token")?;
            print_token(&token);
            Ok(())
        }
        TokenAction::Show => {
            match jyc_utils::inspect_token::read().context("failed to read token")? {
                Some(token) => {
                    println!("{token}");
                    Ok(())
                }
                None => {
                    let path = jyc_utils::inspect_token::token_path().map_or_else(
                        || "<unresolved data dir>".to_string(),
                        |p| p.display().to_string(),
                    );
                    anyhow::bail!(
                        "no token file at {path}; run `jyc token generate` to create one"
                    );
                }
            }
        }
        TokenAction::Rotate => {
            let token = jyc_utils::inspect_token::rotate().context("failed to rotate token")?;
            print_token(&token);
            Ok(())
        }
    }
}

/// Prints the freshly-generated token to stdout and the file location to
/// stderr, so the token can be piped into another command while the
/// reminder still surfaces in the terminal.
fn print_token(token: &str) {
    let path = jyc_utils::inspect_token::token_path().map_or_else(
        || "<unresolved data dir>".to_string(),
        |p| p.display().to_string(),
    );
    println!("{token}");
    eprintln!("written to {path} (mode 0600)");
    eprintln!(
        "existing authenticated connections are unaffected; restart `jyc serve` to invalidate them"
    );
}

#[cfg(test)]
mod tests {
    use jyc_utils::inspect_token as it_mod;
    use std::path::PathBuf;

    /// Path to the inspect-token file under a test base directory.
    /// Tests use the parameterized APIs (no env var mutation) so they
    /// are parallel-safe across test binaries.
    fn token_path_under(base: &std::path::Path) -> PathBuf {
        it_mod::token_path_in(base)
    }

    #[test]
    fn generate_creates_token_file() {
        // The no-arg `jyc token generate` writes to the env-var data
        // home; we instead exercise the underlying primitive with a
        // hermetic tempdir. Integration tests cover the subcommand
        // against the real data home.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = token_path_under(tmp.path());
        let token = it_mod::generate_at(&path).unwrap();
        assert!(it_mod::read_at(&path).unwrap().is_some());
        // The returned token matches what was persisted.
        let read_back = it_mod::read_at(&path).unwrap().unwrap();
        assert_eq!(read_back, token);
    }

    #[test]
    fn show_errors_when_no_token() {
        // The no-arg `jyc token show` reads the env-var data home. We
        // instead verify the underlying primitive: a fresh tempdir has
        // no token, so the read returns None and the no-arg `show` would
        // error out (covered by integration testing).
        let tmp = tempfile::TempDir::new().unwrap();
        let path = token_path_under(tmp.path());
        assert!(it_mod::read_at(&path).unwrap().is_none());
    }

    #[test]
    fn rotate_replaces_existing_token() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = token_path_under(tmp.path());
        let first = it_mod::generate_at(&path).unwrap();
        std::fs::remove_file(&path).ok();
        let second = it_mod::generate_at(&path).unwrap();
        assert_ne!(first, second);
    }
}
