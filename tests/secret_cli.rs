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
