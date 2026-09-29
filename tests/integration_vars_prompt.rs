//! End-to-end `vars-prompt` tests that drive the `glidesh` binary.
//!
//! stdin is not a terminal here, so every answer comes from `--var` or a default. What the
//! host received is read back from files the plan writes.

mod common;

use assert_cmd::Command;
use std::path::Path;
use std::time::Duration;

const SECRET: &str = "s3cret-prompt-pw";

const PLAN: &str = r#"
plan "prompted" {
    vars-prompt {
        release "Release to deploy"
        region "Region" default="eu-west"
        db-password "Database password" secret=#true
    }
    step "Write" {
        shell "echo ${release} > /root/prompt-release"
        shell "echo ${region} > /root/prompt-region"
        shell "echo ${db-password} > /root/prompt-password"
        shell "echo leaked=${db-password} >&2"
    }
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn var_flags_reach_the_host_and_a_secret_answer_is_masked() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    write_inventory(dir.path(), &container);
    std::fs::write(dir.path().join("plan.kdl"), PLAN).unwrap();

    let secret_flag = format!("db-password={SECRET}");
    let (ok, out) = run(
        dir.path(),
        &["--var", "release=v1.4.2", "--var", &secret_flag],
    );
    assert!(ok, "{out}");

    let read = |path: &'static str| {
        let ssh = &ssh;
        async move { ssh.exec(&format!("cat {path}")).await.unwrap().stdout }
    };
    assert_eq!(read("/root/prompt-release").await.trim(), "v1.4.2");
    assert_eq!(
        read("/root/prompt-region").await.trim(),
        "eu-west",
        "without a terminal an unanswered prompt takes its default"
    );
    assert_eq!(
        read("/root/prompt-password").await.trim(),
        SECRET,
        "the host gets the real value"
    );

    assert!(out.contains("leaked=***"), "{out}");
    assert!(
        !out.contains(SECRET),
        "a secret answer must never be shown:\n{out}"
    );

    let (ok, preview) = run(
        dir.path(),
        &[
            "--var",
            "release=v1.4.2",
            "--var",
            &secret_flag,
            "--dry-run",
        ],
    );
    assert!(ok, "{preview}");
    assert!(
        !preview.contains(SECRET),
        "nor in a preview of the commands that use it:\n{preview}"
    );
}

fn run(dir: &Path, extra: &[&str]) -> (bool, String) {
    let out = Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir)
        .args(["run", "-i", "inventory.kdl", "-p", "plan.kdl"])
        .args(["--no-tui", "--no-host-key-check"])
        .args(extra)
        .timeout(Duration::from_secs(120))
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn write_inventory(dir: &Path, container: &common::TestContainer) {
    let key = container.write_key_file(dir);
    std::fs::write(
        dir.join("inventory.kdl"),
        format!(
            "host \"target\" \"127.0.0.1\" user=\"root\" port={} {{\n    vars {{\n        ssh-key {:?}\n    }}\n}}\n",
            container.port,
            key.to_string_lossy()
        ),
    )
    .unwrap();
}
