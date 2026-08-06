pub mod activity_log_store;
pub mod agent;
pub mod attachment_storage;
pub mod billing_log_store;
pub mod channel_orchestrator;
pub mod chat_log_store;
pub mod command;
pub mod email_parser;
pub mod job_store;
pub mod message_router;
pub mod message_storage;
pub mod metrics;
pub mod pending_delivery;
pub mod security;
pub mod session_state;
pub mod state_manager;
pub mod static_agent;
pub mod template_dirs;
pub mod template_utils;
pub mod thread_event;
pub mod thread_event_bus;
pub mod thread_json;
pub mod thread_manager;
pub mod thread_path;

/// Directory under `<thread>/.jyc/` holding agent-published files that are
/// served over HTTP at `/public/<channel>/<thread>/<name>`.
pub const PUBLIC_DIR_NAME: &str = "public";

/// Per-thread token file under `<thread>/.jyc/` guarding `/public/...` URLs.
/// Created on first `jyc_publish_file` call; deleted by `/reset` so links die.
pub const PUBLIC_TOKEN_FILENAME: &str = "public-token";
