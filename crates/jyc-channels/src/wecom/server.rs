//! Shared HTTP server for WeCom (企业微信) webhook callbacks.
//!
//! All WeCom channels share a single axum HTTP server that listens on
//! the global `[wecom].bind_addr`. Each channel registers a handler for
//! `/webhook/{channel_name}`.
//!
//! ## WeCom Callback Protocol
//!
//! ### URL Verification (GET)
//! WeCom sends a GET request with query parameters:
//! - `msg_signature` — SHA1 signature
//! - `timestamp` — current timestamp
//! - `nonce` — random nonce
//! - `echostr` — encrypted string to echo back
//!
//! Response: the decrypted `echostr` as plain text (status 200).
//!
//! ### Message Callback (POST)
//! WeCom sends a POST request with XML body containing `Encrypt`.
//! Query parameters: `msg_signature`, `timestamp`, `nonce`.
//! Response: empty body with status 200.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::extract::{Path, Query};
use axum::response::IntoResponse;
use axum::routing::get;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::wecom::crypto;

/// Per-channel configuration stored in the webhook server.
#[derive(Clone)]
pub struct ChannelWebhookConfig {
    /// Token for signature verification.
    pub token: String,
    /// Encoding AES key for message decryption.
    pub encoding_aes_key: String,
    /// Corp ID.
    pub corp_id: String,
    /// Handler for incoming decrypted messages.
    pub on_message: Arc<dyn Fn(String) -> Result<()> + Send + Sync>,
}

/// The shared WeCom webhook server state.
pub struct WecomWebhookServer {
    bind_addr: String,
    channels: Arc<RwLock<HashMap<String, ChannelWebhookConfig>>>,
}

impl WecomWebhookServer {
    /// Create a new shared webhook server with the given bind address.
    pub fn new(bind_addr: &str) -> Self {
        Self {
            bind_addr: bind_addr.to_string(),
            channels: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a channel's webhook handler.
    pub async fn register_channel(&self, channel_name: &str, config: ChannelWebhookConfig) {
        self.channels
            .write()
            .await
            .insert(channel_name.to_string(), config);
    }

    /// Start the HTTP server.
    ///
    /// Runs until the cancellation token is triggered.
    pub async fn start(&self, cancel: CancellationToken) -> Result<()> {
        let channels = self.channels.clone();

        let app = Router::new()
            .route("/webhook/{channel_name}", get(handle_get).post(handle_post))
            .with_state(channels);

        let bind_addr: std::net::SocketAddr = self
            .bind_addr
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid bind_addr '{}': {}", self.bind_addr, e))?;

        let listener = tokio::net::TcpListener::bind(bind_addr)
            .await
            .map_err(|e| anyhow::anyhow!("failed to bind to {}: {}", bind_addr, e))?;

        tracing::info!(
            bind_addr = %bind_addr,
            "WeCom webhook server started"
        );

        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                cancel.cancelled().await;
            })
            .await
            .map_err(|e| anyhow::anyhow!("wecom webhook server error: {}", e))?;

        Ok(())
    }
}

/// Query parameters for WeCom callback.
#[derive(Debug, serde::Deserialize)]
pub struct CallbackQuery {
    pub msg_signature: String,
    pub timestamp: String,
    pub nonce: String,
    pub echostr: Option<String>,
}

/// Handle GET request — URL verification.
///
/// WeCom sends this to verify the callback URL. We must:
/// 1. Verify the signature using token + timestamp + nonce + echostr
/// 2. Decrypt the echostr using encoding_aes_key
/// 3. Return the decrypted echostr as plain text
async fn handle_get(
    Path(channel_name): Path<String>,
    Query(query): Query<CallbackQuery>,
    channels: axum::extract::State<Arc<RwLock<HashMap<String, ChannelWebhookConfig>>>>,
) -> impl IntoResponse {
    let config = match channels.read().await.get(&channel_name) {
        Some(c) => c.clone(),
        None => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                format!("channel '{channel_name}' not found"),
            );
        }
    };

    let echostr = match &query.echostr {
        Some(s) => s,
        None => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                "missing echostr parameter".to_string(),
            );
        }
    };

    // Verify signature
    if !crypto::verify_signature(
        &config.token,
        &query.timestamp,
        &query.nonce,
        echostr,
        &query.msg_signature,
    ) {
        tracing::warn!(
            channel = %channel_name,
            "WeCom URL verification: signature mismatch"
        );
        return (
            axum::http::StatusCode::FORBIDDEN,
            "signature verification failed".to_string(),
        );
    }

    // Decrypt echostr
    match crypto::decrypt_msg(&config.encoding_aes_key, echostr) {
        Ok(plaintext) => {
            tracing::info!(
                channel = %channel_name,
                "WeCom URL verification successful"
            );
            (axum::http::StatusCode::OK, plaintext)
        }
        Err(e) => {
            tracing::error!(
                channel = %channel_name,
                error = %e,
                "WeCom URL verification: decryption failed"
            );
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("decryption failed: {}", e),
            )
        }
    }
}

/// Handle POST request — message callback.
///
/// WeCom sends a POST with XML body containing `<xml><Encrypt>...</Encrypt></xml>`.
/// We must:
/// 1. Verify the signature using token + timestamp + nonce + encrypted content
/// 2. Decrypt the message
/// 3. Call the channel's on_message handler
/// 4. Return 200 OK
async fn handle_post(
    Path(channel_name): Path<String>,
    Query(query): Query<CallbackQuery>,
    channels: axum::extract::State<Arc<RwLock<HashMap<String, ChannelWebhookConfig>>>>,
    body: String,
) -> impl IntoResponse {
    let config = match channels.read().await.get(&channel_name) {
        Some(c) => c.clone(),
        None => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                format!("channel '{channel_name}' not found"),
            );
        }
    };

    // Parse the XML body to extract <Encrypt>
    let encrypt = match extract_encrypt_from_xml(&body) {
        Some(e) => e,
        None => {
            tracing::warn!(
                channel = %channel_name,
                "WeCom callback: no Encrypt element in XML body"
            );
            return (
                axum::http::StatusCode::BAD_REQUEST,
                "missing Encrypt element".to_string(),
            );
        }
    };

    // Verify signature
    if !crypto::verify_signature(
        &config.token,
        &query.timestamp,
        &query.nonce,
        &encrypt,
        &query.msg_signature,
    ) {
        tracing::warn!(
            channel = %channel_name,
            "WeCom callback: signature mismatch"
        );
        return (
            axum::http::StatusCode::FORBIDDEN,
            "signature verification failed".to_string(),
        );
    }

    // Decrypt the message
    match crypto::decrypt_msg(&config.encoding_aes_key, &encrypt) {
        Ok(decrypted) => {
            tracing::debug!(
                channel = %channel_name,
                "WeCom callback: message decrypted successfully"
            );

            // Call the channel's message handler
            if let Err(e) = (config.on_message)(decrypted) {
                tracing::error!(
                    channel = %channel_name,
                    error = %e,
                    "WeCom callback: message handler error"
                );
            }

            (axum::http::StatusCode::OK, String::new())
        }
        Err(e) => {
            tracing::error!(
                channel = %channel_name,
                error = %e,
                "WeCom callback: decryption failed"
            );
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("decryption failed: {}", e),
            )
        }
    }
}

/// Extract the `<Encrypt>` content from a WeCom XML message body.
///
/// The XML format is:
/// ```xml
/// <xml>
///   <ToUserName><![CDATA[wx1234567890]]></ToUserName>
///   <Encrypt><![CDATA[base64_encrypted_content]]></Encrypt>
///   <AgentID><![CDATA[1000002]]></AgentID>
/// </xml>
/// ```
fn extract_encrypt_from_xml(xml: &str) -> Option<String> {
    // Simple extraction using string search
    // Look for <Encrypt><![CDATA[...]]></Encrypt>
    let start_marker = "<Encrypt><![CDATA[";
    let end_marker = "]]></Encrypt>";

    let start = xml.find(start_marker)?;
    let content_start = start + start_marker.len();
    let end = xml[content_start..].find(end_marker)?;

    Some(xml[content_start..content_start + end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_callback_query_deserialize() {
        let query = CallbackQuery {
            msg_signature: "abc123".to_string(),
            timestamp: "1700000000".to_string(),
            nonce: "123456".to_string(),
            echostr: Some("encrypted_echo".to_string()),
        };
        assert_eq!(query.msg_signature, "abc123");
        assert_eq!(query.timestamp, "1700000000");
        assert_eq!(query.nonce, "123456");
        assert_eq!(query.echostr, Some("encrypted_echo".to_string()));
    }

    #[test]
    fn test_callback_query_no_echostr() {
        let query = CallbackQuery {
            msg_signature: "abc".to_string(),
            timestamp: "100".to_string(),
            nonce: "xyz".to_string(),
            echostr: None,
        };
        assert_eq!(query.msg_signature, "abc");
        assert!(query.echostr.is_none());
    }

    #[test]
    fn test_extract_encrypt_from_xml() {
        let xml = r#"<xml>
            <ToUserName><![CDATA[ww123456]]></ToUserName>
            <Encrypt><![CDATA[base64encryptedcontent]]></Encrypt>
            <AgentID><![CDATA[1000002]]></AgentID>
        </xml>"#;

        let encrypt = extract_encrypt_from_xml(xml);
        assert_eq!(encrypt, Some("base64encryptedcontent".to_string()));
    }

    #[test]
    fn test_extract_encrypt_from_xml_missing() {
        let xml = r#"<xml><ToUserName>test</ToUserName></xml>"#;
        assert!(extract_encrypt_from_xml(xml).is_none());
    }

    #[test]
    fn test_extract_encrypt_from_xml_empty() {
        let xml = r#"<xml><Encrypt><![CDATA[]]></Encrypt></xml>"#;
        let encrypt = extract_encrypt_from_xml(xml);
        assert_eq!(encrypt, Some("".to_string()));
    }
}
