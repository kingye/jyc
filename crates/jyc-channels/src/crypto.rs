//! Shared crypto helpers for WeCom-family channels.
//!
//! Both `wecom` (callback message encryption) and `wecom_bot` (media payload
//! decryption) implement the same AES-256-CBC scheme with a permissive base64
//! decoder. The identical parts live here; each caller keeps its own
//! PKCS#7 padding and post-processing rules (WeCom callbacks use block size
//! 32 + XML framing, media payloads use block size 16 + raw bytes).

use aes::cipher::{BlockModeDecrypt, KeyIvInit};
use anyhow::{Context, Result};
use base64::{Engine, alphabet, engine::GeneralPurpose};

/// Permissive base64 engine that allows non-zero trailing bits.
///
/// WeCom's encoding keys may have non-zero trailing bits in base64 padding,
/// which the standard strict decoder rejects. This engine accepts them.
static PERMISSIVE_BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    base64::engine::GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true),
);

/// Decode a WeCom-style 32-byte AES-256 key (base64, `=` padding optional).
///
/// Returns the 32-byte key and the 16-byte IV (first half of the key), per
/// the WeCom spec: AES key = all 32 decoded bytes, IV = first 16 bytes.
pub(crate) fn decode_aes256_key(b64_key: &str) -> Result<([u8; 32], [u8; 16])> {
    let key = b64_key.trim();
    if key.is_empty() {
        anyhow::bail!("aes key is empty");
    }

    // WeCom sometimes omits the trailing '=' padding.
    let padded = match key.len() % 4 {
        0 => key.to_string(),
        n => format!("{}{}", key, "=".repeat(4 - n)),
    };

    let raw = PERMISSIVE_BASE64.decode(&padded).with_context(|| {
        format!(
            "failed to decode aes key from base64 (len={}, padded_len={})",
            key.len(),
            padded.len()
        )
    })?;

    if raw.len() != 32 {
        anyhow::bail!(
            "aes key decoded length is {}, expected 32 bytes (AES-256)",
            raw.len()
        );
    }

    let key: [u8; 32] = raw[..32].try_into().expect("length checked == 32");
    let iv: [u8; 16] = raw[..16].try_into().expect("16 <= 32");
    Ok((key, iv))
}

/// Decode base64 with the permissive engine (WeCom trailing-bits tolerance).
pub(crate) fn base64_decode(s: &str) -> Result<Vec<u8>> {
    Ok(PERMISSIVE_BASE64.decode(s)?)
}

/// AES-256-CBC decrypt (NoPadding).
///
/// Returns the raw decrypted bytes **including** any PKCS#7 padding; callers
/// strip padding per their own protocol.
pub(crate) fn decrypt_aes256_cbc(
    key: &[u8; 32],
    iv: &[u8; 16],
    ciphertext: &[u8],
) -> Result<Vec<u8>> {
    let mut buf = ciphertext.to_vec();
    let decrypted = cbc::Decryptor::<aes::Aes256>::new_from_slices(key, iv)
        .map_err(|e| anyhow::anyhow!("invalid AES key/iv length: {e:?}"))?
        .decrypt_padded::<aes::cipher::block_padding::NoPadding>(&mut buf)
        .map_err(|e| anyhow::anyhow!("AES-256-CBC decryption failed: {e:?}"))?;
    Ok(decrypted.to_vec())
}
