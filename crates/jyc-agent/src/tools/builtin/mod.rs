//! Built-in tool implementations.

pub mod bash;
pub mod read;
pub mod read_image;
pub mod write;
pub mod edit;
pub mod glob_tool;
pub mod grep;
pub mod webfetch;

use super::registry::ToolRegistry;

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

    registry
}

/// Register the `read_image` built-in tool. Call this only when the active
/// model supports image content blocks.
pub fn register_read_image(registry: &mut ToolRegistry) {
    registry.register(Box::new(read_image::ReadImageTool));
}
