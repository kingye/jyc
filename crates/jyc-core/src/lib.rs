pub mod activity_log_store;
pub mod agent;
pub mod attachment_storage;
pub mod billing_log_store;
pub mod channel_orchestrator;
pub mod chat_log_store;
pub mod command;
pub mod duration;
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
pub mod topic_event;
pub mod topic_event_bus;
pub mod topic_manager;
pub mod topic_path;

/// Directory under `<topic>/.jyc/` holding agent-published files that are
/// served over HTTP at `/exchange/<channel>/<topic>/<name>`.
pub const EXCHANGE_DIR_NAME: &str = "exchange";

/// Per-topic token file under `<topic>/.jyc/` guarding `/exchange/...` URLs.
/// Created on first `jyc_publish_file` call; deleted by `/reset` so links die.
pub const EXCHANGE_TOKEN_FILENAME: &str = "exchange-token";

/// Build the shareable URL for a published file.
///
/// Single source of truth for the link format so the `jyc_publish_file` tool
/// and the `/exchange` command can never disagree. `base` is an opaque prefix
/// (scheme + host, optionally port and subpath) with no trailing slash — see
/// `InspectConfig::effective_base_url`.
pub fn exchange_url(base: &str, channel: &str, topic: &str, name: &str, token: &str) -> String {
    format!(
        "{base}/exchange/{}/{}/{}?token={token}",
        url_encode_segment(channel),
        url_encode_segment(topic),
        url_encode_segment(name),
    )
}

/// Percent-encode a single URL path segment (RFC 3986 unreserved set kept,
/// everything else %-encoded) so names with spaces, `#`, `%`, etc. produce
/// working links. The axum `Path` extractor decodes it on serve.
pub fn url_encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod exchange_url_tests {
    use super::*;

    #[test]
    fn plain_names_are_untouched() {
        assert_eq!(
            exchange_url(
                "https://x.example.com",
                "email",
                "weather",
                "report.pdf",
                "tok"
            ),
            "https://x.example.com/exchange/email/weather/report.pdf?token=tok"
        );
    }

    /// Spaces and `#` would truncate or mangle the URL if not encoded.
    #[test]
    fn special_chars_are_percent_encoded() {
        assert_eq!(
            exchange_url(
                "https://x.example.com",
                "email",
                "weather",
                "a b#c.pdf",
                "tok"
            ),
            "https://x.example.com/exchange/email/weather/a%20b%23c.pdf?token=tok"
        );
    }

    /// Channel and topic names reach the URL too, so they need encoding.
    #[test]
    fn channel_and_topic_are_encoded() {
        assert_eq!(
            exchange_url("http://h:1", "my chan", "issue #7", "a.txt", "tok"),
            "http://h:1/exchange/my%20chan/issue%20%237/a.txt?token=tok"
        );
    }
}
