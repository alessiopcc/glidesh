//! Textual read/write helpers for `secrets.kdl` used by the `glidesh secret` CLI.
//!
//! Edits are line-oriented so hand-written comments and layout survive every write —
//! `secret set`, `secret rm`, and `secret edit` alike. A full re-serialization would
//! discard them. Values are escaped on the way in ([`kdl_quote`]), because `secret edit`
//! can write back a plaintext value that a token never contains, such as a quote mark.

use crate::error::GlideshError;
use crate::secrets::config::SecretRecipient;
use std::path::Path;

/// Read a secrets file, mapping IO errors to a clear message.
pub fn read(path: &Path) -> Result<String, GlideshError> {
    std::fs::read_to_string(path).map_err(|e| GlideshError::Secret {
        message: format!("failed to read secrets file '{}': {e}", path.display()),
    })
}

/// Write a secrets file, restricting permissions to the owner on Unix.
///
/// The content lands in a sibling temp file that is then renamed over the original, so an
/// interrupted write cannot leave a truncated vault behind — `secret rekey --rotate-data-key`
/// rewrites every value at once, and a half-written file there would be unrecoverable. The
/// temp file holds ciphertext only, never plaintext.
pub fn write(path: &Path, content: &str) -> Result<(), GlideshError> {
    let map_err = |e: std::io::Error| GlideshError::Secret {
        message: format!("failed to write secrets file '{}': {e}", path.display()),
    };
    let tmp = path.with_extension("kdl.tmp");
    std::fs::write(&tmp, content).map_err(map_err)?;
    restrict_perms(&tmp);
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(map_err(e));
    }
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
/// atomically at creation (no world-readable window before a chmod), and `create_new`
/// refuses to open anything that already exists, so a symlink planted at the path cannot
/// redirect the plaintext somewhere else.
pub fn write_private(path: &Path, content: &str) -> Result<(), GlideshError> {
    use std::io::Write;
    let map_err = |e: std::io::Error| GlideshError::Secret {
        message: format!("failed to write '{}': {e}", path.display()),
    };
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
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

/// The initial file contents for `secret init --provider age`.
pub fn init_content_age(encryptedkey: &str, recipients: &[SecretRecipient]) -> String {
    format!(
        "secrets {{\n{}}}\n",
        secrets_block_body(encryptedkey, recipients)
    )
}

/// The inside of a `secrets { … }` block for the age provider, indented and newline
/// terminated. Shared by `init` and by every recipient change, which rewrites the block.
fn secrets_block_body(encryptedkey: &str, recipients: &[SecretRecipient]) -> String {
    let mut out = String::from("    provider \"age\"\n    recipients {\n");
    for recipient in recipients {
        out.push_str(&format!(
            "        - name={} key={}\n",
            kdl_quote(&recipient.name),
            kdl_quote(&recipient.key)
        ));
    }
    out.push_str("    }\n");
    out.push_str(&format!("    encryptedkey \"{encryptedkey}\"\n"));
    out
}

/// Replace the whole `secrets { … }` block, leaving everything around it alone.
///
/// Recipient changes rewrite the block wholesale rather than editing lines inside it: the
/// block is machine-managed, and a rewrite keeps the recipient rows and the `encryptedkey`
/// they wrap consistent with each other. Comments *outside* the block survive; comments
/// inside it do not.
pub fn replace_secrets_block(
    content: &str,
    encryptedkey: &str,
    recipients: &[SecretRecipient],
) -> Result<String, GlideshError> {
    let mut lines: Vec<String> = Vec::new();
    let mut depth: i32 = 0;
    let mut replacing = false;
    let mut replaced = false;

    for line in content.lines() {
        if replacing {
            depth += brace_delta(line);
            if depth <= 0 {
                replacing = false;
                depth = 0;
            }
            continue;
        }
        let trimmed = line.trim_start();
        if depth == 0 && !replaced && line_key(trimmed) == Some("secrets") {
            replaced = true;
            lines.push(format!(
                "secrets {{\n{}}}",
                secrets_block_body(encryptedkey, recipients)
            ));
            let delta = brace_delta(line);
            if delta > 0 {
                replacing = true;
                depth = delta;
            }
            continue;
        }
        depth += brace_delta(line);
        lines.push(line.to_string());
    }

    if !replaced {
        return Err(GlideshError::Secret {
            message: "no `secrets` block to update (is this an initialized secrets file?)"
                .to_string(),
        });
    }
    let mut result = lines.join("\n");
    if content.ends_with('\n') && !result.ends_with('\n') {
        result.push('\n');
    }
    Ok(result)
}

/// Quote and escape a string as a KDL basic string value.
pub fn kdl_quote(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
        .replace('\r', "\\r");
    format!("\"{escaped}\"")
}

/// Insert or replace a top-level scalar `key "value"` node, preserving everything else.
pub fn upsert_scalar(content: &str, key: &str, value: &str) -> String {
    let mut depth: i32 = 0;
    let mut replaced = false;
    let mut lines: Vec<String> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim_start();
        if depth == 0 && !replaced && line_key(trimmed) == Some(key) {
            lines.push(format!("{key} {}", kdl_quote(value)));
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
        result.push_str(&format!("{key} {}\n", kdl_quote(value)));
    }
    result
}

/// Delete a top-level node, scalar or block, preserving everything around it. A node that
/// opens a block (`api-keys { … }`) takes its whole block with it; brace depth is tracked
/// so a stray brace in a comment or a value cannot make the deletion run away.
pub fn remove_node(content: &str, key: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut depth: i32 = 0;
    let mut removing = false;
    let mut removed = false;

    for line in content.lines() {
        if removing {
            depth += brace_delta(line);
            if depth <= 0 {
                removing = false;
                depth = 0;
            }
            continue;
        }
        let trimmed = line.trim_start();
        if depth == 0 && !removed && line_key(trimmed) == Some(key) {
            removed = true;
            let delta = brace_delta(line);
            if delta > 0 {
                removing = true;
                depth = delta;
            }
            continue;
        }
        depth += brace_delta(line);
        lines.push(line.to_string());
    }

    let mut result = lines.join("\n");
    if content.ends_with('\n') && !result.is_empty() && !result.ends_with('\n') {
        result.push('\n');
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
    fn upsert_escapes_values_that_need_it() {
        let content = "secrets {\n    provider \"passphrase\"\n    encryptedkey \"v1:AA\"\n}\n";
        // A plaintext value `secret edit` could hand back: quotes and a backslash, which
        // a base64 token never contains and the old unescaped write would have mangled.
        let raw = r#"say "hi" C:\path"#;
        let out = upsert_scalar(content, "note", raw);
        assert!(out.contains(r#"note "say \"hi\" C:\\path""#), "raw: {out}");
        // Round-trips through the parser it will be read back with.
        let parsed = super::super::config::parse_secrets_file(&out).unwrap();
        assert_eq!(parsed.vars.get("note").unwrap(), raw);
    }

    #[test]
    fn remove_node_drops_a_scalar_and_keeps_the_rest() {
        let content = "// keep me\nsecrets {\n    provider \"passphrase\"\n    encryptedkey \"v1:AA\"\n}\ndb-password \"secret:v1:x\"\napi-token \"secret:v1:y\"\n";
        let out = remove_node(content, "db-password");
        assert!(!out.contains("db-password"));
        assert!(out.contains("// keep me"));
        assert!(out.contains("api-token \"secret:v1:y\""));
        assert!(out.contains("encryptedkey \"v1:AA\""));
    }

    #[test]
    fn remove_node_takes_a_whole_block_with_it() {
        let content = "secrets {\n    provider \"passphrase\"\n    encryptedkey \"v1:AA\"\n}\napi-keys {\n    - name=\"a\" value=\"secret:v1:one\"\n    - name=\"b\" value=\"secret:v1:two\"\n}\nregion \"eu\"\n";
        let out = remove_node(content, "api-keys");
        assert!(!out.contains("api-keys"));
        assert!(!out.contains("secret:v1:one"));
        assert!(out.contains("region \"eu\""));
        // The provider block above it is untouched.
        assert!(out.contains("encryptedkey \"v1:AA\""));
    }

    #[test]
    fn remove_node_leaves_an_absent_key_alone() {
        let content = "region \"eu\"\n";
        assert_eq!(remove_node(content, "nothing"), content);
    }

    #[test]
    fn rekey_replaces_blob_preserving_indent() {
        let content = "secrets {\n    provider \"passphrase\"\n    encryptedkey \"v1:OLD\"\n}\n";
        let out = replace_encryptedkey(content, "v1:NEW").unwrap();
        assert!(out.contains("    encryptedkey \"v1:NEW\""));
        assert!(!out.contains("v1:OLD"));
    }

    #[test]
    fn replacing_the_secrets_block_keeps_everything_around_it() {
        let content = "// top comment\nsecrets {\n    provider \"age\"\n    recipients {\n        - name=\"alice\" key=\"ssh-ed25519 AAA\"\n    }\n    encryptedkey \"agev1:OLD\"\n}\ndb-password \"secret:v1:x\"\n";
        let recipients = vec![
            SecretRecipient {
                name: "alice".into(),
                key: "ssh-ed25519 AAA".into(),
            },
            SecretRecipient {
                name: "bob".into(),
                key: "ssh-ed25519 BBB".into(),
            },
        ];
        let out = replace_secrets_block(content, "agev1:NEW", &recipients).unwrap();
        assert!(out.starts_with("// top comment\n"), "{out}");
        assert!(out.contains("db-password \"secret:v1:x\""), "{out}");
        assert!(out.contains("agev1:NEW"), "{out}");
        assert!(!out.contains("agev1:OLD"), "{out}");
        assert!(out.contains("name=\"bob\""), "{out}");
        // Exactly one block, correctly closed.
        assert_eq!(out.matches("secrets {").count(), 1, "{out}");
        assert!(
            super::super::config::parse_secrets_file(&out).is_ok(),
            "{out}"
        );
    }

    #[test]
    fn replacing_without_a_block_errors() {
        assert!(replace_secrets_block("region \"eu\"\n", "agev1:X", &[]).is_err());
    }

    #[test]
    fn rekey_without_block_errors() {
        assert!(replace_encryptedkey("region \"x\"\n", "v1:NEW").is_err());
    }
}
