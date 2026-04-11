//! GitHub API client for polling and sending messages.

use anyhow::{Context, Result};
use reqwest::Client;
use std::time::Duration;

use super::config::GitHubConfig;
use super::types::{
    CreateIssueCommentRequest, CreateIssueCommentResponse, GitHubIssue, GitHubIssueComment,
};

pub struct GitHubClient {
    config: GitHubConfig,
    http_client: Client,
}

impl GitHubClient {
    pub fn new(config: GitHubConfig) -> Self {
        let http_client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            config,
            http_client,
        }
    }

    fn base_url(&self) -> String {
        format!(
            "https://api.github.com/repos/{}/{}",
            self.config.owner, self.config.repo
        )
    }

    fn headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::ACCEPT,
            "application/vnd.github.v3+json".parse().unwrap(),
        );
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", self.config.token).parse().unwrap(),
        );
        headers.insert(
            reqwest::header::USER_AGENT,
            "jyc-bot".parse().unwrap(),
        );
        headers
    }

    pub async fn get_issue_comments(
        &self,
        since: Option<&str>,
    ) -> Result<Vec<GitHubIssueComment>> {
        let mut url = format!("{}/issues/comments", self.base_url());
        if let Some(since) = since {
            url = format!("{}?since={}", url, since);
        }

        tracing::debug!(url = %url, "Fetching GitHub issue comments");

        let response = self
            .http_client
            .get(&url)
            .headers(self.headers())
            .send()
            .await
            .context("Failed to fetch issue comments")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            anyhow::bail!("GitHub API error ({}): {} - URL: {}", status, text, url);
        }

        let comments: Vec<GitHubIssueComment> = response
            .json()
            .await
            .context("Failed to parse issue comments")?;

        Ok(comments)
    }

    pub async fn get_issues(&self, since: Option<&str>, state: Option<&str>) -> Result<Vec<GitHubIssue>> {
        let mut url = format!("{}/issues", self.base_url());
        let mut params = Vec::new();

        if let Some(since) = since {
            params.push(format!("since={}", since));
        }
        if let Some(state) = state {
            params.push(format!("state={}", state));
        } else {
            params.push("state=all".to_string());
        }

        if !params.is_empty() {
            url = format!("{}?{}", url, params.join("&"));
        }

        let response = self
            .http_client
            .get(&url)
            .headers(self.headers())
            .send()
            .await
            .context("Failed to fetch issues")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            anyhow::bail!("GitHub API error: {} - {}", status, text);
        }

        let issues: Vec<GitHubIssue> = response
            .json()
            .await
            .context("Failed to parse issues")?;

        Ok(issues)
    }

    pub async fn create_issue_comment(
        &self,
        issue_number: i64,
        body: &str,
    ) -> Result<CreateIssueCommentResponse> {
        let url = format!("{}/issues/{}/comments", self.base_url(), issue_number);

        let request = CreateIssueCommentRequest {
            body: body.to_string(),
        };

        let response = self
            .http_client
            .post(&url)
            .headers(self.headers())
            .json(&request)
            .send()
            .await
            .context("Failed to create issue comment")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            anyhow::bail!("GitHub API error: {} - {}", status, text);
        }

        let result: CreateIssueCommentResponse = response
            .json()
            .await
            .context("Failed to parse create comment response")?;

        Ok(result)
    }

    pub async fn get_rate_limit_status(&self) -> Result<GitHubRateLimitResponse> {
        let url = "https://api.github.com/rate_limit".to_string();

        let response = self
            .http_client
            .get(&url)
            .headers(self.headers())
            .send()
            .await
            .context("Failed to fetch rate limit status")?;

        let result: GitHubRateLimitResponse = response
            .json()
            .await
            .context("Failed to parse rate limit response")?;

        Ok(result)
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct GitHubRateLimitResponse {
    pub resources: RateLimitResources,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RateLimitResources {
    pub core: RateLimit,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RateLimit {
    pub limit: i64,
    pub remaining: i64,
    pub reset: i64,
    pub used: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_creation() {
        let config = GitHubConfig {
            owner: "testorg".to_string(),
            repo: "testrepo".to_string(),
            token: "test_token".to_string(),
            ..Default::default()
        };
        let client = GitHubClient::new(config);
        assert_eq!(client.base_url(), "https://api.github.com/repos/testorg/testrepo");
    }
}