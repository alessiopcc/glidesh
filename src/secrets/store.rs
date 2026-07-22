//! Textual read/write helpers for `secrets.kdl` used by the `glidesh secret` CLI.
//!
//! Edits are line-oriented so hand-written comments and layout survive a `secret set`
//! (a full re-serialization would discard them). Values written here are always safe —
//! base64url tokens or the provider's own blobs — so quoting never needs escaping.

use crate::error::GlideshError;
use std::path::Path;

/// Read a secrets file, mapping IO errors to a clear message.
pub fn read(path: &Path) -> Result<String, GlideshError> {
    std::fs::read_to_string(path).map_err(|e| GlideshError::Secret {
        message: format!("failed to read secrets file '{}': {e}", path.display()),
    })
}

/// Write a secrets file, restricting permissions to the owner on Unix.
pub fn write(path: &Path, content: &str) -> Result<(), GlideshError> {
    std::fs::write(path, content).map_err(|e| GlideshError::Secret {
        message: format!("failed to write secrets file '{}': {e}", path.display()),
    })?;
    restrict_perms(path);
    Ok(())
}

#[cfg(unix)]
fn restrict_perms(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_perms(_path: &Path) {}

/// Write plaintext to a fresh file created owner-only from the start. Used for the
/// transient decrypted view in `secret edit`: on Unix the `0o600` mode is applied
/// atomically at creation (no world-readable window before a chmod), and any stale
/// file is truncated. Returns an error rather than following an existing symlink target.
pub fn write_private(path: &Path, content: &str) -> Result<(), GlideshError> {
    use std::io::Write;
    let map_err = |e: std::io::Error| GlideshError::Secret {
        message: format!("failed to write '{}': {e}", path.display()),
    };
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path).map_err(map_err)?;
    file.write_all(content.as_bytes()).map_err(map_err)?;
    Ok(())
}

/// The initial file contents for `secret init` (passphrase provider).
pub fn init_content(encryptedkey: &str) -> String {
    format!("secrets {{\n    provider \"passphrase\"\n    encryptedkey \"{encryptedkey}\"\n}}\n")
}

/// Insert or replace a top-level scalar `key "value"` node, preserving everything else.
pub fn upsert_scalar(content: &str, key: &str, value: &str) -> String {
    let mut depth: i32 = 0;
    let mut replaced = false;
    let mut lines: Vec<String> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim_start();
        if depth == 0 && !replaced && line_key(trimmed) == Some(key) {
            lines.push(format!("{key} \"{value}\""));
            replaced = true;
        } else {
            lines.push(line.to_string());
        }
        depth += brace_delta(line);
    }

    let mut result = lines.join("\n");
    if replaced {
        if content.ends_with('\n') && !result.ends_with('\n') {
            result.push('\n');
        }
    } else {
        if !result.is_empty() && !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str(&format!("{key} \"{value}\"\n"));
    }
    result
}

/// Replace the `encryptedkey` value wherever it appears (inside the `secrets` block),
/// preserving indentation. Errors if there is no block to rekey.
pub fn replace_encryptedkey(content: &str, new_blob: &str) -> Result<String, GlideshError> {
    let mut found = false;
    let mut lines: Vec<String> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        if !found && line_key(trimmed) == Some("encryptedkey") {
            let indent = &line[..line.len() - trimmed.len()];
            lines.push(format!("{indent}encryptedkey \"{new_blob}\""));
            found = true;
        } else {
            lines.push(line.to_string());
        }
    }
    if !found {
        return Err(GlideshError::Secret {
            message: "no `encryptedkey` found to rekey (is this an initialized secrets file?)"
                .to_string(),
        });
    }
    let mut result = lines.join("\n");
    if content.ends_with('\n') {
        result.push('\n');
    }
    Ok(result)
}

/// The node name a line declares, if it looks like `name …` (not a comment, `-` row,
/// or block delimiter). Borrows from `trimmed` rather than allocating.
fn line_key(trimmed: &str) -> Option<&str> {
    if trimmed.is_empty()
        || trimmed.starts_with("//")
        || trimmed.starts_with('-')
        || trimmed.starts_with('}')
        || trimmed.starts_with('{')
    {
        return None;
    }
    let end = trimmed
        .find(|c: char| c.is_whitespace() || c == '=' || c == '"')
        .unwrap_or(trimmed.len());
    let token = &trimmed[..end];
    if token.is_empty() { None } else { Some(token) }
}

/// Net brace depth change for a line, ignoring braces inside quoted strings and `//`
/// comments — otherwise a lone brace in a value or comment would desync depth tracking
/// and cause `upsert_scalar` to append a duplicate node.
fn brace_delta(line: &str) -> i32 {
    let mut delta = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '/' if chars.peek() == Some(&'/') => break,
            '{' => delta += 1,
            '}' => delta -= 1,
            _ => {}
        }
    }
    delta
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_replaces_existing_and_keeps_comments() {
        let content = "// my secrets\nsecrets {\n    provider \"passphrase\"\n    encryptedkey \"v1:AA\"\n}\ndb-password \"secret:v1:old\"\n";
        let out = upsert_scalar(content, "db-password", "secret:v1:new");
        assert!(out.contains("// my secrets"));
        assert!(out.contains("db-password \"secret:v1:new\""));
        assert!(!out.contains("secret:v1:old"));
        // The encryptedkey inside the block must not be touched.
        assert!(out.contains("encryptedkey \"v1:AA\""));
    }

    #[test]
    fn upsert_ignores_braces_in_comments_and_values() {
        let content = "secrets {\n    provider \"passphrase\"\n    encryptedkey \"v1:AA\"\n}\n// a lone brace { in a comment\nnote \"has a } brace\"\ndb-password \"secret:v1:old\"\n";
        let out = upsert_scalar(content, "db-password", "secret:v1:new");
        assert!(out.contains("db-password \"secret:v1:new\""));
        assert!(!out.contains("secret:v1:old"));
        // The desync bug would append a second db-password node.
        assert_eq!(
            out.matches("db-password").count(),
            1,
            "duplicate appended: {out}"
        );
    }

    #[test]
    fn upsert_appends_new_key() {
        let content = "secrets {\n    provider \"passphrase\"\n    encryptedkey \"v1:AA\"\n}\n";
        let out = upsert_scalar(content, "api-token", "secret:v1:tok");
        assert!(out.contains("api-token \"secret:v1:tok\""));
    }

    #[test]
    fn rekey_replaces_blob_preserving_indent() {
        let content = "secrets {\n    provider \"passphrase\"\n    encryptedkey \"v1:OLD\"\n}\n";
        let out = replace_encryptedkey(content, "v1:NEW").unwrap();
        assert!(out.contains("    encryptedkey \"v1:NEW\""));
        assert!(!out.contains("v1:OLD"));
    }

    #[test]
    fn rekey_without_block_errors() {
        assert!(replace_encryptedkey("region \"x\"\n", "v1:NEW").is_err());
    }
}
