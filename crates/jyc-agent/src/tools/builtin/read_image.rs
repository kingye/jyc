//! `read_image` tool — load an image file or URL into the next user turn.
//!
//! Registered only when the active provider has `supports_images() == true`.
//! Pushes the loaded image onto `ToolContext.pending_images`; the agent loop
//! drains the queue after the tool batch completes and emits a synthetic
//! user-role message carrying the image content blocks.
//!
//! ## Why a side-channel instead of a tool-result block
//!
//! OpenAI-compatible Chat Completions defines `role: "tool"` content as a
//! plain string on most servers, so we cannot embed images in the tool
//! result itself. Anthropic *does* accept image blocks inside `tool_result`,
//! but emitting a unified shape (textual confirmation as the tool result,
//! images as the next user turn) keeps both providers identical.
//!
//! ## Inputs (one of `path` / `url` is required)
//!
//! - `path` (string): absolute path to a local image file. Boundary-checked
//!   against `working_dir` and any roots in `additional_read_roots`.
//! - `url` (string): http or https URL. Fetched server-side and base64-encoded.
//!
//! ## Output
//!
//! On success: a small JSON confirmation containing `loaded`, `bytes`, and
//! `media_type`. The actual image content rides on the next user turn.

use anyhow::Result;
use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{json, Value};

use super::super::{Tool, ToolContext, ToolOutput};
use crate::types::ImageSource;

/// Maximum image size accepted by the tool (in bytes). Conservative cap to
/// avoid blowing past per-request size limits on most vision providers and
/// to keep `agent-context.json` from inflating uncontrollably. The existing
/// `[attachments.inbound].max_size_per_attachment` config caps inbound
/// attachments at download time; this is the equivalent inner cap for
/// agent-driven loads.
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024; // 10 MB

/// Allowed image MIME types. Mirrors the set common to both DeepSeek-V4-Pro
/// and Anthropic Claude. `image/svg+xml` is intentionally omitted — most
/// vision models reject it and it can carry executable content.
const ALLOWED_MIME: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/jpg",
    "image/gif",
    "image/webp",
];

/// `read_image` tool.
pub struct ReadImageTool;

#[async_trait]
impl Tool for ReadImageTool {
    fn name(&self) -> &str {
        "read_image"
    }

    fn description(&self) -> &str {
        "Load an image into the next user turn so the model can see it. \
         Provide either `path` (an absolute path inside the working directory \
         or the configured attachments directory) OR `url` (an http/https URL). \
         Supported MIME types: image/png, image/jpeg, image/gif, image/webp. \
         Maximum size: 10 MB. The image content rides on the next user message; \
         the tool result itself is a small JSON confirmation."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path to a local image file. Must be inside the working directory or the configured attachments directory.",
                },
                "url": {
                    "type": "string",
                    "description": "An http(s) URL pointing to an image. Fetched and base64-inlined.",
                },
            },
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let path = input.get("path").and_then(|v| v.as_str());
        let url = input.get("url").and_then(|v| v.as_str());

        match (path, url) {
            (Some(p), None) => load_from_path(p, ctx),
            (None, Some(u)) => load_from_url(u, ctx).await,
            (Some(_), Some(_)) => Ok(ToolOutput::error(
                "Provide either `path` or `url`, not both",
            )),
            (None, None) => Ok(ToolOutput::error(
                "Missing required parameter: `path` or `url`",
            )),
        }
    }
}

/// Load an image from a local filesystem path.
fn load_from_path(path_str: &str, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
    let path = std::path::Path::new(path_str);

    if !path.is_absolute() {
        return Ok(ToolOutput::error(format!(
            "`path` must be absolute, got: {}",
            path_str
        )));
    }

    if !path.exists() {
        return Ok(ToolOutput::error(format!("Path not found: {}", path_str)));
    }
    if !path.is_file() {
        return Ok(ToolOutput::error(format!(
            "Path is not a regular file: {}",
            path_str
        )));
    }

    // Boundary check: path must lie under working_dir or one of the
    // additional_read_roots. Canonicalize both sides to defeat `..`-based
    // escapes; tolerate non-canonical roots (defensive).
    let canonical = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf());
    let working_canonical = ctx
        .working_dir
        .canonicalize()
        .unwrap_or_else(|_| ctx.working_dir.to_path_buf());

    let mut allowed = canonical.starts_with(&working_canonical);
    if !allowed {
        for root in &ctx.additional_read_roots {
            let root_canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
            if canonical.starts_with(&root_canonical) {
                allowed = true;
                break;
            }
        }
    }
    if !allowed {
        return Ok(ToolOutput::error(format!(
            "Access denied: path '{}' is outside the working directory and configured attachment roots",
            path_str
        )));
    }

    // Detect MIME from extension. Keep it simple — we don't sniff content.
    let media_type = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
    {
        Some(ref e) if e == "png" => "image/png",
        Some(ref e) if e == "jpg" || e == "jpeg" => "image/jpeg",
        Some(ref e) if e == "gif" => "image/gif",
        Some(ref e) if e == "webp" => "image/webp",
        Some(ref e) => {
            return Ok(ToolOutput::error(format!(
                "Unsupported image extension: .{}. Supported: png, jpg, jpeg, gif, webp",
                e
            )));
        }
        None => {
            return Ok(ToolOutput::error(format!(
                "File has no extension; cannot determine image type: {}",
                path_str
            )));
        }
    };

    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return Ok(ToolOutput::error(format!("Failed to read file: {e}"))),
    };

    if bytes.len() > MAX_IMAGE_BYTES {
        return Ok(ToolOutput::error(format!(
            "Image too large: {} bytes (max {} bytes)",
            bytes.len(),
            MAX_IMAGE_BYTES
        )));
    }

    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let size = bytes.len();

    push_pending(
        ctx,
        ImageSource::Base64 {
            media_type: media_type.to_string(),
            data,
        },
    );

    Ok(ToolOutput::success(
        json!({
            "loaded": path_str,
            "bytes": size,
            "media_type": media_type,
            "note": "Image queued for next user turn",
        })
        .to_string(),
    ))
}

/// Fetch an image from an http(s) URL and queue it.
///
/// Two transmission modes:
/// - For providers that natively accept remote URLs (Anthropic, OpenAI), we
///   could pass the URL through verbatim. But not every URL is reachable
///   from the provider's network and some providers strip data:URLs from
///   `image_url`. To keep behavior consistent and offline-friendly, we
///   fetch + base64 here.
async fn load_from_url(url: &str, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Ok(ToolOutput::error(format!(
            "URL must be http or https, got: {}",
            url
        )));
    }

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => return Ok(ToolOutput::error(format!("HTTP client init failed: {e}"))),
    };

    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => return Ok(ToolOutput::error(format!("Failed to GET {url}: {e}"))),
    };
    if !resp.status().is_success() {
        return Ok(ToolOutput::error(format!(
            "HTTP {} fetching {url}",
            resp.status()
        )));
    }

    // Use Content-Type for media_type — required by both Anthropic base64 and
    // OpenAI data: URL forms. Sniff is intentionally avoided (extra deps,
    // marginal value over server-declared type).
    let media_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string());

    if !ALLOWED_MIME.iter().any(|m| *m == media_type) {
        return Ok(ToolOutput::error(format!(
            "Unsupported Content-Type: {media_type}. Supported: {}",
            ALLOWED_MIME.join(", ")
        )));
    }

    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return Ok(ToolOutput::error(format!("Failed to read response body: {e}"))),
    };
    if bytes.len() > MAX_IMAGE_BYTES {
        return Ok(ToolOutput::error(format!(
            "Image too large: {} bytes (max {} bytes)",
            bytes.len(),
            MAX_IMAGE_BYTES
        )));
    }

    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let size = bytes.len();

    push_pending(
        ctx,
        ImageSource::Base64 {
            media_type: media_type.clone(),
            data,
        },
    );

    Ok(ToolOutput::success(
        json!({
            "loaded": url,
            "bytes": size,
            "media_type": media_type,
            "note": "Image queued for next user turn",
        })
        .to_string(),
    ))
}

fn push_pending(ctx: &ToolContext<'_>, src: ImageSource) {
    ctx.pending_images
        .lock()
        .expect("pending_images poisoned")
        .push(src);
}
