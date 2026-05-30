//! WeCom (企业微信) message encryption/decryption.
//!
//! Implements the official WeCom callback message encryption protocol:
//! - Signature verification: SHA1(token, timestamp, nonce, encrypt)
//! - AES-256-CBC decryption with PKCS7 padding
//!
//! Reference: https://developer.work.weixin.qq.com/document/path/90968

use aes::cipher::{BlockDecryptMut, KeyIvInit};
use anyhow::{Context, Result};
use base64::{Engine, alphabet, engine::GeneralPurpose};
use sha1::{Digest, Sha1};

/// Permissive base64 engine that allows non-zero trailing bits.
///
/// WeCom's EncodingAESKey may have non-zero trailing bits in base64 padding,
/// which the standard strict decoder rejects. This engine accepts them.
static PERMISSIVE_BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    base64::engine::GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true),
);

type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

/// Verify the callback signature.
///
/// The signature is computed as SHA1(sorted(token, timestamp, nonce, encrypt)).
/// The four parameters are sorted lexicographically before concatenation.
/// Returns `true` if the computed signature matches the provided one.
///
/// Reference: <https://developer.work.weixin.qq.com/document/path/90968>
pub fn verify_signature(
    token: &str,
    timestamp: &str,
    nonce: &str,
    encrypt: &str,
    msg_signature: &str,
) -> bool {
    let mut params = [token, timestamp, nonce, encrypt];
    params.sort_unstable();

    let mut hasher = Sha1::new();
    for param in &params {
        hasher.update(param.as_bytes());
    }
    let computed = hex::encode(hasher.finalize());
    computed == msg_signature
}

/// Decrypt AES-256-CBC encrypted message with PKCS7 padding.
///
/// The raw key is the Base64-decoded `encoding_aes_key` (with "=" padding).
/// The AES key is all 32 bytes of the decoded key.
/// The IV is the first 16 bytes of the decoded key.
///
/// The decrypted plaintext format is:
/// ```text
/// [16-byte random][4-byte network byte order content length][content][receiveid (corpid)]
/// [padding]
/// ```
///
/// Returns the decrypted content string (the inner XML).
pub fn decrypt_msg(encoding_aes_key: &str, encrypt: &str) -> Result<String> {
    // Trim whitespace (including newlines) that may sneak in from env vars
    let encoding_aes_key = encoding_aes_key.trim();

    if encoding_aes_key.is_empty() {
        anyhow::bail!(
            "encoding_aes_key is empty (check config: if using ${{VAR}} syntax, ensure the environment variable is set)"
        );
    }

    // WeCom's encoding_aes_key is 43 chars with trailing '=' padding removed.
    // Re-add padding so standard base64 decoding works.
    let padded_key = match encoding_aes_key.len() % 4 {
        0 => encoding_aes_key.to_string(),
        n => format!("{}{}", encoding_aes_key, "=".repeat(4 - n)),
    };

    let raw_key = PERMISSIVE_BASE64.decode(&padded_key)
    .with_context(|| {
        format!(
            "failed to decode encoding_aes_key from base64 (len={}, padded_len={}, has_non_ascii={:?})",
            encoding_aes_key.len(),
            padded_key.len(),
            encoding_aes_key.bytes().any(|b| !b.is_ascii_alphanumeric()),
        )
    })?;

    if raw_key.len() != 32 {
        anyhow::bail!(
            "encoding_aes_key decoded length is {}, expected 32 bytes (AES-256 key)",
            raw_key.len()
        );
    }

    // The AES key is all 32 bytes of the decoded encoding_aes_key
    let aes_key = &raw_key[..32];
    // IV is the first 16 bytes of the AES key, per WeCom spec
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&raw_key[..16]);

    let ciphertext = PERMISSIVE_BASE64
        .decode(encrypt)
        .context("failed to decode encrypt from base64")?;

    let mut buf = ciphertext;
    let decrypted = Aes256CbcDec::new(aes_key.into(), &iv.into())
        .decrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(&mut buf)
        .map_err(|e| anyhow::anyhow!("AES decryption failed: {:?}", e))?;

    // WeCom uses PKCS#7 with block_size=32 (not the AES block size of 16).
    // Manual unpadding required.
    let pad_len = decrypted.last().copied().unwrap_or(0) as usize;
    if pad_len == 0 || pad_len > 32 {
        anyhow::bail!("invalid PKCS#7 padding length: {}", pad_len);
    }
    if !decrypted[decrypted.len() - pad_len..]
        .iter()
        .all(|&b| b == pad_len as u8)
    {
        anyhow::bail!("invalid PKCS#7 padding bytes");
    }
    let decrypted = &decrypted[..decrypted.len() - pad_len];

    // Parse: [16-byte random][4-byte content length (network byte order)][content][receiveid]
    if decrypted.len() < 16 + 4 {
        anyhow::bail!("decrypted content too short: {} bytes", decrypted.len());
    }

    let content_len =
        u32::from_be_bytes([decrypted[16], decrypted[17], decrypted[18], decrypted[19]]) as usize;

    if decrypted.len() < 16 + 4 + content_len {
        anyhow::bail!(
            "decrypted content length mismatch: header says {} but remaining is {}",
            content_len,
            decrypted.len() - 16 - 4
        );
    }

    let content = &decrypted[16 + 4..16 + 4 + content_len];
    let content_str =
        String::from_utf8(content.to_vec()).context("decrypted content is not valid UTF-8")?;

    Ok(content_str)
}

/// Get a random nonce string (for generating callback response).
pub fn generate_nonce() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signature_verification() {
        let token = "test_token";
        let timestamp = "1400000000";
        let nonce = "123456";
        let encrypt = "encrypted_content_placeholder";
        let msg_signature = compute_signature(token, timestamp, nonce, encrypt);

        assert!(verify_signature(
            token,
            timestamp,
            nonce,
            encrypt,
            &msg_signature
        ));
        assert!(!verify_signature(
            token,
            timestamp,
            nonce,
            encrypt,
            "invalid_signature"
        ));
    }

    fn compute_signature(token: &str, timestamp: &str, nonce: &str, encrypt: &str) -> String {
        let mut params = [token, timestamp, nonce, encrypt];
        params.sort_unstable();

        let mut hasher = Sha1::new();
        for param in &params {
            hasher.update(param.as_bytes());
        }
        hex::encode(hasher.finalize())
    }

    #[test]
    fn test_signature_different_order_fails() {
        // Changing the order of arguments should produce different signature
        let sig1 = compute_signature("token_a", "1000", "nonce1", "encrypt1");
        let sig2 = compute_signature("token_b", "1000", "nonce1", "encrypt1");
        assert_ne!(sig1, sig2);
    }

    #[test]
    fn test_decrypt_invalid_key() {
        let result = decrypt_msg("not-base64!!!", "dGVzdA==");
        assert!(result.is_err());
    }

    #[test]
    fn test_decrypt_short_key() {
        // Key that decodes to less than 43 bytes
        let short_key =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [0u8; 20]);
        let result = decrypt_msg(&short_key, "dGVzdA==");
        assert!(result.is_err());
    }

    #[test]
    fn test_decrypt_invalid_encrypt() {
        // Invalid base64 in encrypt field
        let long_key =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [0u8; 43]);
        let result = decrypt_msg(&long_key, "not-base64!!!");
        assert!(result.is_err());
    }

    #[test]
    fn test_generate_nonce_is_non_empty() {
        let nonce = generate_nonce();
        assert!(!nonce.is_empty());
    }

    #[test]
    fn test_generate_nonce_unique() {
        let nonce1 = generate_nonce();
        let nonce2 = generate_nonce();
        // Very unlikely to collide
        assert_ne!(nonce1, nonce2);
    }
}
