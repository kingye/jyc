use crate::monitor_api::state::ApiState;
use axum::{
    routing::get,
    Router,
};
use std::net::SocketAddr;
use tower_http::cors::{Any, CorsLayer};
use tracing;

pub async fn start_api_server(bind: String, state: ApiState) {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/api/channels", get(crate::monitor_api::handlers::get_channels))
        .route("/api/threads", get(crate::monitor_api::handlers::get_threads))
        .route("/api/threads/:name", get(crate::monitor_api::handlers::get_thread))
        .route("/api/stats", get(crate::monitor_api::handlers::get_stats))
        .route("/api/events", get(crate::monitor_api::handlers::get_events))
        .layer(cors)
        .with_state(state);

    let addr: SocketAddr = bind.parse().expect("Invalid bind address");
    let listener = tokio::net::TcpListener::bind(addr).await.expect("Failed to bind");
    
    tracing::info!(addr = %addr, "Monitor API server starting");
    
    axum::serve(listener, app)
        .await
        .expect("Monitor API server error");
}