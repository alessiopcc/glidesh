//! The `age` secrets provider: wraps the run's DEK to a set of SSH public keys.
//!
//! Where the [`passphrase`](super::passphrase) provider seals the DEK under one shared
//! secret, this one encrypts it to N recipients using the [age](https://age-encryption.org)
//! format — one stanza per recipient, any one of which unwraps it. Recipients are the
//! `ssh-ed25519 …` public keys people already hold to reach their hosts, so joining a team
//! costs a public key and nothing has to be distributed out of band.
//!
//! The wrapped blob is stored as `encryptedkey "agev1:<base64url>"`. It is an ordinary age
//! file, so `age -d -i ~/.ssh/id_ed25519` reads it without glidesh.
//!
//! Adding or removing a recipient re-wraps the same DEK. That is enough to *grant* access,
//! but not to revoke it: someone who kept the old blob can still unwrap it. Removal is
//! therefore paired with a data-key rotation — see `secret recipients rm`.

use crate::error::GlideshError;
use crate::secrets::config::SecretRecipient;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::io::{BufReader, Read, Write};
use std::path::Path;
use zeroize::Zeroizing;

const WRAP_PREFIX: &str = "agev1:";
const DEK_LEN: usize = 32;

/// Encrypt the DEK to every recipient, returning the `agev1:<base64url>` blob.
pub fn wrap_dek(
    dek: &[u8; DEK_LEN],
    recipients: &[SecretRecipient],
) -> Result<String, GlideshError> {
    if recipients.is_empty() {
        return Err(err("the age provider needs at least one recipient"));
    }
    let parsed = recipients
        .iter()
        .map(parse_recipient)
        .collect::<Result<Vec<_>, _>>()?;

    let encryptor =
        age::Encryptor::with_recipients(parsed.iter().map(|r| r as &dyn age::Recipient))
            .map_err(|e| err(format!("failed to build the recipient set: {e}")))?;

    let mut blob = Vec::new();
    let mut writer = encryptor
        .wrap_output(&mut blob)
        .map_err(|e| err(format!("failed to wrap the data key: {e}")))?;
    writer
        .write_all(dek.as_slice())
        .map_err(|e| err(format!("failed to wrap the data key: {e}")))?;
    writer
        .finish()
        .map_err(|e| err(format!("failed to wrap the data key: {e}")))?;

    Ok(format!("{WRAP_PREFIX}{}", URL_SAFE_NO_PAD.encode(&blob)))
}

/// Recover the DEK using an SSH private key that one of the recipients matches.
pub fn unwrap_dek(
    wrapped: &str,
    identity_path: &Path,
) -> Result<Zeroizing<[u8; DEK_LEN]>, GlideshError> {
    let body = wrapped.strip_prefix(WRAP_PREFIX).ok_or_else(|| {
        err("unsupported `encryptedkey` format for the age provider (expected `agev1:`)")
    })?;
    let blob = URL_SAFE_NO_PAD
        .decode(body)
        .map_err(|_| err("invalid base64 in `encryptedkey`"))?;

    let identity = read_identity(identity_path)?;
    let decryptor = age::Decryptor::new_buffered(&blob[..])
        .map_err(|e| err(format!("`encryptedkey` is not a valid age file: {e}")))?;
    let mut reader = decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .map_err(|e| match e {
            age::DecryptError::NoMatchingKeys => err(format!(
                "'{}' does not match any recipient of this secrets file — ask someone with \
                 access to add your public key with `glidesh secret recipients add`",
                identity_path.display()
            )),
            other => err(format!("could not unwrap the data key: {other}")),
        })?;

    // Zeroizing so the raw key is wiped even on the length-check error path.
    let mut bytes = Zeroizing::new(Vec::new());
    reader
        .read_to_end(&mut bytes)
        .map_err(|e| err(format!("could not read the unwrapped data key: {e}")))?;
    if bytes.len() != DEK_LEN {
        return Err(err("unexpected data key length"));
    }
    let mut dek = Zeroizing::new([0u8; DEK_LEN]);
    dek.copy_from_slice(&bytes[..]);
    Ok(dek)
}

/// Parse one `ssh-…` public key into an age recipient.
fn parse_recipient(recipient: &SecretRecipient) -> Result<age::ssh::Recipient, GlideshError> {
    recipient.key.parse::<age::ssh::Recipient>().map_err(|_| {
        err(format!(
            "recipient '{}' is not a usable SSH public key",
            recipient.name
        ))
    })
}

/// Load an SSH private key as an age identity.
///
/// Passphrase-protected keys are rejected with a clear message rather than silently
/// failing to match: glidesh's SSH transport does not take encrypted keys either, so this
/// keeps one constraint across the tool instead of two behaviours.
fn read_identity(path: &Path) -> Result<age::ssh::Identity, GlideshError> {
    let file = std::fs::File::open(path).map_err(|e| {
        err(format!(
            "failed to read the SSH key '{}': {e} — point at one with --secret-identity or \
             $GLIDESH_SECRET_IDENTITY",
            path.display()
        ))
    })?;
    let identity =
        age::ssh::Identity::from_buffer(BufReader::new(file), Some(path.display().to_string()))
            .map_err(|e| {
                err(format!(
                    "'{}' is not an SSH private key: {e}",
                    path.display()
                ))
            })?;

    match identity {
        age::ssh::Identity::Unencrypted(_) => Ok(identity),
        age::ssh::Identity::Encrypted(_) => Err(err(format!(
            "the SSH key '{}' is passphrase-protected; glidesh needs an unencrypted key, as \
             it does for its SSH transport",
            path.display()
        ))),
        age::ssh::Identity::Unsupported(_) => Err(err(format!(
            "the SSH key '{}' is of a type age cannot use (ed25519 and rsa are supported)",
            path.display()
        ))),
    }
}

fn err(message: impl Into<String>) -> GlideshError {
    GlideshError::Secret {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway ed25519 keypair: the private key on disk, the public key as a recipient.
    fn keypair(dir: &Path, name: &str) -> (std::path::PathBuf, SecretRecipient) {
        let key = ssh_key::PrivateKey::random(&mut rand::thread_rng(), ssh_key::Algorithm::Ed25519)
            .unwrap();
        let path = dir.join(name);
        std::fs::write(
            &path,
            key.to_openssh(ssh_key::LineEnding::LF).unwrap().as_str(),
        )
        .unwrap();
        let recipient = SecretRecipient {
            name: name.to_string(),
            key: key.public_key().to_openssh().unwrap(),
        };
        (path, recipient)
    }

    fn dek() -> [u8; DEK_LEN] {
        let mut k = [0u8; DEK_LEN];
        super::super::random_bytes(&mut k);
        k
    }

    #[test]
    fn any_recipient_can_unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let (alice_key, alice) = keypair(dir.path(), "alice");
        let (bob_key, bob) = keypair(dir.path(), "bob");
        let key = dek();

        let wrapped = wrap_dek(&key, &[alice, bob]).unwrap();
        assert!(wrapped.starts_with(WRAP_PREFIX));

        // Both recipients recover the same data key from the one blob.
        assert_eq!(*unwrap_dek(&wrapped, &alice_key).unwrap(), key);
        assert_eq!(*unwrap_dek(&wrapped, &bob_key).unwrap(), key);
    }

    #[test]
    fn a_non_recipient_is_told_what_to_do() {
        let dir = tempfile::tempdir().unwrap();
        let (_, alice) = keypair(dir.path(), "alice");
        let (mallory_key, _) = keypair(dir.path(), "mallory");

        let wrapped = wrap_dek(&dek(), &[alice]).unwrap();
        let error = unwrap_dek(&wrapped, &mallory_key).unwrap_err().to_string();
        assert!(error.contains("recipients add"), "got: {error}");
    }

    #[test]
    fn rewrapping_to_a_smaller_set_locks_the_removed_recipient_out() {
        let dir = tempfile::tempdir().unwrap();
        let (alice_key, alice) = keypair(dir.path(), "alice");
        let (bob_key, bob) = keypair(dir.path(), "bob");
        let key = dek();

        let both = wrap_dek(&key, &[alice.clone(), bob]).unwrap();
        let alice_only = wrap_dek(&key, &[alice]).unwrap();

        assert!(unwrap_dek(&alice_only, &alice_key).is_ok());
        assert!(unwrap_dek(&alice_only, &bob_key).is_err());
        // …but the blob Bob already had still works, which is why removal rotates the key.
        assert!(unwrap_dek(&both, &bob_key).is_ok());
    }

    #[test]
    fn no_recipients_is_rejected() {
        assert!(wrap_dek(&dek(), &[]).is_err());
    }

    #[test]
    fn a_bad_public_key_names_the_recipient() {
        let recipient = SecretRecipient {
            name: "typo".to_string(),
            key: "ssh-ed25519 not-actually-base64".to_string(),
        };
        let error = wrap_dek(&dek(), &[recipient]).unwrap_err().to_string();
        assert!(error.contains("typo"), "got: {error}");
    }

    #[test]
    fn a_passphrase_protected_key_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let (path, recipient) = keypair(dir.path(), "alice");
        let encrypted = ssh_key::PrivateKey::read_openssh_file(&path)
            .unwrap()
            .encrypt(&mut rand::thread_rng(), "hunter2")
            .unwrap();
        std::fs::write(
            &path,
            encrypted
                .to_openssh(ssh_key::LineEnding::LF)
                .unwrap()
                .as_str(),
        )
        .unwrap();

        let wrapped = wrap_dek(&dek(), &[recipient]).unwrap();
        let error = unwrap_dek(&wrapped, &path).unwrap_err().to_string();
        assert!(error.contains("passphrase-protected"), "got: {error}");
    }
}
