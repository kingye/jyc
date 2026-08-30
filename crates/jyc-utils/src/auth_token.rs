use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Filename used for the dashboard authorization token.
pub const AUTH_TOKEN_FILENAME: &str = "auth.token";

/// Return the authorization token path for a JYC workdir.
pub fn token_path(workdir: &Path) -> PathBuf {
    workdir.join(AUTH_TOKEN_FILENAME)
}

/// Generate a random 256-bit authorization token encoded as hexadecimal.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("getrandom failed");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Write an authorization token with owner-only permissions on Unix.
pub fn write_token(path: &Path, token: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.write_all(token.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Resolve the inspect auth token: reuse the existing token on disk if
/// present (so dashboard connections survive `jyc serve` restarts), else
/// generate and persist a fresh one. Generating a new token on every start
/// invalidates every running dashboard's cached token, surfacing as 401 on
/// reconnect.
pub fn resolve_or_generate_token(workdir: &Path) -> Result<String> {
    let path = token_path(workdir);
    match read_token(&path) {
        Ok(existing) => Ok(existing),
        Err(_) => generate_and_write_token(workdir),
    }
}

/// Generate a fresh random token, persist it to the workdir, and return it.
pub fn generate_and_write_token(workdir: &Path) -> Result<String> {
    let path = token_path(workdir);
    let token = generate_token();
    write_token(&path, &token).with_context(|| {
        format!(
            "failed to write authorization token to {}. \
             Dashboard will not be able to connect. Fix the path and rerun `jyc serve`.",
            path.display()
        )
    })?;
    Ok(token)
}

/// Read and trim an authorization token from disk.
pub fn read_token(path: &Path) -> Result<String> {
    let token = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read authorization token from {}", path.display()))?;
    let token = token.trim();
    if token.is_empty() {
        anyhow::bail!("authorization token file is empty: {}", path.display());
    }
    Ok(token.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_round_trip() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = token_path(temp.path());
        let token = generate_token();

        write_token(&path, &token).unwrap();

        assert_eq!(read_token(&path).unwrap(), token);
        assert_eq!(token.len(), 64);
    }

    #[test]
    fn token_is_unique() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b, "two generated tokens should differ");
    }

    #[test]
    fn resolve_or_generate_reuses_existing_token() {
        let temp = tempfile::TempDir::new().unwrap();
        let workdir = temp.path();
        // First call: no token file yet → generate + write.
        let t1 = resolve_or_generate_token(workdir).unwrap();
        // Second call (restart): token file present → reuse.
        let t2 = resolve_or_generate_token(workdir).unwrap();
        assert_eq!(t1, t2, "token must persist across restarts");
    }

    #[test]
    fn resolve_or_generate_regenerates_when_empty() {
        let temp = tempfile::TempDir::new().unwrap();
        let workdir = temp.path();
        let path = token_path(workdir);
        // Write an empty (invalid) token → read_token bails → regenerate.
        write_token(&path, "   ").unwrap();
        let t = resolve_or_generate_token(workdir).unwrap();
        assert!(!t.is_empty());
        assert_eq!(t.len(), 64);
    }
}
