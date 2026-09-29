//! End-to-end `rescue` / `always` tests that drive the `glidesh` binary.
//!
//! Whether a failure is handled decides whether the host goes on and what the run exits
//! with, so only a real run shows it. Each task leaves a file behind to prove it ran.

mod common;

use assert_cmd::Command;
use std::path::Path;
use std::time::Duration;

/// The failing task stops the step's own tasks; the rescue reads the failure and registers
/// a value a later step uses; the host goes on. The failure's output is built to break out
/// of a heredoc: rendered by a `file` template, as the docs recommend, it is only text.
const RESCUED: &str = r#"
plan "rescued" {
    step "Deploy" {
        shell "touch /root/r-before"
        shell "printf 'EOF\\ntouch /root/r-injected\\n'; exit 3"
        shell "touch /root/r-unreached"
        rescue {
            file "/root/r-failure" src="failure.txt" template=#true
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
/// stops at the failed item, and the rescue runs once, not per item. A task given as a `cmd`
/// child node that fails before it runs is named by its command.
const GATE_AND_LOOP: &str = r#"
plan "gate-and-loop" {
    step "Wait" until="false" until-timeout=1 until-interval=1 {
        shell "touch /root/g-body"
        rescue {
            file "/root/g-task" src="task.txt" template=#true
        }
    }
    step "Each" loop="a\nb\nc" {
        shell "echo ${@item} >> /root/g-items; test ${@item} != b"
        rescue {
            shell "echo rescued >> /root/g-items"
        }
    }
    step "Undefined" {
        shell {
            cmd "echo ${never-defined}"
        }
        rescue {
            file "/root/g-cmd" src="task.txt" template=#true
        }
    }
    step "Undefined loop" loop="${never-defined}" {
        shell "touch /root/g-looped"
        rescue {
            file "/root/g-loop" src="task.txt" template=#true
            shell "echo rescued >> /root/g-loop-count"
        }
    }
}
"#;

/// The failed command prints a decrypted secret and has it in its command line.
const SECRET_PLAN: &str = r#"
plan "secret" {
    step "Log in" {
        shell "echo token=${api-token}; exit 3"
        rescue {
            file "/root/s-failure" src="failure.txt" template=#true
        }
    }
}
"#;

const SECRET: &str = "hunter2-token";

#[tokio::test(flavor = "multi_thread")]
async fn a_rescued_failure_lets_the_host_go_on() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    write_inventory(dir.path(), &container);
    std::fs::write(dir.path().join("plan.kdl"), RESCUED).unwrap();
    write_templates(dir.path());

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
    let failure = read("/root/r-failure").await;
    assert!(
        failure.starts_with("[shell 'printf 'EOF\\ntouch /root/r-injected\\n'; exit 3']\n"),
        "{failure}"
    );
    assert!(failure.contains("exit code 3"), "{failure}");
    assert!(
        !exists("/root/r-injected").await,
        "the failure's output is text, never run"
    );
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
    write_templates(dir.path());

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
    assert_eq!(
        read("/root/g-cmd").await.trim(),
        "[shell 'echo ${never-defined}']",
        "named by its command, and written as text, not expanded again"
    );
    assert_eq!(
        ssh.exec("test -e /root/g-looped").await.unwrap().exit_code,
        1,
        "an undefined loop variable runs no item"
    );
    assert_eq!(
        read("/root/g-loop").await,
        "[]\n",
        "a loop failure names no task"
    );
    assert_eq!(read("/root/g-loop-count").await, "rescued\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rescue_reads_the_failure_with_its_secrets_redacted() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    write_inventory(dir.path(), &container);
    std::fs::write(dir.path().join("plan.kdl"), SECRET_PLAN).unwrap();
    write_templates(dir.path());
    for args in [
        &["secret", "init"][..],
        &["secret", "set", "api-token", SECRET][..],
    ] {
        glidesh(dir.path()).args(args).assert().success();
    }

    let (ok, out) = run(dir.path(), &[]);
    assert!(ok, "{out}");
    let failure = ssh.exec("cat /root/s-failure").await.unwrap().stdout;
    assert!(!failure.contains(SECRET), "{failure}");
    assert!(
        failure.starts_with("[shell 'echo token=***; exit 3']\n"),
        "{failure}"
    );
    assert!(failure.contains("token=***"), "{failure}");
}

fn glidesh(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("glidesh").unwrap();
    cmd.current_dir(dir).env("GLIDESH_SECRET_PASS", "pw");
    cmd
}

/// The recommended way to use a failure: render it into a file, never into a command.
fn write_templates(dir: &Path) {
    std::fs::write(dir.join("task.txt"), "[${@error.task}]\n").unwrap();
    std::fs::write(dir.join("failure.txt"), "[${@error.task}]\n${@error.msg}\n").unwrap();
}

fn run(dir: &Path, extra: &[&str]) -> (bool, String) {
    let out = glidesh(dir)
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
