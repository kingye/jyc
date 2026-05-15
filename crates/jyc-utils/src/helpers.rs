use anyhow::{bail, Result};
use regex::Regex;

/// Parse a human-readable file size string into bytes.
///
/// Supports formats like "25mb", "150kb", "1gb", "1024", "10 MB", "2.5m", "100k".
/// Case-insensitive. If no unit is given, assumes bytes.
pub fn parse_file_size(input: &str) -> Result<u64> {
    let input = input.trim().to_lowercase();
    if input.is_empty() {
        bail!("empty file size string");
    }

    let re = Regex::new(r"^(\d+(?:\.\d+)?)\s*(b|kb?|mb?|gb?|tb?|bytes?)?$").unwrap();
    let caps = re
        .captures(&input)
        .ok_or_else(|| anyhow::anyhow!("invalid file size format: '{input}'"))?;

    let number: f64 = caps[1].parse()?;
    let multiplier: u64 = match caps.get(2).map(|m| m.as_str()) {
        None | Some("") | Some("b") | Some("byte") | Some("bytes") => 1,
        Some("k") | Some("kb") => 1024,
        Some("m") | Some("mb") => 1024 * 1024,
        Some("g") | Some("gb") => 1024 * 1024 * 1024,
        Some("t") | Some("tb") => 1024 * 1024 * 1024 * 1024,
        Some(unit) => bail!("unknown file size unit: '{unit}'"),
    };

    Ok((number * multiplier as f64) as u64)
}

/// Validate that a regex pattern compiles without error.
/// Returns the compiled Regex on success.
pub fn validate_regex(pattern: &str) -> Result<Regex> {
    Regex::new(pattern).map_err(|e| anyhow::anyhow!("invalid regex '{}': {}", pattern, e))
}

/// Extract domain from an email address.
///
/// Returns the part after `@`, lowercased.
/// Returns None if the address doesn't contain `@`.
pub fn extract_domain(email: &str) -> Option<String> {
    email
        .rsplit_once('@')
        .map(|(_, domain)| domain.to_lowercase())
}

/// Safely truncate a string to at most `max_bytes` bytes, respecting UTF-8 char boundaries.
///
/// This prevents panics when truncating strings containing multi-byte characters
/// (e.g., Chinese, emoji) where a simple slice like `&s[..80]` might split a character.
pub fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let boundary = s.floor_char_boundary(max_bytes);
    &s[..boundary]
}

/// Sanitize a string for use as a filesystem directory name.
///
/// Removes or replaces characters that are unsafe in filenames.
/// Trims whitespace, limits length, and handles edge cases.
pub fn sanitize_for_filesystem(input: &str) -> String {
    let mut result = String::with_capacity(input.len());

    for ch in input.chars() {
        match ch {
            // Replace filesystem-unsafe characters with underscore
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => result.push('_'),
            // Replace control characters
            c if c.is_control() => {}
            // Keep everything else (including CJK, emoji, etc.)
            c => result.push(c),
        }
    }

    // Trim whitespace and underscores from edges
    let trimmed = result.trim().trim_matches('_').to_string();

    // Limit length
    let max_len = 200;
    if trimmed.len() > max_len {
        // Find a safe truncation point (don't split multi-byte chars)
        let mut end = max_len;
        while end > 0 && !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        trimmed[..end].to_string()
    } else if trimmed.is_empty() {
        // Fallback for completely empty names
        "unnamed".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_str_ascii() {
        assert_eq!(truncate_str("hello world", 100), "hello world");
        assert_eq!(truncate_str("hello world", 5), "hello");
        assert_eq!(truncate_str("hello world", 0), "");
    }

    #[test]
    fn test_truncate_str_multibyte() {
        let chinese = "我的问题是，close event 通常情况下没有被接收";
        assert_eq!(truncate_str(chinese, 0), "");
        assert_eq!(truncate_str(chinese, 3), "我");
        assert_eq!(truncate_str(chinese, 6), "我的");
        assert_eq!(
            truncate_str(chinese, 80),
            "我的问题是，close event 通常情况下没有被接收"
        );
        let shorter = "我的问题是";
        assert_eq!(truncate_str(shorter, 5), "我");
    }

    #[test]
    fn test_truncate_str_boundary() {
        let s = "我的问题";
        let truncated = truncate_str(s, 4);
        assert_eq!(truncated, "我");
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn test_parse_file_size_bytes() {
        assert_eq!(parse_file_size("1024").unwrap(), 1024);
        assert_eq!(parse_file_size("0").unwrap(), 0);
        assert_eq!(parse_file_size("100b").unwrap(), 100);
        assert_eq!(parse_file_size("100 bytes").unwrap(), 100);
    }

    #[test]
    fn test_parse_file_size_kb() {
        assert_eq!(parse_file_size("1kb").unwrap(), 1024);
        assert_eq!(parse_file_size("150kb").unwrap(), 150 * 1024);
        assert_eq!(parse_file_size("1 KB").unwrap(), 1024);
    }

    #[test]
    fn test_parse_file_size_mb() {
        assert_eq!(parse_file_size("1mb").unwrap(), 1024 * 1024);
        assert_eq!(parse_file_size("25mb").unwrap(), 25 * 1024 * 1024);
        assert_eq!(parse_file_size("10 MB").unwrap(), 10 * 1024 * 1024);
    }

    #[test]
    fn test_parse_file_size_gb() {
        assert_eq!(parse_file_size("1gb").unwrap(), 1024 * 1024 * 1024);
    }

    #[test]
    fn test_parse_file_size_invalid() {
        assert!(parse_file_size("").is_err());
        assert!(parse_file_size("abc").is_err());
        assert!(parse_file_size("10xyz").is_err());
    }

    #[test]
    fn test_validate_regex_valid() {
        assert!(validate_regex(r".*@company\.com").is_ok());
        assert!(validate_regex(r"\[URGENT\].*").is_ok());
    }

    #[test]
    fn test_validate_regex_invalid() {
        assert!(validate_regex(r"[invalid").is_err());
        assert!(validate_regex(r"(?P<>bad)").is_err());
    }

    #[test]
    fn test_extract_domain() {
        assert_eq!(
            extract_domain("user@example.com"),
            Some("example.com".to_string())
        );
        assert_eq!(
            extract_domain("User@EXAMPLE.COM"),
            Some("example.com".to_string())
        );
        assert_eq!(extract_domain("nodomain"), None);
    }

    #[test]
    fn test_sanitize_for_filesystem() {
        assert_eq!(sanitize_for_filesystem("hello world"), "hello world");
        assert_eq!(sanitize_for_filesystem("a/b\\c:d"), "a_b_c_d");
        assert_eq!(sanitize_for_filesystem("  spaces  "), "spaces");
        assert_eq!(sanitize_for_filesystem(""), "unnamed");
        assert_eq!(sanitize_for_filesystem("///"), "unnamed");
        // CJK should be preserved
        assert_eq!(sanitize_for_filesystem("你好世界"), "你好世界");
    }
}
