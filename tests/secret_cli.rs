//! End-to-end tests for the `glidesh secret` CLI. These need no SSH/Docker, so they run
//! in the normal `cargo test` pass (unlike the `integration_*` suites).

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::tempdir;

fn glidesh() -> Command {
    Command::cargo_bin("glidesh").unwrap()
}

#[test]
fn init_set_get_round_trip_and_encrypts_at_rest() {
    let dir = tempdir().unwrap();
    let sf = dir.path().join("secrets.kdl");
    let sf = sf.to_str().unwrap();

    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "init", "--file", sf])
        .assert()
        .success();

    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "set", "db-password", "s3cr3t!value", "--file", sf])
        .assert()
        .success();

    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "get", "db-password", "--file", sf])
        .assert()
        .success()
        .stdout(predicate::str::contains("s3cr3t!value"));

    // The value is stored as ciphertext, never plaintext.
    let content = std::fs::read_to_string(sf).unwrap();
    assert!(
        content.contains("secret:v1:"),
        "expected a token, got: {content}"
    );
    assert!(
        !content.contains("s3cr3t!value"),
        "plaintext leaked to disk"
    );
}

#[test]
fn wrong_passphrase_fails() {
    let dir = tempdir().unwrap();
    let sf = dir.path().join("secrets.kdl");
    let sf = sf.to_str().unwrap();

    glidesh()
        .env("GLIDESH_SECRET_PASS", "right")
        .args(["secret", "init", "--file", sf])
        .assert()
        .success();
    glidesh()
        .env("GLIDESH_SECRET_PASS", "right")
        .args(["secret", "set", "k", "v", "--file", sf])
        .assert()
        .success();
    glidesh()
        .env("GLIDESH_SECRET_PASS", "wrong")
        .args(["secret", "get", "k", "--file", sf])
        .assert()
        .failure()
        .stderr(predicate::str::contains("wrong passphrase"));
}

#[test]
fn get_missing_key_fails() {
    let dir = tempdir().unwrap();
    let sf = dir.path().join("secrets.kdl");
    let sf = sf.to_str().unwrap();
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "init", "--file", sf])
        .assert()
        .success();
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "get", "nope", "--file", sf])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no secret named"));
}

#[test]
fn encrypt_from_stdin_emits_token() {
    let dir = tempdir().unwrap();
    let sf = dir.path().join("secrets.kdl");
    let sf = sf.to_str().unwrap();
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "init", "--file", sf])
        .assert()
        .success();
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "encrypt", "--file", sf])
        .write_stdin("piped-secret")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("secret:v1:"));
}

#[test]
fn double_init_refuses() {
    let dir = tempdir().unwrap();
    let sf = dir.path().join("secrets.kdl");
    let sf = sf.to_str().unwrap();
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "init", "--file", sf])
        .assert()
        .success();
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "init", "--file", sf])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
}

/// A value piped in with CRLF must not smuggle a trailing carriage return into the
/// ciphertext. The token is stored by hand because only `get` can decrypt it back.
#[test]
fn encrypt_strips_crlf_from_stdin() {
    let dir = tempdir().unwrap();
    let sf = dir.path().join("secrets.kdl");
    let sf = sf.to_str().unwrap();
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "init", "--file", sf])
        .assert()
        .success();

    let out = glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "encrypt", "--file", sf])
        .write_stdin("piped-secret\r\n")
        .output()
        .unwrap();
    assert!(out.status.success());
    let token = String::from_utf8(out.stdout).unwrap().trim().to_string();

    let mut content = std::fs::read_to_string(sf).unwrap();
    content.push_str(&format!("piped-key \"{token}\"\n"));
    std::fs::write(sf, content).unwrap();

    let got = glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "get", "piped-key", "--file", sf])
        .output()
        .unwrap();
    assert!(got.status.success());
    assert_eq!(String::from_utf8(got.stdout).unwrap(), "piped-secret\n");
}

/// The transient decrypted view goes to the system temp directory, never next to the
/// secrets file where a hard kill would strand plaintext inside a repository.
#[test]
fn edit_writes_its_plaintext_view_outside_the_secrets_directory() {
    let dir = tempdir().unwrap();
    let sf = dir.path().join("secrets.kdl");
    let sf = sf.to_str().unwrap();
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "init", "--file", sf])
        .assert()
        .success();
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "set", "db-password", "s3cr3t", "--file", sf])
        .assert()
        .success();

    // An "editor" that echoes the path it was handed and exits successfully.
    let editor = if cfg!(windows) { "cmd /c echo" } else { "echo" };
    let out = glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .env("VISUAL", editor)
        .args(["secret", "edit", "--file", sf])
        .output()
        .unwrap();
    assert!(out.status.success(), "edit failed: {out:?}");
    let printed = String::from_utf8_lossy(&out.stdout).to_string();

    let tmp = std::env::temp_dir();
    let tmp = tmp.to_str().unwrap().trim_end_matches(['/', '\\']);
    assert!(
        printed.contains(tmp),
        "editor was not handed a temp-dir path: {printed}"
    );
    assert!(
        !printed.contains("kdl.edit"),
        "plaintext view was created beside the secrets file: {printed}"
    );

    // The round-trip still works and the value is still ciphertext at rest.
    let got = glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "get", "db-password", "--file", sf])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(got.stdout).unwrap(), "s3cr3t\n");
    let content = std::fs::read_to_string(sf).unwrap();
    assert!(content.contains("secret:v1:"), "not encrypted: {content}");
    assert!(!content.contains("s3cr3t"), "plaintext leaked: {content}");
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "stray files left beside the secrets file"
    );
}

/// CI can mount a passphrase file instead of exporting the value into an environment
/// every child process can read. `GLIDESH_SECRET_PASS_FILE` works for every subcommand.
#[test]
fn passphrase_can_come_from_a_file() {
    let dir = tempdir().unwrap();
    let sf = dir.path().join("secrets.kdl");
    let sf = sf.to_str().unwrap();
    let pass_file = dir.path().join("pass.txt");
    std::fs::write(&pass_file, "file-held-passphrase\n").unwrap();
    let pass_file = pass_file.to_str().unwrap();

    glidesh()
        .env_remove("GLIDESH_SECRET_PASS")
        .env("GLIDESH_SECRET_PASS_FILE", pass_file)
        .args(["secret", "init", "--file", sf])
        .assert()
        .success();
    glidesh()
        .env_remove("GLIDESH_SECRET_PASS")
        .env("GLIDESH_SECRET_PASS_FILE", pass_file)
        .args(["secret", "set", "db-password", "s3cr3t", "--file", sf])
        .assert()
        .success();

    // The file really is the key: the same passphrase typed as an env var also unlocks it.
    glidesh()
        .env("GLIDESH_SECRET_PASS", "file-held-passphrase")
        .args(["secret", "get", "db-password", "--file", sf])
        .assert()
        .success()
        .stdout(predicate::str::contains("s3cr3t"));

    // A file holding the wrong passphrase fails the same way a wrong env var does.
    let wrong = dir.path().join("wrong.txt");
    std::fs::write(&wrong, "nope\n").unwrap();
    glidesh()
        .env_remove("GLIDESH_SECRET_PASS")
        .env("GLIDESH_SECRET_PASS_FILE", wrong.to_str().unwrap())
        .args(["secret", "get", "db-password", "--file", sf])
        .assert()
        .failure()
        .stderr(predicate::str::contains("wrong passphrase"));
}

/// An unreadable or empty passphrase file must say so, not fall through to a prompt that
/// nothing is attached to in CI.
#[test]
fn missing_passphrase_file_is_reported() {
    let dir = tempdir().unwrap();
    let sf = dir.path().join("secrets.kdl");
    let sf = sf.to_str().unwrap();
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "init", "--file", sf])
        .assert()
        .success();
    // The key must exist, or `get` reports that instead of ever reaching the passphrase.
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "set", "db-password", "s3cr3t", "--file", sf])
        .assert()
        .success();

    glidesh()
        .env_remove("GLIDESH_SECRET_PASS")
        .env(
            "GLIDESH_SECRET_PASS_FILE",
            dir.path().join("absent.txt").to_str().unwrap(),
        )
        .args(["secret", "get", "db-password", "--file", sf])
        .assert()
        .failure()
        .stderr(predicate::str::contains("passphrase file"));
}

/// A value too short to redact is accepted, but the operator is told at the moment they
/// create it rather than discovering it in a log much later.
#[test]
fn setting_a_too_short_secret_warns() {
    let dir = tempdir().unwrap();
    let sf = dir.path().join("secrets.kdl");
    let sf = sf.to_str().unwrap();
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "init", "--file", sf])
        .assert()
        .success();

    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "set", "pin", "ab", "--file", sf])
        .assert()
        .success()
        .stderr(predicate::str::contains("cannot be masked"));

    // A normal-length value is stored without complaint.
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "set", "db-password", "long-enough", "--file", sf])
        .assert()
        .success()
        .stderr(predicate::str::contains("cannot be masked").not());
}

/// Listing shows what a file holds without decrypting anything — deliberately no
/// passphrase, and never a value.
#[test]
fn list_names_values_without_revealing_them() {
    let dir = tempdir().unwrap();
    let sf = dir.path().join("secrets.kdl");
    let sf = sf.to_str().unwrap();
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "init", "--file", sf])
        .assert()
        .success();
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "set", "db-password", "s3cr3t-value", "--file", sf])
        .assert()
        .success();
    // A plaintext companion value, written by hand the way a user would.
    let mut content = std::fs::read_to_string(sf).unwrap();
    content.push_str("region \"eu-west-1\"\n");
    std::fs::write(sf, content).unwrap();

    let out = glidesh()
        .env_remove("GLIDESH_SECRET_PASS")
        .env_remove("GLIDESH_SECRET_PASS_FILE")
        .args(["secret", "list", "--file", sf])
        .output()
        .unwrap();
    assert!(out.status.success(), "list needs no passphrase: {out:?}");
    let listing = String::from_utf8(out.stdout).unwrap();
    assert!(listing.contains("db-password"), "{listing}");
    assert!(listing.contains("encrypted"), "{listing}");
    assert!(listing.contains("region"), "{listing}");
    assert!(listing.contains("plaintext"), "{listing}");
    assert!(!listing.contains("s3cr3t-value"), "value leaked: {listing}");
}

/// `rm` deletes one value and leaves the file otherwise intact.
#[test]
fn rm_deletes_one_value_and_keeps_the_rest() {
    let dir = tempdir().unwrap();
    let sf = dir.path().join("secrets.kdl");
    let sf = sf.to_str().unwrap();
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "init", "--file", sf])
        .assert()
        .success();
    for key in ["db-password", "api-token"] {
        glidesh()
            .env("GLIDESH_SECRET_PASS", "pw")
            .args(["secret", "set", key, "a-value", "--file", sf])
            .assert()
            .success();
    }

    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "rm", "db-password", "--file", sf])
        .assert()
        .success();

    let content = std::fs::read_to_string(sf).unwrap();
    assert!(!content.contains("db-password"), "{content}");
    assert!(content.contains("api-token"), "{content}");
    assert!(
        content.contains("encryptedkey"),
        "provider block lost: {content}"
    );

    // Removing something that is not there says so rather than silently succeeding.
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "rm", "db-password", "--file", sf])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no secret named"));
}

/// A token minted by `secret encrypt` for pasting inline can be read back directly,
/// without first storing it under some scratch key.
#[test]
fn decrypt_reads_a_bare_token() {
    let dir = tempdir().unwrap();
    let sf = dir.path().join("secrets.kdl");
    let sf = sf.to_str().unwrap();
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "init", "--file", sf])
        .assert()
        .success();

    let out = glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "encrypt", "--file", sf])
        .write_stdin("inline-value")
        .output()
        .unwrap();
    let token = String::from_utf8(out.stdout).unwrap().trim().to_string();

    let back = glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "decrypt", &token, "--file", sf])
        .output()
        .unwrap();
    assert!(back.status.success(), "{back:?}");
    assert_eq!(String::from_utf8(back.stdout).unwrap(), "inline-value\n");
}

/// An edit that changes nothing must change nothing: comments survive, and untouched
/// secrets keep their existing token instead of being re-encrypted under a fresh nonce.
#[test]
fn a_no_op_edit_leaves_the_file_byte_identical() {
    let dir = tempdir().unwrap();
    let sf = dir.path().join("secrets.kdl");
    let sf = sf.to_str().unwrap();
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "init", "--file", sf])
        .assert()
        .success();
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "set", "db-password", "s3cr3t", "--file", sf])
        .assert()
        .success();

    let annotated = format!(
        "// rotated 2026-03, owner: platform\n{}",
        std::fs::read_to_string(sf).unwrap()
    );
    std::fs::write(sf, &annotated).unwrap();

    // An "editor" that saves the file untouched.
    let editor = if cfg!(windows) { "cmd /c echo" } else { "echo" };
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .env("VISUAL", editor)
        .args(["secret", "edit", "--file", sf])
        .assert()
        .success();

    assert_eq!(
        std::fs::read_to_string(sf).unwrap(),
        annotated,
        "a no-op edit rewrote the file"
    );
}

/// `validate` parses a secrets file sitting beside the inventory, so a malformed provider
/// block is caught before a run reaches it.
#[test]
fn validate_checks_the_secrets_file() {
    let dir = tempdir().unwrap();
    let inv = dir.path().join("inventory.kdl");
    std::fs::write(&inv, "host \"node-1\" \"10.0.0.1\" user=\"deploy\"\n").unwrap();
    let sf = dir.path().join("secrets.kdl");
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "init", "--file", sf.to_str().unwrap()])
        .assert()
        .success();

    glidesh()
        .args(["validate", "-i", inv.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Validating secrets"))
        .stdout(predicate::str::contains("provider passphrase"));

    // A broken provider block fails validation instead of surfacing mid-run.
    std::fs::write(&sf, "secrets {\n    provider \"magic\"\n}\n").unwrap();
    glidesh()
        .args(["validate", "-i", inv.to_str().unwrap()])
        .assert()
        .failure()
        .stdout(predicate::str::contains("FAILED"));
}

/// Build a file holding a scalar secret, a structured block whose row carries a token, and
/// a hand-written comment. Returns the file path and the two plaintexts.
fn seeded_vault(dir: &std::path::Path, pass: &str) -> String {
    let sf = dir.join("secrets.kdl").to_str().unwrap().to_string();
    glidesh()
        .env("GLIDESH_SECRET_PASS", pass)
        .args(["secret", "init", "--file", &sf])
        .assert()
        .success();
    glidesh()
        .env("GLIDESH_SECRET_PASS", pass)
        .args([
            "secret",
            "set",
            "db-password",
            "scalar-plaintext",
            "--file",
            &sf,
        ])
        .assert()
        .success();

    // A structured row, minted the way the docs tell you to: `encrypt` then paste.
    let out = glidesh()
        .env("GLIDESH_SECRET_PASS", pass)
        .args(["secret", "encrypt", "--file", &sf])
        .write_stdin("row-plaintext")
        .output()
        .unwrap();
    let row_token = String::from_utf8(out.stdout).unwrap().trim().to_string();

    let content = std::fs::read_to_string(&sf).unwrap();
    std::fs::write(
        &sf,
        format!(
            "// team credentials\n{content}api-keys {{\n    - name=\"billing\" value=\"{row_token}\"\n}}\n"
        ),
    )
    .unwrap();
    sf
}

/// Rotating the data key re-encrypts every token in the file — including one inside a
/// structured block, which the parser-driven paths never touch — and retires the old key.
#[test]
fn rotate_data_key_reencrypts_every_value() {
    let dir = tempdir().unwrap();
    let sf = seeded_vault(dir.path(), "old-pass");
    let before = std::fs::read_to_string(&sf).unwrap();

    let new_pass = dir.path().join("new.txt");
    std::fs::write(&new_pass, "new-pass\n").unwrap();

    glidesh()
        .env("GLIDESH_SECRET_PASS", "old-pass")
        .args([
            "secret",
            "rekey",
            "--rotate-data-key",
            "--new-pass-file",
            new_pass.to_str().unwrap(),
            "--file",
            &sf,
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("2 value(s) re-encrypted"));

    let after = std::fs::read_to_string(&sf).unwrap();

    // Every token changed, and so did the wrapped key.
    assert!(after.contains("secret:v1:"), "no tokens survive: {after}");
    for line in before.lines().filter(|l| l.contains("secret:v1:")) {
        assert!(
            !after.contains(line.trim()),
            "token survived rotation: {line}"
        );
    }
    assert!(
        !after.contains(
            before
                .lines()
                .find(|l| l.contains("encryptedkey"))
                .unwrap()
                .trim()
        )
    );
    assert!(
        after.contains("// team credentials"),
        "comment lost: {after}"
    );
    assert!(after.contains("api-keys"), "block lost: {after}");

    // The old passphrase is dead.
    glidesh()
        .env("GLIDESH_SECRET_PASS", "old-pass")
        .args(["secret", "get", "db-password", "--file", &sf])
        .assert()
        .failure()
        .stderr(predicate::str::contains("wrong passphrase"));

    // The new one reads both values back unchanged — the scalar by name, the row's token
    // straight off the line it now sits on.
    let got = glidesh()
        .env("GLIDESH_SECRET_PASS", "new-pass")
        .args(["secret", "get", "db-password", "--file", &sf])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(got.stdout).unwrap(), "scalar-plaintext\n");

    let row_token: String = after
        .lines()
        .find(|l| l.contains("name=\"billing\""))
        .and_then(|l| l.split('"').find(|p| p.starts_with("secret:v1:")))
        .expect("row token")
        .to_string();
    let got = glidesh()
        .env("GLIDESH_SECRET_PASS", "new-pass")
        .args(["secret", "decrypt", &row_token, "--file", &sf])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(got.stdout).unwrap(), "row-plaintext\n");
}

/// Plain `rekey` is the other operation: the passphrase changes, the data key and every
/// value token do not. This is the distinction the two commands exist to draw.
#[test]
fn plain_rekey_leaves_every_token_untouched() {
    let dir = tempdir().unwrap();
    let sf = seeded_vault(dir.path(), "old-pass");
    let before = std::fs::read_to_string(&sf).unwrap();

    let new_pass = dir.path().join("new.txt");
    std::fs::write(&new_pass, "new-pass\n").unwrap();

    glidesh()
        .env("GLIDESH_SECRET_PASS", "old-pass")
        .args([
            "secret",
            "rekey",
            "--new-pass-file",
            new_pass.to_str().unwrap(),
            "--file",
            &sf,
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("value tokens unchanged"));

    let after = std::fs::read_to_string(&sf).unwrap();
    for line in before.lines().filter(|l| l.contains("secret:v1:")) {
        assert!(after.contains(line), "token was rewritten: {line}");
    }
    // Only the wrapped key moved.
    assert_ne!(before, after);

    glidesh()
        .env("GLIDESH_SECRET_PASS", "new-pass")
        .args(["secret", "get", "db-password", "--file", &sf])
        .assert()
        .success()
        .stdout(predicate::str::contains("scalar-plaintext"));
}

/// A throwaway ed25519 keypair on disk. Returns (private key path, public key path); the
/// public key carries a comment, which is where the recipient name comes from.
fn keypair(dir: &std::path::Path, name: &str) -> (String, String) {
    let key =
        ssh_key::PrivateKey::random(&mut rand::thread_rng(), ssh_key::Algorithm::Ed25519).unwrap();
    let private = dir.join(name);
    std::fs::write(
        &private,
        key.to_openssh(ssh_key::LineEnding::LF).unwrap().as_str(),
    )
    .unwrap();
    let public = dir.join(format!("{name}.pub"));
    std::fs::write(
        &public,
        format!("{} {name}@test\n", key.public_key().to_openssh().unwrap()),
    )
    .unwrap();
    (
        private.to_str().unwrap().to_string(),
        public.to_str().unwrap().to_string(),
    )
}

/// The whole point of the age provider: access is granted and revoked per person, with no
/// shared secret, and revocation actually locks the removed person out.
#[test]
fn secret_identity_flag_unlocks_an_age_vault_and_outranks_the_environment() {
    let dir = tempdir().unwrap();
    let (alice_key, alice_pub) = keypair(dir.path(), "alice");
    let (bob_key, _bob_pub) = keypair(dir.path(), "bob");
    let sf = dir.path().join("secrets.kdl");
    let sf = sf.to_str().unwrap();

    glidesh()
        .args([
            "secret",
            "init",
            "--provider",
            "age",
            "--recipient",
            &alice_pub,
            "--file",
            sf,
        ])
        .assert()
        .success();

    // The flag alone unlocks the vault, with no GLIDESH_SECRET_IDENTITY set and the key
    // nowhere near ~/.ssh — the gap that made `secret` unusable with a non-default key.
    glidesh()
        .args([
            "secret",
            "set",
            "db-password",
            "hunter2-value",
            "--file",
            sf,
            "--secret-identity",
            &alice_key,
        ])
        .assert()
        .success();

    // Global, so it is equally accepted ahead of the subcommand.
    glidesh()
        .args([
            "secret",
            "--secret-identity",
            &alice_key,
            "get",
            "db-password",
            "--file",
            sf,
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hunter2-value"));

    // And it outranks the environment: the env names a key that was never a recipient.
    glidesh()
        .env("GLIDESH_SECRET_IDENTITY", &bob_key)
        .args([
            "secret",
            "get",
            "db-password",
            "--file",
            sf,
            "--secret-identity",
            &alice_key,
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("hunter2-value"));
}

#[test]
fn age_provider_grants_and_revokes_access() {
    let dir = tempdir().unwrap();
    let (alice_key, alice_pub) = keypair(dir.path(), "alice");
    let (bob_key, bob_pub) = keypair(dir.path(), "bob");
    let sf = dir.path().join("secrets.kdl");
    let sf = sf.to_str().unwrap();

    glidesh()
        .args([
            "secret",
            "init",
            "--provider",
            "age",
            "--recipient",
            &alice_pub,
            "--file",
            sf,
        ])
        .assert()
        .success();
    glidesh()
        .env("GLIDESH_SECRET_IDENTITY", &alice_key)
        .args([
            "secret",
            "set",
            "db-password",
            "hunter2-value",
            "--file",
            sf,
        ])
        .assert()
        .success();

    // Bob is not a recipient yet, and the error tells him how to become one.
    glidesh()
        .env("GLIDESH_SECRET_IDENTITY", &bob_key)
        .args(["secret", "get", "db-password", "--file", sf])
        .assert()
        .failure()
        .stderr(predicate::str::contains("recipients add"));

    glidesh()
        .env("GLIDESH_SECRET_IDENTITY", &alice_key)
        .args(["secret", "recipients", "add", &bob_pub, "--file", sf])
        .assert()
        .success();

    // Adding a recipient re-wraps the same data key, so Bob reads existing values.
    glidesh()
        .env("GLIDESH_SECRET_IDENTITY", &bob_key)
        .args(["secret", "get", "db-password", "--file", sf])
        .assert()
        .success()
        .stdout(predicate::str::contains("hunter2-value"));

    glidesh()
        .args(["secret", "recipients", "list", "--file", sf])
        .assert()
        .success()
        .stdout(predicate::str::contains("alice@test"))
        .stdout(predicate::str::contains("bob@test"));

    let before = std::fs::read_to_string(sf).unwrap();

    // Removing Bob rotates the data key, so the copy he already has is worthless.
    glidesh()
        .env("GLIDESH_SECRET_IDENTITY", &alice_key)
        .args(["secret", "recipients", "rm", "bob@test", "--file", sf])
        .assert()
        .success()
        .stdout(predicate::str::contains("Rotated the data key"));

    let after = std::fs::read_to_string(sf).unwrap();
    for line in before.lines().filter(|l| l.contains("secret:v1:")) {
        assert!(
            !after.contains(line.trim()),
            "token survived removal: {line}"
        );
    }
    glidesh()
        .env("GLIDESH_SECRET_IDENTITY", &bob_key)
        .args(["secret", "get", "db-password", "--file", sf])
        .assert()
        .failure();
    glidesh()
        .env("GLIDESH_SECRET_IDENTITY", &alice_key)
        .args(["secret", "get", "db-password", "--file", sf])
        .assert()
        .success()
        .stdout(predicate::str::contains("hunter2-value"));
}

/// `--keep-data-key` is the fast path, and it says out loud what it costs.
#[test]
fn keeping_the_data_key_on_removal_warns_and_leaves_tokens_alone() {
    let dir = tempdir().unwrap();
    let (alice_key, alice_pub) = keypair(dir.path(), "alice");
    let (_bob_key, bob_pub) = keypair(dir.path(), "bob");
    let sf = dir.path().join("secrets.kdl");
    let sf = sf.to_str().unwrap();

    glidesh()
        .args([
            "secret",
            "init",
            "--provider",
            "age",
            "--recipient",
            &alice_pub,
            "--recipient",
            &bob_pub,
            "--file",
            sf,
        ])
        .assert()
        .success();
    glidesh()
        .env("GLIDESH_SECRET_IDENTITY", &alice_key)
        .args([
            "secret",
            "set",
            "db-password",
            "hunter2-value",
            "--file",
            sf,
        ])
        .assert()
        .success();
    let before = std::fs::read_to_string(sf).unwrap();

    glidesh()
        .env("GLIDESH_SECRET_IDENTITY", &alice_key)
        .args([
            "secret",
            "recipients",
            "rm",
            "bob@test",
            "--keep-data-key",
            "--file",
            sf,
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("can still decrypt"));

    let after = std::fs::read_to_string(sf).unwrap();
    let token = |text: &str| {
        text.lines()
            .find(|l| l.starts_with("db-password"))
            .unwrap()
            .to_string()
    };
    assert_eq!(token(&before), token(&after), "tokens should be untouched");
    assert!(
        !after.contains("bob@test"),
        "recipient not removed: {after}"
    );
}

/// The two providers do not blur into each other: recipient commands refuse a passphrase
/// file, and `rekey` refuses an age one instead of writing a passphrase blob into it.
#[test]
fn provider_specific_commands_refuse_the_other_provider() {
    let dir = tempdir().unwrap();
    let (alice_key, alice_pub) = keypair(dir.path(), "alice");

    let passphrase_file = dir.path().join("passphrase.kdl");
    let pf = passphrase_file.to_str().unwrap();
    glidesh()
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "init", "--file", pf])
        .assert()
        .success();
    glidesh()
        .args(["secret", "recipients", "list", "--file", pf])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no recipients"));

    let age_file = dir.path().join("age.kdl");
    let af = age_file.to_str().unwrap();
    glidesh()
        .args([
            "secret",
            "init",
            "--provider",
            "age",
            "--recipient",
            &alice_pub,
            "--file",
            af,
        ])
        .assert()
        .success();
    glidesh()
        .env("GLIDESH_SECRET_IDENTITY", &alice_key)
        .args(["secret", "rekey", "--file", af])
        .assert()
        .failure()
        // Matched on the flag rather than the sentence: miette wraps long messages.
        .stderr(predicate::str::contains("rotate-data-key"));

    // Rotating its data key, however, is meaningful and keeps the recipient list.
    glidesh()
        .env("GLIDESH_SECRET_IDENTITY", &alice_key)
        .args(["secret", "rekey", "--rotate-data-key", "--file", af])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 recipient(s)"));
}

/// `--provider age` without a recipient would produce a file nobody can open.
#[test]
fn age_init_requires_a_recipient() {
    let dir = tempdir().unwrap();
    let sf = dir.path().join("secrets.kdl");
    glidesh()
        .args([
            "secret",
            "init",
            "--provider",
            "age",
            "--file",
            sf.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("at least one --recipient"));
    assert!(!sf.exists(), "a file was left behind");
}

/// The run path resolves an age identity too, and fails fast: the data key is unwrapped
/// before any host is contacted, so a key that is not a recipient is rejected up front
/// rather than after a connection attempt.
#[test]
fn a_run_unlocks_an_age_vault_with_the_ssh_key() {
    let dir = tempdir().unwrap();
    let (alice_key, alice_pub) = keypair(dir.path(), "alice");
    let (mallory_key, _) = keypair(dir.path(), "mallory");

    let sf = dir.path().join("secrets.kdl");
    glidesh()
        .args([
            "secret",
            "init",
            "--provider",
            "age",
            "--recipient",
            &alice_pub,
            "--file",
            sf.to_str().unwrap(),
        ])
        .assert()
        .success();

    let inventory = dir.path().join("inventory.kdl");
    std::fs::write(
        &inventory,
        "host \"node-1\" \"127.0.0.1\" user=\"nobody\" port=1\n",
    )
    .unwrap();
    let plan = dir.path().join("plan.kdl");
    std::fs::write(
        &plan,
        "plan \"demo\" {\n    step \"Ping\" {\n        shell \"true\"\n    }\n}\n",
    )
    .unwrap();

    let run = |identity: &str| {
        glidesh()
            .env("GLIDESH_SECRET_IDENTITY", identity)
            .args([
                "run",
                "-i",
                inventory.to_str().unwrap(),
                "-p",
                plan.to_str().unwrap(),
                "--no-tui",
                "--secret-identity",
                identity,
            ])
            .output()
            .unwrap()
    };

    // A key nobody wrapped to is refused before the run starts.
    let out = run(&mallory_key);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("recipient"),
        "expected a recipient error, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Alice's key unlocks the vault; the run then fails on its own terms (no such host),
    // which is how we know the secrets layer was satisfied.
    let out = run(&alice_key);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("recipient"),
        "the identity should have been accepted, got: {stderr}"
    );
}
