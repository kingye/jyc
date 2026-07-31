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
            .build()
            .expect("reqwest client must build");
        Self { base_url, http }
    }

    /// Full state snapshot.
    pub async fn get_state(&mut self) -> Result<InspectState> {
        self.get_json("/api/state").await
    }

    /// Slim state snapshot (no per-thread activity/messages).
    pub async fn get_overview(&mut self) -> Result<InspectOverview> {
        self.get_json("/api/state/overview").await
    }

    /// Recent activity entries for a thread. Filters internal entries
    /// server-side.
    pub async fn get_thread_activity(
        &mut self,
        channel: &str,
        thread: &str,
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
        let path = format!("/api/threads/{}/{}/activity", channel, thread);
        self.get_json_query(&path, &q).await
    }

    /// Recent chat messages for a thread.
    pub async fn get_thread_chat(
        &mut self,
        channel: &str,
        thread: &str,
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
        let path = format!("/api/threads/{}/{}/chat", channel, thread);
        self.get_json_query(&path, &q).await
    }

    /// Pattern names configured for a channel.
    pub async fn list_patterns(&mut self, channel: &str) -> Result<Vec<String>> {
        #[derive(serde::Deserialize)]
        struct R {
            patterns: Vec<String>,
        }
        let r: R = self
            .get_json(&format!("/api/channels/{}/patterns", channel))
            .await?;
        Ok(r.patterns)
    }

    /// Register a new ad-hoc thread. Returns `(success, message)`.
    pub async fn create_thread(
        &mut self,
        channel: &str,
        thread: &str,
        path: &str,
    ) -> Result<(bool, String)> {
        #[derive(serde::Serialize)]
        struct B<'a> {
            channel: &'a str,
            thread: &'a str,
            path: &'a str,
        }
        // `created_thread` endpoint returns 201 + {message: ...}; we use
        // the message field for both success and failure.
        let url = self.url("/api/threads");
        let resp = self
            .http
            .post(url)
            .json(&B {
                channel,
                thread,
                path,
            })
            .send()
            .await
            .context("POST /api/threads request failed")?;
        let status = resp.status();
        #[derive(serde::Deserialize, Default)]
        struct R {
            #[serde(default)]
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

    /// Reload config. Returns `(success, message)`.
    pub async fn reload_config(&mut self) -> Result<(bool, String)> {
        #[derive(serde::Deserialize)]
        struct R {
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

    async fn get_json<T: DeserializeOwned>(&mut self, path: &str) -> Result<T> {
        self.get_json_query(path, &[]).await
    }

    async fn get_json_query<T: DeserializeOwned>(
        &mut self,
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
