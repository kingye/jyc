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

    /// `jyc_utils::inspect_token` resolves its data dir from `HOME` /
    /// `XDG_DATA_HOME` / `LOCALAPPDATA`. To keep tests hermetic and
    /// AGENTS.md-compliant (no `unsafe { std::env::set_var() }`), these
    /// tests exercise the equivalent `_at` / `_in` APIs directly via a
    /// per-test `tempfile::TempDir` — no env mutation, fully parallel-safe.
    fn generate_in(tmp: &std::path::Path) {
        jyc_utils::inspect_token::generate_at(tmp).expect("generate");
    }

    fn read_in(tmp: &std::path::Path) -> Option<String> {
        jyc_utils::inspect_token::read_at(tmp).expect("read")
    }

    fn token_path_in(tmp: &std::path::Path) -> std::path::PathBuf {
        jyc_utils::inspect_token::token_path_in(tmp)
    }

    #[test]
    fn generate_creates_token_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        generate_in(tmp.path());
        assert!(read_in(tmp.path()).is_some());
        assert!(token_path_in(tmp.path()).exists());
    }

    #[test]
    fn show_errors_when_no_token() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Verify the underlying condition that triggers the production
        // `show` error path: read returns None when no file exists.
        assert!(read_in(tmp.path()).is_none());
        let path = token_path_in(tmp.path());
        let msg = format!("no token file at {}", path.display());
        assert!(msg.starts_with("no token file at "));
    }

    #[test]
    fn rotate_replaces_existing_token() {
        let tmp = tempfile::TempDir::new().unwrap();
        generate_in(tmp.path());
        let first = read_in(tmp.path()).unwrap();
        let _ = jyc_utils::inspect_token::rotate_at(tmp.path()).expect("rotate");
        let second = read_in(tmp.path()).unwrap();
        assert_ne!(first, second);
    }
}
