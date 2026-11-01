//! A passphrase-protected SSH key, end to end: `run` unlocks it with `$GLIDESH_KEY_PASS`
//! and reaches the host with it.

mod common;

use assert_cmd::Command;
use russh::keys::ssh_key::{LineEnding, PrivateKey};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn a_protected_key_unlocked_from_the_environment_reaches_the_host() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    let key = container.write_key_file(dir.path());
    let protected = PrivateKey::from_openssh(std::fs::read_to_string(&key).unwrap())
        .unwrap()
        .encrypt(&mut rand::rng(), "hunter2")
        .unwrap();
    std::fs::write(&key, protected.to_openssh(LineEnding::LF).unwrap().as_str()).unwrap();
    std::fs::write(
        dir.path().join("inventory.kdl"),
        format!(
            "host \"target\" \"127.0.0.1\" user=\"root\" port={}\n",
            container.port
        ),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("plan.kdl"),
        "plan \"p\" {\n    step \"s\" {\n        shell \"echo unlocked > /root/key-pass\"\n    }\n}\n",
    )
    .unwrap();

    let out = Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir.path())
        .args(["run", "-i", "inventory.kdl", "-p", "plan.kdl"])
        .args(["--no-tui", "--no-host-key-check", "--key"])
        .arg(&key)
        .env("GLIDESH_KEY_PASS", "hunter2")
        .timeout(Duration::from_secs(120))
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(
        !text.contains("hunter2"),
        "the passphrase was shown:\n{text}"
    );

    let written = ssh.exec("cat /root/key-pass").await.unwrap().stdout;
    assert_eq!(written.trim(), "unlocked");
}
