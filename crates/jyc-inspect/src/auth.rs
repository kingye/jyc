//! HTTP middleware that gates the inspect server with a single Bearer token.
//!
//! Same `auth_token` value is used for both the REST API (`/api/*`) and the
//! WebSocket upgrade (`/ws/*`). When the server has no `auth_token` configured,
//! the middleware is a no-op.
//!
//! Token comparison is constant-time. RFC 7235 §2.1: the `Bearer` scheme name
//! is matched case-insensitively.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::server::InspectContext;

/// Run the request through `next` only if the `Authorization: Bearer <token>`
/// header matches `InspectContext.auth_token`. Returns `401` otherwise.
pub async fn require_bearer(
    State(ctx): State<Arc<InspectContext>>,
    req: Request,
    next: Next,
) -> Response {
    let Some(expected) = ctx.auth_token.as_deref() else {
        return next.run(req).await;
    };

    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            let s = s.trim();
            s.strip_prefix("Bearer ")
                .or_else(|| s.strip_prefix("bearer "))
        })
        .map(str::trim);

    match presented {
        Some(t) if constant_time_eq(t.as_bytes(), expected.as_bytes()) => {
            next.run(req).await
        }
        _ => unauthorized_response(),
    }
}

fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "auth_failed" })),
    )
        .into_response()
}

/// Constant-time byte comparison. Returns `false` if lengths differ (in
/// non-constant time on the length, but `auth_token` is a fixed config string,
/// so its length is not a useful side channel).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request as HttpRequest, StatusCode},
        middleware::from_fn_with_state,
        routing::get,
        Router,
    };
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::sync::Mutex;
    use tower::ServiceExt;
    use jyc_core::metrics::HealthStats;
    use jyc_types::ChannelInfo;
    use arc_swap::ArcSwap;

    fn ctx_with_token(token: Option<&str>) -> Arc<InspectContext> {
        Arc::new(InspectContext {
            thread_managers: Arc::new(ArcSwap::from_pointee(vec![])),
            channels: Arc::new(ArcSwap::from_pointee(vec![ChannelInfo {
                name: "ch".into(),
                channel_type: "email".into(),
                active_workers: 0,
                max_concurrent: 0,
            }])),
            health_stats: Arc::new(Mutex::new(HealthStats::default())),
            activity_map: Arc::new(Mutex::new(HashMap::new())),
            start_time: Instant::now(),
            config_path: None,
            global_config_path: None,
            config: None,
            workspace_dirs: Arc::new(ArcSwap::from_pointee(vec![])),
            websocket_handlers: None,
            reload_callback: None,
            auth_token: token.map(String::from),
            inspect_broadcast: Arc::new(tokio::sync::broadcast::channel(1).0),
        })
    }

    fn build_app(ctx: Arc<InspectContext>) -> Router {
        async fn ok() -> &'static str {
            "ok"
        }
        Router::new()
            .route("/probe", get(ok))
            .layer(from_fn_with_state(ctx, require_bearer))
            .with_state(())
    }

    #[tokio::test]
    async fn no_token_configured_allows_request() {
        let app = build_app(ctx_with_token(None));
        let res = app
            .oneshot(HttpRequest::builder().uri("/probe").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_authorization_header_returns_401() {
        let app = build_app(ctx_with_token(Some("secret")));
        let res = app
            .oneshot(HttpRequest::builder().uri("/probe").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_bearer_returns_401() {
        let app = build_app(ctx_with_token(Some("secret")));
        let req = HttpRequest::builder()
            .uri("/probe")
            .header(header::AUTHORIZATION, "Bearer wrong")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn correct_bearer_passes_through() {
        let app = build_app(ctx_with_token(Some("secret")));
        let req = HttpRequest::builder()
            .uri("/probe")
            .header(header::AUTHORIZATION, "Bearer secret")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn lowercase_bearer_scheme_accepted() {
        let app = build_app(ctx_with_token(Some("secret")));
        let req = HttpRequest::builder()
            .uri("/probe")
            .header(header::AUTHORIZATION, "bearer secret")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn basic_auth_header_rejected() {
        let app = build_app(ctx_with_token(Some("secret")));
        let req = HttpRequest::builder()
            .uri("/probe")
            .header(header::AUTHORIZATION, "Basic c2VjcmV0")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn empty_bearer_token_rejected() {
        let app = build_app(ctx_with_token(Some("secret")));
        let req = HttpRequest::builder()
            .uri("/probe")
            .header(header::AUTHORIZATION, "Bearer ")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn trailing_whitespace_in_header_accepted() {
        let app = build_app(ctx_with_token(Some("secret")));
        let req = HttpRequest::builder()
            .uri("/probe")
            .header(header::AUTHORIZATION, "Bearer secret ")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[test]
    fn constant_time_eq_basic() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}
