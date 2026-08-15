//! Axum routes + WebSocket upgrade handlers for the inspect server.
//!
//! Extracted from the monolithic `server.rs`.

use std::sync::Arc;

use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path as AxPath, State as AxState};
use axum::middleware::from_fn_with_state;
use axum::response::IntoResponse;
use axum::routing::{get, post};

use super::{InspectContext, InspectServer, WsRoute};

pub fn build_router(context: Arc<InspectContext>) -> Router {
    use crate::api;

    let authed = Router::new()
        .route("/api/state", get(api::get_state))
        .route("/api/state/overview", get(api::get_state_overview))
        .route(
            "/api/topics/{channel}/{topic}/activity",
            get(api::get_topic_activity),
        )
        .route(
            "/api/topics/{channel}/{topic}/chat",
            get(api::get_topic_chat),
        )
        .route("/api/channels/{channel}/patterns", get(api::get_patterns))
        .route("/api/topics", post(api::post_topic))
        .route("/api/config/reload", post(api::post_reload_config))
        .route("/ws", get(ws_bare))
        .route("/ws/{channel}", get(ws_channel))
        .route("/ws/{channel}/{topic}", get(ws_topic))
        .layer(from_fn_with_state(
            context.clone(),
            crate::auth::require_bearer,
        ));

    Router::new()
        .route(
            "/exchange/{channel}/{topic}/{*file_path}",
            get(api::get_exchange_file),
        )
        .merge(authed)
        .with_state(context)
}

/// WS upgrade for `GET /ws` — use the first registered handler.
async fn ws_bare(
    AxState(ctx): AxState<Arc<InspectContext>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws_upgrade_for_route(ws, ctx, WsRoute::Bare).await
}

/// WS upgrade for `GET /ws/<channel>` — adhoc topic on a websocket channel.
async fn ws_channel(
    AxState(ctx): AxState<Arc<InspectContext>>,
    ws: WebSocketUpgrade,
    AxPath(channel): AxPath<String>,
) -> impl IntoResponse {
    ws_upgrade_for_route(ws, ctx, WsRoute::Channel(channel)).await
}

/// WS upgrade for `GET /ws/<channel>/<topic>` — proxy to a topic.
async fn ws_topic(
    AxState(ctx): AxState<Arc<InspectContext>>,
    ws: WebSocketUpgrade,
    AxPath((channel, name)): AxPath<(String, String)>,
) -> impl IntoResponse {
    ws_upgrade_for_route(ws, ctx, WsRoute::Topic { channel, name }).await
}

async fn ws_upgrade_for_route(
    ws: WebSocketUpgrade,
    ctx: Arc<InspectContext>,
    route: WsRoute,
) -> axum::response::Response {
    use axum::extract::ws::Message;
    // Extract the URL-scoped topic name (`/ws/<channel>/<topic>`) before
    // `resolve_ws_handler` consumes `route`. Handlers that bind the topic
    // from the URL (e.g. `WebsocketInboundAdapter` wrapped in
    // `ScopedWsHandler`) rely on this to route inbound chat messages
    // without requiring a `topic` field in the payload.
    let scoped_topic: Option<String> = match &route {
        WsRoute::Topic { name, .. } => Some(name.clone()),
        _ => None,
    };
    match InspectServer::resolve_ws_handler(&ctx, route) {
        Ok(handler) => {
            let addr = std::net::SocketAddr::from(([0, 0, 0, 0], 0));
            ws.on_upgrade(move |socket| async move {
                if let Err(e) = handler.handle(socket, addr, scoped_topic.as_deref()).await {
                    tracing::debug!(error = %e, "ws handler exited with error");
                }
            })
        }
        Err(e) => {
            tracing::debug!(error = %e, "ws route resolution failed");
            ws.on_upgrade(|mut socket| async move {
                let _ = socket.send(Message::Close(None)).await;
            })
        }
    }
}
