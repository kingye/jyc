//! Thread-safe WeCom access token cache.
//!
//! Stores the token and its expiry [`std::time::Instant`]. Refreshes automatically
//! when the token is missing or will expire within 5 minutes.
//! Used by both `WecomOutboundAdapter` and `WecomKfOutboundAdapter`.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};

/// The token refresh API base URL.
const TOKEN_API: &str = "https://qyapi.weixin.qq.com/cgi-bin/gettoken";

/// The number of seconds before token expiry to proactively refresh.
const TOKEN_REFRESH_MARGIN_SECS: u64 = 300;

/// Thread-safe access token cache.
///
/// Stores the token and its expiry Instant. Refreshes automatically
/// when the token is missing or will expire within 5 minutes.
pub struct AccessTokenCache {
    inner: Arc<std::sync::Mutex<Option<(String, Instant)>>>,
    corp_id: String,
    corp_secret: String,
    client: reqwest::Client,
}

impl AccessTokenCache {
    /// Create a new access token cache.
    pub fn new(corp_id: String, corp_secret: String) -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(None)),
            corp_id,
            corp_secret,
            client: reqwest::Client::new(),
        }
    }

    /// Get a valid access token, refreshing if necessary.
    ///
    /// If the cached token exists and will not expire within
    /// `TOKEN_REFRESH_MARGIN_SECS` seconds, returns it directly.
    /// Otherwise calls the WeCom gettoken API to obtain a new one.
    pub async fn get_token(&self) -> Result<String> {
        // Check if cached token is still valid
        {
            let cache = self.inner.lock().unwrap();
            if let Some((token, expiry)) = cache.as_ref() {
                let now = Instant::now();
                let remaining = if *expiry > now {
                    *expiry - now
                } else {
                    std::time::Duration::from_secs(0)
                };
                if remaining.as_secs() > TOKEN_REFRESH_MARGIN_SECS {
                    return Ok(token.clone());
                }
            }
        }

        // Need to refresh: fetch new token from API
        let url = format!(
            "{}?corpid={}&corpsecret={}",
            TOKEN_API, self.corp_id, self.corp_secret
        );

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context("failed to request WeCom access_token")?;

        let body: serde_json::Value = response
            .json()
            .await
            .context("failed to parse WeCom access_token response")?;

        let errcode = body["errcode"].as_i64().unwrap_or(-1);
        if errcode != 0 {
            let errmsg = body["errmsg"].as_str().unwrap_or("unknown error");
            anyhow::bail!("WeCom gettoken API returned error {}: {}", errcode, errmsg);
        }

        let token = body["access_token"]
            .as_str()
            .context("missing access_token in gettoken response")?
            .to_string();
        let expires_in = body["expires_in"].as_i64().unwrap_or(7200) as u64;

        let expiry = Instant::now()
            .checked_add(std::time::Duration::from_secs(expires_in))
            .context("token expiry overflow")?;

        // Update cache
        {
            let mut cache = self.inner.lock().unwrap();
            *cache = Some((token.clone(), expiry));
        }

        tracing::debug!(
            expires_in_secs = expires_in,
            "WeCom access_token obtained and cached"
        );

        Ok(token)
    }

    /// Get a clone of the inner mutex for testing/sharing.
    #[cfg(test)]
    pub fn inner_clone(&self) -> Arc<std::sync::Mutex<Option<(String, Instant)>>> {
        self.inner.clone()
    }
}
