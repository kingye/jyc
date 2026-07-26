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
//!
//! Two flavours of API are provided:
//!
//! - The no-arg forms (`read`, `generate`, `rotate`, `token_path`,
//!   `ensure_data_dir`, `matches`) resolve the platform data directory via
//!   `jyc_utils::paths::data_home()` — they target the user's real
//!   `~/.local/share/jyc`.
//! - The `_at(base)` and `_in(base)` forms (`read_at`, `generate_at`,
//!   `rotate_at`, `token_path_in`, `ensure_data_dir_in`, `matches_at`,
//!   `data_home_in`) take an explicit base directory. Tests use these with a
//!   per-test `tempfile::TempDir` to avoid mutating `HOME` / `XDG_DATA_HOME`.
//!
//! The data dir on Linux/macOS is normally `$XDG_DATA_HOME/jyc`, but
//! `data_home_in(base)` treats `base` as the platform data root directly
//! (i.e. `base/jyc` on Linux/macOS, `base` on Windows) — matching what
//! `data_home()` returns when `XDG_DATA_HOME=<base>` and `HOME=<base>`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use subtle::ConstantTimeEq;

use crate::paths::{APP_DIR_NAME, data_home};

/// Number of random bytes in the token (256 bits).
const TOKEN_BYTES: usize = 32;

/// Prefix that identifies a jyc inspect-server token.
const TOKEN_PREFIX: &str = "jyc_";

// ── Public API — platform-resolved (no args) ─────────────────────────────

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
    read_at_token_path(&path)
}

/// Generates a fresh token, writes it to the token file (creating or
/// overwriting), and returns it.
///
/// The file is written atomically via `tempfile::NamedTempFile::persist`
/// and, on Unix, with mode `0600`. The parent directory is created if
/// missing.
pub fn generate() -> Result<String> {
    let dir = ensure_data_dir()?;
    let token = random_token()?;
    write_token_to(&dir, &token)?;
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

// ── Public API — explicit base directory (for tests) ─────────────────────

/// Returns `<base>/inspect-token` (Linux/macOS) or `<base>\inspect-token`
/// (Windows) — i.e. the token file path that `data_home_in(base)` would
/// resolve to.
pub fn token_path_in(base: &Path) -> PathBuf {
    data_home_in(base).join("inspect-token")
}

/// Same semantics as `data_home()` but rooted at `base` instead of the
/// real `HOME` / `XDG_DATA_HOME`.
pub fn data_home_in(base: &Path) -> PathBuf {
    #[cfg(not(windows))]
    {
        base.join(APP_DIR_NAME)
    }
    #[cfg(windows)]
    {
        base.to_path_buf()
    }
}

/// Same as `ensure_data_dir()` but creates `<base>/jyc` (Linux/macOS) or
/// `<base>` (Windows) instead of resolving through env vars.
pub fn ensure_data_dir_in(base: &Path) -> Result<PathBuf> {
    let dir = data_home_in(base);
    create_dir_secure(&dir)
        .with_context(|| format!("failed to create data directory {}", dir.display()))?;
    Ok(dir)
}

/// Reads the token from `<base>/inspect-token` (or `<base>\inspect-token` on
/// Windows).
pub fn read_at(base: &Path) -> Result<Option<String>> {
    let path = token_path_in(base);
    read_at_token_path(&path)
}

/// Generates a fresh token into `<base>/inspect-token`.
pub fn generate_at(base: &Path) -> Result<String> {
    let dir = ensure_data_dir_in(base)?;
    let token = random_token()?;
    write_token_to(&dir, &token)?;
    Ok(token)
}

/// Rotates the token under `<base>/inspect-token`.
pub fn rotate_at(base: &Path) -> Result<String> {
    let path = token_path_in(base);
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("failed to delete existing token file {}", path.display()))?;
    }
    generate_at(base)
}

/// Constant-time check against the token under `<base>/inspect-token`.
pub fn matches_at(base: &Path, provided: &str) -> bool {
    let Ok(Some(expected)) = read_at(base) else {
        return false;
    };
    constant_time_eq(provided, &expected)
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn read_at_token_path(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let token = parse_token(&content)
                .with_context(|| format!("malformed token in {}", path.display()))?;
            Ok(Some(token))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context(format!("failed to read {}", path.display()))),
    }
}

fn write_token_to(dir: &Path, token: &str) -> Result<()> {
    let path = dir.join("inspect-token");
    write_atomically(&path, token.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn constant_time_eq(provided: &str, expected: &str) -> bool {
    let a = provided.as_bytes();
    let b = expected.as_bytes();
    a.ct_eq(b).into()
}

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
    fn read_at_returns_none_when_file_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(read_at(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn generate_at_writes_file_and_read_round_trips() {
        let tmp = tempfile::TempDir::new().unwrap();
        let token = generate_at(tmp.path()).unwrap();
        let read_back = read_at(tmp.path()).unwrap().expect("file should exist");
        assert_eq!(read_back, token);

        let path = token_path_in(tmp.path());
        assert!(path.exists());
    }

    #[test]
    fn rotate_at_replaces_existing_token() {
        let tmp = tempfile::TempDir::new().unwrap();
        let first = generate_at(tmp.path()).unwrap();
        let second = rotate_at(tmp.path()).unwrap();
        assert_ne!(first, second);
        assert_eq!(
            read_at(tmp.path()).unwrap().as_deref(),
            Some(second.as_str())
        );
    }

    #[test]
    fn matches_at_returns_true_for_correct_token() {
        let tmp = tempfile::TempDir::new().unwrap();
        let token = generate_at(tmp.path()).unwrap();
        assert!(matches_at(tmp.path(), &token));
    }

    #[test]
    fn matches_at_returns_false_for_wrong_token() {
        let tmp = tempfile::TempDir::new().unwrap();
        let token = generate_at(tmp.path()).unwrap();
        let mut wrong = token.clone();
        let last = wrong.pop().unwrap();
        let replacement = if last == 'a' { 'b' } else { 'a' };
        wrong.push(replacement);
        assert_ne!(wrong, token);
        assert!(!matches_at(tmp.path(), &wrong));
    }

    #[test]
    fn matches_at_returns_false_when_file_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(!matches_at(tmp.path(), "anything"));
    }

    #[test]
    fn matches_at_returns_false_for_different_length() {
        let tmp = tempfile::TempDir::new().unwrap();
        let token = generate_at(tmp.path()).unwrap();
        let shorter = &token[..token.len() - 1];
        assert!(!matches_at(tmp.path(), shorter));
    }

    #[cfg(unix)]
    #[test]
    fn generated_file_has_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        generate_at(tmp.path()).unwrap();
        let path = token_path_in(tmp.path());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0o600, got {:o}", mode);
    }

    #[test]
    fn ensure_data_dir_in_creates_subdirectory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = ensure_data_dir_in(tmp.path()).unwrap();
        assert!(dir.exists());

        #[cfg(not(windows))]
        {
            // On Linux/macOS the data dir is a `<base>/jyc` subdirectory.
            assert!(dir.ends_with("jyc"));
            assert_eq!(dir.parent().unwrap(), tmp.path());
        }
    }
}
