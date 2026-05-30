//! WeCom KF (Customer Service) API client.
//!
//! Unlike the regular WeCom external contact API, the KF channel uses
//! dedicated KF APIs:
//! - `kf/sync_msg` — pull customer messages incrementally (cursor-based)
//! - `kf/send_msg` — send reply messages to customers
//!
//! Reference: https://developer.work.weixin.qq.com/document/path/94677

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::wecom::token_cache::AccessTokenCache;

/// The KF sync message API base URL.
const KF_SYNC_API: &str = "https://qyapi.weixin.qq.com/cgi-bin/kf/sync_msg";

/// The KF send message API base URL.
const KF_SEND_API: &str = "https://qyapi.weixin.qq.com/cgi-bin/kf/send_msg";

/// The external contact get API base URL.
const EXTERNAL_CONTACT_API: &str = "https://qyapi.weixin.qq.com/cgi-bin/externalcontact/get";

/// Response from the WeCom KF `sync_msg` API.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KfSyncResponse {
    /// Error code (0 = success).
    pub errcode: i64,
    /// Error message.
    pub errmsg: String,
    /// Next cursor for pagination.
    #[serde(default)]
    pub next_cursor: String,
    /// Whether there are more messages to sync.
    #[serde(default)]
    pub has_more: Option<i64>,
    /// List of synced messages.
    #[serde(default)]
    pub msg_list: Vec<KfMessage>,
}

/// A single KF message from the `sync_msg` API.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KfMessage {
    /// Message ID.
    pub msgid: String,
    /// Open KF ID — the KF account that received this message.
    #[serde(default)]
    pub open_kfid: String,
    /// External user ID — the customer who sent the message.
    #[serde(default)]
    pub external_userid: String,
    /// Send time (unix timestamp).
    #[serde(default)]
    pub send_time: i64,
    /// Message type (e.g. "text", "image", "voice", "video", "file", "location", "event").
    #[serde(default)]
    pub msgtype: String,
    /// Text content (present when msgtype == "text").
    #[serde(default)]
    pub text: Option<KfTextContent>,
}

/// Text content of a KF message.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KfTextContent {
    /// The message content.
    pub content: String,
}

/// Response from the WeCom `externalcontact/get` API.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExternalContactResponse {
    /// Error code (0 = success).
    pub errcode: i64,
    /// Error message.
    pub errmsg: String,
    /// External contact details.
    #[serde(default)]
    pub external_contact: Option<ExternalContact>,
}

/// External contact (customer) details.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExternalContact {
    /// External user ID.
    pub external_userid: String,
    /// Display name.
    pub name: String,
    /// Avatar URL.
    #[serde(default)]
    pub avatar: Option<String>,
    /// Gender (0: unknown, 1: male, 2: female).
    #[serde(default)]
    pub gender: Option<i32>,
    /// Type (1: WeChat user, 2: enterprise WeCom user).
    #[serde(default)]
    pub r#type: Option<i32>,
}

/// WeCom KF API client.
///
/// Provides methods to sync incoming messages and send replies
/// via the WeCom Customer Service API.
pub struct KfApiClient {
    access_token_cache: Arc<AccessTokenCache>,
    client: reqwest::Client,
    /// Cache for external contact names (external_userid → name).
    name_cache: Arc<RwLock<HashMap<String, String>>>,
}

impl KfApiClient {
    /// Create a new KF API client.
    pub fn new(access_token_cache: Arc<AccessTokenCache>) -> Self {
        Self {
            access_token_cache,
            client: reqwest::Client::new(),
            name_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get the display name of an external contact (customer).
    ///
    /// Uses an in-memory cache to avoid repeated API calls.
    /// Falls back to the external_userid itself if the API call fails.
    pub async fn get_external_contact_name(&self, external_userid: &str) -> String {
        // Check cache first
        {
            let cache = self.name_cache.read().await;
            if let Some(name) = cache.get(external_userid) {
                return name.clone();
            }
        }

        // Fetch from API
        let name = match self.fetch_external_contact(external_userid).await {
            Ok(contact) => contact.name,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    external_userid = %external_userid,
                    "Failed to fetch external contact name, falling back to userid"
                );
                external_userid.to_string()
            }
        };

        // Store in cache
        {
            let mut cache = self.name_cache.write().await;
            cache.insert(external_userid.to_string(), name.clone());
        }

        name
    }

    async fn fetch_external_contact(&self, external_userid: &str) -> Result<ExternalContact> {
        let access_token = self.access_token_cache.get_token().await?;

        let url = format!(
            "{}?access_token={}&external_userid={}",
            EXTERNAL_CONTACT_API, access_token, external_userid
        );

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context("failed to send externalcontact/get request")?;

        let status = response.status();
        let body: ExternalContactResponse = response
            .json()
            .await
            .context("failed to parse externalcontact/get response")?;

        if !status.is_success() || body.errcode != 0 {
            anyhow::bail!(
                "externalcontact/get API returned error {}: {} (status: {})",
                body.errcode,
                body.errmsg,
                status,
            );
        }

        body.external_contact.ok_or_else(|| {
            anyhow::anyhow!("externalcontact/get response missing external_contact field")
        })
    }

    /// Verify connectivity by fetching a fresh access token.
    ///
    /// Used by the outbound adapter's `connect()` method to validate
    /// that the corp_id and corp_secret are valid at startup.
    pub async fn verify_connectivity(&self) -> Result<()> {
        self.access_token_cache.get_token().await?;
        Ok(())
    }

    /// Sync messages from a KF account using cursor-based pagination.
    ///
    /// - `token`: The token from the KF event notification
    /// - `cursor`: Pagination cursor (empty string for first request)
    /// - `open_kfid`: KF account ID to sync messages for
    /// - `limit`: Maximum number of messages to return (max: 1000)
    pub async fn sync_messages(
        &self,
        token: &str,
        cursor: &str,
        open_kfid: &str,
        limit: u32,
    ) -> Result<KfSyncResponse> {
        let access_token = self.access_token_cache.get_token().await?;

        let url = format!("{}?access_token={}", KF_SYNC_API, access_token);

        let payload = serde_json::json!({
            "token": token,
            "cursor": cursor,
            "open_kfid": open_kfid,
            "limit": limit,
        });

        let response = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .context("failed to send KF sync_msg request")?;

        let status = response.status();
        let body: KfSyncResponse = response
            .json()
            .await
            .context("failed to parse KF sync_msg response")?;

        if !status.is_success() || body.errcode != 0 {
            anyhow::bail!(
                "KF sync_msg API returned error {}: {} (status: {})",
                body.errcode,
                body.errmsg,
                status,
            );
        }

        Ok(body)
    }

    /// Send a reply message to a customer via the KF API.
    ///
    /// - `open_kfid`: The KF account ID
    /// - `touser`: The external user ID (customer)
    /// - `msgtype`: Message type: "text" (markdown is NOT supported by the KF API)
    /// - `content`: Message content
    pub async fn send_message(
        &self,
        open_kfid: &str,
        touser: &str,
        msgtype: &str,
        content: &str,
    ) -> Result<()> {
        let access_token = self.access_token_cache.get_token().await?;

        let url = format!("{}?access_token={}", KF_SEND_API, access_token);

        // Build payload with dynamic content key matching msgtype.
        // KF API expects the content key to match the msgtype value:
        //   msgtype="text"  → "text": {"content": "..."}
        //   msgtype="image" → "image": {"media_id": "..."}
        let content_obj = serde_json::json!({ "content": content });
        let mut payload = serde_json::json!({
            "touser": touser,
            "open_kfid": open_kfid,
            "msgtype": msgtype,
        });
        payload[msgtype] = content_obj;

        #[derive(Deserialize)]
        struct SendResponse {
            errcode: i64,
            errmsg: String,
        }

        let response = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .context("failed to send KF send_msg request")?;

        let status = response.status();
        let body: SendResponse = response
            .json()
            .await
            .context("failed to parse KF send_msg response")?;

        if !status.is_success() || body.errcode != 0 {
            anyhow::bail!(
                "KF send_msg API returned error {}: {} (status: {})",
                body.errcode,
                body.errmsg,
                status,
            );
        }

        Ok(())
    }

    /// Build the sync request payload (for unit testing).
    #[cfg(test)]
    pub fn build_sync_request(
        token: &str,
        cursor: &str,
        open_kfid: &str,
        limit: u32,
    ) -> serde_json::Value {
        serde_json::json!({
            "token": token,
            "cursor": cursor,
            "open_kfid": open_kfid,
            "limit": limit,
        })
    }

    /// Build the send message payload (for unit testing).
    #[cfg(test)]
    pub fn build_send_payload(
        open_kfid: &str,
        touser: &str,
        msgtype: &str,
        content: &str,
    ) -> serde_json::Value {
        let content_obj = serde_json::json!({ "content": content });
        let mut payload = serde_json::json!({
            "touser": touser,
            "open_kfid": open_kfid,
            "msgtype": msgtype,
        });
        payload[msgtype] = content_obj;
        payload
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_sync_request() {
        let payload = KfApiClient::build_sync_request("token123", "cursor_abc", "kf001", 100);
        assert_eq!(payload["token"], "token123");
        assert_eq!(payload["cursor"], "cursor_abc");
        assert_eq!(payload["open_kfid"], "kf001");
        assert_eq!(payload["limit"], 100);
    }

    #[test]
    fn test_build_sync_request_empty_cursor() {
        let payload = KfApiClient::build_sync_request("token123", "", "kf001", 50);
        assert_eq!(payload["cursor"], "");
        assert_eq!(payload["limit"], 50);
    }

    #[test]
    fn test_build_send_payload() {
        let payload = KfApiClient::build_send_payload("kf001", "user123", "text", "Hello, world!");
        assert_eq!(payload["touser"], "user123");
        assert_eq!(payload["open_kfid"], "kf001");
        assert_eq!(payload["msgtype"], "text");
        assert_eq!(payload["text"]["content"], "Hello, world!");
    }

    #[test]
    fn test_build_send_payload_markdown() {
        let payload =
            KfApiClient::build_send_payload("kf001", "user123", "markdown", "## Title\n\nbody");
        assert_eq!(payload["msgtype"], "markdown");
        assert_eq!(payload["markdown"]["content"], "## Title\n\nbody");
    }

    #[test]
    fn test_kf_sync_response_deserialize() {
        let json = r#"{
            "errcode": 0,
            "errmsg": "ok",
            "next_cursor": "next_cursor_xyz",
            "has_more": 1,
            "msg_list": [
                {
                    "msgid": "msg_001",
                    "open_kfid": "kf001",
                    "external_userid": "user123",
                    "send_time": 1700000000,
                    "msgtype": "text",
                    "text": {
                        "content": "Hello"
                    }
                },
                {
                    "msgid": "msg_002",
                    "open_kfid": "kf001",
                    "external_userid": "user456",
                    "send_time": 1700000001,
                    "msgtype": "text",
                    "text": {
                        "content": "Hi there"
                    }
                }
            ]
        }"#;

        let resp: KfSyncResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.errcode, 0);
        assert_eq!(resp.errmsg, "ok");
        assert_eq!(resp.next_cursor, "next_cursor_xyz");
        assert_eq!(resp.has_more, Some(1));
        assert_eq!(resp.msg_list.len(), 2);

        assert_eq!(resp.msg_list[0].msgid, "msg_001");
        assert_eq!(resp.msg_list[0].open_kfid, "kf001");
        assert_eq!(resp.msg_list[0].external_userid, "user123");
        assert_eq!(resp.msg_list[0].text.as_ref().unwrap().content, "Hello");

        assert_eq!(resp.msg_list[1].msgid, "msg_002");
        assert_eq!(resp.msg_list[1].external_userid, "user456");
    }

    #[test]
    fn test_kf_sync_response_empty_msg_list() {
        let json = r#"{
            "errcode": 0,
            "errmsg": "ok",
            "next_cursor": "",
            "has_more": 0,
            "msg_list": []
        }"#;

        let resp: KfSyncResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.errcode, 0);
        assert!(resp.msg_list.is_empty());
        assert_eq!(resp.has_more, Some(0));
    }

    #[test]
    fn test_kf_sync_response_error() {
        let json = r#"{
            "errcode": 40001,
            "errmsg": "invalid credential"
        }"#;

        let resp: KfSyncResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.errcode, 40001);
        assert_eq!(resp.errmsg, "invalid credential");
        assert!(resp.msg_list.is_empty());
    }

    #[test]
    fn test_kf_sync_response_missing_fields() {
        let json = r#"{
            "errcode": 0,
            "errmsg": "ok"
        }"#;

        let resp: KfSyncResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.errcode, 0);
        assert_eq!(resp.next_cursor, "");
        assert!(resp.msg_list.is_empty());
    }
}
