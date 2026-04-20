use anyhow::Result;
use axum::{
    routing::get,
    Router,
};
use clap::Args;
use std::net::SocketAddr;
use tower_http::cors::{Any, CorsLayer};

const PLACEHOLDER_HTML: &str = r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <title>JYC Dashboard</title>
    <style>
        body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; margin: 40px; background: #f5f5f5; }
        .container { max-width: 800px; margin: 0 auto; background: white; padding: 40px; border-radius: 8px; box-shadow: 0 2px 10px rgba(0,0,0,0.1); }
        h1 { color: #333; }
        .message { color: #666; margin: 20px 0; }
        code { background: #f0f0f0; padding: 2px 6px; border-radius: 4px; }
    </style>
</head>
<body>
    <div class="container">
        <h1>JYC Dashboard</h1>
        <div class="message">
            <p>The dashboard UI has not been built yet.</p>
            <p>To build the dashboard, run:</p>
            <pre><code>cd ui && npm install && npm run build</code></pre>
            <p>Then restart the dashboard server.</p>
        </div>
    </div>
</body>
</html>
"#;

#[derive(Args, Debug)]
pub struct DashboardArgs {
    /// Monitor API URL (default: http://127.0.0.1:9090)
    #[arg(long, default_value = "http://127.0.0.1:9090")]
    pub api_url: String,

    /// Bind address for dashboard server (default: 0.0.0.0:8080)
    #[arg(long, default_value = "0.0.0.0:8080")]
    pub bind: String,
}

pub async fn run(args: &DashboardArgs) -> Result<()> {
    tracing::info!(api_url = %args.api_url, bind = %args.bind, "Starting dashboard");

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/", get(serve_index))
        .route("/index.html", get(serve_index))
        .layer(cors);

    let addr: SocketAddr = args.bind.parse().expect("Invalid bind address");
    let listener = tokio::net::TcpListener::bind(addr).await.expect("Failed to bind");
    
    tracing::info!(addr = %addr, "Dashboard server starting");
    
    axum::serve(listener, app)
        .await
        .expect("Dashboard server error");
    
    Ok(())
}

async fn serve_index() -> axum::response::Html<&'static str> {
    axum::response::Html(PLACEHOLDER_HTML)
}