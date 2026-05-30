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

/// A parsed WeCom callback message from the decrypted XML.
///
/// Fields correspond to the inner XML body after AES decryption.
/// Reference: https://developer.work.weixin.qq.com/document/path/90255
///
/// This struct is also used for WeCom KF (Customer Service) event notifications.
/// KF events have:
/// - `msg_type == "event"`
/// - empty `content`/`chat_id`/`msg_id`
/// - `token` and `open_kfid` populated from `<Token>` and `<OpenKfId>` fields
#[derive(Debug, Clone)]
pub struct ParsedWecomMessage {
    /// Message content (text from `<Content>`).
    pub content: String,
    /// Sender's UserName (from `<FromUserName>`).
    pub from_user: String,
    /// Chat ID — the group chat room ID (from `<ChatId>`).
    /// This is used for routing outbound messages to the correct group.
    /// Optional — KF events may not have this field.
    pub chat_id: String,
    /// Message type (from `<MsgType>`, e.g. "text", "image", "event").
    pub msg_type: String,
    /// Message ID (from `<MsgId>`).
    pub msg_id: String,
    /// Create time (from `<CreateTime>`).
    pub create_time: String,
    /// Token — used in KF event notifications (from `<Token>`).
    /// Empty for regular WeCom messages.
    pub token: String,
    /// Open KF ID — the KF account ID (from `<OpenKfId>`).
    /// Empty for regular WeCom messages.
    pub open_kfid: String,
}

/// Per-channel configuration stored in the webhook server.
#[derive(Clone)]
pub struct ChannelWebhookConfig {
    /// Token for signature verification.
    pub token: String,
    /// Encoding AES key for message decryption.
    pub encoding_aes_key: String,
    /// Corp ID.
    pub corp_id: String,
    /// Handler for incoming decrypted parsed messages.
    pub on_message: Arc<dyn Fn(ParsedWecomMessage) -> Result<()> + Send + Sync>,
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

            // Parse the decrypted XML into structured fields
            let parsed = match parse_decrypted_xml(&decrypted) {
                Some(p) => p,
                None => {
                    tracing::warn!(
                        channel = %channel_name,
                        "WeCom callback: failed to parse decrypted XML"
                    );
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        "failed to parse decrypted XML".to_string(),
                    );
                }
            };

            // Call the channel's message handler with the parsed message
            if let Err(e) = (config.on_message)(parsed) {
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

/// Parse the decrypted XML message body into a `ParsedWecomMessage`.
///
/// The XML format (after AES decryption) is:
/// ```xml
/// <xml>
///   <ToUserName><![CDATA[ww1234567890]]></ToUserName>
///   <FromUserName><![CDATA[UserID]]></FromUserName>
///   <CreateTime>1700000000</CreateTime>
///   <MsgType><![CDATA[text]]></MsgType>
///   <Content><![CDATA[Hello]]></Content>
///   <MsgId>1234567890</MsgId>
///   <ChatId><![CDATA[wr1234567890]]></ChatId>
/// </xml>
/// ```
///
/// For KF event notifications, the XML may include `<Token>` and `<OpenKfId>`
/// instead of `<Content>`, `<ChatId>`, and `<MsgId>`:
/// ```xml
/// <xml>
///   <ToUserName><![CDATA[ww1234567890]]></ToUserName>
///   <FromUserName><![CDATA[KF_EVENT]]></FromUserName>
///   <CreateTime>1700000000</CreateTime>
///   <MsgType><![CDATA[event]]></MsgType>
///   <Event><![CDATA[kf_msg_or_event]]></Event>
///   <Token><![CDATA[xxxxxx]]></Token>
///   <OpenKfId><![CDATA[kf1234567]]></OpenKfId>
/// </xml>
/// ```
///
/// Uses simple string extraction (same style as `extract_encrypt_from_xml`).
pub fn parse_decrypted_xml(xml: &str) -> Option<ParsedWecomMessage> {
    let content = extract_xml_field(xml, "Content").unwrap_or_default();
    let from_user = extract_xml_field(xml, "FromUserName")?;
    let chat_id = extract_xml_field(xml, "ChatId").unwrap_or_default();
    let msg_type = extract_xml_field(xml, "MsgType")?;
    let msg_id = extract_xml_field(xml, "MsgId").unwrap_or_default();
    let create_time = extract_xml_field(xml, "CreateTime").unwrap_or_default();
    let token = extract_xml_field(xml, "Token").unwrap_or_default();
    let open_kfid = extract_xml_field(xml, "OpenKfId").unwrap_or_default();

    Some(ParsedWecomMessage {
        content,
        from_user,
        chat_id,
        msg_type,
        msg_id,
        create_time,
        token,
        open_kfid,
    })
}

/// Extract the CDATA content of an XML field (e.g. `<FieldName><![CDATA[value]]></FieldName>`).
///
/// Also supports non-CDATA fields like `<CreateTime>1700000000</CreateTime>`.
fn extract_xml_field(xml: &str, field: &str) -> Option<String> {
    // Try CDATA format first: <Field><![CDATA[value]]></Field>
    let cdata_start = format!("<{}><![CDATA[", field);
    let cdata_end = format!("]]></{}>", field);
    if let Some(start) = xml.find(&cdata_start) {
        let value_start = start + cdata_start.len();
        if let Some(end) = xml[value_start..].find(&cdata_end) {
            return Some(xml[value_start..value_start + end].to_string());
        }
    }

    // Try plain format: <Field>value</Field>
    let plain_start = format!("<{}>", field);
    let plain_end = format!("</{}>", field);
    if let Some(start) = xml.find(&plain_start) {
        let value_start = start + plain_start.len();
        if let Some(end) = xml[value_start..].find(&plain_end) {
            return Some(xml[value_start..value_start + end].to_string());
        }
    }

    None
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

    #[test]
    fn test_parse_decrypted_xml_full() {
        let xml = r#"<xml>
            <ToUserName><![CDATA[ww123456]]></ToUserName>
            <FromUserName><![CDATA[user001]]></FromUserName>
            <CreateTime>1700000000</CreateTime>
            <MsgType><![CDATA[text]]></MsgType>
            <Content><![CDATA[Hello World]]></Content>
            <MsgId>1234567890</MsgId>
            <ChatId><![CDATA[wr9876543210]]></ChatId>
        </xml>"#;

        let parsed = parse_decrypted_xml(xml).expect("should parse");
        assert_eq!(parsed.content, "Hello World");
        assert_eq!(parsed.from_user, "user001");
        assert_eq!(parsed.chat_id, "wr9876543210");
        assert_eq!(parsed.msg_type, "text");
        assert_eq!(parsed.msg_id, "1234567890");
        assert_eq!(parsed.create_time, "1700000000");
    }

    #[test]
    fn test_parse_decrypted_xml_missing_chat_id() {
        let xml = r#"<xml>
            <FromUserName><![CDATA[user001]]></FromUserName>
            <MsgType><![CDATA[text]]></MsgType>
            <Content><![CDATA[Hello]]></Content>
        </xml>"#;
        // ChatId is now optional — parsing should succeed with empty chat_id
        let parsed = parse_decrypted_xml(xml).expect("should parse without ChatId");
        assert_eq!(parsed.from_user, "user001");
        assert_eq!(parsed.chat_id, "");
        assert_eq!(parsed.msg_type, "text");
        assert_eq!(parsed.content, "Hello");
    }

    #[test]
    fn test_parse_decrypted_xml_missing_from_user() {
        let xml = r#"<xml>
            <ChatId><![CDATA[wr123]]></ChatId>
            <MsgType><![CDATA[text]]></MsgType>
            <Content><![CDATA[Hello]]></Content>
        </xml>"#;
        assert!(parse_decrypted_xml(xml).is_none());
    }

    #[test]
    fn test_extract_xml_field_cdata() {
        let xml = r#"<xml><Content><![CDATA[hello]]></Content></xml>"#;
        assert_eq!(extract_xml_field(xml, "Content"), Some("hello".to_string()));
    }

    #[test]
    fn test_extract_xml_field_plain() {
        let xml = r#"<xml><CreateTime>1700000000</CreateTime></xml>"#;
        assert_eq!(
            extract_xml_field(xml, "CreateTime"),
            Some("1700000000".to_string())
        );
    }

    #[test]
    fn test_extract_xml_field_not_found() {
        let xml = r#"<xml><Other>value</Other></xml>"#;
        assert_eq!(extract_xml_field(xml, "Missing"), None);
    }

    #[test]
    fn test_parse_decrypted_xml_empty_content() {
        let xml = r#"<xml>
            <FromUserName><![CDATA[user001]]></FromUserName>
            <CreateTime>1700000000</CreateTime>
            <MsgType><![CDATA[text]]></MsgType>
            <Content><![CDATA[]]></Content>
            <MsgId>1234567890</MsgId>
            <ChatId><![CDATA[wr9876543210]]></ChatId>
        </xml>"#;

        let parsed = parse_decrypted_xml(xml).expect("should parse with empty content");
        assert_eq!(parsed.content, "");
        assert_eq!(parsed.from_user, "user001");
        assert_eq!(parsed.chat_id, "wr9876543210");
    }

    #[test]
    fn test_parse_decrypted_xml_missing_content_optional() {
        let xml = r#"<xml>
            <FromUserName><![CDATA[user001]]></FromUserName>
            <CreateTime>1700000000</CreateTime>
            <MsgType><![CDATA[image]]></MsgType>
            <MsgId>1234567890</MsgId>
            <ChatId><![CDATA[wr9876543210]]></ChatId>
        </xml>"#;

        let parsed = parse_decrypted_xml(xml).expect("should parse without Content");
        assert_eq!(parsed.content, "");
        assert_eq!(parsed.msg_type, "image");
    }

    #[test]
    fn test_parse_kf_event_xml() {
        let xml = r#"<xml>
            <ToUserName><![CDATA[ww123456]]></ToUserName>
            <FromUserName><![CDATA[KF_EVENT]]></FromUserName>
            <CreateTime>1700000000</CreateTime>
            <MsgType><![CDATA[event]]></MsgType>
            <Event><![CDATA[kf_msg_or_event]]></Event>
            <Token><![CDATA[xxxxxx]]></Token>
            <OpenKfId><![CDATA[kf1234567]]></OpenKfId>
        </xml>"#;

        let parsed = parse_decrypted_xml(xml).expect("should parse KF event");
        assert_eq!(parsed.msg_type, "event");
        assert_eq!(parsed.content, "");
        assert_eq!(parsed.chat_id, "");
        assert_eq!(parsed.msg_id, "");
        assert_eq!(parsed.token, "xxxxxx");
        assert_eq!(parsed.open_kfid, "kf1234567");
        assert_eq!(parsed.from_user, "KF_EVENT");
    }

    #[test]
    fn test_parse_kf_event_missing_token() {
        let xml = r#"<xml>
            <ToUserName><![CDATA[ww123456]]></ToUserName>
            <FromUserName><![CDATA[KF_EVENT]]></FromUserName>
            <CreateTime>1700000000</CreateTime>
            <MsgType><![CDATA[event]]></MsgType>
            <Event><![CDATA[kf_msg_or_event]]></Event>
            <OpenKfId><![CDATA[kf1234567]]></OpenKfId>
        </xml>"#;

        let parsed = parse_decrypted_xml(xml).expect("should parse KF event without Token");
        assert_eq!(parsed.msg_type, "event");
        assert_eq!(parsed.token, "");
        assert_eq!(parsed.open_kfid, "kf1234567");
    }

    #[test]
    fn test_parse_kf_event_missing_open_kfid() {
        let xml = r#"<xml>
            <ToUserName><![CDATA[ww123456]]></ToUserName>
            <FromUserName><![CDATA[KF_EVENT]]></FromUserName>
            <CreateTime>1700000000</CreateTime>
            <MsgType><![CDATA[event]]></MsgType>
            <Event><![CDATA[kf_msg_or_event]]></Event>
            <Token><![CDATA[xxxxxx]]></Token>
        </xml>"#;

        let parsed = parse_decrypted_xml(xml).expect("should parse KF event without OpenKfId");
        assert_eq!(parsed.token, "xxxxxx");
        assert_eq!(parsed.open_kfid, "");
    }

    #[test]
    fn test_parse_regular_message_still_works() {
        let xml = r#"<xml>
            <ToUserName><![CDATA[ww123456]]></ToUserName>
            <FromUserName><![CDATA[user001]]></FromUserName>
            <CreateTime>1700000000</CreateTime>
            <MsgType><![CDATA[text]]></MsgType>
            <Content><![CDATA[Hello World]]></Content>
            <MsgId>1234567890</MsgId>
            <ChatId><![CDATA[wr9876543210]]></ChatId>
        </xml>"#;

        let parsed = parse_decrypted_xml(xml).expect("should parse regular message");
        assert_eq!(parsed.content, "Hello World");
        assert_eq!(parsed.from_user, "user001");
        assert_eq!(parsed.chat_id, "wr9876543210");
        assert_eq!(parsed.msg_type, "text");
        assert_eq!(parsed.msg_id, "1234567890");
        assert_eq!(parsed.create_time, "1700000000");
        // New fields should be empty for regular messages
        assert_eq!(parsed.token, "");
        assert_eq!(parsed.open_kfid, "");
    }
}
