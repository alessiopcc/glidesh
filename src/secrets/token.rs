//! The `secret:v1:` value token and its XChaCha20-Poly1305 encryption under the
//! run's data-encryption key (DEK).
//!
//! A token is `secret:v1:<base64url( nonce(24) ‖ ciphertext ‖ tag(16) )>`. Tokens are
//! self-describing and short; the DEK that decrypts them is unwrapped once per run from
//! the provider metadata in `secrets.kdl` (see [`super::passphrase`]).

use super::random_bytes;
use crate::error::GlideshError;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{Key, KeyInit, XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

/// Prefix marking a value as an encrypted secret token.
pub const SECRET_PREFIX: &str = "secret:v1:";

/// XChaCha20-Poly1305 nonce length.
const NONCE_LEN: usize = 24;
/// Poly1305 authentication tag length.
const TAG_LEN: usize = 16;

/// True if the whole string is a secret token.
pub fn is_secret_token(s: &str) -> bool {
    s.starts_with(SECRET_PREFIX)
}

/// True if a secret token appears anywhere in the string (inline usage).
pub fn contains_secret_token(s: &str) -> bool {
    s.contains(SECRET_PREFIX)
}

/// Encrypt `plaintext` under `dek`, producing a `secret:v1:…` token.
pub fn encrypt_value(dek: &[u8; 32], plaintext: &[u8]) -> Result<String, GlideshError> {
    let cipher = XChaCha20Poly1305::new(Key::from_slice(dek));
    let mut nonce = [0u8; NONCE_LEN];
    random_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce), plaintext)
        .map_err(|_| GlideshError::Secret {
            message: "failed to encrypt secret value".to_string(),
        })?;

    let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&ciphertext);
    Ok(format!("{}{}", SECRET_PREFIX, URL_SAFE_NO_PAD.encode(blob)))
}

/// Decrypt a `secret:v1:…` token under `dek`. The plaintext is returned in a
/// [`Zeroizing`] wrapper so it is wiped from memory when dropped.
pub fn decrypt_value(dek: &[u8; 32], token: &str) -> Result<Zeroizing<String>, GlideshError> {
    let body = token
        .strip_prefix(SECRET_PREFIX)
        .ok_or_else(|| malformed("not a secret token"))?;
    let blob = URL_SAFE_NO_PAD
        .decode(body)
        .map_err(|_| malformed("invalid base64 in secret token"))?;
    if blob.len() < NONCE_LEN + TAG_LEN {
        return Err(malformed("secret token too short"));
    }
    let (nonce, ciphertext) = blob.split_at(NONCE_LEN);
    let cipher = XChaCha20Poly1305::new(Key::from_slice(dek));
    // Wrap the plaintext bytes so they are wiped on drop, including on the UTF-8 error path.
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(XNonce::from_slice(nonce), ciphertext)
            .map_err(|_| GlideshError::Secret {
                message: "failed to decrypt secret value (wrong key or corrupted token)"
                    .to_string(),
            })?,
    );
    let text = std::str::from_utf8(&plaintext)
        .map_err(|_| malformed("secret is not valid UTF-8"))?
        .to_string();
    Ok(Zeroizing::new(text))
}

fn malformed(reason: &str) -> GlideshError {
    GlideshError::Secret {
        message: format!("malformed secret token: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dek() -> [u8; 32] {
        let mut k = [0u8; 32];
        random_bytes(&mut k);
        k
    }

    #[test]
    fn round_trip() {
        let k = dek();
        let token = encrypt_value(&k, b"hunter2").unwrap();
        assert!(is_secret_token(&token));
        assert_eq!(decrypt_value(&k, &token).unwrap().as_str(), "hunter2");
    }

    #[test]
    fn distinct_nonces_produce_distinct_tokens() {
        let k = dek();
        let a = encrypt_value(&k, b"same").unwrap();
        let b = encrypt_value(&k, b"same").unwrap();
        assert_ne!(a, b, "nonce reuse would make identical ciphertext");
    }

    #[test]
    fn wrong_key_fails_cleanly() {
        let token = encrypt_value(&dek(), b"secret").unwrap();
        let err = decrypt_value(&dek(), &token).unwrap_err();
        assert!(matches!(err, GlideshError::Secret { .. }));
    }

    #[test]
    fn malformed_token_is_rejected() {
        assert!(decrypt_value(&dek(), "secret:v1:not-base64!!").is_err());
        assert!(decrypt_value(&dek(), "plain text").is_err());
    }

    #[test]
    fn detects_inline_token() {
        let token = encrypt_value(&dek(), b"x").unwrap();
        assert!(contains_secret_token(&format!("psql {token}@db")));
        assert!(!contains_secret_token("no secrets here"));
    }
}
