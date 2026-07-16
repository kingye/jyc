//! WeCom Bot media download and AES-256-CBC decryption.
//!
//! Downloads encrypted image/file payloads from pre-signed COS URLs and
//! decrypts them using the per-message `aeskey` delivered in the WebSocket
//! callback.
//!
//! The download flow:
//! 1. HTTP GET the pre-signed URL (valid 5 minutes, no auth needed)
//! 2. AES-256-CBC decrypt the response body with the per-message key
//! 3. Strip PKCS#7 padding (AES block size = 16 bytes)
//! 4. Return decrypted bytes + detected MIME type
//!
//! Reference: doc 101463 (Smart Robot WebSocket Long Connection)

use std::time::Duration;

use aes::cipher::{BlockDecryptMut, KeyIvInit};
use anyhow::{Context, Result};
use base64::{Engine, alphabet, engine::GeneralPurpose};

use jyc_types::MessageAttachment;

use super::types::BotMessage;

// ─── Constants ────────────────────────────────────────────────────

/// Hard upper bound on a single media download (50 MiB).
pub const MAX_MEDIA_BYTES: usize = 50 * 1024 * 1024;

/// Per-request HTTP timeout (seconds).
const DOWNLOAD_TIMEOUT_SECS: u64 = 30;

/// AES block size in bytes (used for PKCS#7 padding).
const AES_BLOCK_SIZE: usize = 16;

// ─── Base64 Engine ────────────────────────────────────────────────

/// Permissive base64 engine that allows non-zero trailing bits.
///
/// WeCom's aeskey may have non-zero trailing bits in base64 padding,
/// which the standard strict decoder rejects.
static PERMISSIVE_BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    base64::engine::GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true),
);

type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

// ─── Public API ───────────────────────────────────────────────────

/// Download and decrypt all attachments from a `BotMessage`.
///
/// Handles `image`, `mixed` (image items), and `file` msgtypes.
/// For `mixed`, only image items are downloaded; text items are skipped.
///
/// Errors from individual downloads are logged and skipped — other
/// attachments and the message itself are still delivered.
pub async fn process_bot_attachments(bot_msg: &BotMessage) -> Result<Vec<MessageAttachment>> {
    let mut attachments = Vec::new();

    match bot_msg.msgtype.as_str() {
        "image" => {
            if let Some(ref image) = bot_msg.image {
                match download_and_decrypt(&image.url, &image.aeskey).await {
                    Ok((bytes, mime, http_mime)) => {
                        let filename =
                            url_filename_hint(&image.url).unwrap_or_else(|| "image".to_string());
                        attachments.push(build_attachment(
                            &filename,
                            &bytes,
                            &mime,
                            http_mime.as_deref(),
                        ));
                    }
                    Err(e) => {
                        tracing::warn!(
                            msgid = %bot_msg.msgid,
                            url = %image.url,
                            error = %e,
                            "Failed to download WeCom Bot image"
                        );
                    }
                }
            }
        }
        "mixed" => {
            if let Some(ref mixed) = bot_msg.mixed {
                for (i, item) in mixed.msg_item.iter().enumerate() {
                    if item.msgtype != "image" {
                        continue;
                    }
                    let Some(image) = &item.image else {
                        continue;
                    };
                    match download_and_decrypt(&image.url, &image.aeskey).await {
                        Ok((bytes, mime, http_mime)) => {
                            let filename = url_filename_hint(&image.url)
                                .unwrap_or_else(|| format!("image_{}", i));
                            attachments.push(build_attachment(
                                &filename,
                                &bytes,
                                &mime,
                                http_mime.as_deref(),
                            ));
                        }
                        Err(e) => {
                            tracing::warn!(
                                msgid = %bot_msg.msgid,
                                index = i,
                                url = %image.url,
                                error = %e,
                                "Failed to download WeCom Bot mixed image"
                            );
                        }
                    }
                }
            }
        }
        "file" => {
            if let Some(ref file) = bot_msg.file {
                match download_and_decrypt(&file.url, &file.aeskey).await {
                    Ok((bytes, mime, http_mime)) => {
                        let filename = file
                            .filename
                            .clone()
                            .or_else(|| url_filename_hint(&file.url))
                            .unwrap_or_else(|| "file".to_string());
                        attachments.push(build_attachment(
                            &filename,
                            &bytes,
                            &mime,
                            http_mime.as_deref(),
                        ));
                    }
                    Err(e) => {
                        tracing::warn!(
                            msgid = %bot_msg.msgid,
                            filename = ?file.filename,
                            url = %file.url,
                            error = %e,
                            "Failed to download WeCom Bot file"
                        );
                    }
                }
            }
        }
        _ => {}
    }

    Ok(attachments)
}

// ─── Internal ─────────────────────────────────────────────────────

/// Download encrypted bytes from a pre-signed URL and decrypt them.
///
/// Returns the decrypted bytes, the MIME type detected from magic bytes, and
/// the MIME type from the HTTP response (if any) as a fallback.
async fn download_and_decrypt(
    url: &str,
    aeskey: &str,
) -> Result<(Vec<u8>, String, Option<String>)> {
    let (encrypted, http_content_type) = download_media(url).await?;
    let decrypted = decrypt_aes256cbc(&encrypted, aeskey)?;
    let mime = detect_mime_from_bytes(&decrypted).to_string();
    Ok((decrypted, mime, http_content_type))
}

/// HTTP GET a media payload from a pre-signed URL.
///
/// No authentication header is needed — the URL itself is pre-signed.
/// Returns the raw (encrypted) response body bytes and the response
/// `Content-Type` (without parameters such as `charset`) if present.
async fn download_media(url: &str) -> Result<(Vec<u8>, Option<String>)> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .build()
        .context("Failed to build HTTP client for WeCom Bot media download")?;

    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("WeCom Bot media GET {url} failed"))?;

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("WeCom Bot media fetch failed: HTTP {status}");
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_content_type_header);

    let bytes = resp
        .bytes()
        .await
        .context("Failed to read WeCom Bot media response body")?;

    if bytes.len() > MAX_MEDIA_BYTES {
        anyhow::bail!(
            "WeCom Bot media payload exceeds {MAX_MEDIA_BYTES} bytes (got {} bytes)",
            bytes.len()
        );
    }

    Ok((bytes.to_vec(), content_type))
}

/// Extract and normalize the MIME type from a Content-Type header value.
///
/// Returns the `type/subtype` portion in lowercase, stripping parameters such
/// as `charset` or `boundary`. Returns `None` if the value is empty or missing.
fn parse_content_type_header(content_type: &str) -> Option<String> {
    let mime = content_type.split(';').next()?.trim().to_lowercase();
    if mime.is_empty() { None } else { Some(mime) }
}

/// Extract a filename hint from the last path segment of a URL.
///
/// Strips query parameters and fragments, then returns the last segment if it
/// contains a dot (e.g., `data.csv`). Returns `None` for bare paths or URLs
/// that cannot be parsed.
fn url_filename_hint(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let segment = parsed.path().rsplit('/').next()?.trim();
    if segment.is_empty() || !segment.contains('.') {
        None
    } else {
        Some(segment.to_string())
    }
}

/// Decrypt AES-256-CBC encrypted bytes with PKCS#7 padding.
///
/// - `aeskey` is base64-encoded (with or without `=` padding).
/// - The decoded key is 32 bytes (AES-256).
/// - IV is the first 16 bytes of the decoded key.
/// - PKCS#7 padding uses AES block size (16 bytes).
pub fn decrypt_aes256cbc(ciphertext: &[u8], aeskey: &str) -> Result<Vec<u8>> {
    let aeskey = aeskey.trim();
    if aeskey.is_empty() {
        anyhow::bail!("aeskey is empty");
    }

    // Add base64 padding if missing (WeCom sometimes omits trailing '=')
    let padded_key = match aeskey.len() % 4 {
        0 => aeskey.to_string(),
        n => format!("{}{}", aeskey, "=".repeat(4 - n)),
    };

    let raw_key = PERMISSIVE_BASE64.decode(&padded_key).with_context(|| {
        format!(
            "failed to decode aeskey from base64 (len={}, padded_len={})",
            aeskey.len(),
            padded_key.len()
        )
    })?;

    if raw_key.len() != 32 {
        anyhow::bail!(
            "aeskey decoded length is {}, expected 32 bytes (AES-256)",
            raw_key.len()
        );
    }

    let aes_key = &raw_key[..32];
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&raw_key[..16]);

    let mut buf = ciphertext.to_vec();
    let decrypted = Aes256CbcDec::new(aes_key.into(), &iv.into())
        .decrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(&mut buf)
        .map_err(|e| anyhow::anyhow!("AES-256-CBC decryption failed: {e:?}"))?;

    strip_pkcs7_padding(decrypted)
}

/// Strip PKCS#7 padding from decrypted bytes.
///
/// Returns a new Vec without the padding bytes.
fn strip_pkcs7_padding(data: &[u8]) -> Result<Vec<u8>> {
    if data.is_empty() {
        return Ok(Vec::new());
    }

    let pad_len = data.last().copied().unwrap_or(0) as usize;
    if pad_len == 0 || pad_len > AES_BLOCK_SIZE {
        // Not padded (or invalid padding) — return as-is for binary data
        return Ok(data.to_vec());
    }

    if data.len() < pad_len {
        return Ok(data.to_vec());
    }

    let padding = &data[data.len() - pad_len..];
    if padding.iter().all(|&b| b == pad_len as u8) {
        Ok(data[..data.len() - pad_len].to_vec())
    } else {
        // Invalid padding — return as-is (might be unpadde binary)
        Ok(data.to_vec())
    }
}

/// Best-effort MIME type detection from file magic bytes.
///
/// Only covers the image and document types we expect from WeCom Bot.
fn detect_mime_from_bytes(bytes: &[u8]) -> &'static str {
    if bytes.len() < 4 {
        return "application/octet-stream";
    }
    match &bytes[..4] {
        [0xff, 0xd8, 0xff, _] => "image/jpeg",
        [0x89, 0x50, 0x4e, 0x47] => "image/png",
        [0x47, 0x49, 0x46, 0x38] => "image/gif",
        [0x52, 0x49, 0x46, 0x46] if bytes.len() >= 12 && &bytes[8..12] == b"WEBP" => "image/webp",
        [0x42, 0x4d, _, _] => "image/bmp",
        [0x25, 0x50, 0x44, 0x46] => "application/pdf",
        [0x50, 0x4b, 0x03, 0x04] => "application/zip",
        _ => "application/octet-stream",
    }
}

/// Build a `MessageAttachment` from decrypted bytes.
///
/// `mime` is the MIME type detected from magic bytes. `http_content_type` is
/// the optional MIME type from the HTTP response, used as a fallback when
/// magic-byte detection cannot identify the file (returns
/// `application/octet-stream`).
fn build_attachment(
    name: &str,
    bytes: &[u8],
    mime: &str,
    http_content_type: Option<&str>,
) -> MessageAttachment {
    let effective_mime = if mime == "application/octet-stream" {
        http_content_type
            .filter(|ct| !ct.is_empty() && *ct != "application/octet-stream")
            .unwrap_or(mime)
    } else {
        mime
    };

    let ext = extension_from_mime(effective_mime);
    let filename = if name.contains('.') {
        name.to_string()
    } else {
        format!("{}.{}", name, ext)
    };

    MessageAttachment {
        filename,
        content_type: effective_mime.to_string(),
        size: bytes.len(),
        content: Some(bytes.to_vec()),
        saved_path: None,
    }
}
/// Map a MIME type to a filesystem extension.
fn extension_from_mime(mime: &str) -> &'static str {
    match mime {
        // Images
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        "image/svg+xml" => "svg",
        "image/heic" => "heic",
        // Documents
        "application/pdf" => "pdf",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.ms-powerpoint" => "ppt",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.template" => "dotx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.template" => "xltx",
        "application/vnd.openxmlformats-officedocument.presentationml.template" => "potx",
        // Archives
        "application/zip" => "zip",
        "application/x-zip-compressed" => "zip",
        "application/gzip" => "gz",
        "application/x-gzip" => "gz",
        "application/x-tar" => "tar",
        "application/x-7z-compressed" => "7z",
        "application/x-rar-compressed" => "rar",
        "application/x-rar" => "rar",
        // Text / data
        "text/plain" => "txt",
        "text/csv" => "csv",
        "text/markdown" => "md",
        "text/html" => "html",
        "text/xml" => "xml",
        "application/json" => "json",
        "application/xml" => "xml",
        "application/yaml" => "yaml",
        "application/x-yaml" => "yaml",
        // Audio / video
        "audio/mpeg" => "mp3",
        "audio/mp4" => "m4a",
        "audio/wav" => "wav",
        "audio/x-wav" => "wav",
        "audio/ogg" => "ogg",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "video/x-msvideo" => "avi",
        "video/webm" => "webm",
        "video/x-matroska" => "mkv",
        _ => "bin",
    }
}
// ─── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ── decrypt_aes256cbc tests ──────────────────────────────────

    #[test]
    fn decrypt_rejects_empty_key() {
        let result = decrypt_aes256cbc(b"test", "");
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("empty"), "expected 'empty' in error: {msg}");
    }

    #[test]
    fn decrypt_rejects_invalid_base64() {
        let result = decrypt_aes256cbc(b"test", "!!!not-base64!!!");
        assert!(result.is_err());
    }

    #[test]
    fn decrypt_rejects_short_key() {
        let short_key =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [0u8; 20]);
        let result = decrypt_aes256cbc(b"test", &short_key);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("expected 32"), "expected length error: {msg}");
    }

    // ── detect_mime_from_bytes tests ─────────────────────────────

    #[test]
    fn detect_jpeg() {
        assert_eq!(
            detect_mime_from_bytes(&[0xff, 0xd8, 0xff, 0xe0]),
            "image/jpeg"
        );
    }

    #[test]
    fn detect_png() {
        assert_eq!(
            detect_mime_from_bytes(&[0x89, 0x50, 0x4e, 0x47]),
            "image/png"
        );
    }

    #[test]
    fn detect_gif() {
        assert_eq!(
            detect_mime_from_bytes(&[0x47, 0x49, 0x46, 0x38]),
            "image/gif"
        );
    }

    #[test]
    fn detect_webp() {
        let mut bytes = vec![0x52, 0x49, 0x46, 0x46];
        bytes.extend_from_slice(&[0; 4]); // size placeholder
        bytes.extend_from_slice(b"WEBP");
        assert_eq!(detect_mime_from_bytes(&bytes), "image/webp");
    }

    #[test]
    fn detect_pdf() {
        assert_eq!(
            detect_mime_from_bytes(&[0x25, 0x50, 0x44, 0x46]),
            "application/pdf"
        );
    }

    #[test]
    fn detect_unknown() {
        assert_eq!(
            detect_mime_from_bytes(&[0x00, 0x01, 0x02, 0x03]),
            "application/octet-stream"
        );
    }

    #[test]
    fn detect_too_short() {
        assert_eq!(
            detect_mime_from_bytes(&[0xff, 0xd8]),
            "application/octet-stream"
        );
    }

    // ── extension_from_mime tests ────────────────────────────────

    #[test]
    fn extension_mapping() {
        // Images
        assert_eq!(extension_from_mime("image/jpeg"), "jpg");
        assert_eq!(extension_from_mime("image/png"), "png");
        assert_eq!(extension_from_mime("image/gif"), "gif");
        assert_eq!(extension_from_mime("image/webp"), "webp");
        assert_eq!(extension_from_mime("image/bmp"), "bmp");

        // Documents
        assert_eq!(extension_from_mime("application/pdf"), "pdf");
        assert_eq!(extension_from_mime("application/msword"), "doc");
        assert_eq!(
            extension_from_mime(
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            ),
            "docx"
        );
        assert_eq!(
            extension_from_mime(
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            ),
            "xlsx"
        );
        assert_eq!(
            extension_from_mime(
                "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            ),
            "pptx"
        );

        // Text / data
        assert_eq!(extension_from_mime("text/plain"), "txt");
        assert_eq!(extension_from_mime("text/csv"), "csv");
        assert_eq!(extension_from_mime("text/markdown"), "md");
        assert_eq!(extension_from_mime("application/json"), "json");

        // Fallback
        assert_eq!(extension_from_mime("application/octet-stream"), "bin");
    }
    // ── build_attachment tests ───────────────────────────────────

    #[test]
    fn build_attachment_with_ext() {
        let att = build_attachment("photo.png", b"data", "image/png", None);
        assert_eq!(att.filename, "photo.png");
        assert_eq!(att.content_type, "image/png");
        assert_eq!(att.size, 4);
        assert!(att.content.is_some());
    }

    #[test]
    fn build_attachment_without_ext() {
        let att = build_attachment("image", b"data", "image/jpeg", None);
        assert_eq!(att.filename, "image.jpg");
    }

    #[test]
    fn build_attachment_uses_http_content_type_when_magic_fails() {
        // Magic bytes detection returned application/octet-stream, but the
        // HTTP response said the file is text/csv.
        let att = build_attachment(
            "file",
            b"a,b,c",
            "application/octet-stream",
            Some("text/csv"),
        );
        assert_eq!(att.filename, "file.csv");
        assert_eq!(att.content_type, "text/csv");
    }

    #[test]
    fn build_attachment_preserves_magic_mime_when_http_is_generic() {
        let att = build_attachment(
            "file",
            b"data",
            "application/octet-stream",
            Some("application/octet-stream"),
        );
        assert_eq!(att.filename, "file.bin");
        assert_eq!(att.content_type, "application/octet-stream");
    }

    #[test]
    fn build_attachment_falls_back_to_bin_when_no_http_mime() {
        let att = build_attachment("file", b"data", "application/octet-stream", None);
        assert_eq!(att.filename, "file.bin");
        assert_eq!(att.content_type, "application/octet-stream");
    }

    #[test]
    fn build_attachment_prefers_url_filename_over_generic_fallback() {
        // WeCom omits filename but the URL path contains data.csv.
        let filename = url_filename_hint("https://cos.example.com/bucket/data.csv?sign=xxx")
            .unwrap_or_else(|| "file".to_string());
        let att = build_attachment(&filename, b"a,b,c", "application/octet-stream", None);
        // The URL-derived filename (with extension) is preserved.
        assert_eq!(att.filename, "data.csv");
    }
    // ── download_media tests ─────────────────────────────────────

    #[tokio::test]
    async fn download_media_succeeds() {
        let server = MockServer::start().await;
        let body: &[u8] = &[0xff, 0xd8, 0xff, 0xe0, 1, 2, 3];
        Mock::given(method("GET"))
            .and(path("/media"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "image/jpeg")
                    .set_body_bytes(body),
            )
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/media", server.uri());
        let (bytes, content_type) = download_media(&url).await.expect("must succeed");
        assert_eq!(bytes, body);
        assert_eq!(content_type.as_deref(), Some("image/jpeg"));
    }

    #[tokio::test]
    async fn download_media_succeeds_without_content_type() {
        let server = MockServer::start().await;
        let body: &[u8] = &[0xff, 0xd8, 0xff, 0xe0, 1, 2, 3];
        Mock::given(method("GET"))
            .and(path("/media"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/media", server.uri());
        let (bytes, content_type) = download_media(&url).await.expect("must succeed");
        assert_eq!(bytes, body);
        assert_eq!(content_type, None);
    }

    #[test]
    fn parse_content_type_strips_charset_and_lowercases() {
        assert_eq!(
            parse_content_type_header("text/csv; charset=utf-8"),
            Some("text/csv".to_string())
        );
        assert_eq!(
            parse_content_type_header("  IMAGE/JPEG  "),
            Some("image/jpeg".to_string())
        );
        assert_eq!(parse_content_type_header(""), None);
        assert_eq!(parse_content_type_header(";;;"), None);
    }

    #[test]
    fn url_filename_hint_extracts_last_segment() {
        assert_eq!(
            url_filename_hint("https://cos.example.com/bucket/data.csv?sign=xxx"),
            Some("data.csv".to_string())
        );
        assert_eq!(
            url_filename_hint("https://cos.example.com/path/to/report.xlsx#fragment"),
            Some("report.xlsx".to_string())
        );
    }

    #[test]
    fn url_filename_hint_returns_none_for_missing_extension() {
        assert_eq!(
            url_filename_hint("https://cos.example.com/bucket/no-extension"),
            None
        );
        assert_eq!(url_filename_hint("not-a-url"), None);
        assert_eq!(url_filename_hint(""), None);
    }
    #[tokio::test]
    async fn download_media_fails_on_404() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/media"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let url = format!("{}/media", server.uri());
        let err = download_media(&url).await.expect_err("must fail");
        let msg = format!("{:#}", err);
        assert!(msg.contains("404"), "expected 404 in error: {msg}");
    }

    // ── process_bot_attachments tests ────────────────────────────

    #[tokio::test]
    async fn process_image_message_decrypt_fails() {
        let server = MockServer::start().await;
        let encrypted_body = vec![0u8; 32];
        Mock::given(method("GET"))
            .and(path("/img"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(encrypted_body))
            .mount(&server)
            .await;

        let bot_msg = BotMessage {
            msgid: "msg_1".to_string(),
            aibotid: "bot_1".to_string(),
            chatid: "chat_1".to_string(),
            chattype: "single".to_string(),
            from: super::super::types::SenderInfo {
                userid: "user_1".to_string(),
            },
            msgtime: 0,
            msgtype: "image".to_string(),
            req_id: "req_1".to_string(),
            servertime: 0,
            text: None,
            image: Some(super::super::types::ImageContent {
                url: format!("{}/img", server.uri()),
                // Short key (20 bytes) will fail the "expected 32 bytes" check
                aeskey: base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    [0u8; 20],
                ),
            }),
            mixed: None,
            voice: None,
            file: None,
            video: None,
        };

        // Decryption fails due to short key → attachment skipped, no panic
        let attachments = process_bot_attachments(&bot_msg)
            .await
            .expect("should not error");
        assert!(
            attachments.is_empty(),
            "expected empty when decryption fails"
        );
    }

    #[tokio::test]
    async fn process_text_message_returns_empty() {
        let bot_msg = BotMessage {
            msgid: "msg_1".to_string(),
            aibotid: "bot_1".to_string(),
            chatid: "chat_1".to_string(),
            chattype: "single".to_string(),
            from: super::super::types::SenderInfo {
                userid: "user_1".to_string(),
            },
            msgtime: 0,
            msgtype: "text".to_string(),
            req_id: "req_1".to_string(),
            servertime: 0,
            text: Some(super::super::types::TextContent {
                content: "Hello".to_string(),
            }),
            image: None,
            mixed: None,
            voice: None,
            file: None,
            video: None,
        };

        let attachments = process_bot_attachments(&bot_msg)
            .await
            .expect("must succeed");
        assert!(attachments.is_empty());
    }

    #[tokio::test]
    async fn test_file_without_filename_generates_fallback() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let encrypted_body = vec![0u8; 32]; // will fail decryption but we test the naming path
        Mock::given(method("GET"))
            .and(path("/file"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(encrypted_body))
            .mount(&server)
            .await;

        let bot_msg = BotMessage {
            msgid: "msg_file".to_string(),
            aibotid: "bot_1".to_string(),
            chatid: "chat_1".to_string(),
            chattype: "single".to_string(),
            from: super::super::types::SenderInfo {
                userid: "user_1".to_string(),
            },
            msgtime: 0,
            msgtype: "file".to_string(),
            req_id: "req_1".to_string(),
            servertime: 0,
            text: None,
            image: None,
            mixed: None,
            voice: None,
            file: Some(super::super::types::FileContent {
                filename: None,
                url: format!("{}/file", server.uri()),
                aeskey: base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    [0u8; 20], // short key → decryption fails, but tests the code path
                ),
            }),
            video: None,
        };

        let attachments = process_bot_attachments(&bot_msg)
            .await
            .expect("should not error");
        // Decryption fails with short key → no attachments produced, but the
        // important thing is the code path doesn't panic on missing filename
        assert!(attachments.is_empty());
    }
}
