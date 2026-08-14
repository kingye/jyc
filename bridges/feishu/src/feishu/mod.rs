//! Feishu native side of the bridge: openlark WebSocket event reception,
//! API client, message formatting and event types.
//!
//! Moved from `jyc-channels/src/feishu/` (minus the adapter/matcher layers,
//! which the route table + jyc WS client replace).

pub mod client;
pub mod formatter;
pub mod types;
pub mod websocket;

use jyc_utils::helpers::sanitize_for_filesystem;

/// Local copy of `jyc_core::attachment_storage::sanitize_attachment_filename`.
///
/// The bridge must not depend on `jyc-core` (one-way dependency rule in
/// `docs/plugin-architecture.md` §13), so the small filename-sanitizer used
/// by the websocket event handler lives here instead.
pub(crate) fn sanitize_attachment_filename(filename: &str) -> String {
    let trimmed = filename.trim();
    if trimmed.is_empty() {
        return "unnamed_attachment".to_string();
    }

    // Strip any directory components (path traversal protection)
    let name = trimmed
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or(trimmed)
        .to_string();

    // Remove null bytes and other control characters
    let name: String = name.chars().filter(|c| !c.is_control()).collect();

    // Apply filesystem sanitization
    let safe = sanitize_for_filesystem(&name);

    if safe.is_empty() {
        "unnamed_attachment".to_string()
    } else {
        safe
    }
}
