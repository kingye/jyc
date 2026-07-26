/// Adaptive web UI for JYC dashboard.
///
/// Provides static HTML/CSS/JS content embedded at compile time via `include_str!`.
/// The actual HTTP routing and handler setup lives in `jyc-inspect`.
///
/// # Content
///
/// - `INDEX_HTML` — main dashboard page (channel + thread list + chat pane)
/// - `THREAD_HTML` — thread chat page (separate view for thread details)
/// - `NOT_FOUND_HTML` — 404 error page
/// - `STYLE_CSS` — responsive CSS (desktop + mobile via media queries)
/// - `APP_JS` — vanilla JS (~100 lines): state fetching, WebSocket chat,
///   `POST /inject_message`, login dialog, token localStorage, 401 handling

pub static INDEX_HTML: &str = include_str!("../assets/index.html");
pub static THREAD_HTML: &str = include_str!("../assets/thread.html");
pub static NOT_FOUND_HTML: &str = include_str!("../assets/404.html");
pub static STYLE_CSS: &str = include_str!("../assets/style.css");
pub static APP_JS: &str = include_str!("../assets/app.js");
