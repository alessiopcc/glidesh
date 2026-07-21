//! Secrets: encryption-at-rest for variable values (the Vault / Pulumi-config equivalent).
//!
//! Values are `secret:v1:…` tokens ([`token`]) encrypted under a per-run data-encryption
//! key (DEK). The DEK is wrapped by a provider ([`passphrase`]) and stored, together with
//! the encrypted values, in a committed `secrets.kdl` ([`config`]). At run time the DEK is
//! unwrapped once, every token reachable through the variable system is decrypted, and each
//! plaintext is registered in a [`SecretRegistry`] so it can be scrubbed from all output.

pub mod config;
pub mod passphrase;
pub mod store;
pub mod token;

use crate::config::template::TemplateData;
use crate::error::GlideshError;
use config::{Provider, SecretsConfig};
use passphrase::PassphraseProvider;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use zeroize::Zeroizing;

/// The secrets identity, sourced once at startup: the passphrase (`GLIDESH_SECRET_PASS`
/// or `--ask-secret-pass`). Global because it applies to the whole run; process memory
/// only, never logged or persisted. Mirrors [`crate::modules`] escalation password handling.
static IDENTITY: OnceLock<Option<String>> = OnceLock::new();

/// Set the global secrets identity. Idempotent; the first value wins.
pub fn set_identity(id: Option<String>) {
    let _ = IDENTITY.set(id);
}

/// The configured secrets identity, if any.
pub fn identity() -> Option<&'static str> {
    IDENTITY.get().and_then(|p| p.as_deref())
}

/// The set of decrypted plaintexts to scrub from emitted output. Populated lazily as
/// tokens are decrypted; consulted by the executor's redacting event sink.
#[derive(Default)]
pub struct SecretRegistry {
    plaintexts: RwLock<Vec<String>>,
}

impl SecretRegistry {
    fn register(&self, plaintext: &str) {
        if plaintext.is_empty() {
            return;
        }
        let mut guard = self.plaintexts.write().unwrap();
        if !guard.iter().any(|p| p == plaintext) {
            guard.push(plaintext.to_string());
        }
    }

    /// True if nothing has been registered (fast path: redaction is a no-op).
    pub fn is_empty(&self) -> bool {
        self.plaintexts.read().unwrap().is_empty()
    }

    /// Replace every registered plaintext in `text` with `***`.
    ///
    /// Secrets are replaced longest-first: if one secret's plaintext is a substring of
    /// another's, redacting the shorter one first would leave the longer one's remainder
    /// in the output, leaking a fragment. Longest-first replacement closes that.
    pub fn redact(&self, text: &str) -> String {
        let guard = self.plaintexts.read().unwrap();
        if guard.is_empty() {
            return text.to_string();
        }
        let mut secrets: Vec<&String> = guard.iter().collect();
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        let mut out = text.to_string();
        for secret in secrets {
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
        identity: Option<&str>,
    ) -> Result<Arc<Secrets>, GlideshError> {
        let registry = Arc::new(SecretRegistry::default());
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
                    registry,
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
    pub fn decrypt_vars(&self, vars: &mut HashMap<String, String>) -> Result<(), GlideshError> {
        for value in vars.values_mut() {
            if token::is_secret_token(value) {
                *value = self.decrypt_token(value)?;
            }
        }
        Ok(())
    }

    /// Decrypt tokens hiding in inventory `@`-refs and structured collections, in place.
    pub fn decrypt_template_data(&self, data: &mut TemplateData) -> Result<(), GlideshError> {
        for value in data.extra_vars.values_mut() {
            if token::is_secret_token(value) {
                *value = self.decrypt_token(value)?;
            }
        }
        for rows in data.collections.values_mut() {
            for row in rows.iter_mut() {
                for value in row.values_mut() {
                    if token::is_secret_token(value) {
                        *value = self.decrypt_token(value)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Decrypt tokens embedded inside a larger string (inline usage in a task arg).
    pub fn decrypt_inline(&self, s: &str) -> Result<String, GlideshError> {
        if !token::contains_secret_token(s) {
            return Ok(s.to_string());
        }
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        while let Some(pos) = rest.find(token::SECRET_PREFIX) {
            out.push_str(&rest[..pos]);
            let after = &rest[pos..];
            let body = token::SECRET_PREFIX.len();
            let end = after[body..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
                .map(|i| body + i)
                .unwrap_or(after.len());
            let tok = &after[..end];
            out.push_str(&self.decrypt_token(tok)?);
            rest = &after[end..];
        }
        out.push_str(rest);
        Ok(out)
    }

    fn locked_error(&self) -> GlideshError {
        let hint = if self.has_metadata {
            "provide the passphrase via GLIDESH_SECRET_PASS or --ask-secret-pass"
        } else {
            "no secrets.kdl found — run `glidesh secret init`, or point at one with --secrets / GLIDESH_SECRETS"
        };
        GlideshError::Secret {
            message: format!("encrypted value found but the secrets key is unavailable — {hint}"),
        }
    }
}

/// Unwrap the DEK using the configured provider and identity.
pub fn unwrap_dek(
    config: &SecretsConfig,
    identity: &str,
) -> Result<Zeroizing<[u8; 32]>, GlideshError> {
    match config.provider {
        Provider::Passphrase => {
            PassphraseProvider::new(identity.to_string()).unwrap_dek(&config.encryptedkey)
        }
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
        };
        let secrets = Secrets::open(Some(&cfg), Some("pw")).unwrap();
        let tok = token::encrypt_value(&dek, b"v").unwrap();
        assert_eq!(secrets.decrypt_token(&tok).unwrap(), "v");
    }
}
