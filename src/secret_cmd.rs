//! The `glidesh secret` subcommands: everything that reads or writes a `secrets.kdl`.
//!
//! Kept apart from `main.rs` because it is a self-contained CLI of its own — two key-wrapping
//! providers, recipient management, and the file-editing helpers that keep a committed vault
//! readable. The executor's side of secrets lives in [`glidesh::secrets`].

use crate::cli;
use crate::{default_ssh_key, expand_tilde};
use glidesh::error::GlideshError;
use glidesh::secrets::config::SecretRecipient;
use glidesh::secrets::{
    self, config as secrets_config, passphrase as secret_passphrase, store as secret_store,
    token as secret_token,
};
use std::path::PathBuf;

pub(crate) fn cmd_secret(args: cli::SecretArgs) -> Result<(), GlideshError> {
    use cli::SecretCommand::*;
    match args.command {
        Init(a) => secret_init(&a.file, a.provider, &a.recipients),
        Recipients(a) => match a.command {
            cli::RecipientsCommand::List(a) => secret_recipients_list(&a.file),
            cli::RecipientsCommand::Add(a) => secret_recipients_add(&a.file, &a.recipient),
            cli::RecipientsCommand::Rm(a) => {
                secret_recipients_rm(&a.file, &a.name, a.keep_data_key)
            }
        },
        Set(a) => secret_set(&a.file, &a.key, a.value),
        Get(a) | Decrypt(a) => secret_get(&a.file, &a.key),
        List(a) => secret_list(&a.file),
        Rm(a) => secret_remove(&a.file, &a.key),
        Encrypt(a) => secret_encrypt(&a.file),
        Rekey(a) => secret_rekey(&a.file, a.rotate_data_key, a.new_pass_file.as_deref()),
        Edit(a) => secret_edit(&a.file),
    }
}

fn read_secret_pass(prompt: &str) -> Result<String, GlideshError> {
    rpassword::prompt_password(prompt).map_err(|e| GlideshError::Secret {
        message: format!("failed to read passphrase: {e}"),
    })
}

/// The non-empty `GLIDESH_SECRET_PASS` value, if set.
pub(crate) fn secret_pass_from_env() -> Option<String> {
    std::env::var("GLIDESH_SECRET_PASS")
        .ok()
        .filter(|p| !p.is_empty())
}

/// The passphrase stored in a file: its first line, with any trailing carriage return
/// dropped so a file written on Windows or by `echo` works unchanged. First line only —
/// a second line is far likelier to be an editor artifact than part of the passphrase.
pub(crate) fn read_pass_file(path: &std::path::Path) -> Result<String, GlideshError> {
    let content = std::fs::read_to_string(path).map_err(|e| GlideshError::Secret {
        message: format!("failed to read passphrase file '{}': {e}", path.display()),
    })?;
    let pass = content.lines().next().unwrap_or_default();
    if pass.is_empty() {
        return Err(GlideshError::Secret {
            message: format!("passphrase file '{}' is empty", path.display()),
        });
    }
    Ok(pass.to_string())
}

/// The passphrase named by `GLIDESH_SECRET_PASS_FILE`, if that variable is set. Honoured
/// by every subcommand, so CI can mount a passphrase file instead of exporting the value
/// into an environment that every child process can read.
pub(crate) fn secret_pass_file_from_env() -> Result<Option<String>, GlideshError> {
    match std::env::var("GLIDESH_SECRET_PASS_FILE") {
        Ok(p) if !p.is_empty() => {
            let path = expand_tilde(std::path::Path::new(&p));
            Ok(Some(read_pass_file(&path)?))
        }
        _ => Ok(None),
    }
}

/// Unlock passphrase for existing files: `GLIDESH_SECRET_PASS`, then
/// `GLIDESH_SECRET_PASS_FILE`, then an interactive prompt with the given wording.
fn unlock_pass_with_prompt(prompt: &str) -> Result<String, GlideshError> {
    if let Some(p) = secret_pass_from_env() {
        return Ok(p);
    }
    if let Some(p) = secret_pass_file_from_env()? {
        return Ok(p);
    }
    read_secret_pass(prompt)
}

/// Prompt for a new passphrase twice and require the two entries to match.
fn prompt_new_passphrase_confirmed() -> Result<String, GlideshError> {
    let a = read_secret_pass("new passphrase: ")?;
    let b = read_secret_pass("confirm passphrase: ")?;
    if a != b {
        return Err(GlideshError::Secret {
            message: "passphrases do not match".to_string(),
        });
    }
    Ok(a)
}

/// A brand-new passphrase: `GLIDESH_SECRET_PASS` or `GLIDESH_SECRET_PASS_FILE` (for CI),
/// else prompt with confirmation.
fn new_pass() -> Result<String, GlideshError> {
    if let Some(p) = secret_pass_from_env() {
        return Ok(p);
    }
    if let Some(p) = secret_pass_file_from_env()? {
        return Ok(p);
    }
    prompt_new_passphrase_confirmed()
}

fn not_initialized(file: &std::path::Path) -> GlideshError {
    GlideshError::Secret {
        message: format!(
            "{} has no `secrets` block — run `glidesh secret init`",
            file.display()
        ),
    }
}

/// Read a secrets file and return its raw content plus the required provider block.
fn load_config(
    file: &std::path::Path,
) -> Result<(String, secrets_config::SecretsConfig), GlideshError> {
    let content = secret_store::read(file)?;
    let cfg = secrets_config::parse_secrets_file(&content)?
        .config
        .ok_or_else(|| not_initialized(file))?;
    Ok((content, cfg))
}

/// Where to find the SSH private key that unwraps an age-wrapped file: `--secret-identity`,
/// then `GLIDESH_SECRET_IDENTITY`, then the key glidesh would use to reach the hosts. Your
/// host key is your vault key unless you say otherwise, which is the point of the provider.
pub(crate) fn secret_identity_path(
    explicit: Option<&std::path::Path>,
    ssh_key: Option<&std::path::Path>,
) -> PathBuf {
    if let Some(path) = explicit {
        return expand_tilde(path);
    }
    if let Ok(env) = std::env::var("GLIDESH_SECRET_IDENTITY") {
        if !env.is_empty() {
            return expand_tilde(std::path::Path::new(&env));
        }
    }
    ssh_key.map(expand_tilde).unwrap_or_else(default_ssh_key)
}

/// Unwrap the data key with whatever credential this file's provider calls for.
fn unlock_dek(
    cfg: &secrets_config::SecretsConfig,
) -> Result<zeroize::Zeroizing<[u8; 32]>, GlideshError> {
    unlock_dek_with_prompt(cfg, "secret passphrase: ")
}

/// [`unlock_dek`] with a prompt of its own, so `rekey` can ask for the *current* passphrase.
/// The prompt is only reached by the passphrase provider; an age-wrapped file never asks for
/// one, since the SSH key is the credential.
fn unlock_dek_with_prompt(
    cfg: &secrets_config::SecretsConfig,
    prompt: &str,
) -> Result<zeroize::Zeroizing<[u8; 32]>, GlideshError> {
    let identity = match cfg.provider {
        secrets_config::Provider::Passphrase => {
            secrets::Identity::Passphrase(unlock_pass_with_prompt(prompt)?)
        }
        secrets_config::Provider::Age => {
            secrets::Identity::SshKey(secret_identity_path(None, None))
        }
    };
    secrets::unwrap_dek(cfg, &identity)
}

/// Turn a `--recipient` argument into a stored recipient. The argument is either the key
/// itself or a path to a `.pub` file; a trailing key comment becomes the display name,
/// since that is already how people label their keys.
fn resolve_recipient(spec: &str, index: usize) -> Result<SecretRecipient, GlideshError> {
    let (line, from_filename) = if spec.starts_with("ssh-") {
        (spec.trim().to_string(), None)
    } else {
        let path = expand_tilde(std::path::Path::new(spec));
        let text = std::fs::read_to_string(&path).map_err(|e| GlideshError::Secret {
            message: format!(
                "not an SSH public key, and not a readable file: {} ({e})",
                path.display()
            ),
        })?;
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().trim_end_matches(".pub").to_string());
        (
            text.lines().next().unwrap_or_default().trim().to_string(),
            stem,
        )
    };

    let mut fields = line.split_whitespace();
    let algorithm = fields.next().unwrap_or_default();
    let body = fields.next().ok_or_else(|| GlideshError::Secret {
        message: format!("incomplete SSH public key: {spec}"),
    })?;
    let comment = fields.next().map(|c| c.to_string());

    Ok(SecretRecipient {
        name: comment
            .or(from_filename)
            .unwrap_or_else(|| format!("recipient-{}", index + 1)),
        key: format!("{algorithm} {body}"),
    })
}

fn secret_init(
    file: &std::path::Path,
    provider: cli::ProviderArg,
    recipients: &[String],
) -> Result<(), GlideshError> {
    if file.exists() {
        return Err(GlideshError::Secret {
            message: format!("{} already exists", file.display()),
        });
    }
    let dek = secret_passphrase::generate_dek();

    let content = match provider {
        cli::ProviderArg::Passphrase => {
            if !recipients.is_empty() {
                return Err(GlideshError::Secret {
                    message: "--recipient applies to --provider age".to_string(),
                });
            }
            let wrapped = secret_passphrase::PassphraseProvider::new(new_pass()?).wrap_dek(&dek)?;
            secret_store::init_content(&wrapped)
        }
        cli::ProviderArg::Age => {
            if recipients.is_empty() {
                return Err(GlideshError::Secret {
                    message: "--provider age needs at least one --recipient: an SSH public key \
                              or a path to a .pub file"
                        .to_string(),
                });
            }
            let resolved = recipients
                .iter()
                .enumerate()
                .map(|(i, spec)| resolve_recipient(spec, i))
                .collect::<Result<Vec<_>, _>>()?;
            let wrapped = secrets::age::wrap_dek(&dek, &resolved)?;
            secret_store::init_content_age(&wrapped, &resolved)
        }
    };

    secret_store::write(file, &content)?;
    println!("Initialized {}", file.display());
    Ok(())
}

fn secret_set(
    file: &std::path::Path,
    key: &str,
    value: Option<String>,
) -> Result<(), GlideshError> {
    let (content, cfg) = load_config(file)?;
    let dek = unlock_dek(&cfg)?;
    let plaintext = match value {
        Some(v) => v,
        None => read_secret_pass(&format!("value for '{key}': "))?,
    };
    if plaintext.len() < secrets::MIN_REDACTABLE_LEN {
        eprintln!(
            "warning: '{key}' is under {} characters, so it cannot be masked in run output \
             without shredding unrelated text — glidesh will leave it visible in logs and \
             the TUI. Prefer a longer value.",
            secrets::MIN_REDACTABLE_LEN
        );
    }
    let token = secret_token::encrypt_value(&dek, plaintext.as_bytes())?;
    let updated = secret_store::upsert_scalar(&content, key, &token);
    secret_store::write(file, &updated)?;
    println!("Set '{key}' in {}", file.display());
    Ok(())
}

/// The `secrets` block of an age-wrapped file, or a clear error for a passphrase one.
fn age_config(
    file: &std::path::Path,
) -> Result<(String, secrets_config::SecretsConfig), GlideshError> {
    let (content, cfg) = load_config(file)?;
    if cfg.provider != secrets_config::Provider::Age {
        return Err(GlideshError::Secret {
            message: format!(
                "{} uses the {} provider, which has no recipients: everyone shares one passphrase",
                file.display(),
                cfg.provider.as_str()
            ),
        });
    }
    Ok((content, cfg))
}

fn secret_recipients_list(file: &std::path::Path) -> Result<(), GlideshError> {
    let (_content, cfg) = age_config(file)?;
    let width = cfg
        .recipients
        .iter()
        .map(|r| r.name.len())
        .max()
        .unwrap_or(0);
    for recipient in &cfg.recipients {
        println!("{:<width$}  {}", recipient.name, recipient.key);
    }
    Ok(())
}

fn secret_recipients_add(file: &std::path::Path, spec: &str) -> Result<(), GlideshError> {
    let (content, cfg) = age_config(file)?;
    let dek = unlock_dek(&cfg)?;

    let recipient = resolve_recipient(spec, cfg.recipients.len())?;
    if cfg.recipients.iter().any(|r| r.key == recipient.key) {
        return Err(GlideshError::Secret {
            message: format!("that key is already a recipient: {}", recipient.name),
        });
    }
    let mut recipients = cfg.recipients.clone();
    recipients.push(recipient.clone());

    let wrapped = secrets::age::wrap_dek(&dek, &recipients)?;
    let updated = secret_store::replace_secrets_block(&content, &wrapped, &recipients)?;
    secret_store::write(file, &updated)?;
    println!("Added {} to {}", recipient.name, file.display());
    Ok(())
}

fn secret_recipients_rm(
    file: &std::path::Path,
    name: &str,
    keep_data_key: bool,
) -> Result<(), GlideshError> {
    let (content, cfg) = age_config(file)?;
    let dek = unlock_dek(&cfg)?;

    let remaining: Vec<SecretRecipient> = cfg
        .recipients
        .iter()
        .filter(|r| r.name != name)
        .cloned()
        .collect();
    if remaining.len() == cfg.recipients.len() {
        return Err(GlideshError::Secret {
            message: format!("no recipient named {name} in {}", file.display()),
        });
    }
    if remaining.is_empty() {
        return Err(GlideshError::Secret {
            message: "that is the last recipient; removing it would lock everyone out".to_string(),
        });
    }

    // Re-wrapping alone revokes nothing: the removed recipient can still unwrap the blob
    // they already have, and it opens the same data key. Rotating the key is what makes the
    // removal real, so it is the default.
    let (updated, wrapped) = if keep_data_key {
        (content, secrets::age::wrap_dek(&dek, &remaining)?)
    } else {
        let new_dek = secret_passphrase::generate_dek();
        let (rotated, count) = rotate_tokens(&content, &dek, &new_dek)?;
        println!("Rotated the data key ({count} value(s) re-encrypted)");
        (rotated, secrets::age::wrap_dek(&new_dek, &remaining)?)
    };

    let updated = secret_store::replace_secrets_block(&updated, &wrapped, &remaining)?;
    secret_store::write(file, &updated)?;
    println!("Removed {name} from {}", file.display());
    if keep_data_key {
        println!(
            "Warning: --keep-data-key was given, so {name} can still decrypt every value with \
             the copy of this file they already have."
        );
    } else {
        println!(
            "Note: the previous tokens remain in your version control history and {name} can \
             still read them. Rotate the underlying credentials to revoke access."
        );
    }
    Ok(())
}

/// Print the names in a secrets file, and whether each is encrypted. Deliberately never
/// prints a value and never asks for the passphrase: seeing what a file holds should not
/// require the ability to read it.
fn secret_list(file: &std::path::Path) -> Result<(), GlideshError> {
    let content = secret_store::read(file)?;
    let parsed = secrets_config::parse_secrets_file(&content)?;

    let mut entries: Vec<(&str, &str)> = parsed
        .vars
        .iter()
        .map(|(name, value)| {
            let kind = if secret_token::is_secret_token(value) {
                "encrypted"
            } else {
                "plaintext"
            };
            (name.as_str(), kind)
        })
        .chain(parsed.structured.keys().map(|n| (n.as_str(), "structured")))
        .collect();
    entries.sort_unstable();

    if entries.is_empty() {
        println!("{} holds no values", file.display());
        return Ok(());
    }
    let width = entries.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    for (name, kind) in entries {
        println!("{name:<width$}  {kind}");
    }
    Ok(())
}

fn secret_remove(file: &std::path::Path, key: &str) -> Result<(), GlideshError> {
    let content = secret_store::read(file)?;
    let parsed = secrets_config::parse_secrets_file(&content)?;
    if !parsed.vars.contains_key(key) && !parsed.structured.contains_key(key) {
        return Err(GlideshError::Secret {
            message: format!("no secret named '{key}' in {}", file.display()),
        });
    }
    secret_store::write(file, &secret_store::remove_node(&content, key))?;
    println!("Removed '{key}' from {}", file.display());
    Ok(())
}

fn secret_get(file: &std::path::Path, key: &str) -> Result<(), GlideshError> {
    // A `secret:v1:…` argument is a token to decrypt in place rather than a name to look
    // up. `secret encrypt` mints tokens for pasting inline into a plan, and this is the
    // only way to read one back without first storing it under some scratch key.
    if secret_token::is_secret_token(key) {
        let (_content, cfg) = load_config(file)?;
        let dek = unlock_dek(&cfg)?;
        println!("{}", secret_token::decrypt_value(&dek, key)?.as_str());
        return Ok(());
    }

    let content = secret_store::read(file)?;
    let parsed = secrets_config::parse_secrets_file(&content)?;
    let value = parsed.vars.get(key).ok_or_else(|| GlideshError::Secret {
        message: format!("no secret named '{key}' in {}", file.display()),
    })?;
    if !secret_token::is_secret_token(value) {
        println!("{value}");
        return Ok(());
    }
    let cfg = parsed.config.ok_or_else(|| not_initialized(file))?;
    let dek = unlock_dek(&cfg)?;
    let plaintext = secret_token::decrypt_value(&dek, value)?;
    println!("{}", plaintext.as_str());
    Ok(())
}

/// Strip one trailing line ending from piped stdin. `\r\n` is checked before `\n` so a
/// value piped in on Windows is not silently stored with a trailing carriage return.
fn strip_trailing_newline(input: &str) -> &str {
    input
        .strip_suffix("\r\n")
        .or_else(|| input.strip_suffix('\n'))
        .unwrap_or(input)
}

fn secret_encrypt(file: &std::path::Path) -> Result<(), GlideshError> {
    let (_content, cfg) = load_config(file)?;
    let dek = unlock_dek(&cfg)?;
    use std::io::Read;
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| GlideshError::Secret {
            message: format!("failed to read stdin: {e}"),
        })?;
    let plaintext = strip_trailing_newline(&input);
    let token = secret_token::encrypt_value(&dek, plaintext.as_bytes())?;
    println!("{token}");
    Ok(())
}

/// Re-encrypt every token in `content` from `old` to `new`, returning the rewritten text
/// and how many values moved.
///
/// The sweep is textual rather than parser-driven so values inside structured blocks rotate
/// too: a token left behind becomes unreadable the moment the old key is dropped, so partial
/// coverage here means data loss. Each value is read straight back under the new key before
/// the caller writes anything — cheap insurance that the file being overwritten is replaced
/// by one that actually decrypts.
fn rotate_tokens(
    content: &str,
    old: &[u8; 32],
    new: &[u8; 32],
) -> Result<(String, usize), GlideshError> {
    let mut rotated = 0usize;
    let updated = secret_token::rewrite_tokens(content, |tok| {
        let plaintext = secret_token::decrypt_value(old, tok)?;
        let fresh = secret_token::encrypt_value(new, plaintext.as_bytes())?;
        if secret_token::decrypt_value(new, &fresh)?.as_str() != plaintext.as_str() {
            return Err(GlideshError::Secret {
                message: "re-encrypted value failed verification; the file was not written"
                    .to_string(),
            });
        }
        rotated += 1;
        Ok(fresh)
    })?;
    Ok((updated, rotated))
}

/// Re-wrap the data key, optionally replacing the key itself.
///
/// Without `rotate_data_key` this changes only the passphrase that unlocks the existing DEK:
/// one line of diff, every value token byte-identical. With it, a fresh DEK is generated and
/// every token in the file is re-encrypted under it — the operation that makes removing
/// someone's access mean something, since re-wrapping alone leaves the old DEK usable by
/// anyone who kept a copy of the old blob.
fn secret_rekey(
    file: &std::path::Path,
    rotate_data_key: bool,
    new_pass_file: Option<&std::path::Path>,
) -> Result<(), GlideshError> {
    let (content, cfg) = load_config(file)?;
    let dek = unlock_dek_with_prompt(&cfg, "current passphrase: ")?;

    // An age-wrapped file has no passphrase to change: who can unlock it is the recipient
    // list. Rotating its data key is still meaningful, so that half is honoured here and
    // the new key is re-wrapped to the same recipients.
    if cfg.provider == secrets_config::Provider::Age {
        if !rotate_data_key {
            return Err(GlideshError::Secret {
                message: "this file is wrapped to SSH recipients and has no passphrase to \
                          change — use `glidesh secret recipients` to change who can unlock \
                          it, or --rotate-data-key to replace the key itself"
                    .to_string(),
            });
        }
        let new_dek = secret_passphrase::generate_dek();
        let (updated, rotated) = rotate_tokens(&content, &dek, &new_dek)?;
        let wrapped = secrets::age::wrap_dek(&new_dek, &cfg.recipients)?;
        let updated = secret_store::replace_secrets_block(&updated, &wrapped, &cfg.recipients)?;
        secret_store::write(file, &updated)?;
        println!(
            "Rotated the data key in {} ({rotated} value(s) re-encrypted, re-wrapped to {} \
             recipient(s))",
            file.display(),
            cfg.recipients.len()
        );
        return Ok(());
    }

    // The new passphrase needs a channel of its own — the usual sources already hold the
    // current one — so a scripted rotation reads it from a file instead of a prompt.
    let new_pass = match new_pass_file {
        Some(path) => read_pass_file(&expand_tilde(path))?,
        None => prompt_new_passphrase_confirmed()?,
    };

    if !rotate_data_key {
        let wrapped = secret_passphrase::PassphraseProvider::new(new_pass).wrap_dek(&dek)?;
        let updated = secret_store::replace_encryptedkey(&content, &wrapped)?;
        secret_store::write(file, &updated)?;
        println!("Rekeyed {} (value tokens unchanged)", file.display());
        return Ok(());
    }

    let new_dek = secret_passphrase::generate_dek();
    let (updated, rotated) = rotate_tokens(&content, &dek, &new_dek)?;

    let wrapped = secret_passphrase::PassphraseProvider::new(new_pass).wrap_dek(&new_dek)?;
    let updated = secret_store::replace_encryptedkey(&updated, &wrapped)?;
    secret_store::write(file, &updated)?;

    println!(
        "Rotated the data key in {} ({rotated} value(s) re-encrypted)",
        file.display()
    );
    println!(
        "Note: the previous tokens remain in your version control history and can still be \
         read with the old passphrase. Rotate the underlying credentials to revoke access."
    );
    Ok(())
}

/// Removes and best-effort shreds a temp file on every exit path.
struct TempShredder(PathBuf);

impl Drop for TempShredder {
    fn drop(&mut self) {
        shred(&self.0);
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Overwrite a file's bytes with zeros, in fixed-size chunks.
///
/// The chunk is fixed rather than the file's length because the file is a decrypted view of
/// a vault of unknown size: allocating a buffer as large as it would mirror the plaintext in
/// memory at the very moment we are trying to be rid of it, and on a 32-bit target the
/// `u64 → usize` cast could wrap. Best-effort throughout — this runs from `Drop`, and a
/// journaling or copy-on-write filesystem may keep the original blocks regardless.
fn shred(path: &std::path::Path) {
    use std::io::Write;
    const CHUNK: usize = 8 * 1024;

    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    let Ok(mut file) = std::fs::OpenOptions::new().write(true).open(path) else {
        return;
    };
    let zeros = [0u8; CHUNK];
    let mut remaining = meta.len();
    while remaining > 0 {
        let n = remaining.min(CHUNK as u64) as usize;
        if file.write_all(&zeros[..n]).is_err() {
            return;
        }
        remaining -= n as u64;
    }
    let _ = file.flush();
}

/// Path for `secret edit`'s transient decrypted view: a uniquely named file in the system
/// temp directory, never beside the secrets file. [`TempShredder`] runs on `Drop`, which a
/// hard kill skips — and a leftover plaintext file inside a repository is one `git add` away
/// from being committed. The random suffix also keeps the path unpredictable, so the
/// `create_new` in `write_private` cannot be pre-empted by a symlink planted in a shared
/// temp directory.
fn edit_temp_path(file: &std::path::Path) -> PathBuf {
    use rand::RngCore;
    let mut suffix = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut suffix);
    let hex: String = suffix.iter().map(|b| format!("{b:02x}")).collect();
    let stem = file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("secrets");
    std::env::temp_dir().join(format!("{stem}-{hex}.kdl"))
}

fn secret_edit(file: &std::path::Path) -> Result<(), GlideshError> {
    let content = secret_store::read(file)?;
    let parsed = secrets_config::parse_secrets_file(&content)?;
    let cfg = parsed.config.ok_or_else(|| not_initialized(file))?;
    if !parsed.structured.is_empty() {
        return Err(GlideshError::Secret {
            message:
                "`secret edit` supports scalar values only; use `secret set` for structured secrets"
                    .to_string(),
        });
    }
    let dek = unlock_dek(&cfg)?;

    // Decrypt values into an editable view; remember which keys were secret so they are
    // re-encrypted on save (new keys added in the editor are stored as plaintext — use
    // `secret set` to encrypt them).
    let mut secret_keys = std::collections::HashSet::new();
    // What each secret decrypted to, and the token it is stored as. A value the editor
    // hands back unchanged keeps its original token: re-encrypting would mint a fresh
    // nonce and show every secret as modified in a diff where nothing actually changed.
    let mut original: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    let mut keys: Vec<&String> = parsed.vars.keys().collect();
    keys.sort();
    let mut editable = String::from(
        "// glidesh secret edit — values are decrypted here and re-encrypted on save.\n\
         // The `secrets` block holding the provider and wrapped key is managed separately,\n\
         // and is not shown here. Add new secrets with `secret set`.\n\n",
    );
    for key in &keys {
        let value = &parsed.vars[*key];
        if secret_token::is_secret_token(value) {
            secret_keys.insert((*key).clone());
            let plaintext = secret_token::decrypt_value(&dek, value)?;
            original.insert(
                (*key).clone(),
                (plaintext.as_str().to_string(), value.clone()),
            );
            editable.push_str(&format!(
                "{key} {}\n",
                secret_store::kdl_quote(plaintext.as_str())
            ));
        } else {
            editable.push_str(&format!("{key} {}\n", secret_store::kdl_quote(value)));
        }
    }

    let tmp = edit_temp_path(file);
    let _guard = TempShredder(tmp.clone());
    secret_store::write_private(&tmp, &editable)?;
    launch_editor(&tmp)?;

    let edited = secret_store::read(&tmp)?;
    let doc: kdl::KdlDocument =
        edited
            .parse()
            .map_err(|e: kdl::KdlError| GlideshError::Secret {
                message: format!("edited file is not valid KDL: {e}"),
            })?;

    // Write back onto the original text rather than regenerating the file, so comments,
    // ordering, and the provider block survive an edit.
    let mut out = content.clone();
    let mut seen = std::collections::HashSet::new();
    for node in doc.nodes() {
        let key = node.name().to_string();
        let value = node
            .entries()
            .iter()
            .find(|e| e.name().is_none())
            .and_then(|e| e.value().as_string())
            .unwrap_or("");
        seen.insert(key.clone());
        let stored = match original.get(&key) {
            Some((plaintext, token)) if plaintext == value => token.clone(),
            _ if secret_keys.contains(&key) => secret_token::encrypt_value(&dek, value.as_bytes())?,
            _ => value.to_string(),
        };
        out = secret_store::upsert_scalar(&out, &key, &stored);
    }
    for key in parsed.vars.keys().filter(|k| !seen.contains(*k)) {
        out = secret_store::remove_node(&out, key);
    }
    secret_store::write(file, &out)?;
    println!("Updated {}", file.display());
    Ok(())
}

/// Split `$VISUAL`/`$EDITOR` into a program and its arguments, honouring quotes.
///
/// The variable routinely carries flags (`code --wait`), and on Windows the program is
/// routinely a quoted path with spaces in it (`"C:\Program Files\…\Code.exe" --wait`).
/// Splitting on whitespace alone would tear that path into three unusable arguments.
/// Quotes only group; no escape processing, which is what a shell would do to a Windows
/// path's backslashes.
fn split_editor_command(command: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;

    for ch in command.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => current.push(ch),
            None if ch == '"' || ch == '\'' => {
                quote = Some(ch);
                started = true;
            }
            None if ch.is_whitespace() => {
                if started {
                    parts.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None => {
                current.push(ch);
                started = true;
            }
        }
    }
    if started {
        parts.push(current);
    }
    parts
}

fn launch_editor(path: &std::path::Path) -> Result<(), GlideshError> {
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "notepad".to_string()
            } else {
                "vi".to_string()
            }
        });
    let mut parts = split_editor_command(&editor).into_iter();
    let program = parts.next().ok_or_else(|| GlideshError::Secret {
        message: format!("could not read an editor command from '{editor}'"),
    })?;
    let status = std::process::Command::new(&program)
        .args(parts)
        .arg(path)
        .status()
        .map_err(|e| GlideshError::Secret {
            message: format!("failed to launch editor '{editor}': {e}"),
        })?;
    if !status.success() {
        return Err(GlideshError::Secret {
            message: "editor exited with an error; secrets unchanged".to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_one_trailing_line_ending_from_stdin() {
        assert_eq!(strip_trailing_newline("val\r\n"), "val");
        assert_eq!(strip_trailing_newline("val\n"), "val");
        assert_eq!(strip_trailing_newline("val"), "val");
        // Only the final line ending goes; a deliberate trailing blank line survives.
        assert_eq!(strip_trailing_newline("val\n\n"), "val\n");
        // A bare \r is not a line ending any pipe appends, so it is part of the secret.
        assert_eq!(strip_trailing_newline("val\r"), "val\r");
    }

    #[test]
    fn pass_file_takes_the_first_line_and_drops_a_carriage_return() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pass");
        std::fs::write(&path, "s3cr3t\r\nan editor artifact\n").unwrap();
        assert_eq!(read_pass_file(&path).unwrap(), "s3cr3t");
        std::fs::write(&path, "no-trailing-newline").unwrap();
        assert_eq!(read_pass_file(&path).unwrap(), "no-trailing-newline");
    }

    #[test]
    fn empty_or_missing_pass_file_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pass");
        std::fs::write(&path, "\n").unwrap();
        let err = read_pass_file(&path).unwrap_err().to_string();
        assert!(err.contains("empty"), "got: {err}");
        assert!(read_pass_file(&dir.path().join("absent")).is_err());
    }

    #[test]
    fn editor_command_keeps_a_quoted_program_path_in_one_piece() {
        assert_eq!(split_editor_command("vi"), ["vi"]);
        assert_eq!(
            split_editor_command("  code   --wait  "),
            ["code", "--wait"]
        );
        assert_eq!(
            split_editor_command(r#""C:\Program Files\Microsoft VS Code\Code.exe" --wait"#),
            [r"C:\Program Files\Microsoft VS Code\Code.exe", "--wait"]
        );
        assert_eq!(
            split_editor_command("'/usr/local/bin/my editor' -f"),
            ["/usr/local/bin/my editor", "-f"]
        );
        // A quote closing mid-token joins what is on either side of it, the way a shell
        // would: this is one program name, not two arguments.
        assert_eq!(
            split_editor_command(r#"/opt/"my ed"/run"#),
            ["/opt/my ed/run"]
        );
        assert!(split_editor_command("   ").is_empty());
    }

    #[test]
    fn shred_zeroes_a_file_without_changing_its_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain");
        // Larger than one chunk, so the loop runs more than once.
        let content = "db-password \"hunter2\"\n".repeat(1000);
        std::fs::write(&path, &content).unwrap();

        shred(&path);

        let after = std::fs::read(&path).unwrap();
        assert_eq!(after.len(), content.len());
        assert!(
            after.iter().all(|b| *b == 0),
            "plaintext survived the shred"
        );
    }

    #[test]
    fn shredding_a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        shred(&dir.path().join("never-existed"));
    }

    #[test]
    fn edit_temp_path_lives_in_the_temp_dir_and_is_unpredictable() {
        let file = std::path::Path::new("repo/config/secrets.kdl");
        let a = edit_temp_path(file);
        let b = edit_temp_path(file);
        let tmp = std::env::temp_dir();
        assert_eq!(a.parent(), Some(tmp.as_path()));
        assert_ne!(a, b);
        let name = a.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("secrets-"), "got: {name}");
        assert!(name.ends_with(".kdl"), "got: {name}");
    }
}
