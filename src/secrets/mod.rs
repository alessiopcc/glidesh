//! Secrets: encryption-at-rest for variable values (the Vault / Pulumi-config equivalent).
//!
//! Values are `secret:v1:…` tokens ([`token`]) encrypted under a per-run data-encryption
//! key (DEK). The DEK is wrapped by a provider ([`passphrase`] or [`age`]) and stored,
//! together with the encrypted values, in a committed `secrets.kdl` ([`config`]). At run
//! time the DEK is unwrapped once, every token reachable through the variable system is
//! decrypted, and each plaintext is registered in a [`SecretRegistry`] so it can be scrubbed
//! from all output.
//!
//! # What is wiped from memory, and what is not
//!
//! Keys and ciphertext-adjacent material are held in [`Zeroizing`] and wiped on drop: the
//! DEK, the KEK derived from a passphrase, the plaintext a token decrypts to, and the
//! registry's copy of it.
//!
//! Decrypted values are *not* wiped once they enter the variable system. `decrypt_token`
//! hands back an ordinary `String` because that is what the vars map, the template data, and
//! module parameters are made of; wrapping those would mean threading [`Zeroizing`] through
//! the whole configuration and executor layer for no gain against any attacker the tool can
//! actually defend against. A decrypted secret therefore lives in process memory for the
//! duration of the run.
//!
//! None of this defends against an attacker who can read this process's memory, a core dump,
//! or swap — that is the operating system's boundary, not glidesh's. What zeroization buys
//! is narrower and still worth having: freed allocations do not keep key material around
//! after the code that owned it is done.

pub mod age;
pub mod config;
pub mod passphrase;
pub mod store;
pub mod token;

use crate::config::template::TemplateData;
use crate::error::GlideshError;
use config::{Provider, SecretsConfig};
use passphrase::PassphraseProvider;
use rand::RngCore;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use zeroize::Zeroizing;

/// Fill a buffer with cryptographically secure random bytes. Shared by [`token`] and
/// [`passphrase`] for nonces, salts, and key generation.
pub(crate) fn random_bytes(buf: &mut [u8]) {
    rand::rngs::OsRng.fill_bytes(buf);
}

/// What can unwrap the data key, which depends on the provider that wrapped it.
#[derive(Debug, Clone)]
pub enum Identity {
    /// A passphrase, for the `passphrase` provider.
    Passphrase(String),
    /// The path to an SSH private key, for the `age` provider.
    SshKey(std::path::PathBuf),
}

/// The secrets identity, sourced once at startup. Global because it applies to the whole
/// run; process memory only, never logged or persisted. Mirrors [`crate::modules`]
/// escalation password handling.
static IDENTITY: OnceLock<Option<Identity>> = OnceLock::new();

/// Set the global secrets identity. Idempotent; the first value wins.
pub fn set_identity(id: Option<Identity>) {
    let _ = IDENTITY.set(id);
}

/// The configured secrets identity, if any.
pub fn identity() -> Option<&'static Identity> {
    IDENTITY.get().and_then(|p| p.as_ref())
}

/// Plaintexts shorter than this are recorded but never masked. Replacing a one- or
/// two-character value everywhere it appears would shred unrelated output while barely
/// protecting anything, so [`SecretRegistry::register`] warns instead. Short values are
/// still withheld from external plugins, which costs nothing.
pub const MIN_REDACTABLE_LEN: usize = 4;

/// The set of decrypted plaintexts. Populated lazily as tokens are decrypted, and used
/// for both directions of the trust boundary: scrubbing values out of emitted output
/// ([`Self::redact`], via the executor's event sink) and keeping them from crossing into
/// a third-party process ([`Self::contains_secret`], via the external-plugin runner).
#[derive(Default)]
pub struct SecretRegistry {
    /// Wrapped so the run's longest-lived collection of plaintext is wiped when the run
    /// ends, rather than left in freed heap. See the memory note in this module's docs for
    /// what that does and does not buy.
    plaintexts: RwLock<Vec<Zeroizing<String>>>,
}

impl SecretRegistry {
    fn register(&self, plaintext: &str) {
        if plaintext.is_empty() {
            return;
        }
        let mut guard = self.plaintexts.write().unwrap();
        if !guard.iter().any(|p| p.as_str() == plaintext) {
            if plaintext.len() < MIN_REDACTABLE_LEN {
                tracing::warn!(
                    "a decrypted secret is only {} characters long: too short to mask \
                     without shredding unrelated output, so it will NOT be redacted from \
                     the TUI or run logs (it is still withheld from external plugins)",
                    plaintext.len()
                );
            }
            guard.push(Zeroizing::new(plaintext.to_string()));
            // Keep the list longest-first so `redact` can iterate without re-sorting on
            // every call: a secret that is a substring of another must be masked after it.
            guard.sort_by_key(|s| std::cmp::Reverse(s.len()));
        }
    }

    /// True if nothing has been registered (fast path: redaction is a no-op).
    pub fn is_empty(&self) -> bool {
        self.plaintexts.read().unwrap().is_empty()
    }

    /// True if any registered secret appears anywhere in `text`. Unlike [`Self::redact`]
    /// this ignores [`MIN_REDACTABLE_LEN`]: a value too short to mask in output is still
    /// worth withholding from a process that has no business seeing it.
    pub fn contains_secret(&self, text: &str) -> bool {
        self.plaintexts
            .read()
            .unwrap()
            .iter()
            .any(|p| text.contains(p.as_str()))
    }

    /// Replace every registered plaintext in `text` with `***`. Registered secrets are
    /// kept longest-first (see [`Self::register`]) so a secret that is a substring of
    /// another cannot leak the longer one's remainder. Values below
    /// [`MIN_REDACTABLE_LEN`] are left alone.
    pub fn redact(&self, text: &str) -> String {
        let guard = self.plaintexts.read().unwrap();
        if guard.is_empty() {
            return text.to_string();
        }
        let mut out = text.to_string();
        for secret in guard.iter() {
            // Longest-first, so everything from here on is below the threshold.
            if secret.len() < MIN_REDACTABLE_LEN {
                break;
            }
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), "***");
            }
        }
        out
    }
}

/// The run's secrets context: an optional unwrapped DEK plus the redaction registry.
///
/// - No `secrets.kdl` → `dek: None, has_metadata: false`: a token anywhere is a hard error.
/// - `secrets.kdl` present but no identity → `dek: None, has_metadata: true`: tokens error
///   with a "provide the passphrase" hint; runs that touch no secret still succeed.
/// - identity present → `dek: Some(_)`: tokens decrypt.
pub struct Secrets {
    dek: Option<Zeroizing<[u8; 32]>>,
    has_metadata: bool,
    registry: Arc<SecretRegistry>,
}

impl Secrets {
    /// A context with no secrets configured.
    pub fn locked() -> Arc<Secrets> {
        Arc::new(Secrets {
            dek: None,
            has_metadata: false,
            registry: Arc::new(SecretRegistry::default()),
        })
    }

    /// Build the run context from a parsed provider block and the available identity.
    /// A present config + present identity unwraps the DEK now (wrong passphrase fails
    /// fast). A present config + no identity defers: tokens error only if actually used.
    pub fn open(
        config: Option<&SecretsConfig>,
        identity: Option<&Identity>,
    ) -> Result<Arc<Secrets>, GlideshError> {
        match config {
            None => Ok(Secrets::locked()),
            Some(cfg) => {
                let dek = match identity {
                    None => None,
                    Some(id) => Some(unwrap_dek(cfg, id)?),
                };
                Ok(Arc::new(Secrets {
                    dek,
                    has_metadata: true,
                    registry: Arc::new(SecretRegistry::default()),
                }))
            }
        }
    }

    pub fn registry(&self) -> Arc<SecretRegistry> {
        self.registry.clone()
    }

    /// Decrypt one token, registering its plaintext for redaction.
    pub fn decrypt_token(&self, token: &str) -> Result<String, GlideshError> {
        let dek = self.dek.as_ref().ok_or_else(|| self.locked_error())?;
        let plaintext = token::decrypt_value(dek, token)?;
        self.registry.register(plaintext.as_str());
        Ok(plaintext.as_str().to_string())
    }

    /// Decrypt any whole-value tokens in a flat var map, in place.
    fn decrypt_map(&self, map: &mut HashMap<String, String>) -> Result<(), GlideshError> {
        for value in map.values_mut() {
            if token::is_secret_token(value) {
                *value = self.decrypt_token(value)?;
            }
        }
        Ok(())
    }

    /// Decrypt any whole-value tokens in a flat var map, in place.
    pub fn decrypt_vars(&self, vars: &mut HashMap<String, String>) -> Result<(), GlideshError> {
        self.decrypt_map(vars)
    }

    /// Decrypt tokens hiding in inventory `@`-refs and structured collections, in place.
    pub fn decrypt_template_data(&self, data: &mut TemplateData) -> Result<(), GlideshError> {
        self.decrypt_map(&mut data.extra_vars)?;
        for rows in data.collections.values_mut() {
            for row in rows.iter_mut() {
                self.decrypt_map(row)?;
            }
        }
        Ok(())
    }

    /// Decrypt tokens embedded inside a larger string (inline usage in a task arg).
    pub fn decrypt_inline(&self, s: &str) -> Result<String, GlideshError> {
        if !token::contains_secret_token(s) {
            return Ok(s.to_string());
        }
        token::rewrite_tokens(s, |tok| self.decrypt_token(tok))
    }

    fn locked_error(&self) -> GlideshError {
        // `has_metadata` is false both when no file was found and when one was found without
        // a `secrets` block, so the hint speaks to the missing provider metadata rather than
        // claiming a file that may be sitting right there does not exist.
        let hint = if self.has_metadata {
            "provide the passphrase via GLIDESH_SECRET_PASS or --ask-secret-pass"
        } else {
            "no secrets provider is configured — run `glidesh secret init` to create or \
             initialize a secrets.kdl, or point at an existing one with --secrets / GLIDESH_SECRETS"
        };
        GlideshError::Secret {
            message: format!("encrypted value found but the secrets key is unavailable — {hint}"),
        }
    }
}

/// Unwrap the DEK using the configured provider and identity. A mismatch between the two
/// is reported in terms of what the file needs, not what the caller happened to supply.
pub fn unwrap_dek(
    config: &SecretsConfig,
    identity: &Identity,
) -> Result<Zeroizing<[u8; 32]>, GlideshError> {
    match (&config.provider, identity) {
        (Provider::Passphrase, Identity::Passphrase(pass)) => {
            PassphraseProvider::new(pass.clone()).unwrap_dek(&config.encryptedkey)
        }
        (Provider::Age, Identity::SshKey(path)) => age::unwrap_dek(&config.encryptedkey, path),
        (Provider::Passphrase, _) => Err(GlideshError::Secret {
            message: "this secrets file is passphrase-protected: set GLIDESH_SECRET_PASS or \
                      use --ask-secret-pass"
                .to_string(),
        }),
        (Provider::Age, _) => Err(GlideshError::Secret {
            message: "this secrets file is wrapped to SSH recipients: point at your private \
                      key with --secret-identity or GLIDESH_SECRET_IDENTITY"
                .to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::passphrase::{PassphraseProvider, generate_dek};

    fn seeded() -> (Arc<Secrets>, [u8; 32]) {
        let dek = generate_dek();
        let secrets = Arc::new(Secrets {
            dek: Some(dek.clone()),
            has_metadata: true,
            registry: Arc::new(SecretRegistry::default()),
        });
        (secrets, *dek)
    }

    #[test]
    fn redact_handles_substring_secrets_without_leaking() {
        // "pass" is a substring of "password"; longest-first replacement must fully
        // mask "password" rather than leaving "word".
        let reg = SecretRegistry::default();
        reg.register("pass");
        reg.register("password");
        assert_eq!(reg.redact("login password now"), "login *** now");
        assert_eq!(reg.redact("bare pass end"), "bare *** end");
    }

    #[test]
    fn short_secrets_are_withheld_but_not_masked() {
        let reg = SecretRegistry::default();
        reg.register("ab");
        // Masking a two-character value would turn every unrelated "ab" into ***.
        assert_eq!(
            reg.redact("a stable absolute path"),
            "a stable absolute path"
        );
        // It is still known to be a secret, so it never reaches an external plugin.
        assert!(reg.contains_secret("token=ab"));
        assert!(!reg.contains_secret("nothing here"));
    }

    #[test]
    fn contains_secret_matches_embedded_values() {
        let reg = SecretRegistry::default();
        reg.register("hunter2");
        assert!(reg.contains_secret("postgres://app:hunter2@db/prod"));
        assert!(!reg.contains_secret("postgres://app@db/prod"));
    }

    #[test]
    fn decrypts_and_registers_for_redaction() {
        let (secrets, dek) = seeded();
        let tok = token::encrypt_value(&dek, b"hunter2").unwrap();
        let mut vars = HashMap::from([("db".to_string(), tok)]);
        secrets.decrypt_vars(&mut vars).unwrap();
        assert_eq!(vars.get("db").unwrap(), "hunter2");
        assert_eq!(secrets.registry().redact("pw=hunter2 end"), "pw=*** end");
    }

    #[test]
    fn inline_token_decrypts() {
        let (secrets, dek) = seeded();
        let tok = token::encrypt_value(&dek, b"s3cr3t").unwrap();
        let out = secrets
            .decrypt_inline(&format!("psql -p {tok} db"))
            .unwrap();
        assert_eq!(out, "psql -p s3cr3t db");
    }

    #[test]
    fn token_without_key_is_actionable_error() {
        let secrets = Secrets::locked();
        let err = secrets
            .decrypt_token("secret:v1:AAAA")
            .unwrap_err()
            .to_string();
        assert!(err.contains("secret init"), "got: {err}");
    }

    #[test]
    fn locked_with_metadata_hints_passphrase() {
        let secrets = Secrets {
            dek: None,
            has_metadata: true,
            registry: Arc::new(SecretRegistry::default()),
        };
        let err = secrets
            .decrypt_token("secret:v1:AAAA")
            .unwrap_err()
            .to_string();
        assert!(err.contains("passphrase"), "got: {err}");
    }

    #[test]
    fn open_unwraps_with_passphrase() {
        let dek = generate_dek();
        let wrapped = PassphraseProvider::new("pw".to_string())
            .wrap_dek(&dek)
            .unwrap();
        let cfg = SecretsConfig {
            provider: Provider::Passphrase,
            encryptedkey: wrapped,
            recipients: Vec::new(),
        };
        let identity = Identity::Passphrase("pw".to_string());
        let secrets = Secrets::open(Some(&cfg), Some(&identity)).unwrap();
        let tok = token::encrypt_value(&dek, b"v").unwrap();
        assert_eq!(secrets.decrypt_token(&tok).unwrap(), "v");
    }
}
