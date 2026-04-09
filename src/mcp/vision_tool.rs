//! MCP Vision Tool — analyzes images using an OpenAI-compatible vision API.
//!
//! Reads configuration from environment variables (passed via opencode.json):
//! - `VISION_API_KEY` — API key for the vision provider
//! - `VISION_API_URL` — Chat completions endpoint URL
//! - `VISION_MODEL` — Model name (e.g., "kimi-k2.5")

use anyhow::{Context, Result};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_router, tool_handler,
};
use std::path::{Path, PathBuf};

/// File-based logger for the MCP tool (stdout is used for MCP protocol).
struct McpLogger {
    path: PathBuf,
}

impl McpLogger {
    fn new(cwd: &Path) -> Self {
        let jyc_dir = cwd.join(".jyc");
        std::fs::create_dir_all(&jyc_dir).ok();
        Self {
            path: jyc_dir.join("vision-tool.log"),
        }
    }

    fn log(&self, level: &str, msg: &str) {
        let line = format!(
            "[{}] [{}] {}\n",
            chrono::Utc::now().to_rfc3339(),
            level,
            msg
        );
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// Parameters for the analyze_image tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct AnalyzeImageParams {
    #[schemars(description = "Absolute path to a local image file, or an HTTP(S) URL")]
    pub image_path: String,
    #[schemars(description = "Analysis prompt (what to look for in the image)")]
    #[serde(default = "default_prompt")]
    pub prompt: String,
}

fn default_prompt() -> String {
    "Please describe this image in detail.".to_string()
}

/// Vision API configuration read from environment variables.
struct VisionApiConfig {
    api_key: String,
    api_url: String,
    model: String,
}

impl VisionApiConfig {
    fn from_env() -> Result<Self> {
        let api_key = std::env::var("VISION_API_KEY")
            .context("VISION_API_KEY environment variable not set")?;
        let api_url = std::env::var("VISION_API_URL")
            .unwrap_or_else(|_| "https://api.moonshot.cn/v1/chat/completions".to_string());
        let model = std::env::var("VISION_MODEL")
            .unwrap_or_else(|_| "kimi-k2.5".to_string());
        Ok(Self { api_key, api_url, model })
    }
}

/// The MCP vision tool handler.
#[derive(Debug, Clone)]
pub struct VisionToolHandler {
    tool_router: ToolRouter<Self>,
}

impl VisionToolHandler {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl VisionToolHandler {
    #[tool(description = "Analyze an image using a vision AI model. Accepts a local file path (absolute) or an HTTP(S) URL. Returns the model's description/analysis of the image.")]
    async fn analyze_image(
        &self,
        Parameters(params): Parameters<AnalyzeImageParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let cwd = std::env::current_dir().unwrap_or_default();
        let logger = McpLogger::new(&cwd);

        logger.log("INFO", &format!(
            "analyze_image called: image_path={}, prompt_len={}",
            params.image_path, params.prompt.len()
        ));

        match handle_analyze_image(&logger, &params.image_path, &params.prompt).await {
            Ok(text) => {
                logger.log("INFO", &format!("analyze_image completed: {} chars", text.len()));
                Ok(CallToolResult::success(vec![Content::text(text)]))
            }
            Err(e) => {
                let err_msg = format!("Error: {e}");
                logger.log("ERROR", &format!("analyze_image FAILED: {e}"));
                Ok(CallToolResult::error(vec![Content::text(err_msg)]))
            }
        }
    }
}

#[tool_handler]
impl ServerHandler for VisionToolHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "vision",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions("MCP vision tool — analyzes images using an OpenAI-compatible vision API")
    }
}

/// Core image analysis logic.
async fn handle_analyze_image(
    logger: &McpLogger,
    image_path: &str,
    prompt: &str,
) -> Result<String> {
    let config = VisionApiConfig::from_env()?;

    logger.log("INFO", &format!(
        "Using model={}, api_url={}", config.model, config.api_url
    ));

    // Load image and convert to base64 data URI
    let (base64_data, mime_type) = load_image(image_path).await?;
    let image_url = format!("data:{};base64,{}", mime_type, base64_data);

    logger.log("INFO", &format!(
        "Image loaded: mime={}, base64_len={}", mime_type, base64_data.len()
    ));

    // Call the vision API
    let response = call_vision_api(&config, prompt, &image_url).await?;

    logger.log("INFO", &format!("API response received: {} chars", response.len()));

    Ok(response)
}

/// Load an image from a local file path or URL, returning (base64, mime_type).
async fn load_image(image_path: &str) -> Result<(String, String)> {
    if image_path.starts_with("http://") || image_path.starts_with("https://") {
        // Download from URL
        let client = reqwest::Client::new();
        let resp = client.get(image_path)
            .send()
            .await
            .context("Failed to download image from URL")?;

        if !resp.status().is_success() {
            anyhow::bail!("Image download failed: HTTP {}", resp.status());
        }

        let content_type = resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("image/jpeg")
            .to_string();

        let bytes = resp.bytes().await.context("Failed to read image bytes")?;
        let base64 = base64_encode(&bytes);
        Ok((base64, content_type))
    } else {
        // Read local file
        let path = Path::new(image_path);
        if !path.exists() {
            anyhow::bail!("File not found: {}", image_path);
        }
        if !path.is_absolute() {
            anyhow::bail!("File path must be absolute: {}", image_path);
        }

        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("Failed to read file: {}", image_path))?;

        let mime_type = guess_mime_type(image_path);
        let base64 = base64_encode(&bytes);
        Ok((base64, mime_type))
    }
}

/// Guess MIME type from file extension.
fn guess_mime_type(path: &str) -> String {
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "tiff" | "tif" => "image/tiff",
        _ => "image/jpeg", // Default to JPEG for jpg, jpeg, and unknown
    }
    .to_string()
}

/// Base64 encode bytes using the standard engine.
fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Call an OpenAI-compatible vision API.
async fn call_vision_api(
    config: &VisionApiConfig,
    prompt: &str,
    image_url: &str,
) -> Result<String> {
    let client = reqwest::Client::new();

    let body = serde_json::json!({
        "model": config.model,
        "messages": [
            {
                "role": "user",
                "content": [
                    { "type": "text", "text": prompt },
                    { "type": "image_url", "image_url": { "url": image_url } }
                ]
            }
        ],
        "temperature": 0.7
    });

    let resp = client
        .post(&config.api_url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", config.api_key))
        .json(&body)
        .send()
        .await
        .context("Vision API request failed")?;

    let status = resp.status();
    let text = resp.text().await.context("Failed to read API response")?;

    if !status.is_success() {
        anyhow::bail!("Vision API error ({}): {}", status, text);
    }

    let parsed: serde_json::Value =
        serde_json::from_str(&text).context("Failed to parse API response JSON")?;

    // Extract the assistant's message content
    let content = parsed["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Unexpected API response format: {}", text))?;

    Ok(content.to_string())
}

/// Start the MCP vision tool server on stdio.
pub async fn run_server() -> Result<()> {
    let handler = VisionToolHandler::new();
    let service = handler.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
