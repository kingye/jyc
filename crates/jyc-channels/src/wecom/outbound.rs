//! WeCom (企业微信) outbound wire-format helpers.
//!
//! Sends group messages via the WeCom External Contact API
//! (`/cgi-bin/externalcontact/message/send`). Authentication uses
//! `corpid` + `corpsecret` to obtain an access_token.
//!
//! Pipe-only architecture (see docs/architecture/overview.md): the hub
//! channel owns the reply lifecycle; this module only knows how to put a
//! message on the wire.
//!
//! Reference: https://developer.work.weixin.qq.com/document/path/92135

use anyhow::{Context, Result};

use crate::wecom::token_cache::AccessTokenCache;

/// The external contact message send API base URL.
const EXTERNAL_CONTACT_API: &str =
    "https://qyapi.weixin.qq.com/cgi-bin/externalcontact/message/send";

/// Stateless sender for WeCom external-contact group messages.
pub struct WecomSender {
    access_token_cache: AccessTokenCache,
    /// Shared HTTP client with connection pool.
    client: reqwest::Client,
}

impl WecomSender {
    pub fn new(corp_id: String, corp_secret: String) -> Self {
        Self {
            access_token_cache: AccessTokenCache::new(corp_id, corp_secret),
            client: reqwest::Client::new(),
        }
    }

    /// Verify connectivity by fetching an access token.
    pub async fn verify_connectivity(&self) -> Result<()> {
        self.access_token_cache.get_token().await?;
        Ok(())
    }

    /// Send a text/markdown message to a chat group; markdown is
    /// auto-detected from the content.
    pub async fn send(&self, chat_id: &str, text: &str) -> Result<()> {
        self.post(build_payload(chat_id, text)).await
    }

    async fn post(&self, payload: serde_json::Value) -> Result<()> {
        let token = self.access_token_cache.get_token().await?;
        let url = format!("{}?access_token={}", EXTERNAL_CONTACT_API, token);
        let response = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .context("failed to send WeCom external contact message")?;

        let status = response.status();
        let body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
        let errcode = body["errcode"].as_i64().unwrap_or(-1);
        if !status.is_success() || errcode != 0 {
            let errmsg = body["errmsg"].as_str().unwrap_or("unknown error");
            anyhow::bail!(
                "WeCom external contact API error {}: {} (status: {})",
                errcode,
                errmsg,
                status
            );
        }
        Ok(())
    }
}

/// Build the JSON payload for an external contact message: `text` by
/// default, `markdown` when the content looks like markdown.
fn build_payload(chat_id: &str, text: &str) -> serde_json::Value {
    let is_markdown = text.contains("```")
        || text.contains("**")
        || text.contains("##")
        || text.contains("|")
        || text.contains("- [")
        || text.contains("![");

    if is_markdown {
        serde_json::json!({
            "chat_id": chat_id,
            "msgtype": "markdown",
            "markdown": {
                "content": text
            }
        })
    } else {
        serde_json::json!({
            "chat_id": chat_id,
            "msgtype": "text",
            "text": {
                "content": text
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_payload_text() {
        let payload = build_payload("wr12345", "Hello World");
        assert_eq!(payload["chat_id"], "wr12345");
        assert_eq!(payload["msgtype"], "text");
        assert_eq!(payload["text"]["content"], "Hello World");
    }

    #[test]
    fn test_build_payload_markdown() {
        let payload = build_payload("wr12345", "## Title\n\n**bold** text");
        assert_eq!(payload["chat_id"], "wr12345");
        assert_eq!(payload["msgtype"], "markdown");
        assert_eq!(payload["markdown"]["content"], "## Title\n\n**bold** text");
    }

    #[test]
    fn test_build_payload_markdown_with_code_block() {
        let payload = build_payload("wr12345", "```rust\nfn main() {}\n```");
        assert_eq!(payload["msgtype"], "markdown");
    }

    #[test]
    fn test_build_payload_markdown_with_table() {
        let payload = build_payload("wr12345", "| A | B |\n|---|---|");
        assert_eq!(payload["msgtype"], "markdown");
    }
}
