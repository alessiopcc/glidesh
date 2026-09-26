//! End-to-end `--dry-run` tests that drive the `glidesh` binary.
//!
//! The counting these assert lives in `NodeRunner`, which needs a live SSH session and so
//! cannot be reached from a unit test. Running the binary against the test container is the
//! only way to observe what a preview actually reports.

mod common;

use assert_cmd::Command;
use std::path::Path;

/// A plan whose second step is a handler that would be satisfied on its own: its `check=`
/// guard succeeds, so only `subscribe` makes it run. The first step is pending because the
/// destination does not exist yet.
const PLAN: &str = r#"
plan "preview" {
    step "Deploy config" {
        file "/root/dry-run-app.conf" src="app.conf"
    }
    step "Restart app" subscribe="Deploy config" {
        shell "touch /root/dry-run-restarted" check="true"
    }
}
"#;

fn write_fixtures(dir: &Path, port: u16, key: &Path) {
    std::fs::write(dir.join("app.conf"), b"managed by glidesh\n").unwrap();
    std::fs::write(dir.join("plan.kdl"), PLAN).unwrap();
    std::fs::write(
        dir.join("inventory.kdl"),
        format!(
            "host \"target\" \"127.0.0.1\" user=\"root\" port={} {{\n    vars {{\n        ssh-key {:?}\n    }}\n}}\n",
            port,
            key.to_string_lossy()
        ),
    )
    .unwrap();
}

fn run(dir: &Path, extra: &[&str]) -> String {
    let mut cmd = Command::cargo_bin("glidesh").unwrap();
    cmd.current_dir(dir)
        .args(["run", "-i", "inventory.kdl", "-p", "plan.kdl"])
        .args(["--no-tui", "--no-host-key-check"])
        .args(extra);
    let out = cmd.assert().success().get_output().stdout.clone();
    String::from_utf8(out).unwrap()
}

/// A preview must report the same total as the run it previews. A handler forced by
/// `subscribe` is the case that catches a preview counting only what `check` found
/// pending: its own check is satisfied, so it counts solely because the step it subscribes
/// to changed — exactly as it would on a real run.
#[tokio::test(flavor = "multi_thread")]
async fn a_preview_reports_the_same_total_as_the_real_run() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    let key = container.write_key_file(dir.path());
    write_fixtures(dir.path(), container.port, &key);

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

    let preview = run(dir.path(), &["--dry-run"]);
    assert!(
        preview.contains("2 would change"),
        "a preview must count the forced handler alongside the file:\n{preview}"
    );
    assert!(
        !exists("/root/dry-run-app.conf").await,
        "the preview must not have uploaded the file"
    );
    assert!(
        !exists("/root/dry-run-restarted").await,
        "the preview must not have run the handler"
    );

    let applied = run(dir.path(), &[]);
    assert!(
        applied.contains("2 changed"),
        "the real run must reach the total the preview promised:\n{applied}"
    );
    assert!(exists("/root/dry-run-app.conf").await);
    assert!(
        exists("/root/dry-run-restarted").await,
        "the handler must have run even though its own check was satisfied"
    );
}

/// With nothing left to do, a preview reports no changes — the handler included, since its
/// subscribed step no longer changes either.
#[tokio::test(flavor = "multi_thread")]
async fn a_preview_of_a_settled_host_reports_nothing() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let _ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    let key = container.write_key_file(dir.path());
    write_fixtures(dir.path(), container.port, &key);

    run(dir.path(), &[]);

    let preview = run(dir.path(), &["--dry-run"]);
    assert!(
        preview.contains("0 would change"),
        "a settled host must preview as no changes:\n{preview}"
    );
}
