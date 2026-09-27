//! `mode "sync"`: no host starts a step until every live host has finished the one before.
//!
//! The container is listed under several host names, so one container gives a multi-host
//! run. Ordering is read from timestamps each host writes on the container, not from console
//! output.

mod common;

use assert_cmd::Command;
use std::path::Path;
use std::time::Duration;

/// Host `a` sleeps through the first step; `b` does not. Under sync, `b` must not start the
/// second step until `a` has finished the first.
const ORDER_PLAN: &str = r#"
plan "order" {
    step "First" {
        shell "sleep ${delay}; date +%s%N > /root/first-${@host.name}"
    }
    step "Second" {
        shell "date +%s%N > /root/second-${@host.name}"
    }
}
"#;

fn inventory(dir: &Path, container: &common::TestContainer, hosts: &[(&str, &str)]) {
    let key = container.write_key_file(dir);
    let mut inv = String::from("group \"fleet\" {\n");
    for (name, delay) in hosts {
        inv.push_str(&format!(
            "    host \"{name}\" \"127.0.0.1\" user=\"root\" port={} {{\n        vars {{\n            ssh-key {:?}\n            delay \"{delay}\"\n        }}\n    }}\n",
            container.port,
            key.to_string_lossy()
        ));
    }
    inv.push_str("}\n");
    std::fs::write(dir.join("inventory.kdl"), inv).unwrap();
}

fn run(dir: &Path, extra: &[&str]) -> (bool, String) {
    let out = Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir)
        .args([
            "run",
            "-i",
            "inventory.kdl",
            "-p",
            "plan.kdl",
            "-t",
            "fleet",
        ])
        .args(["--no-tui", "--no-host-key-check"])
        .args(extra)
        // A deadlock would hang forever; fail the test instead.
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

async fn stamp(ssh: &glidesh::ssh::SshSession, file: &str) -> u128 {
    let out = ssh.exec(&format!("cat /root/{file}")).await.unwrap();
    out.stdout
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("/root/{file} was not written: {:?}", out.stdout))
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_holds_every_host_at_each_step_and_async_does_not() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    inventory(dir.path(), &container, &[("a", "3"), ("b", "0")]);
    std::fs::write(dir.path().join("plan.kdl"), ORDER_PLAN).unwrap();

    let (ok, out) = run(dir.path(), &["-m", "sync"]);
    assert!(ok, "{out}");
    let (a_first, b_second) = (stamp(&ssh, "first-a").await, stamp(&ssh, "second-b").await);
    assert!(
        b_second >= a_first,
        "sync: b started step 2 before a finished step 1 ({b_second} < {a_first})"
    );

    let (ok, out) = run(dir.path(), &["-m", "async"]);
    assert!(ok, "{out}");
    let (a_first, b_second) = (stamp(&ssh, "first-a").await, stamp(&ssh, "second-b").await);
    assert!(
        b_second < a_first,
        "async: b should not have waited for a ({b_second} >= {a_first})"
    );
}

/// More hosts than permits: if a host held its permit while waiting at the barrier, the
/// run would deadlock.
#[tokio::test(flavor = "multi_thread")]
async fn sync_with_more_hosts_than_concurrency_completes() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let _ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    inventory(
        dir.path(),
        &container,
        &[("a", "0"), ("b", "0"), ("c", "0")],
    );
    std::fs::write(dir.path().join("plan.kdl"), ORDER_PLAN).unwrap();

    let (ok, out) = run(dir.path(), &["-m", "sync", "--concurrency", "1"]);
    assert!(ok, "{out}");
    assert!(out.contains("3 total, 3 ok"), "{out}");
}

/// A host that fails leaves the barrier, so the others go on.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_host_does_not_hold_the_others() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    inventory(dir.path(), &container, &[("a", "0"), ("b", "0")]);
    std::fs::write(
        dir.path().join("plan.kdl"),
        r#"
plan "fail" {
    step "First" {
        shell "test ${@host.name} != b"
    }
    step "Second" {
        shell "touch /root/second-${@host.name}"
    }
}
"#,
    )
    .unwrap();

    let (_, out) = run(dir.path(), &["-m", "sync"]);
    assert!(out.contains("1 ok, 1 failed"), "{out}");
    let ran = ssh.exec("test -e /root/second-a").await.unwrap();
    assert_eq!(ran.exit_code, 0, "a must finish without b:\n{out}");
    let skipped = ssh.exec("test -e /root/second-b").await.unwrap();
    assert_ne!(skipped.exit_code, 0, "b failed and must not run step 2");
}
