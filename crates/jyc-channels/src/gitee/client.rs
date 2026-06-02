//! Gitee API client.
//!
//! Minimal client for polling Gitee events: issues, pull requests, comments.
//! Uses reqwest with the Gitee API v5.

use anyhow::{Context, Result};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;

use jyc_types::GiteeConfig;
use jyc_utils::helpers::truncate_str;

/// Gitee API client.
pub struct GiteeClient {
    client: reqwest::Client,
    owner: String,
    repo: String,
    api_url: String,
}

/// Gitee user (minimal fields).
#[derive(Debug, Deserialize)]
pub struct GiteeUser {
    pub login: String,
    pub name: Option<String>,
}

/// Gitee label.
#[derive(Debug, Clone, Deserialize)]
pub struct GiteeLabel {
    pub name: String,
}

/// Gitee issue or pull request (from /issues endpoint).
#[derive(Debug, Deserialize)]
pub struct GiteeIssue {
    /// Issue/PR number — Gitee uses string identifiers (e.g. "IJROW7"), not integers.
    pub number: String,
    pub title: String,
    pub state: String,
    pub user: GiteeUser,
    pub labels: Vec<GiteeLabel>,
    /// Single assignee (Gitee uses `assignee`, not `assignees` array).
    pub assignee: Option<GiteeUser>,
    pub created_at: String,
    pub updated_at: String,
}

/// Gitee comment target (issue or PR reference).
#[derive(Debug, Deserialize)]
pub struct GiteeCommentTarget {
    pub issue: Option<GiteeCommentTargetIssue>,
    pub pull_request: Option<GiteeCommentTargetIssue>,
}

#[derive(Debug, Deserialize)]
pub struct GiteeCommentTargetIssue {
    pub number: String,
}

/// Gitee comment (on issue or PR).
#[derive(Debug, Deserialize)]
pub struct GiteeComment {
    pub id: u64,
    pub user: GiteeUser,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
    pub target: Option<GiteeCommentTarget>,
}

impl GiteeComment {
    /// Extract the issue/PR number from the target field.
    /// Gitee API v5 returns issue_url as null; the number is in target.issue.number.
    pub fn issue_number(&self) -> Option<String> {
        self.target
            .as_ref()
            .and_then(|t| t.issue.as_ref().map(|i| i.number.clone()))
            .or_else(|| {
                self.target
                    .as_ref()
                    .and_then(|t| t.pull_request.as_ref().map(|pr| pr.number.clone()))
            })
    }

    /// Returns true if this comment is on a pull request (not an issue).
    pub fn is_pull_request_comment(&self) -> bool {
        self.target
            .as_ref()
            .map(|t| t.pull_request.is_some())
            .unwrap_or(false)
    }
}

/// Gitee pull request (from /pulls endpoint).
#[derive(Debug, Deserialize)]
pub struct GiteePullRequest {
    /// PR number — Gitee uses string identifiers (e.g. "IJROW7"), not integers.
    pub number: String,
    pub title: String,
    pub state: String,
    pub user: GiteeUser,
    pub labels: Vec<GiteeLabel>,
    pub assignees: Vec<GiteeUser>,
    pub head: GiteePullRequestHead,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Deserialize)]
pub struct GiteePullRequestHead {
    pub sha: String,
    pub label: String,
    pub ref_field: String,
}

/// Gitee build status (from /commits/{sha}/build_status endpoint).
#[derive(Debug, Clone, Deserialize)]
pub struct GiteeBuildStatus {
    pub sha: String,
    pub state: String,
    pub statuses: Vec<GiteeBuildStatusItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GiteeBuildStatusItem {
    pub context: String,
    pub state: String,
    pub description: Option<String>,
    pub target_url: Option<String>,
}

impl GiteeClient {
    /// Create a new Gitee API client.
    pub fn new(config: &GiteeConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("token {}", config.token))
                .context("Invalid Gitee token format")?,
        );
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static("jyc-gitee-channel/1.0"),
        );

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("Failed to create HTTP client")?;

        // Remove trailing slash from api_url if present
        let api_url = config.api_url.trim_end_matches('/').to_string();

        Ok(Self {
            client,
            owner: config.owner.clone(),
            repo: config.repo.clone(),
            api_url,
        })
    }

    /// Get the authenticated user (bot identity).
    pub async fn get_authenticated_user(&self) -> Result<GiteeUser> {
        let url = format!("{}/user", self.api_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch authenticated user")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "GET /user failed: {} — {}",
                status,
                truncate_str(&body, 200)
            );
        }

        resp.json::<GiteeUser>()
            .await
            .context("Failed to parse user response")
    }

    /// List ALL open issues and PRs (paginated, no `since` filter).
    pub async fn list_all_open_issues(&self) -> Result<Vec<GiteeIssue>> {
        let mut all_issues = Vec::new();
        let max_pages = 5;

        for page in 1..=max_pages {
            let url = format!(
                "{}/repos/{}/{}/issues?state=open&sort=updated&direction=desc&per_page=100&page={}",
                self.api_url, self.owner, self.repo, page
            );

            let resp = self
                .client
                .get(&url)
                .send()
                .await
                .context("Failed to fetch all open issues")?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!(
                    "GET all open issues failed: {} — {}",
                    status,
                    truncate_str(&body, 200)
                );
            }

            let issues: Vec<GiteeIssue> = resp
                .json()
                .await
                .context("Failed to parse all open issues response")?;

            let count = issues.len();
            all_issues.extend(issues);

            // If we got fewer than 100, we've reached the last page
            if count < 100 {
                break;
            }
        }

        Ok(all_issues)
    }

    /// List comments on issues/PRs since a given timestamp.
    pub async fn list_comments_since(&self, since: &str) -> Result<Vec<GiteeComment>> {
        let url = format!(
            "{}/repos/{}/{}/issues/comments?since={}&sort=updated&direction=asc&per_page=100",
            self.api_url, self.owner, self.repo, since
        );

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch comments")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "GET comments failed: {} — {}",
                status,
                truncate_str(&body, 200)
            );
        }

        resp.json::<Vec<GiteeComment>>()
            .await
            .context("Failed to parse comments response")
    }

    /// List recently closed issues/PRs (for close event detection).
    pub async fn list_closed_since(&self, since: &str) -> Result<Vec<GiteeIssue>> {
        let url = format!(
            "{}/repos/{}/{}/issues?state=closed&since={}&sort=updated&direction=asc&per_page=100",
            self.api_url, self.owner, self.repo, since
        );

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch closed issues")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "GET closed issues failed: {} — {}",
                status,
                truncate_str(&body, 200)
            );
        }

        resp.json::<Vec<GiteeIssue>>()
            .await
            .context("Failed to parse closed issues response")
    }

    /// List open pull requests.
    pub async fn list_open_pulls(&self) -> Result<Vec<GiteePullRequest>> {
        let url = format!(
            "{}/repos/{}/{}/pulls?state=open&sort=updated&direction=desc&per_page=100",
            self.api_url, self.owner, self.repo
        );

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch open pull requests")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "GET open pulls failed: {} — {}",
                status,
                truncate_str(&body, 200)
            );
        }

        resp.json::<Vec<GiteePullRequest>>()
            .await
            .context("Failed to parse open pulls response")
    }

    /// Get build status for a commit SHA.
    pub async fn get_build_status(&self, sha: &str) -> Result<GiteeBuildStatus> {
        let url = format!(
            "{}/repos/{}/{}/commits/{}/build_status",
            self.api_url, self.owner, self.repo, sha
        );

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch build status")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "GET build status failed: {} — {}",
                status,
                truncate_str(&body, 200)
            );
        }

        resp.json::<GiteeBuildStatus>()
            .await
            .context("Failed to parse build status response")
    }

    /// Create a comment on an issue or PR.
    pub async fn create_comment(&self, number: &str, body: &str) -> Result<u64> {
        let url = format!(
            "{}/repos/{}/{}/issues/{}/comments",
            self.api_url, self.owner, self.repo, number
        );

        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await
            .context("Failed to create comment")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let resp_body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "POST comment failed: {} — {}",
                status,
                truncate_str(&resp_body, 200)
            );
        }

        let comment: GiteeComment = resp
            .json()
            .await
            .context("Failed to parse comment response")?;

        Ok(comment.id)
    }

    /// Get PR head SHA for CI status polling.
    pub async fn get_pr_head_sha(&self, pr_number: &str) -> Result<String> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{}",
            self.api_url, self.owner, self.repo, pr_number
        );

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch PR detail")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "GET PR detail failed: {} — {}",
                status,
                truncate_str(&body, 200)
            );
        }

        let pr: GiteePullRequest = resp
            .json()
            .await
            .context("Failed to parse PR detail response")?;

        Ok(pr.head.sha)
    }
}
