//! `read_image` tool — the **single entry point** for all image analysis.
//!
//! All images, whether loaded by the LLM on demand or auto-injected from
//! inbound attachments, go through `read_image` for processing. The tool
//! operates in one of two modes:
//!
//! 1. **Image injection mode** (when `supports_images = true`):
//!    Pushes the loaded image onto `ToolContext.pending_images`; the agent loop
//!    drains the queue after the tool batch completes and emits a synthetic
//!    user-role message carrying the image content blocks. This is the original
//!    behavior for multimodal models.
//!
//! 2. **Vision fallback mode** (when `supports_images = false` + VisionClient):
//!    Loads the image, sends it to an external vision model (e.g., DeepSeek-OCR)
//!    via the VisionClient, and returns the textual analysis directly as the
//!    tool output. No pending_images queue is used. This mode is only active
//!    when the current message's pattern has `inject_inbound_images = true`,
//!    ensuring consistent behavior with the auto-injection path in
//!    `build_user_blocks` (see `service.rs`).
//!
//! ## Design rationale
//!
//! - `read_image` is always registered regardless of model capabilities, so
//!   the tool schema is stable and honest across sessions.
//! - The execution mode (injection vs. fallback) is determined at runtime
//!   based on `ToolContext.pattern_inject_images` (set from the per-message
//!   pattern config) and the tool's own `supports_images` / `vision_client`
//!   fields.
//! - When neither condition is met, the tool returns a descriptive error
//!   guiding the user to configure `[agent.vision]` or enable
//!   `inject_inbound_images`.
//!
//! ## Why a side-channel instead of a tool-result block (mode 1 only)
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
//! Mode 1 (image injection): a small JSON confirmation containing `loaded`,
//! `bytes`, and `media_type`. The actual image content rides on the next user
//! turn.
//!
//! Mode 2 (vision fallback): a JSON object containing `loaded`, `media_type`,
//! and `analysis` (the vision model's text response).

use anyhow::Result;
use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{Value, json};
use std::sync::Arc;

use super::super::{Tool, ToolContext, ToolOutput};
use crate::types::ImageSource;
use crate::vision::VisionClient;

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
///
/// Dual-mode: see module-level documentation.
pub struct ReadImageTool {
    /// Whether the primary model supports native image content blocks.
    supports_images: bool,
    /// Optional vision model client for text-only model fallback.
    vision_client: Option<Arc<VisionClient>>,
}

impl ReadImageTool {
    pub fn new(supports_images: bool, vision_client: Option<Arc<VisionClient>>) -> Self {
        Self {
            supports_images,
            vision_client,
        }
    }
}

#[async_trait]
impl Tool for ReadImageTool {
    fn name(&self) -> &str {
        "read_image"
    }

    fn description(&self) -> &str {
        "Load an image into the next user turn so the model can see it, \
         or (for text-only models) analyze it via a vision model and return text. \
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
            (Some(p), None) => self.load_from_path(p, ctx).await,
            (None, Some(u)) => self.load_from_url(u, ctx).await,
            (Some(_), Some(_)) => Ok(ToolOutput::error(
                "Provide either `path` or `url`, not both",
            )),
            (None, None) => Ok(ToolOutput::error(
                "Missing required parameter: `path` or `url`",
            )),
        }
    }
}

impl ReadImageTool {
    /// Load an image from a local filesystem path.
    async fn load_from_path(&self, path_str: &str, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
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
        // additional_read_roots.
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
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

        // Detect MIME from extension.
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

        self.process_image(media_type, bytes, path_str, ctx).await
    }

    /// Fetch an image from an http(s) URL and process it.
    async fn load_from_url(&self, url: &str, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
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

        // Use Content-Type for media_type
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
            Ok(b) => b.to_vec(),
            Err(e) => {
                return Ok(ToolOutput::error(format!(
                    "Failed to read response body: {e}"
                )));
            }
        };

        self.process_image(&media_type, bytes, url, ctx).await
    }

    /// Process loaded image bytes according to the current mode.
    ///
    /// `input_str` is the original path or URL for display in the response.
    async fn process_image(
        &self,
        media_type: &str,
        bytes: Vec<u8>,
        input_str: &str,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput> {
        if bytes.len() > MAX_IMAGE_BYTES {
            return Ok(ToolOutput::error(format!(
                "Image too large: {} bytes (max {} bytes)",
                bytes.len(),
                MAX_IMAGE_BYTES
            )));
        }

        if self.supports_images {
            // Mode 1: Image injection — queue for next user turn
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
                    "loaded": input_str,
                    "bytes": size,
                    "media_type": media_type,
                    "note": "Image queued for next user turn",
                })
                .to_string(),
            ))
        } else if ctx.pattern_inject_images && self.vision_client.is_some() {
            // Mode 2: Vision fallback — send to vision model (only when
            // the pattern allows image handling, consistent with
            // `build_user_blocks` in service.rs).
            let vc = self.vision_client.as_ref().unwrap();
            let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
            match vc.analyze(media_type, &data).await {
                Ok(text) => Ok(ToolOutput::success(
                    json!({
                        "loaded": input_str,
                        "media_type": media_type,
                        "analysis": text,
                        "note": "Image analyzed via vision model",
                    })
                    .to_string(),
                )),
                Err(e) => Ok(ToolOutput::error(format!("Vision analysis failed: {e}"))),
            }
        } else {
            // No vision capability available
            let msg = if !ctx.pattern_inject_images {
                "The current pattern does not allow image handling \
                 (inject_inbound_images is disabled). Enable it in \
                 config.toml or use a different pattern to use the \
                 read_image tool with vision fallback."
            } else {
                "The current model does not support images and no vision \
                 fallback model is configured. Set [agent.vision] in \
                 config.toml to enable image analysis via a separate vision model."
            };
            Ok(ToolOutput::error(msg))
        }
    }
}

fn push_pending(ctx: &ToolContext<'_>, src: ImageSource) {
    ctx.pending_images
        .lock()
        .expect("pending_images poisoned")
        .push(src);
}
