//! End-to-end `rescue` / `always` tests that drive the `glidesh` binary.
//!
//! Whether a failure is handled decides whether the host goes on and what the run exits
//! with, so only a real run shows it. Each task leaves a file behind to prove it ran.

mod common;

use assert_cmd::Command;
use std::path::Path;
use std::time::Duration;

/// The failing task stops the step's own tasks; the rescue reads the failure and registers
/// a value a later step uses; the host goes on.
const RESCUED: &str = r#"
plan "rescued" {
    step "Deploy" {
        shell "touch /root/r-before"
        shell "exit 3"
        shell "touch /root/r-unreached"
        rescue {
            shell "echo \"${@error.task}\" > /root/r-task"
            shell "cat > /root/r-msg <<'EOF'\n${@error.msg}\nEOF"
            shell "echo rolled-back" register="recovered"
        }
        always {
            shell "touch /root/r-always"
        }
    }
    step "After" {
        shell "echo ${recovered} > /root/r-after"
    }
}
"#;

const UNRESCUED: &str = r#"
plan "unrescued" {
    step "Deploy" {
        shell "exit 3"
        rescue {
            shell "exit 4"
            shell "touch /root/u-unreached"
        }
        always {
            shell "touch /root/u-always"
        }
    }
    step "After" {
        shell "touch /root/u-after"
    }
}
"#;

/// Nothing fails: the rescue never runs, so its `register=` stays undefined, and `always`
/// has no failure to read.
const CLEAN: &str = r#"
plan "clean" {
    step "Deploy" {
        shell "true"
        rescue {
            shell "echo x" register="recovered"
        }
        always {
            shell "touch /root/c-error" when="defined ${@error.msg}"
            shell "touch /root/c-always"
        }
    }
    step "Only after a rescue" when="defined ${recovered}" {
        shell "touch /root/c-rescued"
    }
}
"#;

/// A gate that times out and a loop item that fails are the step's failures too. The loop
/// stops at the failed item, and the rescue runs once, not per item.
const GATE_AND_LOOP: &str = r#"
plan "gate-and-loop" {
    step "Wait" until="false" until-timeout=1 until-interval=1 {
        shell "touch /root/g-body"
        rescue {
            shell "echo \"[${@error.task}]\" > /root/g-task"
        }
    }
    step "Each" loop="a\nb\nc" {
        shell "echo ${@item} >> /root/g-items; test ${@item} != b"
        rescue {
            shell "echo rescued >> /root/g-items"
        }
    }
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_rescued_failure_lets_the_host_go_on() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    write_inventory(dir.path(), &container);
    std::fs::write(dir.path().join("plan.kdl"), RESCUED).unwrap();

    let (ok, out) = run(dir.path(), &[]);
    assert!(ok, "a rescued failure must not fail the run:\n{out}");
    assert!(out.contains("RESCUE step 'Deploy'"), "{out}");
    assert!(out.contains("ALWAYS step 'Deploy'"), "{out}");

    let exists = |path: &'static str| {
        let ssh = &ssh;
        async move {
            ssh.exec(&format!("test -e {path}"))
                .await
                .unwrap()
                .exit_code
                == 0
        }
    };
    let read = |path: &'static str| {
        let ssh = &ssh;
        async move { ssh.exec(&format!("cat {path}")).await.unwrap().stdout }
    };
    assert!(exists("/root/r-before").await);
    assert!(
        !exists("/root/r-unreached").await,
        "the step's tasks stop at the failure"
    );
    assert_eq!(read("/root/r-task").await.trim(), "shell 'exit 3'");
    let msg = read("/root/r-msg").await;
    assert!(msg.contains("exit code 3"), "{msg}");
    assert!(exists("/root/r-always").await);
    assert_eq!(read("/root/r-after").await.trim(), "rolled-back");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_rescue_fails_the_host_after_always() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    write_inventory(dir.path(), &container);
    std::fs::write(dir.path().join("plan.kdl"), UNRESCUED).unwrap();

    let (ok, out) = run(dir.path(), &[]);
    assert!(!ok, "{out}");
    let exists = |path: &'static str| {
        let ssh = &ssh;
        async move {
            ssh.exec(&format!("test -e {path}"))
                .await
                .unwrap()
                .exit_code
                == 0
        }
    };
    assert!(exists("/root/u-always").await, "always runs anyway:\n{out}");
    assert!(
        !exists("/root/u-unreached").await,
        "the rescue stops at its failure"
    );
    assert!(
        !exists("/root/u-after").await,
        "the host stops after the step"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_failure_only_always_runs() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    write_inventory(dir.path(), &container);
    std::fs::write(dir.path().join("plan.kdl"), CLEAN).unwrap();

    let (ok, preview) = run(dir.path(), &["--dry-run"]);
    assert!(ok, "{preview}");
    assert!(
        preview.contains("undetermined in preview"),
        "a preview cannot know the rescue will not run:\n{preview}"
    );

    let (ok, out) = run(dir.path(), &[]);
    assert!(ok, "{out}");
    assert!(!out.contains("RESCUE"), "{out}");
    let exists = |path: &'static str| {
        let ssh = &ssh;
        async move {
            ssh.exec(&format!("test -e {path}"))
                .await
                .unwrap()
                .exit_code
                == 0
        }
    };
    assert!(exists("/root/c-always").await);
    assert!(!exists("/root/c-error").await, "no failure to read");
    assert!(
        !exists("/root/c-rescued").await,
        "a rescue that did not run leaves its register= undefined:\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gate_timeout_and_a_failed_item_are_rescued_once() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    write_inventory(dir.path(), &container);
    std::fs::write(dir.path().join("plan.kdl"), GATE_AND_LOOP).unwrap();

    let (ok, out) = run(dir.path(), &[]);
    assert!(ok, "{out}");
    let read = |path: &'static str| {
        let ssh = &ssh;
        async move { ssh.exec(&format!("cat {path}")).await.unwrap().stdout }
    };
    assert_eq!(
        ssh.exec("test -e /root/g-body").await.unwrap().exit_code,
        1,
        "the gate never opened"
    );
    assert_eq!(
        read("/root/g-task").await.trim(),
        "[]",
        "a gate failure names no task"
    );
    assert_eq!(read("/root/g-items").await, "a\nb\nrescued\n");
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
