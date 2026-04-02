use anyhow::{Context, Result};
use async_imap::Session;
use async_native_tls::TlsStream;
use futures::StreamExt;
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

use crate::config::types::ImapConfig;

/// Timeout for IMAP commands (select, fetch, etc.) to detect dead TCP connections.
const IMAP_CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The stream type: TLS over tokio TCP (via compat layers).
/// async-native-tls gives us a futures-io TlsStream.
/// async-imap with runtime-tokio wants tokio::io::AsyncRead/Write.
/// So we wrap: TcpStream (tokio) → compat (futures-io) → TLS → compat (back to tokio-io).
type ImapStream = Compat<TlsStream<Compat<TcpStream>>>;

/// Wrapper around async-imap providing a higher-level API.
pub struct ImapClient {
    session: Option<Session<ImapStream>>,
    config: ImapConfig,
}

/// A fetched email message with raw data.
#[derive(Debug)]
pub struct FetchedEmail {
    /// IMAP UID
    pub uid: u32,
    /// IMAP sequence number
    pub seq: u32,
    /// Raw RFC 5322 email bytes
    pub body: Vec<u8>,
}

impl ImapClient {
    pub fn new(config: ImapConfig) -> Self {
        Self {
            session: None,
            config,
        }
    }

    /// Connect to the IMAP server with TLS and authenticate.
    pub async fn connect(&mut self) -> Result<()> {
        let addr = format!("{}:{}", self.config.host, self.config.port);
        tracing::debug!(addr = %addr, "Connecting to IMAP server");

        let tcp = tokio::time::timeout(
            IMAP_CMD_TIMEOUT,
            TcpStream::connect(&addr),
        )
        .await
        .map_err(|_| anyhow::anyhow!("TCP connect to {addr} timed out ({}s)", IMAP_CMD_TIMEOUT.as_secs()))?
        .with_context(|| format!("failed to connect to {addr}"))?;

        // tokio TcpStream → futures-io compat (for async-native-tls)
        let tcp_compat = tcp.compat();

        let tls = if self.config.tls {
            let tls_connector = async_native_tls::TlsConnector::new();
            tls_connector
                .connect(&self.config.host, tcp_compat)
                .await
                .context("TLS handshake failed")?
        } else {
            anyhow::bail!("non-TLS IMAP connections not yet supported");
        };

        // futures-io TlsStream → tokio-io compat (for async-imap with runtime-tokio)
        let tls_tokio = tls.compat();

        let client = async_imap::Client::new(tls_tokio);

        let mut session = client
            .login(&self.config.username, &self.config.password)
            .await
            .map_err(|e| anyhow::anyhow!("IMAP login failed: {}", e.0))?;

        // Send IMAP ID command (RFC 2971).
        // Required by some servers like 163.com (NetEase) — without this,
        // they reject SELECT with "Unsafe Login".
        let id_result = session
            .id([
                ("name", Some("jyc")),
                ("version", Some(env!("CARGO_PKG_VERSION"))),
                ("vendor", Some("jyc")),
            ])
            .await;
        match id_result {
            Ok(Some(server_id)) => {
                let name = server_id.get("name").map(|s| s.as_str()).unwrap_or("unknown");
                let vendor = server_id.get("vendor").map(|s| s.as_str()).unwrap_or("unknown");
                let trans_id = server_id.get("TransID").map(|s| s.as_str()).unwrap_or("-");
                tracing::debug!(
                    server_name = %name,
                    server_vendor = %vendor,
                    trans_id = %trans_id,
                    "IMAP ID exchanged"
                );
            }
            Ok(None) => {
                tracing::debug!("IMAP ID sent, server returned NIL");
            }
            Err(e) => {
                // ID command failure is non-fatal — some servers don't support it
                tracing::debug!(error = %e, "IMAP ID command failed (non-fatal)");
            }
        }

        tracing::info!(
            host = %self.config.host,
            user = %self.config.username,
            "IMAP connected and authenticated"
        );

        self.session = Some(session);
        Ok(())
    }

    /// Select a mailbox (e.g., "INBOX") and return the message count.
    pub async fn select(&mut self, mailbox: &str) -> Result<u32> {
        let session = self.session_mut()?;

        let mbox = tokio::time::timeout(IMAP_CMD_TIMEOUT, session.select(mailbox))
            .await
            .map_err(|_| anyhow::anyhow!("IMAP SELECT '{}' timed out ({}s)", mailbox, IMAP_CMD_TIMEOUT.as_secs()))?
            .map_err(|e| anyhow::anyhow!("IMAP SELECT '{}' failed: {}", mailbox, e))?;

        let count = mbox.exists;
        tracing::trace!(mailbox = %mailbox, count = count, "Mailbox selected");
        Ok(count)
    }

    /// Fetch emails by sequence number range.
    /// Returns raw email bodies with UIDs and sequence numbers.
    pub async fn fetch_range(&mut self, from: u32, to: u32) -> Result<Vec<FetchedEmail>> {
        let session = self.session_mut()?;

        let range = format!("{from}:{to}");
        let mut messages = tokio::time::timeout(
            IMAP_CMD_TIMEOUT,
            session.fetch(&range, "(UID BODY.PEEK[] FLAGS)"),
        )
        .await
        .map_err(|_| anyhow::anyhow!("IMAP FETCH {range} timed out ({}s)", IMAP_CMD_TIMEOUT.as_secs()))?
        .with_context(|| format!("failed to fetch range {range}"))?;

        let mut results = Vec::new();
        while let Some(msg) = messages.next().await {
            let msg = msg.context("error reading fetch stream")?;
            if let Some(body) = msg.body() {
                results.push(FetchedEmail {
                    uid: msg.uid.unwrap_or(0),
                    seq: msg.message,
                    body: body.to_vec(),
                });
            }
        }

        tracing::debug!(range = %range, count = results.len(), "Fetched emails");
        Ok(results)
    }

    /// Fetch a single email by UID.
    #[allow(dead_code)]
    pub async fn fetch_uid(&mut self, uid: u32) -> Result<Option<FetchedEmail>> {
        let session = self.session_mut()?;

        let uid_str = uid.to_string();
        let mut messages = tokio::time::timeout(
            IMAP_CMD_TIMEOUT,
            session.uid_fetch(&uid_str, "(UID BODY.PEEK[] FLAGS)"),
        )
        .await
        .map_err(|_| anyhow::anyhow!("IMAP UID FETCH {uid} timed out ({}s)", IMAP_CMD_TIMEOUT.as_secs()))?
        .with_context(|| format!("failed to fetch UID {uid}"))?;

        while let Some(msg) = messages.next().await {
            let msg = msg.context("error reading fetch stream")?;
            if let Some(body) = msg.body() {
                return Ok(Some(FetchedEmail {
                    uid: msg.uid.unwrap_or(uid),
                    seq: msg.message,
                    body: body.to_vec(),
                }));
            }
        }

        Ok(None)
    }

    /// Start IMAP IDLE and wait for new mail notification.
    /// Returns when the server signals new mail or the timeout expires.
    pub async fn idle(&mut self) -> Result<()> {
        let session = self
            .session
            .take()
            .ok_or_else(|| anyhow::anyhow!("IMAP: not connected"))?;

        let mut idle_handle = session.idle();
        idle_handle.init().await.context("IDLE init failed")?;

        tracing::debug!("IDLE started, waiting for new mail...");

        // Wait for up to 29 minutes (RFC recommends re-IDLE before 30 min)
        let timeout = std::time::Duration::from_secs(29 * 60);
        let (idle_wait, _stop) = idle_handle.wait_with_timeout(timeout);
        let _response = idle_wait.await.context("IDLE wait failed")?;

        // Get the session back from the idle handle
        let session = idle_handle.done().await.context("IDLE done failed")?;
        self.session = Some(session);

        tracing::debug!("IDLE ended");
        Ok(())
    }

    /// Disconnect from the IMAP server.
    ///
    /// Attempts a clean IMAP LOGOUT with a short timeout. If the connection is
    /// dead, just drops the session to avoid hanging on TCP retransmissions.
    pub async fn disconnect(&mut self) -> Result<()> {
        if let Some(mut session) = self.session.take() {
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                session.logout(),
            ).await {
                Ok(_) => {
                    tracing::debug!("IMAP disconnected (clean logout)");
                }
                Err(_) => {
                    tracing::warn!("IMAP logout timed out (5s), dropping connection");
                    // Session is already taken from self.session, it will be dropped here
                }
            }
        }
        Ok(())
    }

    /// Check if the client is currently connected.
    pub fn is_connected(&self) -> bool {
        self.session.is_some()
    }

    fn session_mut(&mut self) -> Result<&mut Session<ImapStream>> {
        self.session
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("IMAP: not connected"))
    }
}
