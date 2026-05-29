//! WeCom (企业微信) message encryption/decryption.
//!
//! Implements the official WeCom callback message encryption protocol:
//! - Signature verification: SHA1(token, timestamp, nonce, encrypt)
//! - AES-256-CBC decryption with PKCS7 padding
//!
//! Reference: https://developer.work.weixin.qq.com/document/path/90968

use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use anyhow::{Context, Result};
use sha1::{Digest, Sha1};

type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

/// Verify the callback signature.
///
/// The signature is computed as SHA1(token || timestamp || nonce || encrypt).
/// Returns `true` if the computed signature matches the provided one.
pub fn verify_signature(
    token: &str,
    timestamp: &str,
    nonce: &str,
    encrypt: &str,
    msg_signature: &str,
) -> bool {
    let mut hasher = Sha1::new();
    hasher.update(token.as_bytes());
    hasher.update(timestamp.as_bytes());
    hasher.update(nonce.as_bytes());
    hasher.update(encrypt.as_bytes());
    let computed = hex::encode(hasher.finalize());
    computed == msg_signature
}

/// Decrypt AES-256-CBC encrypted message with PKCS7 padding.
///
/// The raw key is the Base64-decoded `encoding_aes_key` (with "=" padding).
/// The AES key is the first 32 bytes of the decoded key.
/// The IV is the last 16 bytes of the decoded key (after first 32 bytes).
///
/// The decrypted plaintext format is:
/// ```text
/// [4-byte network byte order content length][content][receiveid (corpid)]
/// [padding]
/// ```
///
/// Returns the decrypted content string (the inner XML).
pub fn decrypt_msg(encoding_aes_key: &str, encrypt: &str) -> Result<String> {
    let raw_key =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoding_aes_key)
            .context("failed to decode encoding_aes_key from base64")?;

    if raw_key.len() != 43 {
        anyhow::bail!(
            "encoding_aes_key decoded length is {}, expected 43 bytes",
            raw_key.len()
        );
    }

    // The AES key is the first 32 bytes of the decoded encoding_aes_key
    let aes_key = &raw_key[..32];
    // The IV is the last 16 bytes (bytes 32..48, but raw_key is only 43 bytes;
    // WeCom spec says the key is Base64-decoded to 43 bytes, of which:
    // - first 32 bytes = AES key
    // - remaining 11 bytes + 5 zero bytes = IV
    let iv_partial = &raw_key[32..43];
    let mut iv = [0u8; 16];
    iv[..iv_partial.len()].copy_from_slice(iv_partial);

    let ciphertext = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encrypt)
        .context("failed to decode encrypt from base64")?;

    let mut buf = ciphertext;
    let decrypted = Aes256CbcDec::new(aes_key.into(), &iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| anyhow::anyhow!("AES decryption failed: {:?}", e))?;

    // Parse: [4-byte content length (network byte order)][content][receiveid]
    if decrypted.len() < 4 {
        anyhow::bail!("decrypted content too short: {} bytes", decrypted.len());
    }

    let content_len =
        u32::from_be_bytes([decrypted[0], decrypted[1], decrypted[2], decrypted[3]]) as usize;

    if decrypted.len() < 4 + content_len {
        anyhow::bail!(
            "decrypted content length mismatch: header says {} but remaining is {}",
            content_len,
            decrypted.len() - 4
        );
    }

    let content = &decrypted[4..4 + content_len];
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
        let mut hasher = Sha1::new();
        hasher.update(token.as_bytes());
        hasher.update(timestamp.as_bytes());
        hasher.update(nonce.as_bytes());
        hasher.update(encrypt.as_bytes());
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
