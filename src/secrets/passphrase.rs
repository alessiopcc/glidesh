//! The `passphrase` secrets provider: wraps the run's DEK under a key derived from a
//! passphrase (scrypt) and sealed with XChaCha20-Poly1305.
//!
//! The wrapped-DEK blob is stored in `secrets.kdl` as `encryptedkey "v1:<base64url>"`.
//! Because the AEAD tag authenticates the DEK, a wrong passphrase fails decryption
//! cleanly — the blob is its own verifier, so no separate check value is needed.
//!
//! This is the envelope's passphrase mode. The `age`-recipient provider (SSH/X25519
//! keys) is a follow-up that wraps the same DEK to a set of recipients instead.

use crate::error::GlideshError;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{Key, KeyInit, XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

const WRAP_PREFIX: &str = "v1:";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const DEK_LEN: usize = 32;
const TAG_LEN: usize = 16;

/// scrypt cost for **release** builds: log2(N)=16 (N=65536), r=8, p=1 → ~64 MiB
/// memory-hardness. `secrets.kdl` is committed and therefore offline-grindable, so the cost
/// is raised above the default while keeping per-unlock latency snappy (~0.1s optimized).
/// The chosen `log_n` is stored in each wrapped blob, so this can be raised later without
/// invalidating existing files.
#[cfg(not(debug_assertions))]
const SCRYPT_LOG_N: u8 = 16;

/// scrypt cost for **debug/test** builds: log2(N)=12. Unoptimized scrypt is orders of
/// magnitude slower, so a low cost keeps the test suite (and dev CLI) fast. Blobs are
/// self-describing, so a dev-created file still unlocks under a release binary and can be
/// rotated to the stronger cost with `glidesh secret rekey`.
#[cfg(debug_assertions)]
const SCRYPT_LOG_N: u8 = 12;
const SCRYPT_R: u32 = 8;
const SCRYPT_P: u32 = 1;

/// Passphrase-based DEK wrapping.
pub struct PassphraseProvider {
    passphrase: Zeroizing<String>,
}

impl PassphraseProvider {
    pub fn new(passphrase: String) -> Self {
        Self {
            passphrase: Zeroizing::new(passphrase),
        }
    }

    /// Wrap a freshly generated DEK, returning the `v1:<base64url>` blob for `encryptedkey`.
    ///
    /// The blob is `log_n(1) ‖ salt(16) ‖ nonce(24) ‖ sealed`. Storing `log_n` makes the
    /// blob self-describing: raising [`SCRYPT_LOG_N`] later only affects new wraps, and
    /// every existing `encryptedkey` still unwraps with the cost it was written at.
    pub fn wrap_dek(&self, dek: &[u8; DEK_LEN]) -> Result<String, GlideshError> {
        let log_n = SCRYPT_LOG_N;
        let mut salt = [0u8; SALT_LEN];
        super::random_bytes(&mut salt);
        let kek = derive_kek(&self.passphrase, &salt, log_n)?;

        let mut nonce = [0u8; NONCE_LEN];
        super::random_bytes(&mut nonce);
        let cipher = XChaCha20Poly1305::new(Key::from_slice(&kek[..]));
        let sealed = cipher
            .encrypt(XNonce::from_slice(&nonce), dek.as_slice())
            .map_err(|_| wrap_err("failed to seal data key"))?;

        let mut blob = Vec::with_capacity(1 + SALT_LEN + NONCE_LEN + sealed.len());
        blob.push(log_n);
        blob.extend_from_slice(&salt);
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&sealed);
        Ok(format!("{WRAP_PREFIX}{}", URL_SAFE_NO_PAD.encode(blob)))
    }

    /// Recover the DEK from an `encryptedkey` blob. A wrong passphrase surfaces as a
    /// clean [`GlideshError::Secret`] (AEAD tag mismatch), never a panic.
    pub fn unwrap_dek(&self, wrapped: &str) -> Result<Zeroizing<[u8; DEK_LEN]>, GlideshError> {
        let body = wrapped
            .strip_prefix(WRAP_PREFIX)
            .ok_or_else(|| wrap_err("unsupported encryptedkey version"))?;
        let blob = URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| wrap_err("invalid base64 in encryptedkey"))?;
        if blob.len() < 1 + SALT_LEN + NONCE_LEN + DEK_LEN + TAG_LEN {
            return Err(wrap_err("encryptedkey blob too short"));
        }
        let log_n = blob[0];
        let salt = &blob[1..1 + SALT_LEN];
        let nonce = &blob[1 + SALT_LEN..1 + SALT_LEN + NONCE_LEN];
        let sealed = &blob[1 + SALT_LEN + NONCE_LEN..];

        let kek = derive_kek(&self.passphrase, salt, log_n)?;
        let cipher = XChaCha20Poly1305::new(Key::from_slice(&kek[..]));
        // Wrap the decrypted bytes so the raw DEK is wiped from the heap on drop rather
        // than lingering in a freed `Vec` after it is copied into the fixed array.
        let dek_bytes = Zeroizing::new(cipher.decrypt(XNonce::from_slice(nonce), sealed).map_err(
            |_| GlideshError::Secret {
                message: "wrong passphrase (could not unlock the secrets data key)".to_string(),
            },
        )?);
        if dek_bytes.len() != DEK_LEN {
            return Err(wrap_err("unexpected data key length"));
        }
        let mut dek = Zeroizing::new([0u8; DEK_LEN]);
        dek.copy_from_slice(&dek_bytes[..]);
        Ok(dek)
    }
}

/// Generate a random 256-bit DEK.
pub fn generate_dek() -> Zeroizing<[u8; DEK_LEN]> {
    let mut dek = Zeroizing::new([0u8; DEK_LEN]);
    super::random_bytes(&mut dek[..]);
    dek
}

fn derive_kek(
    passphrase: &str,
    salt: &[u8],
    log_n: u8,
) -> Result<Zeroizing<[u8; DEK_LEN]>, GlideshError> {
    let params = scrypt::Params::new(log_n, SCRYPT_R, SCRYPT_P, DEK_LEN)
        .map_err(|e| wrap_err(&format!("invalid scrypt params: {e}")))?;
    let mut kek = Zeroizing::new([0u8; DEK_LEN]);
    scrypt::scrypt(passphrase.as_bytes(), salt, &params, &mut kek[..])
        .map_err(|e| wrap_err(&format!("key derivation failed: {e}")))?;
    Ok(kek)
}

fn wrap_err(reason: &str) -> GlideshError {
    GlideshError::Secret {
        message: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_unwrap_round_trip() {
        let dek = generate_dek();
        let provider = PassphraseProvider::new("correct horse".to_string());
        let wrapped = provider.wrap_dek(&dek).unwrap();
        assert!(wrapped.starts_with(WRAP_PREFIX));
        let recovered = provider.unwrap_dek(&wrapped).unwrap();
        assert_eq!(*recovered, *dek);
    }

    #[test]
    fn wrong_passphrase_fails() {
        let dek = generate_dek();
        let wrapped = PassphraseProvider::new("right".to_string())
            .wrap_dek(&dek)
            .unwrap();
        let err = PassphraseProvider::new("wrong".to_string())
            .unwrap_dek(&wrapped)
            .unwrap_err();
        assert!(matches!(err, GlideshError::Secret { .. }));
    }

    #[test]
    fn rewrap_preserves_dek() {
        // Rotating the passphrase re-wraps the same DEK, so value tokens stay valid.
        let dek = generate_dek();
        let old = PassphraseProvider::new("old".to_string());
        let new = PassphraseProvider::new("new".to_string());
        let blob = old.wrap_dek(&dek).unwrap();
        let recovered = old.unwrap_dek(&blob).unwrap();
        let rewrapped = new.wrap_dek(&recovered).unwrap();
        assert_eq!(*new.unwrap_dek(&rewrapped).unwrap(), *dek);
    }

    #[test]
    fn malformed_blob_rejected() {
        let p = PassphraseProvider::new("x".to_string());
        assert!(p.unwrap_dek("v2:abc").is_err());
        assert!(p.unwrap_dek("v1:!!!notbase64").is_err());
    }
}
