//! `shell` `changed-when`, end to end through the binary.

mod common;

use assert_cmd::Command;
use std::path::Path;

/// A read-only probe that should never count, and a command whose change is decided by a
/// follow-up probe — once saying "changed", once "not changed".
const PLAN: &str = r#"
plan "changed-when" {
    step "Read-only" {
        shell "lsblk" changed-when=#false
    }
    step "Probed" {
        shell "true" changed-when="true"
        shell "true" changed-when="false"
    }
}
"#;

fn run(dir: &Path, extra: &[&str]) -> String {
    let out = Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir)
        .args(["run", "-i", "inventory.kdl", "-p", "plan.kdl"])
        .args(["--no-tui", "--no-host-key-check"])
        .args(extra)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn changed_when_decides_what_counts() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let _ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    let key = container.write_key_file(dir.path());
    std::fs::write(dir.path().join("plan.kdl"), PLAN).unwrap();
    std::fs::write(
        dir.path().join("inventory.kdl"),
        format!(
            "host \"target\" \"127.0.0.1\" user=\"root\" port={} {{\n    vars {{\n        ssh-key {:?}\n    }}\n}}\n",
            container.port,
            key.to_string_lossy()
        ),
    )
    .unwrap();

    let applied = run(dir.path(), &[]);
    assert!(
        applied.contains("shell 'lsblk': ok"),
        "#false must report ok:\n{applied}"
    );
    assert!(
        applied.contains("1 changed"),
        "only the probe that exited 0 counts:\n{applied}"
    );

    // A preview cannot run the probes, so both probed tasks would change; `#false` is known
    // without running anything and must not count here either.
    let preview = run(dir.path(), &["--dry-run"]);
    assert!(
        preview.contains("shell 'lsblk': ok"),
        "#false must not count in a preview:\n{preview}"
    );
    assert!(preview.contains("2 would change"), "{preview}");
}
