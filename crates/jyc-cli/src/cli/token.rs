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
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that mutate the shared `HOME` / `LOCALAPPDATA`
    /// env vars (parallel-safe).
    static HOME_LOCK: Mutex<()> = Mutex::new(());

    /// Redirects the platform data dir to `tmp` for the duration of the
    /// test. Restores the previous env var on drop.
    fn with_tmp_data_home<F: FnOnce()>(tmp: &std::path::Path, f: F) {
        #[cfg(unix)]
        {
            // SAFETY: tests are serialized via `HOME_LOCK`.
            unsafe {
                let prev = std::env::var_os("HOME");
                std::env::set_var("HOME", tmp);
                let prev_xdg = std::env::var_os("XDG_DATA_HOME");
                std::env::set_var("XDG_DATA_HOME", tmp);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
                match prev {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
                match prev_xdg {
                    Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                    None => std::env::remove_var("XDG_DATA_HOME"),
                }
                if let Err(e) = result {
                    std::panic::resume_unwind(e);
                }
            }
        }
        #[cfg(not(unix))]
        {
            // SAFETY: tests are serialized via `HOME_LOCK`.
            unsafe {
                let prev = std::env::var_os("LOCALAPPDATA");
                std::env::set_var("LOCALAPPDATA", tmp);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
                match prev {
                    Some(v) => std::env::set_var("LOCALAPPDATA", v),
                    None => std::env::remove_var("LOCALAPPDATA"),
                }
                if let Err(e) = result {
                    std::panic::resume_unwind(e);
                }
            }
        }
    }

    fn args(action: TokenAction) -> TokenArgs {
        TokenArgs { action }
    }

    #[test]
    fn generate_creates_token_file() {
        let _guard = HOME_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        with_tmp_data_home(tmp.path(), || {
            run(&args(TokenAction::Generate)).expect("generate");
            assert!(jyc_utils::inspect_token::read().unwrap().is_some());
        });
    }

    #[test]
    fn show_errors_when_no_token() {
        let _guard = HOME_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        with_tmp_data_home(tmp.path(), || {
            let err = run(&args(TokenAction::Show)).unwrap_err().to_string();
            assert!(err.contains("no token file"), "unexpected error: {err}");
        });
    }

    #[test]
    fn rotate_replaces_existing_token() {
        let _guard = HOME_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        with_tmp_data_home(tmp.path(), || {
            run(&args(TokenAction::Generate)).unwrap();
            let first = jyc_utils::inspect_token::read().unwrap().unwrap();
            run(&args(TokenAction::Rotate)).unwrap();
            let second = jyc_utils::inspect_token::read().unwrap().unwrap();
            assert_ne!(first, second);
        });
    }
}
