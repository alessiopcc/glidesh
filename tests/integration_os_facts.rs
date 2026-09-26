//! End-to-end `@os.*` tests that drive the `glidesh` binary.
//!
//! The facts are injected in `NodeRunner` from the detection it runs on connect, which needs
//! a live SSH session, so only a real run proves they reach module arguments and templates.

mod common;

use assert_cmd::Command;
use std::path::Path;

/// One reference through module arguments and one through a `file` template: the two go
/// through different interpolation paths, and both must see the facts.
const PLAN: &str = r#"
plan "facts" {
    step "Echo facts" {
        shell "echo facts=${@os.id}/${@os.version}/${@os.family}/${@os.pkg-manager}/${@os.init}/rt=${@os.container-runtime}/nix=${@os.nix-installed}"
    }
    step "Render facts" {
        file "/root/os-facts.txt" src="os-facts.tmpl" template=#true
    }
    step "Read rendered facts" {
        shell "cat /root/os-facts.txt"
    }
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn os_facts_reach_module_args_and_templates() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let _ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    let key = container.write_key_file(dir.path());
    write_fixtures(dir.path(), container.port, &key);

    let out = run(dir.path());
    // The test image installs neither a container runtime nor Nix.
    assert!(
        out.contains("facts=ubuntu/22.04/debian/apt/systemd/rt=/nix=false"),
        "module args must see the detected facts:\n{out}"
    );
    assert!(
        out.contains("rendered=debian"),
        "a file template must see the detected facts:\n{out}"
    );
}

/// Detection only asks whether a `docker` binary is on the path, so a stub is enough to
/// flip the fact without running a real daemon.
#[tokio::test(flavor = "multi_thread")]
async fn a_detected_runtime_is_exposed() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let stub = ssh
        .exec("printf '#!/bin/sh\\n' > /usr/local/bin/docker && chmod +x /usr/local/bin/docker")
        .await
        .unwrap();
    assert_eq!(stub.exit_code, 0, "stub install failed: {}", stub.stderr);

    let dir = tempfile::tempdir().unwrap();
    let key = container.write_key_file(dir.path());
    write_fixtures(dir.path(), container.port, &key);

    let out = run(dir.path());
    assert!(
        out.contains("/rt=docker/"),
        "a host with docker must expose it:\n{out}"
    );
}

fn run(dir: &Path) -> String {
    let mut cmd = Command::cargo_bin("glidesh").unwrap();
    cmd.current_dir(dir)
        .args(["run", "-i", "inventory.kdl", "-p", "plan.kdl"])
        .args(["--no-tui", "--no-host-key-check"]);
    let out = cmd.assert().success().get_output().stdout.clone();
    String::from_utf8(out).unwrap()
}

fn write_fixtures(dir: &Path, port: u16, key: &Path) {
    std::fs::write(dir.join("os-facts.tmpl"), b"rendered=${@os.family}\n").unwrap();
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
