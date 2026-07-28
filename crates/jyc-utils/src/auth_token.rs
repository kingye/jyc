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
    getrandom::getrandom(&mut bytes).expect("getrandom failed");
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
}
