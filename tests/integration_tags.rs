//! End-to-end `--tags` / `--skip-tags` tests that drive the `glidesh` binary.
//!
//! Each effect is checked through a file the plan writes on the host, not through console
//! output alone.

mod common;

use assert_cmd::Command;
use std::path::Path;

const PLAN: &str = r#"
plan "tags" {
    step "Probe" tags="always" {
        shell "echo yes" register="probe"
    }
    step "Config" tags="config" {
        shell "touch /root/tags-config-${probe}"
    }
    step "Slow" tags="config,slow" {
        shell "touch /root/tags-slow"
    }
    step "Untagged" {
        shell "touch /root/tags-untagged"
    }
    step "Handler" subscribe="Untagged" tags="config" {
        shell "touch /root/tags-handler" check="true"
    }
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn tags_select_the_steps_that_run() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    write_fixtures(dir.path(), container.port, &container);

    let out = glidesh(dir.path(), &["--tags", "config", "--skip-tags", "slow"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();

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
        exists("/root/tags-config-yes").await,
        "a selected step runs, with what an `always` step registered"
    );
    assert!(
        !exists("/root/tags-slow").await,
        "--skip-tags wins over --tags"
    );
    assert!(
        !exists("/root/tags-untagged").await,
        "an untagged step is not selected by --tags"
    );
    assert!(
        !exists("/root/tags-handler").await,
        "a step left out must not fire its subscribers"
    );
    assert!(out.contains("2 changed, 2 skipped"), "{out}");
    assert!(
        out.contains("skipped (--skip-tags slow)"),
        "a skip must say why:\n{out}"
    );
}

/// A misspelled tag would otherwise run nothing, or run what it meant to hold back — so it
/// fails before connecting to any host.
#[test]
fn an_unknown_tag_fails_before_connecting() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("plan.kdl"), PLAN).unwrap();
    std::fs::write(
        dir.path().join("inventory.kdl"),
        "host \"target\" \"192.0.2.1\" user=\"root\"\n",
    )
    .unwrap();
    for flag in ["--tags", "--skip-tags"] {
        let out = glidesh(dir.path(), &[flag, "confg"])
            .assert()
            .failure()
            .get_output()
            .clone();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("no step is tagged 'confg'") && err.contains("always, config, slow"),
            "{flag}: {err}"
        );
    }
}

fn glidesh(dir: &Path, extra: &[&str]) -> Command {
    let mut cmd = Command::cargo_bin("glidesh").unwrap();
    cmd.current_dir(dir)
        .args(["run", "-i", "inventory.kdl", "-p", "plan.kdl"])
        .args(["--no-tui", "--no-host-key-check"])
        .args(extra);
    cmd
}

fn write_fixtures(dir: &Path, port: u16, container: &common::TestContainer) {
    let key = container.write_key_file(dir);
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
