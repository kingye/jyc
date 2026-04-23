pub mod client;
pub mod prompt_builder;
pub mod service;
pub mod session;
#[allow(dead_code)]
pub mod types;

use anyhow::{Context, Result};
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::net::TcpListener;

use crate::utils::constants::{OPENCODE_PORT_RANGE_START, OPENCODE_PORT_RANGE_END, OPENCODE_HEALTH_CHECK_TIMEOUT, OPENCODE_STARTUP_TIMEOUT};

/// Manages the OpenCode server process lifecycle.
///
/// A single server instance handles all threads. It's auto-started on first use,
/// auto-finds a free port, and readiness is detected by parsing stdout for
/// `"opencode server listening on http://..."`.
pub struct OpenCodeServer {
    port: Mutex<Option<u16>>,
    base_url: Mutex<Option<String>>,
    process: Mutex<Option<Child>>,
    http_client: reqwest::Client,
}

impl OpenCodeServer {
    pub fn new() -> Self {
        Self {
            port: Mutex::new(None),
            base_url: Mutex::new(None),
            process: Mutex::new(None),
            http_client: reqwest::Client::new(),
        }
    }

    /// Ensure the server is running. Returns the port.
    pub async fn ensure_started(&self) -> Result<u16> {
        // Check if already running and healthy
        if let Some(port) = *self.port.lock().await {
            if self.is_alive(port).await {
                return Ok(port);
            }
            tracing::warn!(port = port, "OpenCode server not responding, restarting...");
            self.stop().await.ok();
        }

        // Find a free port
        let port = find_free_port().await?;

        // Start the server process
        tracing::info!(port = port, "Starting OpenCode server...");

        let mut child = Command::new("opencode")
            .arg("serve")
            .arg(format!("--hostname=127.0.0.1"))
            .arg(format!("--port={port}"))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("failed to start opencode server — is 'opencode' in PATH?")?;

        // Wait for the server to print its listening URL on stdout
        let url = self.wait_for_listening(&mut child, port).await?;

        *self.process.lock().await = Some(child);
        *self.port.lock().await = Some(port);
        *self.base_url.lock().await = Some(url.clone());

        // Wait for the server to actually be ready to serve requests.
        // The "listening" message may appear before the API is fully initialized.
        self.wait_for_ready(port).await?;

        tracing::info!(port = port, url = %url, "OpenCode server started");
        Ok(port)
    }

    /// Wait for the server stdout to print "opencode server listening on <url>".
    /// This is how the SDK detects readiness.
    async fn wait_for_listening(&self, child: &mut Child, _port: u16) -> Result<String> {
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("failed to capture stdout from opencode process"))?;

        let mut reader = tokio::io::BufReader::new(stdout).lines();

        let timeout = OPENCODE_STARTUP_TIMEOUT;
        let result = tokio::time::timeout(timeout, async {
            while let Some(line) = reader.next_line().await? {
                tracing::debug!(line = %line, "opencode stdout");
                if line.contains("opencode server listening") {
                    // Parse URL: "opencode server listening on http://127.0.0.1:49157"
                    if let Some(url_match) = line.split("on ").nth(1) {
                        let url = url_match.trim().to_string();
                        return Ok::<String, anyhow::Error>(url);
                    }
                }
            }
            anyhow::bail!("opencode process closed stdout without printing listening URL")
        }).await;

        match result {
            Ok(Ok(url)) => Ok(url),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                // Timeout — check if process died
                if let Some(status) = child.try_wait()? {
                    anyhow::bail!(
                        "OpenCode server exited with status {status} before becoming ready"
                    );
                }
                anyhow::bail!(
                    "OpenCode server did not become ready within {}s",
                    timeout.as_secs()
                );
            }
        }
    }

    /// Stop the server.
    pub async fn stop(&self) -> Result<()> {
        if let Some(mut child) = self.process.lock().await.take() {
            tracing::debug!("Stopping OpenCode server...");
            child.kill().await.ok();
            child.wait().await.ok();
        }
        *self.port.lock().await = None;
        *self.base_url.lock().await = None;
        tracing::debug!("OpenCode server stopped");
        Ok(())
    }

    /// Check if the server is alive via health check.
    pub async fn is_alive(&self, port: u16) -> bool {
        let url = format!("http://127.0.0.1:{port}/session");
        match self.http_client
            .get(&url)
            .timeout(OPENCODE_HEALTH_CHECK_TIMEOUT)
            .send()
            .await
        {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }

    /// Wait for the server API to be ready after it starts listening.
    ///
    /// Polls the `/session` endpoint up to 10 times with 500ms intervals.
    async fn wait_for_ready(&self, port: u16) -> Result<()> {
        for attempt in 1..=10 {
            if self.is_alive(port).await {
                tracing::debug!(attempt = attempt, "OpenCode server API ready");
                return Ok(());
            }
            tracing::debug!(attempt = attempt, "Waiting for OpenCode API to be ready");
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        anyhow::bail!("OpenCode server API not ready after 5s")
    }

    /// Get the current port (if running).
    #[allow(dead_code)]
    pub async fn port(&self) -> Option<u16> {
        *self.port.lock().await
    }

    /// Get the base URL for the server.
    pub async fn base_url(&self) -> Result<String> {
        let port = self.ensure_started().await?;
        // Return cached URL if available
        if let Some(ref url) = *self.base_url.lock().await {
            return Ok(url.clone());
        }
        Ok(format!("http://127.0.0.1:{port}"))
    }
}

/// Find a free TCP port in the ephemeral range.
async fn find_free_port() -> Result<u16> {
    for port in OPENCODE_PORT_RANGE_START..=OPENCODE_PORT_RANGE_END {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
            drop(listener);
            return Ok(port);
        }
    }
    anyhow::bail!(
        "no free port found in range {}–{}",
        OPENCODE_PORT_RANGE_START,
        OPENCODE_PORT_RANGE_END
    )
}
