//! HTTP client for the jyc inspect server (REST + Bearer auth).
//!
//! The old line-delimited JSON protocol was replaced by real HTTP REST
//! (`axum` server). Same public method names as before so the dashboard's
//! call sites are unchanged.

use anyhow::{Context, Result};
use reqwest::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::de::DeserializeOwned;
use url::Url;

use jyc_types::{ActivityEntry, ChatMessageEntry, InspectOverview, InspectState};

/// HTTP client for the inspect server.
#[derive(Clone)]
pub struct InspectClient {
    base_url: Url,
    http: Client,
}

impl InspectClient {
    /// Connect to an inspect server at `addr` (e.g. `127.0.0.1:9876`).
    pub fn new(addr: &str) -> Self {
        Self::with_token(addr, None)
    }

    /// Connect with a bearer token. When `token` is `Some`, every request
    /// carries `Authorization: Bearer <token>`.
    pub fn with_token(addr: &str, token: Option<&str>) -> Self {
        let base_url = Url::parse(&format!("http://{addr}"))
            .expect("inspect address must be a valid host:port");
        let mut headers = HeaderMap::new();
        if let Some(t) = token {
            let val = HeaderValue::from_str(&format!("Bearer {t}"))
                .expect("bearer token must be a valid header value");
            headers.insert(AUTHORIZATION, val);
        }
        let http = Client::builder()
            .default_headers(headers)
            // Generous ceiling so a blackholed server errors out instead
            // of hanging the request forever — the dashboard's off-loop
            // overview poll would otherwise never complete, silently
            // freezing all future polls (poll_in_flight stuck true).
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("reqwest client must build");
        Self { base_url, http }
    }

    /// Full state snapshot.
    pub async fn get_state(&self) -> Result<InspectState> {
        self.get_json("/api/state").await
    }

    /// Slim state snapshot (no per-topic activity/messages).
    pub async fn get_overview(&self) -> Result<InspectOverview> {
        self.get_json("/api/state/overview").await
    }

    /// Recent activity entries for a topic. Filters internal entries
    /// server-side.
    pub async fn get_topic_activity(
        &self,
        channel: &str,
        topic: &str,
        since: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<ActivityEntry>> {
        let mut q: Vec<(&str, String)> = Vec::new();
        if let Some(s) = since {
            q.push(("since", s.to_string()));
        }
        if let Some(n) = limit {
            q.push(("limit", n.to_string()));
        }
        let path = format!("/api/topics/{}/{}/activity", channel, topic);
        self.get_json_query(&path, &q).await
    }

    /// Recent chat messages for a topic.
    pub async fn get_topic_chat(
        &self,
        channel: &str,
        topic: &str,
        since: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<ChatMessageEntry>> {
        let mut q: Vec<(&str, String)> = Vec::new();
        if let Some(s) = since {
            q.push(("since", s.to_string()));
        }
        if let Some(n) = limit {
            q.push(("limit", n.to_string()));
        }
        let path = format!("/api/topics/{}/{}/chat", channel, topic);
        self.get_json_query(&path, &q).await
    }

    /// Pattern names configured for a channel.
    pub async fn list_patterns(&self, channel: &str) -> Result<Vec<String>> {
        #[derive(serde::Deserialize)]
        struct R {
            patterns: Vec<String>,
        }
        let r: R = self
            .get_json(&format!("/api/channels/{}/patterns", channel))
            .await?;
        Ok(r.patterns)
    }

    /// Register a new ad-hoc topic. Returns `(success, message)`.
    pub async fn create_topic(
        &self,
        channel: &str,
        topic: &str,
        path: &str,
    ) -> Result<(bool, String)> {
        #[derive(serde::Serialize)]
        struct B<'a> {
            channel: &'a str,
            topic: &'a str,
            path: &'a str,
        }
        // `created_topic` endpoint returns 201 + {message: ...}; we use
        // the message field for both success and failure.
        let url = self.url("/api/topics");
        let resp = self
            .http
            .post(url)
            .json(&B {
                channel,
                topic,
                path,
            })
            .send()
            .await
            .context("POST /api/topics request failed")?;
        let status = resp.status();
        #[derive(serde::Deserialize, Default)]
        struct R {
            // Success: {"message":"..."}; failure: {"error":"..."}.
            // Read both via serde alias so the same struct works.
            #[serde(default, alias = "error")]
            message: String,
        }
        // Parse the body regardless of status; the server's success body
        // is `{message: "..."}` and failure is `{error: "..."}`.
        let body_text = resp.text().await.unwrap_or_default();
        let r: R = match serde_json::from_str(&body_text) {
            Ok(r) => r,
            Err(_) => R {
                message: format!("unexpected response (status {}): {}", status, body_text),
            },
        };
        Ok((status.is_success(), r.message))
    }

    /// Download a topic-local file as raw bytes.
    ///
    /// `path` is the relative URL from a websocket reply broadcast's
    /// `attachments[].path` (already percent-encoded, leading slash included),
    /// served by `GET /api/topics/{channel}/{topic}/files/{*file_path}`.
    pub async fn download_topic_file(&self, path: &str) -> Result<Vec<u8>> {
        let resp = self
            .http
            .get(self.url(path))
            .send()
            .await
            .context("failed to download topic file")?
            .error_for_status()
            .context("topic file request returned an error status")?;
        Ok(resp
            .bytes()
            .await
            .context("failed to read topic file body")?
            .to_vec())
    }

    /// Reload config. Returns `(success, message)`.
    pub async fn reload_config(&self) -> Result<(bool, String)> {
        #[derive(serde::Deserialize, Default)]
        struct R {
            // Success: {"message":"..."}; failure: {"error":"..."}.
            // Read both via serde alias so the same struct works.
            #[serde(default, alias = "error")]
            message: String,
        }
        let url = self.url("/api/config/reload");
        let resp = self
            .http
            .post(url)
            .send()
            .await
            .context("POST /api/config/reload request failed")?;
        let status = resp.status();
        let r: R = resp.json().await.unwrap_or(R {
            message: "failed to parse response".to_string(),
        });
        Ok((status.is_success(), r.message))
    }

    fn url(&self, path: &str) -> Url {
        self.base_url
            .join(path)
            .expect("path must be a valid URL fragment")
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.get_json_query(path, &[]).await
    }

    async fn get_json_query<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T> {
        let url = self.url(path);
        let mut req = self.http.get(url);
        for (k, v) in query {
            req = req.query(&[(k, v)]);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("GET {path} request failed"))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .with_context(|| format!("GET {path} read body failed"))?;
        if !status.is_success() {
            anyhow::bail!(
                "GET {path} returned {status}: {}",
                String::from_utf8_lossy(&bytes)
            );
        }
        serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "GET {path} response parse failed: {}",
                String::from_utf8_lossy(&bytes)
            )
        })
    }
}
