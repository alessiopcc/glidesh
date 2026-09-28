//! End-to-end `until=` gate tests that drive the `glidesh` binary.
//!
//! The gate polls a command over a live SSH session, so only a real run shows how often it
//! ran and what it did to the run. Attempts are counted in files the gate command writes.

mod common;

use assert_cmd::Command;
use std::path::Path;
use std::time::Duration;

/// `Counted` opens on its third attempt. `Looped` counts its attempts: once per step, not
/// per item. `Barrier` is a gate with no tasks, and `Handler` subscribes to it: waiting is
/// not work, so the handler must not be triggered.
const PLAN: &str = r#"
plan "gates" {
    vars {
        items "a\nb\nc"
    }
    step "Counted" until="n=$(cat /root/until-count 2>/dev/null || echo 0); n=$((n+1)); echo $n > /root/until-count; [ $n -ge 3 ]" until-interval=1 {
        shell "touch /root/until-after-gate"
    }
    step "Looped" loop="${items}" until="echo x >> /root/until-loop-gate" {
        shell "true" changed-when=#false
    }
    step "Barrier" until="true"
    step "Handler" subscribe="Barrier" {
        shell "touch /root/until-handler" check="true"
    }
}
"#;

const TIMEOUT_PLAN: &str = r#"
plan "timeout" {
    step "Never ready" until="echo not-ready-yet; exit 4" until-timeout=2 until-interval=1 {
        shell "touch /root/until-never"
    }
    step "After" {
        shell "touch /root/until-after-timeout"
    }
}
"#;

/// Would block for five minutes if a preview waited.
const PREVIEW_PLAN: &str = r#"
plan "preview" {
    step "Closed" until="test -e /root/until-never-exists" {
        shell "touch /root/until-previewed"
    }
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_gate_waits_then_lets_the_step_run() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    write_inventory(dir.path(), &container);
    std::fs::write(dir.path().join("plan.kdl"), PLAN).unwrap();

    let (ok, out) = run(dir.path(), &[]);
    assert!(ok, "{out}");

    let read = |path: &'static str| {
        let ssh = &ssh;
        async move { ssh.exec(&format!("cat {path}")).await.unwrap().stdout }
    };
    assert_eq!(read("/root/until-count").await.trim(), "3", "{out}");
    assert!(
        out.contains("waiting until:"),
        "a wait must be visible:\n{out}"
    );
    assert_eq!(
        ssh.exec("test -e /root/until-after-gate")
            .await
            .unwrap()
            .exit_code,
        0,
        "the step's tasks run once the gate opens"
    );
    assert_eq!(
        read("/root/until-loop-gate").await.lines().count(),
        1,
        "a looped step's gate runs once, not per item"
    );
    assert_ne!(
        ssh.exec("test -e /root/until-handler")
            .await
            .unwrap()
            .exit_code,
        0,
        "a gate is not a change, so it must not trigger subscribers"
    );
    assert!(
        out.contains("1 changed"),
        "only the gated task counts:\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gate_that_never_opens_fails_the_host_with_its_last_output() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    write_inventory(dir.path(), &container);
    std::fs::write(dir.path().join("plan.kdl"), TIMEOUT_PLAN).unwrap();

    let (ok, out) = run(dir.path(), &[]);
    assert!(!ok, "{out}");
    for expect in [
        "until= did not succeed within 2s",
        "last exited 4",
        "not-ready-yet",
    ] {
        assert!(out.contains(expect), "missing {expect:?}:\n{out}");
    }
    for path in ["/root/until-never", "/root/until-after-timeout"] {
        let ran = ssh.exec(&format!("test -e {path}")).await.unwrap();
        assert_ne!(
            ran.exit_code, 0,
            "{path}: nothing may run after the gate fails"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_preview_checks_a_gate_once_and_never_waits() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    write_inventory(dir.path(), &container);
    std::fs::write(dir.path().join("plan.kdl"), PREVIEW_PLAN).unwrap();

    let (ok, out) = run(dir.path(), &["--dry-run"]);
    assert!(ok, "a closed gate must not fail a preview:\n{out}");
    assert!(
        out.contains("until not met yet") && out.contains("a run would wait up to 300s"),
        "{out}"
    );
    assert!(out.contains("1 would change"), "{out}");
    let ran = ssh.exec("test -e /root/until-previewed").await.unwrap();
    assert_ne!(ran.exit_code, 0, "a preview must not run the task");
}

/// stdout and stderr together, since a failure is reported on stderr. A preview that waited
/// out the gate would take five minutes, so this fails it well before that.
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
