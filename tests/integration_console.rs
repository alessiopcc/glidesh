//! `glidesh console -c` with and without `--vars`, driving the binary against the container.
//!
//! The container is listed twice under different names, so a group target exercises the
//! multi-host path while a single name exercises the single-host one — they print output,
//! and so redact it, in different places.

mod common;

use assert_cmd::Command;
use std::path::Path;

const SECRET: &str = "hunter2-token";

fn glidesh(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("glidesh").unwrap();
    cmd.current_dir(dir).env("GLIDESH_SECRET_PASS", "pw");
    cmd
}

fn setup(dir: &Path, container: &common::TestContainer) {
    let key = container.write_key_file(dir);
    let host = |name: &str| {
        format!(
            "    host \"{name}\" \"127.0.0.1\" user=\"root\" port={} {{\n        vars {{\n            ssh-key {:?}\n        }}\n    }}\n",
            container.port,
            key.to_string_lossy()
        )
    };
    std::fs::write(
        dir.join("inventory.kdl"),
        format!("group \"pair\" {{\n{}{}}}\n", host("a"), host("b")),
    )
    .unwrap();
    glidesh(dir).args(["secret", "init"]).assert().success();
    glidesh(dir)
        .args(["secret", "set", "api-token", SECRET])
        .assert()
        .success();
}

fn console(dir: &Path, target: &str, command: &str, vars: bool) -> String {
    let mut cmd = glidesh(dir);
    cmd.args([
        "console",
        "-i",
        "inventory.kdl",
        "-t",
        target,
        "-c",
        command,
    ])
    .arg("--no-host-key-check");
    if vars {
        cmd.arg("--vars");
    }
    let out = cmd.assert().success().get_output().clone();
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The field ask: a secret in a one-off command. It must reach the host decrypted and never
/// be printed back.
#[tokio::test(flavor = "multi_thread")]
async fn a_secret_reaches_the_host_and_is_redacted_from_output() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path(), &container);

    let command = "printf %s ${api-token} > /root/console-token; echo token=${api-token}";
    for target in ["a", "pair"] {
        let out = console(dir.path(), target, command, true);
        assert!(
            !out.contains(SECRET),
            "target {target}: the secret must not be printed:\n{out}"
        );
        assert!(out.contains("token=***"), "target {target}:\n{out}");

        let written = ssh.exec("cat /root/console-token").await.unwrap();
        assert_eq!(
            written.stdout, SECRET,
            "target {target}: the host got the plaintext"
        );
    }
}

/// Without `--vars` the command is sent as typed: the shell on the host expands `${HOME}`,
/// and a glidesh reference in single quotes arrives verbatim.
#[tokio::test(flavor = "multi_thread")]
async fn without_vars_the_command_is_sent_as_typed() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let _ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path(), &container);

    let out = console(
        dir.path(),
        "a",
        "echo home=${HOME} raw='${api-token}'",
        false,
    );
    assert!(out.contains("home=/root"), "{out}");
    assert!(
        out.contains("raw=${api-token}"),
        "without --vars glidesh must not substitute:\n{out}"
    );
}
