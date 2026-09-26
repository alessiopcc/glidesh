//! End-to-end `when=` tests that drive the `glidesh` binary.
//!
//! Conditions are evaluated in `NodeRunner`, which needs a live SSH session, so only a real
//! run proves what a skip actually does to the host. Each effect is checked through a file
//! the plan writes, not through console output.

mod common;

use assert_cmd::Command;
use std::path::Path;

/// Every step records whether it ran by touching a file named for what it proves. The test
/// image is Ubuntu, so `@os.family` is `debian`.
const PLAN: &str = r#"
plan "when" {
    vars {
        items "a\nb\nc"
    }
    step "Debian only" when="${@os.family} == debian" {
        shell "touch /root/when-debian"
    }
    step "RedHat only" when="${@os.family} == redhat" {
        shell "touch /root/when-redhat"
    }
    step "Handler of a skipped step" subscribe="RedHat only" {
        shell "touch /root/when-handler" check="true"
    }
    step "Triggered but excluded" subscribe="Debian only" when="${@os.family} == redhat" {
        shell "touch /root/when-forced"
    }
    step "Register when skipped" {
        shell "echo captured" register="out" when="${@os.family} == redhat"
    }
    step "Skipped host task" {
        host "false" when="${@os.family} == redhat"
    }
    step "Guarded by registration" {
        shell "touch /root/when-out-defined" when="defined ${out}"
        shell "touch /root/when-out-undefined" when="undefined ${out}"
    }
    step "Filter items" loop="${items}" {
        shell "touch /root/when-item-${@item}" when="${@item} != b"
    }
    step "Guard a loop over a missing var" loop="${missing}" when="defined ${missing}" {
        shell "touch /root/when-missing-${@item}"
    }
}
"#;

/// A preview cannot know what `Probe` will register, so the step that reads it must be
/// reported as undetermined rather than silently skipped.
const PREVIEW_PLAN: &str = r#"
plan "preview" {
    step "Probe" {
        shell "echo yes" register="out"
    }
    step "Depends on probe" {
        shell "touch /root/when-probed" when="${out} == yes"
    }
    step "Known either way" {
        shell "touch /root/when-known" when="${@os.family} == debian"
    }
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn when_decides_what_runs() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    write_fixtures(dir.path(), container.port, &container, PLAN);

    let out = run(dir.path(), &[]);

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

    assert!(
        exists("/root/when-debian").await,
        "a true step condition runs"
    );
    assert!(
        !exists("/root/when-redhat").await,
        "a false step condition skips"
    );
    assert!(
        !exists("/root/when-handler").await,
        "a skipped step must not fire its subscribers"
    );
    assert!(
        !exists("/root/when-forced").await,
        "a subscriber's own when must win over being triggered"
    );
    assert!(
        !exists("/root/when-out-defined").await,
        "a skipped task's register must leave the variable undefined"
    );
    assert!(
        exists("/root/when-out-undefined").await,
        "`undefined` must see the skipped registration"
    );
    assert!(exists("/root/when-item-a").await);
    assert!(
        !exists("/root/when-item-b").await,
        "a task condition filters loop items"
    );
    assert!(exists("/root/when-item-c").await);

    // Skipped: RedHat step, the triggered-but-excluded step, the registering task, the
    // `host` task — whose `false` would fail the run had it executed — the `defined` task,
    // item b, and the guarded loop, which must be skipped rather than fail on its undefined
    // loop variable.
    assert!(
        out.contains("4 changed, 7 skipped"),
        "the summary must count every skip:\n{out}"
    );
    assert!(
        out.contains("skipped (when: ${@os.family} == redhat)"),
        "a skip must give its condition as written:\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_preview_flags_a_condition_it_cannot_decide() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    write_fixtures(dir.path(), container.port, &container, PREVIEW_PLAN);

    let preview = run(dir.path(), &["--dry-run"]);
    assert!(
        preview.contains("undetermined in preview"),
        "the preview must say it could not decide:\n{preview}"
    );
    assert!(
        preview.contains("2 would change, 1 skipped"),
        "only the undecidable step is skipped:\n{preview}"
    );

    let applied = run(dir.path(), &[]);
    assert!(
        applied.contains("3 changed") && !applied.contains("skipped"),
        "the real run decides it:\n{applied}"
    );
    let probed = ssh.exec("test -e /root/when-probed").await.unwrap();
    assert_eq!(probed.exit_code, 0, "the real run must have run the step");
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

fn write_fixtures(dir: &Path, port: u16, container: &common::TestContainer, plan: &str) {
    let key = container.write_key_file(dir);
    std::fs::write(dir.join("plan.kdl"), plan).unwrap();
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
