//! Built-in tool implementations.

pub mod bash;
pub mod edit;
pub mod glob_tool;
pub mod grep;
pub mod job_tools;
pub mod read;
pub mod read_image;
pub mod webfetch;
pub mod write;

use std::sync::Arc;

use super::registry::ToolRegistry;
use crate::vision::VisionClient;

/// Create a tool registry with all built-in tools.
pub fn create_builtin_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();

    registry.register(Box::new(bash::BashTool));
    registry.register(Box::new(read::ReadTool));
    registry.register(Box::new(write::WriteTool));
    registry.register(Box::new(edit::EditTool));
    registry.register(Box::new(glob_tool::GlobTool));
    registry.register(Box::new(grep::GrepTool));
    registry.register(Box::new(webfetch::WebfetchTool));
    registry.register(Box::new(job_tools::JobListTool));
    registry.register(Box::new(job_tools::JobCreateTool));
    registry.register(Box::new(job_tools::JobDeleteTool));
    registry.register(Box::new(job_tools::JobToggleTool));

    registry
}

/// Register the `read_image` built-in tool.
///
/// `supports_images` controls the execution mode:
/// - `true`: images are queued for injection into the next user turn.
/// - `false`: `vision_client` (if configured) is used to analyze the image
///   and return text. If both are false/unavailable, the tool returns an error.
pub fn register_read_image(
    registry: &mut ToolRegistry,
    supports_images: bool,
    vision_client: Option<Arc<VisionClient>>,
) {
    registry.register(Box::new(read_image::ReadImageTool::new(
        supports_images,
        vision_client,
    )));
}
