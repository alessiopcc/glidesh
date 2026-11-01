//! Loading the SSH private key, unlocking it when it is protected by a passphrase.

use crate::error::GlideshError;
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::keys::ssh_key::{Algorithm, HashAlg, PrivateKey};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use zeroize::Zeroizing;

/// The environment variable a protected key's passphrase is read from before asking.
pub const PASS_ENV: &str = "GLIDESH_KEY_PASS";

/// Wrong passphrases typed at the terminal before giving up, as OpenSSH allows.
const TRIES: usize = 3;

/// Reads a passphrase typed at the terminal after showing a prompt.
pub(crate) type Ask<'a> = &'a dyn Fn(&str) -> std::io::Result<Zeroizing<String>>;

/// The key at `path`, ready to authenticate with.
pub fn load(path: &Path) -> Result<PrivateKeyWithHashAlg, GlideshError> {
    tracing::debug!("Using SSH key: {}", path.display());
    let key = load_private_key(path)?;
    let hash_alg = match key.algorithm() {
        Algorithm::Rsa { .. } => Some(HashAlg::Sha256),
        _ => None,
    };
    Ok(PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg))
}

/// The key at `path`, decrypted. Its passphrase is asked for once per run: the secrets
/// provider may read the same key the SSH transport does.
pub fn load_private_key(path: &Path) -> Result<PrivateKey, GlideshError> {
    use std::io::IsTerminal;
    let env = std::env::var(PASS_ENV)
        .ok()
        .filter(|p| !p.is_empty())
        .map(Zeroizing::new);
    let ask = |prompt: &str| rpassword::prompt_password(prompt).map(Zeroizing::new);
    let terminal = std::io::stdin().is_terminal();
    unlock(
        path,
        |pass| open(path, pass),
        env,
        terminal.then_some(&ask as Ask),
    )
}

/// Why a key did not open.
#[derive(Debug)]
pub(crate) enum Locked {
    /// It is protected, and no passphrase was given.
    NeedsPassphrase,
    /// It could not be read, or the passphrase given does not decrypt it.
    Failed(String),
}

fn open(path: &Path, passphrase: Option<&str>) -> Result<PrivateKey, Locked> {
    match russh::keys::load_secret_key(path, passphrase) {
        Ok(key) => Ok(key),
        Err(russh::keys::Error::KeyIsEncrypted) if passphrase.is_none() => {
            Err(Locked::NeedsPassphrase)
        }
        // Encrypted PKCS#8 and PuTTY keys fail to parse without a passphrase instead of
        // reporting that they are encrypted, so their headers tell.
        Err(_)
            if passphrase.is_none()
                && std::fs::read_to_string(path).is_ok_and(|text| is_encrypted(&text)) =>
        {
            Err(Locked::NeedsPassphrase)
        }
        Err(e) => Err(Locked::Failed(e.to_string())),
    }
}

fn is_encrypted(key: &str) -> bool {
    key.contains("-----BEGIN ENCRYPTED PRIVATE KEY-----")
        || (key.starts_with("PuTTY-User-Key-File-")
            && key.lines().any(|line| {
                line.starts_with("Encryption:") && line.trim_end() != "Encryption: none"
            }))
}

fn passphrases() -> &'static Mutex<HashMap<PathBuf, Zeroizing<String>>> {
    static PASSPHRASES: OnceLock<Mutex<HashMap<PathBuf, Zeroizing<String>>>> = OnceLock::new();
    PASSPHRASES.get_or_init(Default::default)
}

/// Keeps the passphrase that unlocked `path` for the rest of the run.
pub(crate) fn remember(path: &Path, passphrase: Zeroizing<String>) {
    if let Ok(mut known) = passphrases().lock() {
        known.insert(path.to_path_buf(), passphrase);
    }
}

/// Opens the key at `path` with `open`: without a passphrase, then with the one this run
/// already used for it, then `env`'s, then up to [`TRIES`] typed at `ask` when there is a
/// terminal to ask at.
pub(crate) fn unlock<T>(
    path: &Path,
    open: impl Fn(Option<&str>) -> Result<T, Locked>,
    env: Option<Zeroizing<String>>,
    ask: Option<Ask>,
) -> Result<T, GlideshError> {
    let failed = |message: String| GlideshError::KeyLoad { message };
    match open(None) {
        Ok(key) => return Ok(key),
        Err(Locked::Failed(e)) => return Err(failed(e)),
        Err(Locked::NeedsPassphrase) => {}
    }
    let known = passphrases().lock().ok().and_then(|p| p.get(path).cloned());
    if let Some(known) = known
        && let Ok(key) = open(Some(&known))
    {
        return Ok(key);
    }
    if let Some(env) = env {
        return match open(Some(&env)) {
            Ok(key) => {
                remember(path, env);
                Ok(key)
            }
            Err(Locked::Failed(e)) => Err(failed(format!(
                "${PASS_ENV} does not unlock the SSH key '{}': {e}",
                path.display()
            ))),
            Err(Locked::NeedsPassphrase) => Err(failed(format!(
                "${PASS_ENV} does not unlock the SSH key '{}'",
                path.display()
            ))),
        };
    }
    let Some(ask) = ask else {
        return Err(failed(format!(
            "the SSH key '{}' is protected by a passphrase: set ${PASS_ENV}, or run at a \
             terminal to be asked for it",
            path.display()
        )));
    };
    let prompt = format!("Passphrase for key '{}': ", path.display());
    let mut last = String::new();
    for _ in 0..TRIES {
        let passphrase = ask(&prompt).map_err(|e| {
            failed(format!(
                "could not read the passphrase for '{}': {e}",
                path.display()
            ))
        })?;
        match open(Some(&passphrase)) {
            Ok(key) => {
                remember(path, passphrase);
                return Ok(key);
            }
            Err(Locked::Failed(e)) => {
                eprintln!("The passphrase does not unlock '{}'.", path.display());
                last = e;
            }
            Err(Locked::NeedsPassphrase) => last = "no passphrase given".to_string(),
        }
    }
    Err(failed(format!(
        "could not unlock the SSH key '{}' after {TRIES} tries: {last}",
        path.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A key that opens only with "right", at a path no other test uses.
    fn locked(dir: &tempfile::TempDir) -> (PathBuf, impl Fn(Option<&str>) -> Result<(), Locked>) {
        let open = |pass: Option<&str>| match pass {
            None => Err(Locked::NeedsPassphrase),
            Some("right") => Ok(()),
            Some(_) => Err(Locked::Failed("wrong passphrase".to_string())),
        };
        (dir.path().join("id"), open)
    }

    fn pass(p: &str) -> Zeroizing<String> {
        Zeroizing::new(p.to_string())
    }

    #[test]
    fn an_unprotected_key_opens_without_asking() {
        let ask = |_: &str| -> std::io::Result<Zeroizing<String>> { panic!("asked") };
        assert!(unlock(Path::new("id"), |_| Ok(()), None, Some(&ask)).is_ok());
    }

    #[test]
    fn the_environment_passphrase_is_used_before_asking() {
        let dir = tempfile::tempdir().unwrap();
        let (path, open) = locked(&dir);
        let ask = |_: &str| -> std::io::Result<Zeroizing<String>> { panic!("asked") };
        assert!(unlock(&path, &open, Some(pass("right")), Some(&ask)).is_ok());

        let dir = tempfile::tempdir().unwrap();
        let (path, open) = locked(&dir);
        let error = unlock(&path, &open, Some(pass("nope")), Some(&ask)).unwrap_err();
        assert!(error.to_string().contains(PASS_ENV), "{error}");
    }

    #[test]
    fn without_a_terminal_or_the_environment_it_says_how_to_unlock() {
        let dir = tempfile::tempdir().unwrap();
        let (path, open) = locked(&dir);
        let error = unlock(&path, open, None, None).unwrap_err().to_string();
        assert!(
            error.contains(PASS_ENV) && error.contains("terminal"),
            "{error}"
        );
    }

    #[test]
    fn a_typed_passphrase_gets_three_tries() {
        let dir = tempfile::tempdir().unwrap();
        let (path, open) = locked(&dir);
        let asked = Cell::new(0);
        let ask = |_: &str| {
            asked.set(asked.get() + 1);
            Ok(pass(if asked.get() == 3 { "right" } else { "nope" }))
        };
        assert!(unlock(&path, &open, None, Some(&ask)).is_ok());
        assert_eq!(asked.get(), 3);

        let dir = tempfile::tempdir().unwrap();
        let (path, open) = locked(&dir);
        let ask = |_: &str| Ok(pass("nope"));
        let error = unlock(&path, open, None, Some(&ask))
            .unwrap_err()
            .to_string();
        assert!(error.contains("after 3 tries"), "{error}");
    }

    #[test]
    fn a_key_is_asked_for_once_per_run() {
        let dir = tempfile::tempdir().unwrap();
        let (path, open) = locked(&dir);
        let ask = |_: &str| Ok(pass("right"));
        assert!(unlock(&path, &open, None, Some(&ask)).is_ok());
        assert!(unlock(&path, &open, None, None).is_ok());
    }

    #[test]
    fn a_protected_openssh_key_needs_its_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("id_ed25519");
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
            .unwrap()
            .encrypt(&mut rand::rng(), "right")
            .unwrap();
        std::fs::write(
            &path,
            key.to_openssh(russh::keys::ssh_key::LineEnding::LF)
                .unwrap()
                .as_str(),
        )
        .unwrap();

        assert!(matches!(open(&path, None), Err(Locked::NeedsPassphrase)));
        assert!(matches!(open(&path, Some("nope")), Err(Locked::Failed(_))));
        assert!(!open(&path, Some("right")).unwrap().is_encrypted());
    }

    #[test]
    fn a_protected_pkcs8_key_needs_its_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("id_pkcs8");
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let mut pem = Vec::new();
        russh::keys::encode_pkcs8_pem_encrypted(&key, b"right", 1, &mut pem).unwrap();
        std::fs::write(&path, pem).unwrap();

        assert!(matches!(open(&path, None), Err(Locked::NeedsPassphrase)));
        assert!(open(&path, Some("right")).is_ok());
    }

    #[test]
    fn a_putty_key_is_encrypted_unless_it_says_none() {
        let ppk = |encryption: &str| {
            format!("PuTTY-User-Key-File-3: ssh-ed25519\nEncryption: {encryption}\nComment: k\n")
        };
        assert!(is_encrypted(&ppk("aes256-cbc")));
        assert!(!is_encrypted(&ppk("none")));
    }
}
