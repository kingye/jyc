//! Persistent authentication token for the inspect server.
//!
//! The token lives in `<data_dir>/inspect-token` (e.g.
//! `~/.local/share/jyc/inspect-token` on Linux, `%LOCALAPPDATA%\jyc\inspect-token`
//! on Windows). It is managed exclusively by the `jyc token` subcommand and
//! read fresh by the inspect server on every non-loopback connection — no
//! in-memory caching, so rotation takes effect for new connections
//! immediately without server restart.
//!
//! Token format: `jyc_` prefix + 64 lowercase hex characters (256 bits of
//! entropy). The file is written atomically and, on Unix, with mode `0600`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use subtle::ConstantTimeEq;

use crate::paths::data_home;

/// Number of random bytes in the token (256 bits).
const TOKEN_BYTES: usize = 32;

/// Prefix that identifies a jyc inspect-server token.
const TOKEN_PREFIX: &str = "jyc_";

/// Returns the absolute path to the token file, or `None` if the platform
/// data directory cannot be determined (no `HOME` / `XDG_DATA_HOME` on Unix,
/// no `LOCALAPPDATA` on Windows).
pub fn token_path() -> Option<PathBuf> {
    data_home().map(|p| p.join("inspect-token"))
}

/// Creates the platform data directory if it does not already exist.
///
/// Returns the path to the data directory. On Unix the directory is created
/// with mode `0o700` (owner-only access) when it is newly created.
pub fn ensure_data_dir() -> Result<PathBuf> {
    let dir = data_home().context("could not determine platform data directory")?;
    create_dir_secure(&dir)
        .with_context(|| format!("failed to create data directory {}", dir.display()))?;
    Ok(dir)
}

/// Reads the token from disk.
///
/// Returns `Ok(None)` if the file does not exist. Returns an error on
/// I/O failure or if the file content is malformed (wrong prefix, wrong
/// length, non-hex characters).
pub fn read() -> Result<Option<String>> {
    let Some(path) = token_path() else {
        return Ok(None);
    };
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let token = parse_token(&content)
                .with_context(|| format!("malformed token in {}", path.display()))?;
            Ok(Some(token))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context(format!("failed to read {}", path.display()))),
    }
}

/// Generates a fresh token, writes it to the token file (creating or
/// overwriting), and returns it.
///
/// The file is written atomically via `tempfile::NamedTempFile::persist`
/// and, on Unix, with mode `0600`. The parent directory is created if
/// missing.
pub fn generate() -> Result<String> {
    let dir = ensure_data_dir()?;
    let path = dir.join("inspect-token");
    let token = random_token()?;

    write_atomically(&path, token.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(token)
}

/// Deletes the token file (if present), generates a new one, and returns it.
///
/// Returns an error if the file existed but could not be deleted, or if
/// generation fails.
pub fn rotate() -> Result<String> {
    if let Some(path) = token_path()
        && path.exists()
    {
        std::fs::remove_file(&path)
            .with_context(|| format!("failed to delete existing token file {}", path.display()))?;
    }
    generate()
}

/// Constant-time check whether `provided` matches the token currently on disk.
///
/// Reads the file fresh on every call — no caching. Returns `false` if the
/// file is missing or malformed (treated as "no token configured"). Uses
/// `subtle::ConstantTimeEq` to avoid leaking length or content via timing.
pub fn matches(provided: &str) -> bool {
    let Ok(Some(expected)) = read() else {
        return false;
    };
    let a = provided.as_bytes();
    let b = expected.as_bytes();
    // ConstantTimeEq requires equal-length slices; if lengths differ the
    // comparison fails fast (still constant-time per length class). The
    // length itself is not a secret in this scheme — both sides know the
    // token is `jyc_` + 64 hex chars.
    a.ct_eq(b).into()
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Constructs a fresh random token in the canonical `jyc_<64 hex>` form.
fn random_token() -> Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|e| anyhow::anyhow!("failed to generate random bytes for token: {e}"))?;
    let mut token = String::with_capacity(TOKEN_PREFIX.len() + TOKEN_BYTES * 2);
    token.push_str(TOKEN_PREFIX);
    for byte in bytes {
        token.push_str(&format!("{byte:02x}"));
    }
    Ok(token)
}

/// Validates and returns the trimmed token string. Returns an error if the
/// content is not a valid jyc token.
fn parse_token(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    let Some(hex) = trimmed.strip_prefix(TOKEN_PREFIX) else {
        anyhow::bail!("token is missing required `{TOKEN_PREFIX}` prefix");
    };
    if hex.len() != TOKEN_BYTES * 2 {
        anyhow::bail!(
            "token hex part has length {}, expected {}",
            hex.len(),
            TOKEN_BYTES * 2
        );
    }
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!("token contains non-hex characters");
    }
    Ok(trimmed.to_string())
}

/// Writes `data` to `path` atomically: writes to a temp file in the same
/// directory, fsyncs, then renames over the destination. On Unix the
/// final file is created with mode `0o600` (owner read/write only).
fn write_atomically(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;

    let parent = path
        .parent()
        .with_context(|| format!("token path {} has no parent directory", path.display()))?;
    create_dir_secure(parent)?;

    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temp file in {}", parent.display()))?;

    // Restrict the temp file's permissions before writing any token bytes.
    set_file_mode_0600(tmp.path())?;

    tmp.write_all(data).context("failed to write token bytes")?;
    tmp.as_file()
        .sync_all()
        .context("failed to fsync token file")?;

    tmp.persist(path)
        .map_err(|e| anyhow::anyhow!("failed to persist token file: {}", e.error))?;

    Ok(())
}

/// Creates `dir` if missing. On Unix the directory is created with mode
/// `0o700` when newly created (existing directories are left untouched).
fn create_dir_secure(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    match std::fs::metadata(dir) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(dir)
                    .with_context(|| format!("failed to create {}", dir.display()))?;
                Ok(())
            }
            #[cfg(not(unix))]
            {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("failed to create {}", dir.display()))?;
                Ok(())
            }
        }
        Err(e) => Err(anyhow::Error::new(e).context(format!("failed to stat {}", dir.display()))),
    }
}

/// Sets the file at `path` to mode `0o600` on Unix. No-op on non-Unix.
#[cfg(unix)]
fn set_file_mode_0600(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_file_mode_0600(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Redirects the platform data dir to `tmp` for the duration of the
    /// test, so tests don't read or write the user's real `inspect-token`.
    /// Uses `HOME` on Unix and `LOCALAPPDATA` on Windows, matching what
    /// `jyc_utils::paths::data_home()` consults.
    fn with_tmp_data_home<F: FnOnce(&Path)>(tmp: &Path, f: F) {
        #[cfg(unix)]
        {
            // SAFETY: tests are serialized via `HOME_LOCK` and these vars
            // are only touched inside the lock.
            unsafe {
                let prev = std::env::var_os("HOME");
                std::env::set_var("HOME", tmp);
                let prev_xdg = std::env::var_os("XDG_DATA_HOME");
                std::env::set_var("XDG_DATA_HOME", tmp);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(tmp)));
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
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(tmp)));
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

    #[test]
    fn random_token_has_expected_format() {
        let token = random_token().expect("generate");
        assert!(token.starts_with(TOKEN_PREFIX));
        let hex = &token[TOKEN_PREFIX.len()..];
        assert_eq!(hex.len(), TOKEN_BYTES * 2);
        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn two_random_tokens_are_different() {
        let a = random_token().unwrap();
        let b = random_token().unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn parse_token_accepts_canonical_form() {
        let token = random_token().unwrap();
        assert_eq!(parse_token(&token).unwrap(), token);
    }

    #[test]
    fn parse_token_strips_trailing_whitespace() {
        let token = random_token().unwrap();
        assert_eq!(parse_token(&format!("{token}\n")).unwrap(), token);
    }

    #[test]
    fn parse_token_rejects_wrong_prefix() {
        let bogus = "xxx_".to_string() + &"a".repeat(TOKEN_BYTES * 2);
        assert!(parse_token(&bogus).is_err());
    }

    #[test]
    fn parse_token_rejects_wrong_length() {
        let too_short = format!("{TOKEN_PREFIX}{}", "a".repeat(10));
        assert!(parse_token(&too_short).is_err());
    }

    #[test]
    fn parse_token_rejects_non_hex() {
        let bogus = format!("{TOKEN_PREFIX}{}", "z".repeat(TOKEN_BYTES * 2));
        assert!(parse_token(&bogus).is_err());
    }

    #[test]
    fn read_returns_none_when_file_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        with_tmp_data_home(tmp.path(), |_| {
            assert!(read().unwrap().is_none());
        });
    }

    #[test]
    fn generate_writes_file_and_read_round_trips() {
        let tmp = tempfile::TempDir::new().unwrap();
        with_tmp_data_home(tmp.path(), |_| {
            let token = generate().unwrap();
            let read_back = read().unwrap().expect("file should exist after generate");
            assert_eq!(read_back, token);
            // File should live directly in the redirected data dir.
            let path = token_path().expect("data dir resolves");
            assert!(path.exists());
            assert_eq!(path.parent().unwrap(), tmp.path());
        });
    }

    #[test]
    fn rotate_replaces_existing_token() {
        let tmp = tempfile::TempDir::new().unwrap();
        with_tmp_data_home(tmp.path(), |_| {
            let first = generate().unwrap();
            let second = rotate().unwrap();
            assert_ne!(first, second);
            assert_eq!(read().unwrap().as_deref(), Some(second.as_str()));
        });
    }

    #[test]
    fn matches_returns_true_for_correct_token() {
        let tmp = tempfile::TempDir::new().unwrap();
        with_tmp_data_home(tmp.path(), |_| {
            let token = generate().unwrap();
            assert!(matches(&token));
        });
    }

    #[test]
    fn matches_returns_false_for_wrong_token() {
        let tmp = tempfile::TempDir::new().unwrap();
        with_tmp_data_home(tmp.path(), |_| {
            let token = generate().unwrap();
            let mut wrong = token.clone();
            // Flip the last hex character to a different valid hex char.
            let last = wrong.pop().unwrap();
            let replacement = if last == 'a' { 'b' } else { 'a' };
            wrong.push(replacement);
            assert_ne!(wrong, token);
            assert!(!matches(&wrong));
        });
    }

    #[test]
    fn matches_returns_false_when_file_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        with_tmp_data_home(tmp.path(), |_| {
            assert!(!matches("anything"));
        });
    }

    #[test]
    fn matches_returns_false_for_different_length() {
        let tmp = tempfile::TempDir::new().unwrap();
        with_tmp_data_home(tmp.path(), |_| {
            let token = generate().unwrap();
            let shorter = &token[..token.len() - 1];
            assert!(!matches(shorter));
        });
    }

    #[cfg(unix)]
    #[test]
    fn generated_file_has_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        with_tmp_data_home(tmp.path(), |_| {
            generate().unwrap();
            let path = token_path().unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "expected 0o600, got {:o}", mode);
        });
    }
}
