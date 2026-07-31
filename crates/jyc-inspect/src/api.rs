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

pub async fn get_thread_activity(
    State(ctx): State<Arc<InspectContext>>,
    Path((channel, thread)): Path<(String, String)>,
    Query(q): Query<ThreadQuery>,
) -> Result<Json<Vec<ActivityEntry>>, ApiError> {
    let limit = q.limit.unwrap_or(180);
    let tm = ctx
        .thread_managers
        .load()
        .iter()
        .find(|tm| tm.channel_name() == channel)
        .cloned()
        .ok_or_else(|| {
            ApiError::not_found(format!("no thread manager found for channel '{channel}'"))
        })?;
    let thread_path = tm.thread_path(&thread).await.ok_or_else(|| {
        ApiError::not_found(format!(
            "thread '{thread}' not found in channel '{channel}'"
        ))
    })?;
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
    let tm = ctx
        .thread_managers
        .load()
        .iter()
        .find(|tm| tm.channel_name() == channel)
        .cloned()
        .ok_or_else(|| {
            ApiError::not_found(format!("no thread manager found for channel '{channel}'"))
        })?;
    let thread_path = tm.thread_path(&thread).await.ok_or_else(|| {
        ApiError::not_found(format!(
            "thread '{thread}' not found in channel '{channel}'"
        ))
    })?;
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
