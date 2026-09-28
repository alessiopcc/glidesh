//! `serial` and `max-fail`: rolling runs in batches, stopped once too many hosts fail.
//!
//! The container is listed under several host names, so one container gives a multi-host
//! run. What ran, and when, is read from files the plan writes, not from console output.

mod common;

use assert_cmd::Command;
use std::path::Path;
use std::time::Duration;

/// Hosts with a `fail` var: a step fails on the hosts where it is `yes`.
fn inventory(dir: &Path, container: &common::TestContainer, hosts: &[(&str, bool)]) {
    let key = container.write_key_file(dir);
    let mut inv = String::from("group \"fleet\" {\n");
    for (name, fail) in hosts {
        inv.push_str(&format!(
            "    host \"{name}\" \"127.0.0.1\" user=\"root\" port={} {{\n        vars {{\n            ssh-key {:?}\n            fail \"{}\"\n        }}\n    }}\n",
            container.port,
            key.to_string_lossy(),
            if *fail { "yes" } else { "no" }
        ));
    }
    inv.push_str("}\n");
    std::fs::write(dir.join("inventory.kdl"), inv).unwrap();
}

fn run(dir: &Path, plan: &str, extra: &[&str]) -> (bool, String) {
    std::fs::write(dir.join("plan.kdl"), plan).unwrap();
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

async fn ran(ssh: &glidesh::ssh::SshSession, host: &str) -> bool {
    ssh.exec(&format!("test -e /root/ran-{host}"))
        .await
        .unwrap()
        .exit_code
        == 0
}

/// Fails on the hosts whose `fail` var is `yes`, and marks the ones that got past it.
const GATED_PLAN: &str = r#"
plan "gated" {
    step "Deploy" {
        shell "test ${fail} != yes"
    }
    step "Mark" {
        shell "touch /root/ran-${@host.name}"
    }
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_starts_only_after_the_previous_one_finished() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    inventory(
        dir.path(),
        &container,
        &[("a", false), ("b", false), ("c", false), ("d", false)],
    );

    let (ok, out) = run(
        dir.path(),
        r#"
plan "order" {
    serial 2
    step "Work" {
        shell "date +%s%N > /root/start-${@host.name}; sleep 2; date +%s%N > /root/end-${@host.name}"
    }
}
"#,
        &[],
    );
    assert!(ok, "{out}");
    assert!(
        out.contains("Batch 1/2: a, b") && out.contains("Batch 2/2: c, d"),
        "{out}"
    );

    let first_done = stamp(&ssh, "end-a").await.max(stamp(&ssh, "end-b").await);
    let second_started = stamp(&ssh, "start-c")
        .await
        .min(stamp(&ssh, "start-d").await);
    assert!(
        second_started >= first_done,
        "batch 2 started before batch 1 finished ({second_started} < {first_done})"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_canary_batch_then_the_rest() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let _ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    inventory(
        dir.path(),
        &container,
        &[("a", false), ("b", false), ("c", false), ("d", false)],
    );

    let (ok, out) = run(dir.path(), GATED_PLAN, &["--serial", "1,2"]);
    assert!(ok, "{out}");
    for batch in ["Batch 1/3: a", "Batch 2/3: b, c", "Batch 3/3: d"] {
        assert!(out.contains(batch), "missing {batch:?}:\n{out}");
    }
}

/// `--max-fail 0` stops at the first failure; the hosts after it are never touched.
#[tokio::test(flavor = "multi_thread")]
async fn max_fail_stops_the_rollout_and_reports_the_rest_as_aborted() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    inventory(
        dir.path(),
        &container,
        &[("a", true), ("b", false), ("c", false)],
    );

    let (ok, out) = run(
        dir.path(),
        GATED_PLAN,
        &["--serial", "1", "--max-fail", "0"],
    );
    assert!(!ok, "a stopped rollout must fail the run:\n{out}");
    assert!(out.contains("Rollout stopped"), "{out}");
    assert!(
        out.contains("[b] ABORTED") && out.contains("[c] ABORTED"),
        "{out}"
    );
    assert!(out.contains("1 failed, 2 aborted"), "{out}");
    assert!(!ran(&ssh, "b").await && !ran(&ssh, "c").await, "{out}");
}

/// Without `max-fail`, one failure in a batch is not enough to stop — a whole batch is.
#[tokio::test(flavor = "multi_thread")]
async fn without_max_fail_only_a_wholly_failed_batch_stops() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;

    let dir = tempfile::tempdir().unwrap();
    inventory(
        dir.path(),
        &container,
        &[("a", true), ("b", false), ("c", false), ("d", false)],
    );
    let (_, out) = run(dir.path(), GATED_PLAN, &["--serial", "2"]);
    assert!(!out.contains("Rollout stopped"), "{out}");
    assert!(ran(&ssh, "c").await && ran(&ssh, "d").await, "{out}");

    let dir = tempfile::tempdir().unwrap();
    inventory(
        dir.path(),
        &container,
        &[("e", true), ("f", true), ("g", false), ("h", false)],
    );
    let (ok, out) = run(dir.path(), GATED_PLAN, &["--serial", "2"]);
    assert!(!ok, "{out}");
    assert!(out.contains("every host in the last batch failed"), "{out}");
    assert!(!ran(&ssh, "g").await && !ran(&ssh, "h").await, "{out}");
}

/// A `host` task runs once per run, not once per batch: later batches reuse its result.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_once_host_task_runs_once_across_batches() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let _ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    inventory(
        dir.path(),
        &container,
        &[("a", false), ("b", false), ("c", false)],
    );
    let counter = dir.path().join("runs.txt");

    let (ok, out) = run(
        dir.path(),
        &format!(
            r#"
plan "once" {{
    serial 1
    step "Build tag" {{
        host "echo run >> {}"
    }}
}}
"#,
            counter.display()
        ),
        &[],
    );
    assert!(ok, "{out}");
    let runs = std::fs::read_to_string(&counter).unwrap();
    assert_eq!(runs.lines().count(), 1, "ran {runs:?}");
}
