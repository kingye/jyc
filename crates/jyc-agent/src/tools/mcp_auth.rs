//! Persistent OAuth credential storage for the MCP OAuth 2.1 flow (`oauth_dcr`).
//!
//! Backs `[mcps.oauth_dcr]` remote servers. The one-time interactive
//! authorization (`jyc mcp auth <name>`) exchanges the authorization code and
//! persists the resulting client_id + tokens here; the daemon later loads
//! them and refreshes in place — rmcp's `AuthorizationManager` saves the
//! refreshed token back through this same store, so authorization survives
//! restarts and the user never re-authorizes until the refresh token itself
//! dies.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, AuthorizationSession, CredentialStore,
    StoredCredentials,
};

/// Run the one-time interactive OAuth authorization for an `oauth_dcr` MCP
/// server (browser consent + headless paste-back), persisting the resulting
/// client id and tokens into the file store the daemon reads.
///
/// UX: print the authorization URL → the user approves in any browser → the
/// browser lands on a localhost redirect that fails to load (nothing listens;
/// the URI only has to match the DCR registration) → the user pastes the
/// address-bar URL back → rmcp extracts code + CSRF state, verifies PKCE,
/// exchanges and saves the token pair.
///
/// Blocking stdin by design: this runs from the interactive `jyc mcp auth`
/// command, never inside the daemon.
pub async fn authorize_interactively(
    mcp_name: &str,
    server_url: &str,
    dcr: &jyc_types::OAuthDcrConfig,
) -> Result<()> {
    let redirect_uri = dcr.redirect_uri.as_deref().unwrap_or(DEFAULT_REDIRECT_URI);
    let mut manager = AuthorizationManager::new(server_url)
        .await
        .with_context(|| format!("OAuth setup for MCP '{mcp_name}' failed"))?;
    manager.set_credential_store(FileCredentialStore::new(FileCredentialStore::path_for(
        mcp_name,
    )?));
    // AuthorizationSession requires metadata up front; resolve_metadata is a
    // pure query, so seed the manager via the public setter.
    let resolution = manager
        .resolve_metadata()
        .await
        .with_context(|| format!("OAuth endpoint discovery for MCP '{mcp_name}' failed"))?;
    manager.set_metadata(resolution.metadata);

    let mut request = AuthorizationRequest::new(redirect_uri);
    if !dcr.scopes.is_empty() {
        request = request.with_scopes(dcr.scopes.iter().cloned());
    }
    let session = AuthorizationSession::new(manager, request)
        .await
        .map_err(|(_manager, e)| {
            anyhow::Error::from(e).context(format!("starting authorization for '{mcp_name}'"))
        })?;

    eprintln!(
        "\n1. Open this URL in a browser and approve the authorization request:\n\n  {}\n\n2. The browser will then try to load a `{redirect_uri}` URL and show an error — that is expected, nothing listens there.\n3. Copy the ENTIRE URL from the address bar and paste it at the prompt.\n\nRedirect URL> ",
        session.get_authorization_url()
    );
    let mut pasted = String::new();
    std::io::stdin()
        .read_line(&mut pasted)
        .context("failed to read the redirect URL from stdin")?;
    session
        .handle_callback_url(pasted.trim())
        .await
        .context("token exchange failed (stale or mistyped redirect URL?)")?;

    let granted = session.auth_manager.get_current_scopes().await;
    println!("\n✅ Authorized '{mcp_name}'.");
    if !granted.is_empty() {
        println!("   scopes: {}", granted.join(", "));
    }
    println!(
        "   credentials: {}\n   Restart `jyc serve` to pick them up.",
        FileCredentialStore::path_for(mcp_name)?.display()
    );
    Ok(())
}

/// Default OAuth redirect URI for the paste-back authorization flow.
///
/// Nothing ever listens on this port: the user authorizes in a browser, the
/// redirect "fails" to load, and the CLI parses the code straight from the
/// address-bar URL. The URI only needs to be stable (it is registered with
/// the server via DCR) and to match what the authorization server expects.
pub const DEFAULT_REDIRECT_URI: &str = "http://localhost:19876/callback";

/// Filesystem-backed [`CredentialStore`]: one JSON file per MCP server.
pub struct FileCredentialStore {
    path: PathBuf,
}

impl FileCredentialStore {
    /// Wrap an explicit credential file path (parent dir is created on save).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Credential path for an MCP server: `<data_home>/mcp_auth/<name>.json`.
    pub fn path_for(mcp_name: &str) -> Result<PathBuf> {
        let data_home = jyc_utils::paths::data_home()
            .context("cannot determine jyc data home for MCP credentials")?;
        Ok(data_home
            .join("mcp_auth")
            .join(format!("{}.json", sanitize_name(mcp_name)?)))
    }
}

/// Turn an MCP server name into a safe file stem. MCP names are free-form
/// (users have been observed naming servers after their URL, slashes and all),
/// so anything outside `[A-Za-z0-9._-]` becomes `_`.
fn sanitize_name(name: &str) -> Result<String> {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    anyhow::ensure!(
        !s.is_empty() && s != "." && s != "..",
        "MCP name '{name}' is not usable as a credential file name"
    );
    Ok(s)
}

/// Maps an io error, treating "missing file" as none rather than failure.
fn io_to_auth_err(e: std::io::Error, path: &Path, what: &str) -> AuthError {
    AuthError::InternalError(format!("failed to {what} {}: {e}", path.display()))
}

#[async_trait::async_trait]
impl CredentialStore for FileCredentialStore {
    async fn load(&self) -> std::result::Result<Option<StoredCredentials>, AuthError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
                AuthError::InternalError(format!("failed to parse {}: {e}", self.path.display()))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_to_auth_err(e, &self.path, "read")),
        }
    }

    async fn save(&self, credentials: StoredCredentials) -> std::result::Result<(), AuthError> {
        let bytes = serde_json::to_vec_pretty(&credentials)
            .map_err(|e| AuthError::InternalError(format!("failed to serialize: {e}")))?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| io_to_auth_err(e, dir, "create dir"))?;
        }
        write_secret_file(&self.path, &bytes)
    }

    async fn clear(&self) -> std::result::Result<(), AuthError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_to_auth_err(e, &self.path, "delete")),
        }
    }
}

/// Write with owner-only permissions; refresh tokens are long-lived secrets.
#[cfg(unix)]
fn write_secret_file(path: &Path, bytes: &[u8]) -> std::result::Result<(), AuthError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| io_to_auth_err(e, path, "open for write"))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| io_to_auth_err(e, path, "write"))
}

#[cfg(not(unix))]
fn write_secret_file(path: &Path, bytes: &[u8]) -> std::result::Result<(), AuthError> {
    std::fs::write(path, bytes).map_err(|e| io_to_auth_err(e, path, "write"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_creds(client_id: &str) -> StoredCredentials {
        StoredCredentials::new(
            client_id.to_string(),
            None,
            vec!["jira.read".to_string()],
            Some(1_757_000_000),
        )
    }

    #[test]
    fn sanitize_replaces_path_chars_and_rejects_dot_names() {
        assert_eq!(
            sanitize_name("sap.tools/jira.mcp").unwrap(),
            "sap.tools_jira.mcp"
        );
        assert_eq!(sanitize_name("a b:c").unwrap(), "a_b_c");
        assert!(sanitize_name("").is_err());
        assert!(sanitize_name("..").is_err());
        assert!(sanitize_name("/").is_err());
    }

    #[tokio::test]
    async fn save_load_clear_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileCredentialStore::new(dir.path().join("sub/deep/creds.json"));

        assert!(store.load().await.unwrap().is_none());

        store.save(sample_creds("cid-1")).await.unwrap();
        let loaded = store
            .load()
            .await
            .unwrap()
            .expect("saved creds must load back");
        assert_eq!(loaded.client_id, "cid-1");
        assert_eq!(loaded.granted_scopes, vec!["jira.read".to_string()]);

        // save must overwrite (refresh-token rotation reuses the same file).
        store.save(sample_creds("cid-2")).await.unwrap();
        let loaded = store.load().await.unwrap().unwrap();
        assert_eq!(loaded.client_id, "cid-2");

        store.clear().await.unwrap();
        assert!(store.load().await.unwrap().is_none());
        // clear on missing file is not an error
        store.clear().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        FileCredentialStore::new(&path)
            .save(sample_creds("cid"))
            .await
            .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "credential file must be 0600");
    }

    #[tokio::test]
    async fn corrupt_file_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        std::fs::write(&path, b"not json").unwrap();
        let store = FileCredentialStore::new(&path);
        let err = store.load().await.unwrap_err();
        assert!(matches!(err, AuthError::InternalError(_)));
    }
}
