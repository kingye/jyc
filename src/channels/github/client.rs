//! GitHub API client.
//!
//! Minimal client for polling GitHub events: issues, comments, reviews.
//! Uses reqwest with the GitHub REST API v3.

use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use serde::Deserialize;

use super::config::GithubConfig;
use crate::utils::helpers::truncate_str;

/// GitHub API client.
pub struct GithubClient {
    client: reqwest::Client,
    owner: String,
    repo: String,
    api_url: String,
}

/// GitHub user (minimal fields).
#[derive(Debug, Deserialize)]
pub struct GithubUser {
    pub login: String,
}

/// GitHub label.
#[derive(Debug, Clone, Deserialize)]
pub struct GithubLabel {
    pub name: String,
}

/// GitHub issue or pull request (from /issues endpoint).
#[derive(Debug, Deserialize)]
pub struct GithubIssue {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub user: GithubUser,
    pub labels: Vec<GithubLabel>,
    pub assignees: Vec<GithubUser>,
    /// Present only for pull requests
    pub pull_request: Option<GithubPullRequestRef>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
}

impl GithubIssue {
    /// Returns true if this is a pull request (not an issue).
    pub fn is_pull_request(&self) -> bool {
        self.pull_request.is_some()
    }
}

/// Minimal pull_request reference in issue response.
#[derive(Debug, Deserialize)]
pub struct GithubPullRequestRef {
    pub url: Option<String>,
    pub merged_at: Option<String>,
}

/// GitHub comment (on issue or PR).
#[derive(Debug, Deserialize)]
pub struct GithubComment {
    pub id: u64,
    pub user: GithubUser,
    pub body: String,
    pub issue_url: String,
    pub created_at: String,
    pub updated_at: String,
}

impl GithubComment {
    /// Extract the issue/PR number from the issue_url.
    /// issue_url looks like: https://api.github.com/repos/{owner}/{repo}/issues/{number}
    pub fn issue_number(&self) -> Option<u64> {
        self.issue_url
            .rsplit('/')
            .next()
            .and_then(|s| s.parse().ok())
    }
}

impl GithubClient {
    /// Create a new GitHub API client.
    pub fn new(config: &GithubConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", config.token))
                .context("Invalid GitHub token format")?,
        );
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static("jyc-github-channel/1.0"),
        );
        headers.insert(
            "X-GitHub-Api-Version",
            HeaderValue::from_static("2022-11-28"),
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
    pub async fn get_authenticated_user(&self) -> Result<GithubUser> {
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
            anyhow::bail!("GET /user failed: {} — {}", status, truncate_str(&body, 200));
        }

        resp.json::<GithubUser>()
            .await
            .context("Failed to parse user response")
    }

    /// List ALL open issues and PRs (paginated, no `since` filter).
    ///
    /// Unlike `list_issues_since`, this returns the complete set of open issues
    /// regardless of when they were last updated. Used for cache comparison
    /// to reliably detect close events.
    ///
    /// Fetches up to 500 issues (5 pages × 100). Repos with more than 500
    /// open issues will miss some, but this covers the vast majority of cases.
    pub async fn list_all_open_issues(&self) -> Result<Vec<GithubIssue>> {
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
                anyhow::bail!("GET all open issues failed: {} — {}", status, truncate_str(&body, 200));
            }

            let issues: Vec<GithubIssue> = resp
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
    ///
    /// Returns comments across all issues/PRs in the repo.
    pub async fn list_comments_since(&self, since: &str) -> Result<Vec<GithubComment>> {
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
            anyhow::bail!("GET comments failed: {} — {}", status, truncate_str(&body, 200));
        }

        resp.json::<Vec<GithubComment>>()
            .await
            .context("Failed to parse comments response")
    }

    /// List recently closed issues/PRs (for close event detection).
    pub async fn list_closed_since(&self, since: &str) -> Result<Vec<GithubIssue>> {
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
            anyhow::bail!("GET closed issues failed: {} — {}", status, truncate_str(&body, 200));
        }

        resp.json::<Vec<GithubIssue>>()
            .await
            .context("Failed to parse closed issues response")
    }

    /// Post a comment on an issue or PR.
    ///
    /// GitHub API uses the /issues/ endpoint for both issue and PR comments.
    /// Returns the comment ID.
    pub async fn create_comment(&self, number: u64, body: &str) -> Result<u64> {
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
            .context("Failed to post comment")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("POST comment failed: {} — {}", status, truncate_str(&body, 200));
        }

        #[derive(Deserialize)]
        struct CommentResponse {
            id: u64,
        }

        let comment: CommentResponse = resp
            .json()
            .await
            .context("Failed to parse comment response")?;

        Ok(comment.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_comment_issue_number() {
        let comment = GithubComment {
            id: 1,
            user: GithubUser { login: "test".to_string() },
            body: "test comment".to_string(),
            issue_url: "https://api.github.com/repos/kingye/jyc/issues/42".to_string(),
            created_at: "2026-04-15T10:00:00Z".to_string(),
            updated_at: "2026-04-15T10:00:00Z".to_string(),
        };
        assert_eq!(comment.issue_number(), Some(42));
    }

    #[test]
    fn test_issue_is_pull_request() {
        let issue = GithubIssue {
            number: 42,
            title: "Test".to_string(),
            state: "open".to_string(),
            user: GithubUser { login: "test".to_string() },
            labels: vec![],
            assignees: vec![],
            pull_request: None,
            created_at: "2026-04-15T10:00:00Z".to_string(),
            updated_at: "2026-04-15T10:00:00Z".to_string(),
            closed_at: None,
        };
        assert!(!issue.is_pull_request());

        let pr = GithubIssue {
            pull_request: Some(GithubPullRequestRef {
                url: Some("https://api.github.com/repos/kingye/jyc/pulls/43".to_string()),
                merged_at: None,
            }),
            ..issue
        };
        assert!(pr.is_pull_request());
    }
}
