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
    Passphrase,
}

impl Provider {
    fn parse(s: &str) -> Result<Provider, GlideshError> {
        match s {
            "passphrase" => Ok(Provider::Passphrase),
            other => Err(GlideshError::Secret {
                message: format!("unknown secrets provider '{other}' (supported: passphrase)"),
            }),
        }
    }
}

/// The provider block from a `secrets.kdl` file.
#[derive(Debug, Clone)]
pub struct SecretsConfig {
    pub provider: Provider,
    /// The `v1:<base64url>` wrapped-DEK blob.
    pub encryptedkey: String,
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
            file.config = Some(parse_secrets_block(node)?);
            continue;
        }
        if let Some(rows) = parse_structured(node) {
            if file.vars.contains_key(&name) || file.structured.contains_key(&name) {
                return Err(dup(&name));
            }
            file.structured.insert(name, rows);
        } else {
            if file.vars.contains_key(&name) || file.structured.contains_key(&name) {
                return Err(dup(&name));
            }
            let value = node
                .entries()
                .iter()
                .find(|e| e.name().is_none())
                .map(|e| kdl_value_to_string(e.value()))
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
            "recipients" => { /* reserved for the age provider */ }
            other => {
                return Err(GlideshError::Secret {
                    message: format!("unknown key '{other}' in `secrets` block"),
                });
            }
        }
    }

    Ok(SecretsConfig {
        provider: provider.ok_or_else(|| GlideshError::Secret {
            message: "`secrets` block is missing `provider`".to_string(),
        })?,
        encryptedkey: encryptedkey.ok_or_else(|| GlideshError::Secret {
            message: "`secrets` block is missing `encryptedkey`".to_string(),
        })?,
    })
}

/// A list-of-maps structured var (`name { - k="v" … }`), else `None`.
fn parse_structured(node: &kdl::KdlNode) -> Option<Vec<HashMap<String, String>>> {
    let children = node.children()?;
    let nodes = children.nodes();
    if nodes.is_empty() || !nodes.iter().all(|n| n.name().to_string() == "-") {
        return None;
    }
    if !nodes
        .iter()
        .any(|n| n.entries().iter().any(|e| e.name().is_some()))
    {
        return None;
    }
    Some(
        nodes
            .iter()
            .map(|n| {
                n.entries()
                    .iter()
                    .filter_map(|e| {
                        e.name()
                            .map(|name| (name.to_string(), kdl_value_to_string(e.value())))
                    })
                    .collect()
            })
            .collect(),
    )
}

fn kdl_value_to_string(value: &kdl::KdlValue) -> String {
    match value {
        kdl::KdlValue::String(s) => s.clone(),
        kdl::KdlValue::Integer(i) => i.to_string(),
        kdl::KdlValue::Bool(b) => b.to_string(),
        kdl::KdlValue::Float(f) => f.to_string(),
        kdl::KdlValue::Null => String::new(),
    }
}

fn dup(name: &str) -> GlideshError {
    GlideshError::Secret {
        message: format!("duplicate variable '{name}' in secrets file"),
    }
}

/// Locate the secrets file: an explicit path, then `$GLIDESH_SECRETS`, then
/// `secrets.kdl` next to the inventory, then `./secrets.kdl`. Returns the first that
/// exists.
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
    fn vars_only_file_has_no_config() {
        let file = parse_secrets_file(r#"region "eu-west-1""#).unwrap();
        assert!(file.config.is_none());
        assert_eq!(file.vars.get("region").unwrap(), "eu-west-1");
    }
}
