//! GitHub-specific types for API responses and conversions.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct GitHubIssueComment {
    pub id: i64,
    pub body: String,
    pub user: GitHubUser,
    pub created_at: String,
    pub updated_at: String,
    pub html_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct GitHubIssue {
    pub id: i64,
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub user: GitHubUser,
    pub state: String,
    pub labels: Vec<GitHubLabel>,
    pub created_at: String,
    pub updated_at: String,
    pub html_url: String,
    #[serde(default)]
    pub pull_request: Option<GitHubPullRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct GitHubPullRequest {
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct GitHubUser {
    pub login: String,
    pub id: i64,
    pub html_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct GitHubLabel {
    pub name: String,
    pub color: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct GitHubRepository {
    pub id: i64,
    pub name: String,
    pub full_name: String,
    pub owner: GitHubUser,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubEvent {
    pub action: String,
    pub issue: Option<GitHubIssue>,
    pub comment: Option<GitHubIssueComment>,
    pub repository: Option<GitHubRepository>,
    pub sender: Option<GitHubUser>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateIssueCommentRequest {
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateIssueCommentResponse {
    pub id: i64,
    pub html_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_issue_comment_deserialize() {
        let json = r#"{
            "id": 123,
            "body": "Test comment",
            "user": {"login": "testuser", "id": 456, "html_url": "https://github.com/testuser"},
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z",
            "html_url": "https://github.com/test/repo/issues/1#comment_123"
        }"#;

        let comment: GitHubIssueComment = serde_json::from_str(json).unwrap();
        assert_eq!(comment.id, 123);
        assert_eq!(comment.body, "Test comment");
        assert_eq!(comment.user.login, "testuser");
    }

    #[test]
    fn test_issue_deserialize() {
        let json = r#"{
            "id": 1,
            "number": 42,
            "title": "Test Issue",
            "body": "Issue body",
            "user": {"login": "testuser", "id": 456, "html_url": "https://github.com/testuser"},
            "state": "open",
            "labels": [{"name": "bug", "color": "ff0000"}],
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z",
            "html_url": "https://github.com/test/repo/issues/42"
        }"#;

        let issue: GitHubIssue = serde_json::from_str(json).unwrap();
        assert_eq!(issue.number, 42);
        assert_eq!(issue.title, "Test Issue");
        assert_eq!(issue.labels.len(), 1);
        assert_eq!(issue.labels[0].name, "bug");
    }
}
