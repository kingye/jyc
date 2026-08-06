//! HTTP REST handlers for the inspect server.
//!
//! Route table (all under the same axum router as the WebSocket routes; both
//! share the `require_bearer` middleware mounted on the parent `Router`):
//!
//! | Method | Path                                                    | Description |
//! |--------|---------------------------------------------------------|-------------|
//! | GET    | `/api/state`                                            | Full state. |
//! | GET    | `/api/state/overview`                                   | Slim state. |
//! | GET    | `/api/threads/{channel}/{thread}/activity`              | Recent activity (with `?since=`, `?limit=`). |
//! | GET    | `/api/threads/{channel}/{thread}/chat`                  | Recent chat (with `?since=`, `?limit=`). |
//! | GET    | `/api/channels/{channel}/patterns`                      | Pattern names. |
//! | POST   | `/api/threads`                                          | Register a thread. |
//! | POST   | `/api/config/reload`                                    | Reload config. |
//!
//! Mounted WITHOUT the bearer middleware (access control is the per-thread
//! `?token=` in the URL, created by the `jyc_publish_file` tool):
//!
//! | Method | Path                                                    | Description |
//! |--------|---------------------------------------------------------|-------------|
//! | GET    | `/exchange/{channel}/{thread}/{file...}?token=`           | Agent-published file. |
//!
//! Response shape: success returns `200` + JSON body. Errors use `ApiError`
//! which carries an HTTP status and a `{"error": "..."}` JSON body.

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use jyc_core::{activity_log_store::ActivityLogStore, chat_log_store::load_recent_chat_history};
use jyc_types::{ActivityEntry, ChatMessageEntry, InspectOverview, InspectState};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::server::{
    InspectContext, filter_by_since, filter_chat_by_since, is_user_visible_activity,
};

/// Query parameters for the activity / chat endpoints.
#[derive(Debug, Default, Deserialize)]
pub struct ThreadQuery {
    pub since: Option<String>,
    pub limit: Option<usize>,
}

/// Request body for `POST /api/threads`.
#[derive(Debug, Deserialize)]
pub struct CreateThreadBody {
    pub channel: String,
    pub thread: String,
    pub path: String,
}

/// Response body for `GET /api/channels/{channel}/patterns`.
#[derive(Debug, Serialize)]
pub struct PatternsBody {
    pub patterns: Vec<String>,
}

/// Response body for `POST /api/threads`.
#[derive(Debug, Serialize)]
pub struct CreatedThread {
    pub message: String,
}

/// Response body for `POST /api/config/reload`.
#[derive(Debug, Serialize)]
pub struct ReloadBody {
    pub message: String,
}

/// Unified error type for REST handlers. Carries an HTTP status.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
        }
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
        }
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: msg.into(),
        }
    }
    pub fn unprocessable(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: msg.into(),
        }
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

pub async fn get_state(
    State(ctx): State<Arc<InspectContext>>,
) -> Result<Json<InspectState>, ApiError> {
    Ok(Json(crate::server::InspectServer::build_state(&ctx).await))
}

pub async fn get_state_overview(
    State(ctx): State<Arc<InspectContext>>,
) -> Result<Json<InspectOverview>, ApiError> {
    Ok(Json(
        crate::server::InspectServer::build_overview_state(&ctx).await,
    ))
}

/// Resolve a `(channel, thread)` pair to its thread directory path.
/// Returns a descriptive 404 error when the channel or thread is unknown.
async fn resolve_thread_path(
    ctx: &InspectContext,
    channel: &str,
    thread: &str,
) -> Result<PathBuf, ApiError> {
    let tm = ctx
        .thread_managers
        .load()
        .iter()
        .find(|tm| tm.channel_name() == channel)
        .cloned()
        .ok_or_else(|| {
            ApiError::not_found(format!("no thread manager found for channel '{channel}'"))
        })?;
    tm.thread_path(thread).await.ok_or_else(|| {
        ApiError::not_found(format!(
            "thread '{thread}' not found in channel '{channel}'"
        ))
    })
}

/// Query params for `GET /exchange/...`.
#[derive(Debug, Deserialize)]
pub struct ExchangeQuery {
    token: Option<String>,
}

/// `GET /exchange/:channel/:thread/*file_path` — serve an agent-published file.
///
/// Access control is the per-thread token in the URL (created by the
/// `jyc_publish_file` tool, rotated by `/reset`), so this route is mounted
/// WITHOUT the bearer middleware: links must work for end users who have no
/// dashboard token.
pub async fn get_exchange_file(
    State(ctx): State<Arc<InspectContext>>,
    Path((channel, thread, file_path)): Path<(String, String, String)>,
    Query(q): Query<ExchangeQuery>,
) -> Result<Response, ApiError> {
    let thread_path = resolve_thread_path(&ctx, &channel, &thread).await?;
    serve_exchange_file(&thread_path, q.token.as_deref(), &file_path).await
}

/// Token-check, then resolve and read a published file under
/// `<thread>/.jyc/exchange/`, guarding against path traversal (including
/// symlink escapes via canonicalization).
async fn serve_exchange_file(
    thread_path: &std::path::Path,
    token: Option<&str>,
    rel_path: &str,
) -> Result<Response, ApiError> {
    let jyc_dir = thread_path.join(".jyc");

    let expected = tokio::fs::read_to_string(jyc_dir.join(jyc_core::EXCHANGE_TOKEN_FILENAME))
        .await
        .map_err(|_| ApiError::forbidden("exchange access not enabled for this thread"))?;
    let expected = expected.trim();
    if expected.is_empty() || token != Some(expected) {
        return Err(ApiError::forbidden("missing or invalid token"));
    }

    let base = jyc_dir.join(jyc_core::EXCHANGE_DIR_NAME);
    let rel = std::path::Path::new(rel_path);
    if rel
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(ApiError::bad_request("invalid file path"));
    }
    let not_found = || ApiError::not_found("file not found");
    let canonical_base = base.canonicalize().map_err(|_| not_found())?;
    let canonical = base.join(rel).canonicalize().map_err(|_| not_found())?;
    if !canonical.starts_with(&canonical_base) || !canonical.is_file() {
        return Err(not_found());
    }

    let bytes = tokio::fs::read(&canonical)
        .await
        .map_err(|e| ApiError::internal(format!("failed to read file: {e}")))?;
    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            exchange_content_type(&canonical),
        )],
        bytes,
    )
        .into_response())
}

/// Content type from file extension for published files.
///
/// Duplicated from jyc-agent's `mcp_bridge::detect_content_type` (private
/// there); jyc-inspect does not depend on jyc-agent.
fn exchange_content_type(path: &std::path::Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "application/javascript",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "csv" => "text/csv",
        "json" => "application/json",
        "xml" => "application/xml",
        "zip" => "application/zip",
        "txt" | "md" | "log" => "text/plain",
        _ => "application/octet-stream",
    }
}

pub async fn get_thread_activity(
    State(ctx): State<Arc<InspectContext>>,
    Path((channel, thread)): Path<(String, String)>,
    Query(q): Query<ThreadQuery>,
) -> Result<Json<Vec<ActivityEntry>>, ApiError> {
    let limit = q.limit.unwrap_or(180);
    let thread_path = resolve_thread_path(&ctx, &channel, &thread).await?;
    let entries = ActivityLogStore::load_recent(&thread_path, limit)
        .map_err(|e| ApiError::internal(format!("failed to load activity: {e}")))?;
    let entries: Vec<ActivityEntry> = entries
        .into_iter()
        .filter(is_user_visible_activity)
        .collect();
    let entries = filter_by_since(entries, q.since.as_deref());
    Ok(Json(entries))
}

pub async fn get_thread_chat(
    State(ctx): State<Arc<InspectContext>>,
    Path((channel, thread)): Path<(String, String)>,
    Query(q): Query<ThreadQuery>,
) -> Result<Json<Vec<ChatMessageEntry>>, ApiError> {
    let limit = q.limit.unwrap_or(100);
    let thread_path = resolve_thread_path(&ctx, &channel, &thread).await?;
    let mut entries = load_recent_chat_history(&thread_path, limit);
    entries = filter_chat_by_since(entries, q.since.as_deref());
    Ok(Json(entries))
}

pub async fn get_patterns(
    State(ctx): State<Arc<InspectContext>>,
    Path(channel): Path<String>,
) -> Result<Json<PatternsBody>, ApiError> {
    let tm = ctx
        .thread_managers
        .load()
        .iter()
        .find(|tm| tm.channel_name() == channel)
        .cloned()
        .ok_or_else(|| {
            ApiError::not_found(format!("no thread manager found for channel '{channel}'"))
        })?;
    Ok(Json(PatternsBody {
        patterns: tm.pattern_names().await,
    }))
}

pub async fn post_thread(
    State(ctx): State<Arc<InspectContext>>,
    Json(body): Json<CreateThreadBody>,
) -> Result<(StatusCode, Json<CreatedThread>), ApiError> {
    if body.thread.contains("..") || body.thread.contains('/') || body.thread.contains('\\') {
        return Err(ApiError::bad_request(
            "invalid thread_name: path traversal not allowed",
        ));
    }
    let path = PathBuf::from(&body.path);
    let tm = ctx
        .thread_managers
        .load()
        .iter()
        .find(|tm| tm.channel_name() == body.channel)
        .cloned()
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "no thread manager found for channel '{}'",
                body.channel
            ))
        })?;
    tm.set_thread_path(&body.thread, path.clone())
        .await
        .map_err(|e| ApiError::internal(format!("failed to create thread: {e}")))?;
    tracing::info!(
        channel = %body.channel,
        thread = %body.thread,
        path = %path.display(),
        "Dashboard thread created via REST"
    );
    Ok((
        StatusCode::CREATED,
        Json(CreatedThread {
            message: format!("thread '{}' registered at {}", body.thread, path.display()),
        }),
    ))
}

pub async fn post_reload_config(
    State(ctx): State<Arc<InspectContext>>,
) -> Result<Json<ReloadBody>, ApiError> {
    let (config_path, config_swap) = match (&ctx.config_path, &ctx.config) {
        (Some(p), Some(c)) => (p, c),
        _ => {
            return Err(ApiError::unprocessable(
                "config reload not available (no config path)",
            ));
        }
    };
    tracing::info!(path = %config_path.display(), "Reloading configuration");
    let new_config = jyc_types::load_config_layered(ctx.global_config_path.as_deref(), config_path)
        .map_err(|e| ApiError::unprocessable(format!("failed to load config: {e:#}")))?;
    let errors = jyc_types::validation::validate_config(&new_config);
    if !errors.is_empty() {
        let msg = errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(ApiError::unprocessable(format!("validation failed: {msg}")));
    }
    config_swap.store(Arc::new(new_config));
    if let Some(cb) = &ctx.reload_callback
        && let Err(e) = cb().await
    {
        return Err(ApiError::internal(format!(
            "config reloaded, but channel reload failed: {e:#}"
        )));
    }
    Ok(Json(ReloadBody {
        message: "configuration reloaded".to_string(),
    }))
}

#[cfg(test)]
mod exchange_file_tests {
    use super::*;

    /// Seed `<tmp>/.jyc/exchange/<file>` and `<tmp>/.jyc/exchange-token`.
    fn seed(tmp: &tempfile::TempDir, file: &str, body: &[u8], token: Option<&str>) {
        let jyc = tmp.path().join(".jyc");
        let exchange = jyc.join(jyc_core::EXCHANGE_DIR_NAME);
        std::fs::create_dir_all(&exchange).unwrap();
        std::fs::write(exchange.join(file), body).unwrap();
        if let Some(t) = token {
            std::fs::write(jyc.join(jyc_core::EXCHANGE_TOKEN_FILENAME), t).unwrap();
        }
    }

    #[tokio::test]
    async fn serves_file_with_valid_token() {
        let tmp = tempfile::tempdir().unwrap();
        seed(&tmp, "notes.txt", b"hello", Some("tok123"));

        let res = serve_exchange_file(tmp.path(), Some("tok123"), "notes.txt")
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(axum::http::header::CONTENT_TYPE).unwrap(),
            "text/plain"
        );
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"hello");
    }

    #[tokio::test]
    async fn serves_nested_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".jyc/exchange/sub")).unwrap();
        std::fs::write(tmp.path().join(".jyc/exchange/sub/a.json"), b"{}").unwrap();
        std::fs::write(
            tmp.path()
                .join(".jyc")
                .join(jyc_core::EXCHANGE_TOKEN_FILENAME),
            "t",
        )
        .unwrap();

        let res = serve_exchange_file(tmp.path(), Some("t"), "sub/a.json")
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(axum::http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }

    #[tokio::test]
    async fn rejects_missing_or_wrong_token() {
        let tmp = tempfile::tempdir().unwrap();
        seed(&tmp, "f.txt", b"x", Some("right"));

        let err = serve_exchange_file(tmp.path(), None, "f.txt")
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);

        let err = serve_exchange_file(tmp.path(), Some("wrong"), "f.txt")
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn rejects_when_token_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        seed(&tmp, "f.txt", b"x", None);

        let err = serve_exchange_file(tmp.path(), Some("any"), "f.txt")
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn rejects_unknown_file() {
        let tmp = tempfile::tempdir().unwrap();
        seed(&tmp, "f.txt", b"x", Some("t"));

        let err = serve_exchange_file(tmp.path(), Some("t"), "nope.txt")
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn rejects_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        seed(&tmp, "f.txt", b"x", Some("t"));

        let err = serve_exchange_file(tmp.path(), Some("t"), "../exchange-token")
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn rejects_directory() {
        let tmp = tempfile::tempdir().unwrap();
        seed(&tmp, "f.txt", b"x", Some("t"));

        // No directory listing: the base dir itself is not a file.
        let err = serve_exchange_file(tmp.path(), Some("t"), "")
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }
}
