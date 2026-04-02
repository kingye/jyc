#[allow(dead_code)]
pub mod constants;
pub mod helpers;

use thiserror::Error;

/// Top-level application errors
#[allow(dead_code)]
#[derive(Error, Debug)]
pub enum JycError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Configuration file not found: {0}")]
    ConfigNotFound(String),

    #[error("Configuration validation failed:\n{0}")]
    ConfigValidation(String),

    #[error("IMAP error: {0}")]
    Imap(String),

    #[error("SMTP error: {0}")]
    Smtp(String),

    #[error("OpenCode error: {0}")]
    OpenCode(String),

    #[error("MCP error: {0}")]
    Mcp(String),

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Email parsing error: {0}")]
    EmailParse(String),

    #[error("Security violation: {0}")]
    Security(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(String),
}
