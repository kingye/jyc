//! Generic attachment validation utilities for both inbound and outbound attachments.
//! 
//! This module provides validation functions that work across all channel types,
//! ensuring consistent security and size limits regardless of the transport.

use anyhow::Result;
use std::path::Path;

use crate::config::types::{InboundAttachmentConfig, OutboundAttachmentConfig};
use crate::utils::helpers::parse_file_size;

/// Validation errors for attachments.
#[derive(Debug, thiserror::Error)]
pub enum AttachmentValidationError {
    #[error("attachment is too large: {size} bytes exceeds limit of {limit} bytes")]
    FileTooLarge { size: u64, limit: u64 },
    
    #[error("file extension '{ext}' is not allowed. Allowed extensions: {allowed:?}")]
    ExtensionNotAllowed { ext: String, allowed: Vec<String> },
    
    #[error("maximum number of attachments per message exceeded: {count} > {max}")]
    TooManyAttachments { count: usize, max: usize },
    
    #[error("invalid file size format: {0}")]
    InvalidFileSizeFormat(String),
    
    #[error("file not found: {0}")]
    FileNotFound(String),
    
    #[error("cannot read file metadata: {0}")]
    FileMetadataError(String),
}

/// Validates a single file against inbound attachment configuration.
pub async fn validate_inbound_file(
    file_path: &Path,
    filename: &str,
    config: &InboundAttachmentConfig,
) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }
    
    // Check file exists and get metadata
    let metadata = tokio::fs::metadata(file_path)
        .await
        .map_err(|e| AttachmentValidationError::FileNotFound(e.to_string()))?;
    
    let file_size = metadata.len();
    
    // Validate file size if configured
    if let Some(ref max_size_str) = config.max_file_size {
        let max_size = parse_file_size(max_size_str)
            .map_err(|e| AttachmentValidationError::InvalidFileSizeFormat(e.to_string()))?;
        
        if file_size > max_size {
            return Err(AttachmentValidationError::FileTooLarge {
                size: file_size,
                limit: max_size,
            }.into());
        }
    }
    
    // Validate file extension
    validate_file_extension(filename, &config.allowed_extensions)?;
    
    Ok(())
}

/// Validates a single file against outbound attachment configuration.
pub async fn validate_outbound_file(
    file_path: &Path,
    filename: &str,
    config: &OutboundAttachmentConfig,
) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }
    
    // Check file exists and get metadata
    let metadata = tokio::fs::metadata(file_path)
        .await
        .map_err(|e| AttachmentValidationError::FileNotFound(e.to_string()))?;
    
    let file_size = metadata.len();
    
    // Validate file size if configured
    if let Some(ref max_size_str) = config.max_file_size {
        let max_size = parse_file_size(max_size_str)
            .map_err(|e| AttachmentValidationError::InvalidFileSizeFormat(e.to_string()))?;
        
        if file_size > max_size {
            return Err(AttachmentValidationError::FileTooLarge {
                size: file_size,
                limit: max_size,
            }.into());
        }
    }
    
    // Validate file extension
    validate_file_extension(filename, &config.allowed_extensions)?;
    
    Ok(())
}

/// Validates a collection of files against attachment count limits.
pub fn validate_attachment_count(
    attachments: &[impl std::fmt::Debug],
    max_per_message: Option<usize>,
) -> Result<()> {
    if let Some(max) = max_per_message {
        if max == 0 {
            return Err(AttachmentValidationError::TooManyAttachments {
                count: attachments.len(),
                max,
            }.into());
        }
        
        if attachments.len() > max {
            return Err(AttachmentValidationError::TooManyAttachments {
                count: attachments.len(),
                max,
            }.into());
        }
    }
    
    Ok(())
}

/// Validates a filename's extension against allowed extensions.
fn validate_file_extension(filename: &str, allowed_extensions: &[String]) -> Result<()> {
    if allowed_extensions.is_empty() {
        // If no extensions are specified, all are allowed
        return Ok(());
    }
    
    let ext = Path::new(filename)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase());
    
    let has_valid_extension = if let Some(ref ext_value) = ext {
        allowed_extensions.iter().any(|allowed| {
            // Normalize both extensions: ensure they start with dot
            let normalized_allowed = if allowed.starts_with('.') {
                allowed.as_str()
            } else {
                // This shouldn't happen if config validation is working
                &format!(".{}", allowed)
            };
            
            let normalized_ext = if ext_value.starts_with('.') {
                ext_value.as_str()
            } else {
                &format!(".{}", ext_value)
            };
            
            normalized_ext == normalized_allowed
        })
    } else {
        false
    };
    
    if !has_valid_extension {
        return Err(AttachmentValidationError::ExtensionNotAllowed {
            ext: ext.unwrap_or_else(|| "none".to_string()),
            allowed: allowed_extensions.to_vec(),
        }.into());
    }
    
    Ok(())
}

/// Validates a list of outbound attachments against configuration.
/// This is a convenience function that combines size and count validation.
pub async fn validate_outbound_attachments(
    attachments: &[crate::channels::types::OutboundAttachment],
    config: &OutboundAttachmentConfig,
) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }
    
    // Validate total attachment count
    validate_attachment_count(attachments, config.max_per_message)?;
    
    // Validate each individual file
    for attachment in attachments {
        validate_outbound_file(&attachment.path, &attachment.filename, config).await?;
    }
    
    Ok(())
}

/// Helper function to parse file size from human-readable string.
/// This re-exports the helper function for convenience.
pub fn parse_attachment_size(size_str: &str) -> Result<u64> {
    parse_file_size(size_str).map_err(|e| anyhow::anyhow!(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    use std::io::Write;

    fn create_test_file(content: &[u8]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(content).unwrap();
        file
    }

    #[tokio::test]
    async fn test_validate_outbound_file_success() {
        let file = create_test_file(b"test content");
        let config = OutboundAttachmentConfig {
            enabled: true,
            allowed_extensions: vec![".txt".to_string(), ".pdf".to_string()],
            max_file_size: Some("1mb".to_string()),
            max_per_message: Some(5),
        };

        let result = validate_outbound_file(
            file.path(),
            "test.txt",
            &config,
        ).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_validate_outbound_file_disabled() {
        let file = create_test_file(b"test content");
        let config = OutboundAttachmentConfig {
            enabled: false,
            allowed_extensions: vec![".pdf".to_string()],  // Wrong extension, but enabled=false
            max_file_size: Some("1kb".to_string()),  // Too small, but enabled=false
            max_per_message: Some(0),  // Invalid, but enabled=false
        };

        let result = validate_outbound_file(
            file.path(),
            "test.txt",
            &config,
        ).await;

        assert!(result.is_ok());  // Should pass because validation is disabled
    }

    #[tokio::test]
    async fn test_validate_outbound_file_too_large() {
        let content = vec![0u8; 2 * 1024 * 1024];  // 2MB
        let file = create_test_file(&content);
        let config = OutboundAttachmentConfig {
            enabled: true,
            allowed_extensions: vec![".txt".to_string()],
            max_file_size: Some("1mb".to_string()),  // Only 1MB allowed
            max_per_message: Some(5),
        };

        let result = validate_outbound_file(
            file.path(),
            "test.txt",
            &config,
        ).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[tokio::test]
    async fn test_validate_outbound_file_extension_not_allowed() {
        let file = create_test_file(b"test content");
        let config = OutboundAttachmentConfig {
            enabled: true,
            allowed_extensions: vec![".pdf".to_string(), ".docx".to_string()],
            max_file_size: Some("1mb".to_string()),
            max_per_message: Some(5),
        };

        let result = validate_outbound_file(
            file.path(),
            "test.txt",
            &config,
        ).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("extension"));
        assert!(err.to_string().contains("not allowed"));
    }

    #[tokio::test]
    async fn test_validate_outbound_file_extension_without_dot() {
        let file = create_test_file(b"test content");
        let config = OutboundAttachmentConfig {
            enabled: true,
            allowed_extensions: vec!["txt".to_string(), "pdf".to_string()],  // Without dot
            max_file_size: Some("1mb".to_string()),
            max_per_message: Some(5),
        };

        let result = validate_outbound_file(
            file.path(),
            "test.txt",
            &config,
        ).await;

        assert!(result.is_ok());  // Should work even without dot in config
    }

    #[tokio::test]
    async fn test_validate_outbound_file_no_extension() {
        let file = create_test_file(b"test content");
        let config = OutboundAttachmentConfig {
            enabled: true,
            allowed_extensions: vec![".txt".to_string()],
            max_file_size: Some("1mb".to_string()),
            max_per_message: Some(5),
        };

        let result = validate_outbound_file(
            file.path(),
            "test",  // No extension
            &config,
        ).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("extension"));
    }

    #[tokio::test]
    async fn test_validate_outbound_file_empty_allowed_extensions() {
        let file = create_test_file(b"test content");
        let config = OutboundAttachmentConfig {
            enabled: true,
            allowed_extensions: vec![],  // Empty means all extensions allowed
            max_file_size: Some("1mb".to_string()),
            max_per_message: Some(5),
        };

        let result = validate_outbound_file(
            file.path(),
            "test.xyz",  // Any extension should work
            &config,
        ).await;

        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_attachment_count_success() {
        let attachments: Vec<String> = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        
        // With limit
        let result = validate_attachment_count(&attachments, Some(5));
        assert!(result.is_ok());
        
        // Without limit
        let result = validate_attachment_count(&attachments, None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_attachment_count_exceeded() {
        let attachments: Vec<String> = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        
        let result = validate_attachment_count(&attachments, Some(2));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("exceeded"));
    }

    #[test]
    fn test_validate_attachment_count_zero_limit() {
        let attachments: Vec<String> = vec!["a".to_string()];
        
        let result = validate_attachment_count(&attachments, Some(0));
        assert!(result.is_err());
        let err = result.unwrap_err();
        // Error message should indicate that max is 0 but we have 1 attachment
        assert!(err.to_string().contains("exceeded"));
        assert!(err.to_string().contains("1 > 0"));
    }

    #[tokio::test]
    async fn test_validate_outbound_attachments_success() {
        use crate::channels::types::OutboundAttachment;
        
        let file1 = create_test_file(b"content1");
        let file2 = create_test_file(b"content2");
        
        let attachments = vec![
            OutboundAttachment {
                filename: "test1.txt".to_string(),
                path: file1.path().to_path_buf(),
                content_type: "text/plain".to_string(),
            },
            OutboundAttachment {
                filename: "test2.txt".to_string(),
                path: file2.path().to_path_buf(),
                content_type: "text/plain".to_string(),
            },
        ];
        
        let config = OutboundAttachmentConfig {
            enabled: true,
            allowed_extensions: vec![".txt".to_string()],
            max_file_size: Some("1mb".to_string()),
            max_per_message: Some(5),
        };

        let result = validate_outbound_attachments(&attachments, &config).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_validate_outbound_attachments_count_exceeded() {
        use crate::channels::types::OutboundAttachment;
        
        let file1 = create_test_file(b"content1");
        let file2 = create_test_file(b"content2");
        let file3 = create_test_file(b"content3");
        
        let attachments = vec![
            OutboundAttachment {
                filename: "test1.txt".to_string(),
                path: file1.path().to_path_buf(),
                content_type: "text/plain".to_string(),
            },
            OutboundAttachment {
                filename: "test2.txt".to_string(),
                path: file2.path().to_path_buf(),
                content_type: "text/plain".to_string(),
            },
            OutboundAttachment {
                filename: "test3.txt".to_string(),
                path: file3.path().to_path_buf(),
                content_type: "text/plain".to_string(),
            },
        ];
        
        let config = OutboundAttachmentConfig {
            enabled: true,
            allowed_extensions: vec![".txt".to_string()],
            max_file_size: Some("1mb".to_string()),
            max_per_message: Some(2),  // Only 2 allowed
        };

        let result = validate_outbound_attachments(&attachments, &config).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("exceeded"));
    }
}