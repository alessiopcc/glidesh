//! Parsing and discovery of `secrets.kdl` — the committed "stack config" that holds the
//! provider metadata (`secrets { … }`) plus the encrypted variable values.
//!
//! ```kdl
//! secrets {
//!     provider "passphrase"
//!     encryptedkey "v1:<base64url wrapped DEK>"
//! }
//!
//! db-password "secret:v1:…"
//! api-token   "secret:v1:…"
//! ```
//!
//! The `secrets { … }` block configures the provider; every other top-level node is an
//! ordinary variable (scalar or structured) merged at the inventory-global tier.

use crate::error::GlideshError;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The DEK-wrapping provider named in `secrets { provider "…" }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provider {
    /// One shared passphrase wraps the data key.
    Passphrase,
    /// The data key is wrapped to a set of SSH public keys, age-style.
    Age,
}

impl Provider {
    /// The name as it is written in `secrets { provider "…" }`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Passphrase => "passphrase",
            Provider::Age => "age",
        }
    }

    fn parse(s: &str) -> Result<Provider, GlideshError> {
        match s {
            "passphrase" => Ok(Provider::Passphrase),
            "age" => Ok(Provider::Age),
            other => Err(GlideshError::Secret {
                message: format!("unknown secrets provider '{other}' (supported: passphrase, age)"),
            }),
        }
    }
}

/// One party who can unwrap the data key: a name for humans and their SSH public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRecipient {
    pub name: String,
    pub key: String,
}

/// The provider block from a `secrets.kdl` file.
#[derive(Debug, Clone)]
pub struct SecretsConfig {
    pub provider: Provider,
    /// The wrapped-DEK blob: `v1:…` for the passphrase provider, `agev1:…` for age.
    pub encryptedkey: String,
    /// Who can unwrap it. Always empty for the passphrase provider, where the passphrase
    /// itself is the only credential.
    pub recipients: Vec<SecretRecipient>,
}

/// A parsed `secrets.kdl`: the optional provider block plus its variable nodes.
#[derive(Debug, Clone, Default)]
pub struct SecretsFile {
    pub config: Option<SecretsConfig>,
    pub vars: HashMap<String, String>,
    pub structured: HashMap<String, Vec<HashMap<String, String>>>,
}

/// Parse a `secrets.kdl` document.
pub fn parse_secrets_file(input: &str) -> Result<SecretsFile, GlideshError> {
    let doc: kdl::KdlDocument = input
        .parse()
        .map_err(|e: kdl::KdlError| GlideshError::Secret {
            message: format!("failed to parse secrets file: {e}"),
        })?;

    let mut file = SecretsFile::default();

    for node in doc.nodes() {
        let name = node.name().to_string();
        if name == "secrets" {
            // Two blocks are almost always a botched merge. Taking the last one silently
            // would decrypt against provider metadata the file also contradicts.
            if file.config.is_some() {
                return Err(GlideshError::Secret {
                    message: "more than one `secrets` block in the secrets file; keep exactly \
                              one (a merge may have left two behind)"
                        .to_string(),
                });
            }
            file.config = Some(parse_secrets_block(node)?);
            continue;
        }
        crate::config::validate_user_var_name(node.name().value()).map_err(|e| {
            GlideshError::Secret {
                message: e.to_string(),
            }
        })?;
        if file.vars.contains_key(&name) || file.structured.contains_key(&name) {
            return Err(dup(&name));
        }
        if let Some(rows) = crate::config::parse_structured_var(node) {
            file.structured.insert(name, rows);
        } else {
            let value = node
                .entries()
                .iter()
                .find(|e| e.name().is_none())
                .map(|e| crate::config::kdl_value_to_string(e.value()))
                .unwrap_or_default();
            file.vars.insert(name, value);
        }
    }

    Ok(file)
}

fn parse_secrets_block(node: &kdl::KdlNode) -> Result<SecretsConfig, GlideshError> {
    let children = node.children().ok_or_else(|| GlideshError::Secret {
        message: "empty `secrets` block (run `glidesh secret init`)".to_string(),
    })?;

    let mut provider = None;
    let mut encryptedkey = None;
    let mut recipients = Vec::new();
    for child in children.nodes() {
        let key = child.name().to_string();
        let value = child
            .entries()
            .iter()
            .find(|e| e.name().is_none())
            .and_then(|e| e.value().as_string())
            .map(|s| s.to_string());
        match key.as_str() {
            "provider" => {
                provider = Some(Provider::parse(value.as_deref().unwrap_or(""))?);
            }
            "encryptedkey" => encryptedkey = value,
            "recipients" => recipients = parse_recipients(child)?,
            other => {
                return Err(GlideshError::Secret {
                    message: format!("unknown key '{other}' in `secrets` block"),
                });
            }
        }
    }

    let provider = provider.ok_or_else(|| GlideshError::Secret {
        message: "`secrets` block is missing `provider`".to_string(),
    })?;
    if provider == Provider::Age && recipients.is_empty() {
        return Err(GlideshError::Secret {
            message: "the `age` provider needs at least one recipient, or nobody could \
                      unlock this file"
                .to_string(),
        });
    }
    Ok(SecretsConfig {
        provider,
        encryptedkey: encryptedkey.ok_or_else(|| GlideshError::Secret {
            message: "`secrets` block is missing `encryptedkey`".to_string(),
        })?,
        recipients,
    })
}

/// Parse the `recipients { - name=… key=… }` rows inside a `secrets` block.
fn parse_recipients(node: &kdl::KdlNode) -> Result<Vec<SecretRecipient>, GlideshError> {
    let rows = crate::config::parse_structured_var(node).ok_or_else(|| GlideshError::Secret {
        message: "`recipients` must be a block of `- name=… key=…` rows".to_string(),
    })?;
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            let key = row.get("key").ok_or_else(|| GlideshError::Secret {
                message: format!("recipient #{} is missing `key`", i + 1),
            })?;
            Ok(SecretRecipient {
                name: row
                    .get("name")
                    .cloned()
                    .unwrap_or_else(|| format!("recipient-{}", i + 1)),
                key: key.clone(),
            })
        })
        .collect()
}

fn dup(name: &str) -> GlideshError {
    GlideshError::Secret {
        message: format!("duplicate variable '{name}' in secrets file"),
    }
}

/// Locate the secrets file: an explicit path, then `$GLIDESH_SECRETS`, then
/// `secrets.kdl` next to the inventory, then `./secrets.kdl`.
///
/// Only the last two are probed for existence. A path the user named outright — by flag or
/// by environment — is returned whether or not it is there, so a typo fails loudly at open
/// time instead of being skipped in favour of some other vault that happens to exist.
pub fn discover_secrets_path(explicit: Option<&Path>, inv_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    if let Ok(env) = std::env::var("GLIDESH_SECRETS") {
        if !env.is_empty() {
            return Some(PathBuf::from(env));
        }
    }
    if let Some(dir) = inv_dir {
        let candidate = dir.join("secrets.kdl");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    let cwd = PathBuf::from("secrets.kdl");
    if cwd.exists() {
        return Some(cwd);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_secrets_block_is_rejected_rather_than_silently_winning() {
        let input = r#"
secrets {
    provider "passphrase"
    encryptedkey "v1:AAAA"
}

secrets {
    provider "age"
    encryptedkey "agev1:BBBB"
    recipient "alice" key="ssh-ed25519 AAAA"
}
"#;
        let err = parse_secrets_file(input).unwrap_err().to_string();
        assert!(err.contains("more than one"), "got: {err}");
    }

    #[test]
    fn parses_block_and_vars() {
        let input = r#"
secrets {
    provider "passphrase"
    encryptedkey "v1:AAAA"
}

db-password "secret:v1:xxx"
region "us-east-1"
"#;
        let file = parse_secrets_file(input).unwrap();
        let cfg = file.config.unwrap();
        assert_eq!(cfg.provider, Provider::Passphrase);
        assert_eq!(cfg.encryptedkey, "v1:AAAA");
        assert_eq!(file.vars.get("db-password").unwrap(), "secret:v1:xxx");
        assert_eq!(file.vars.get("region").unwrap(), "us-east-1");
    }

    #[test]
    fn parses_structured_secret_vars() {
        let input = r#"
secrets { provider "passphrase"; encryptedkey "v1:AA" }
api-keys {
    - name="a" value="secret:v1:one"
    - name="b" value="secret:v1:two"
}
"#;
        let file = parse_secrets_file(input).unwrap();
        let rows = file.structured.get("api-keys").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get("name").unwrap(), "a");
        assert!(!file.vars.contains_key("api-keys"));
    }

    #[test]
    fn unknown_provider_rejected() {
        let input = r#"secrets { provider "magic"; encryptedkey "v1:AA" }"#;
        assert!(parse_secrets_file(input).is_err());
    }

    #[test]
    fn missing_encryptedkey_rejected() {
        let input = r#"secrets { provider "passphrase" }"#;
        let err = parse_secrets_file(input).unwrap_err().to_string();
        assert!(err.contains("encryptedkey"), "got: {err}");
    }

    #[test]
    fn reserved_at_var_name_rejected() {
        let input = "secrets { provider \"passphrase\"; encryptedkey \"v1:AA\" }\n\"@host\" \"secret:v1:x\"\n";
        let err = parse_secrets_file(input).unwrap_err().to_string();
        assert!(err.contains("reserved"), "got: {err}");
    }

    #[test]
    fn vars_only_file_has_no_config() {
        let file = parse_secrets_file(r#"region "eu-west-1""#).unwrap();
        assert!(file.config.is_none());
        assert_eq!(file.vars.get("region").unwrap(), "eu-west-1");
    }
}
