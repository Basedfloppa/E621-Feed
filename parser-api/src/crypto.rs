//! AES-256-GCM encryption for secrets at rest (per-account e621 API keys).
//!
//! The encryption key is derived from `config.e621_key_encryption_secret`
//! (SHA-256). When the operator leaves that secret empty, a compile-time
//! fallback salt is used and a startup warning is logged — the value is still
//! stored as genuine AES-GCM ciphertext (never plaintext), it just can't add
//! confidentiality against an attacker who also has the binary. Real
//! deployments should set a strong secret in `config.toml`.
//!
//! Storage format is `base64(nonce_12 || ciphertext+tag)`, so each blob owns
//! its random nonce and is authenticated/binding to that nonce.

use crate::models::cfg;
use aes_gcm::aead::{Aead, OsRng};
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// Nonce length for AES-GCM (96 bits, the recommended default).
const NONCE_LEN: usize = 12;

/// Fallback key material used only when `e621_key_encryption_secret` is empty.
/// Used in debug builds (tests/dev) so the suite stays runnable without a
/// secret; release builds refuse to operate with an empty secret instead.
const FALLBACK_SECRET: &[u8] = b"e621-account-parser:key-encryption:fallback:v1";

/// Warn about the empty secret exactly once, so key reads/tests/syncs under
/// load don't spam the log on every encrypt/decrypt.
fn log_empty_secret_once() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        warn!(
            "e621_key_encryption_secret is empty: keys are AES-GCM encrypted with a \
             fixed fallback key — set a strong secret in config.toml"
        );
    });
}

fn encryption_key() -> Result<[u8; 32], String> {
    let secret = cfg().e621_key_encryption_secret.clone();
    if secret.is_empty() {
        log_empty_secret_once();
        // In release builds the fixed fallback would let anyone with a DB dump
        // + the binary decrypt every stored key, so refuse to run on a missing
        // secret rather than silently degrade confidentiality.
        if !cfg!(debug_assertions) {
            return Err(
                "e621_key_encryption_secret is empty; refusing to use a fixed fallback key "
                    .to_string()
                    + "in a release build — set a strong secret in config.toml",
            );
        }
        Ok(Sha256::digest(FALLBACK_SECRET).into())
    } else {
        Ok(Sha256::digest(secret.as_bytes()).into())
    }
}

/// Encrypt `plaintext` → `base64(nonce || ciphertext)`.
pub fn encrypt(plaintext: &[u8]) -> Result<String, String> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&encryption_key()?));
    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| format!("AES-GCM encrypt failed: {e}"))?;
    let mut blob = Vec::with_capacity(NONCE_LEN + ct.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ct);
    Ok(B64.encode(&blob))
}

/// Decrypt a `base64(nonce || ciphertext)` blob produced by [`encrypt`].
pub fn decrypt(encoded: &str) -> Result<Vec<u8>, String> {
    let blob = B64
        .decode(encoded)
        .map_err(|e| format!("invalid base64 ciphertext: {e}"))?;
    if blob.len() < NONCE_LEN + 1 {
        return Err("ciphertext blob too short".to_string());
    }
    let (nonce_bytes, ct) = blob.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&encryption_key()?));
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ct)
        .map_err(|e| format!("AES-GCM decrypt failed (secret mismatch or corrupt blob): {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let plain = b"abc123-private-key";
        let enc = encrypt(plain).expect("encrypt");
        assert_ne!(enc, String::from_utf8_lossy(plain));
        assert!(
            !enc.contains("private-key"),
            "ciphertext must not contain plaintext"
        );
        assert_eq!(decrypt(&enc).expect("decrypt"), plain);
    }

    #[test]
    fn random_nonce_produces_distinct_blobs() {
        let a = encrypt(b"same").unwrap();
        let b = encrypt(b"same").unwrap();
        assert_ne!(a, b, "fresh nonce must yield distinct ciphertexts");
        assert_eq!(decrypt(&a).unwrap(), decrypt(&b).unwrap());
    }

    #[test]
    fn tamper_detected() {
        let enc = encrypt(b"value").unwrap();
        // Flip a char in the middle of the base64 payload.
        let mut bytes = B64.decode(&enc).unwrap();
        let idx = bytes.len() / 2;
        bytes[idx] ^= 0x01;
        let corrupted = B64.encode(&bytes);
        assert!(decrypt(&corrupted).is_err(), "tampered blob must fail auth");
    }
}
