//! A passphrase-protected SSH key at `run`: without a terminal it must fail at once with
//! how to unlock it, never wait for input.
//!
//! Drives the binary with stdin not a terminal; no host or container needed. The inventory
//! host is a TEST-NET address, so reaching the connection stage would show up as a hang or
//! a connection error rather than the expected message.

use assert_cmd::Command;
use russh::keys::ssh_key::{Algorithm, LineEnding, PrivateKey};
use std::path::Path;
use std::time::Duration;

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("plan.kdl"),
        "plan \"p\" {\n    step \"s\" {\n        shell \"true\"\n    }\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("inventory.kdl"),
        "host \"target\" \"192.0.2.1\" user=\"root\"\n",
    )
    .unwrap();
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
        .unwrap()
        .encrypt(&mut rand::rng(), "hunter2")
        .unwrap();
    std::fs::write(
        dir.path().join("id_ed25519"),
        key.to_openssh(LineEnding::LF).unwrap().as_str(),
    )
    .unwrap();
    dir
}

fn run(dir: &Path, pass: Option<&str>) -> (bool, String) {
    let mut cmd = Command::cargo_bin("glidesh").unwrap();
    cmd.current_dir(dir)
        .args(["run", "-i", "inventory.kdl", "-p", "plan.kdl"])
        .args(["--no-tui", "--no-host-key-check", "--key", "id_ed25519"])
        .env_remove("GLIDESH_KEY_PASS")
        .timeout(Duration::from_secs(60));
    if let Some(pass) = pass {
        cmd.env("GLIDESH_KEY_PASS", pass);
    }
    let out = cmd.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), squash(&text))
}

/// The error report is wrapped to the terminal width with a gutter and colors, so compare
/// without them: every whitespace, gutter and escape sequence removed.
fn squash(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            '│' => {}
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out
}

fn has(text: &str, needle: &str) -> bool {
    text.contains(&squash(needle))
}

#[test]
fn a_protected_key_without_a_terminal_says_how_to_unlock_it() {
    let dir = fixture();
    let (ok, out) = run(dir.path(), None);
    assert!(!ok);
    assert!(
        has(&out, "is protected by a passphrase: set $GLIDESH_KEY_PASS"),
        "{out}"
    );
}

#[test]
fn a_wrong_passphrase_in_the_environment_is_reported_without_asking() {
    let dir = fixture();
    let (ok, out) = run(dir.path(), Some("not-it"));
    assert!(!ok);
    assert!(has(&out, "$GLIDESH_KEY_PASS does not unlock"), "{out}");
    assert!(!out.contains("not-it"), "the passphrase was echoed: {out}");
}
