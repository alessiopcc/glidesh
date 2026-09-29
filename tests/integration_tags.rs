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

const INCLUDING_PLAN: &str = r#"
plan "main" {
    step "Top" tags="web" {
        shell "touch /root/inc-top"
    }
    include "roles/db.kdl" tags="db"
}
"#;

const INCLUDED_PLAN: &str = r#"
plan "db" {
    step "Db" {
        shell "touch /root/inc-db"
    }
    step "Db slow" tags="slow" {
        shell "touch /root/inc-db-slow"
    }
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn tags_select_the_steps_that_run() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    write_fixtures(dir.path(), container.port, &container, PLAN);

    let preview = glidesh(dir.path(), &["--tags", "config", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let preview = String::from_utf8(preview).unwrap();
    assert!(
        preview.contains("skipped (not in --tags config)") && preview.contains(", 1 skipped"),
        "a preview reports the same selection:\n{preview}"
    );
    let touched = ssh.exec("ls /root/tags-* 2>/dev/null").await.unwrap();
    assert!(touched.stdout.trim().is_empty(), "{}", touched.stdout);

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
/// fails before connecting to any host, loading an SSH key, or unlocking the secrets file.
#[test]
fn an_unknown_tag_fails_before_connecting() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("plan.kdl"), PLAN).unwrap();
    std::fs::write(
        dir.path().join("inventory.kdl"),
        "host \"target\" \"192.0.2.1\" user=\"root\"\n",
    )
    .unwrap();
    Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir.path())
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "init", "--file", "secrets.kdl"])
        .assert()
        .success();
    for flag in ["--tags", "--skip-tags"] {
        // Unlocking would fail on this missing passphrase file, had it come first.
        let out = glidesh(dir.path(), &[flag, "confg"])
            .env_remove("GLIDESH_SECRET_PASS")
            .env("GLIDESH_SECRET_PASS_FILE", dir.path().join("absent.txt"))
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

#[tokio::test(flavor = "multi_thread")]
async fn an_includes_tags_select_the_steps_it_brings_in() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    write_fixtures(dir.path(), container.port, &container, INCLUDING_PLAN);
    write_included(dir.path());

    let out = glidesh(dir.path(), &["--tags", "db", "--skip-tags", "slow"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();

    let touched = ssh.exec("ls /root/inc-* 2>/dev/null").await.unwrap();
    assert_eq!(
        touched.stdout.split_whitespace().collect::<Vec<_>>(),
        ["/root/inc-db"],
        "only the included step without its own excluded tag runs:\n{out}"
    );
    assert!(out.contains("1 changed, 2 skipped"), "{out}");
}

/// Tags an include adds count as in use, so `--tags` naming one is not rejected as a typo.
#[test]
fn an_includes_tags_are_known_to_the_unknown_tag_check() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("plan.kdl"), INCLUDING_PLAN).unwrap();
    write_included(dir.path());
    std::fs::write(
        dir.path().join("inventory.kdl"),
        "host \"target\" \"192.0.2.1\" user=\"root\"\n",
    )
    .unwrap();
    let out = glidesh(dir.path(), &["--tags", "db,dbb"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("no step is tagged 'dbb'") && err.contains("tags in use: db, slow, web"),
        "{err}"
    );
}

fn glidesh(dir: &Path, extra: &[&str]) -> Command {
    let mut cmd = Command::cargo_bin("glidesh").unwrap();
    cmd.current_dir(dir)
        .args(["run", "-i", "inventory.kdl", "-p", "plan.kdl"])
        .args(["--no-tui", "--no-host-key-check"])
        .args(extra);
    cmd
}

fn write_included(dir: &Path) {
    std::fs::create_dir_all(dir.join("roles")).unwrap();
    std::fs::write(dir.join("roles/db.kdl"), INCLUDED_PLAN).unwrap();
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
