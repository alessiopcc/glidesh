//! End-to-end `@os.*` and `@fact.*` tests that drive the `glidesh` binary.
//!
//! The facts are injected in `NodeRunner` from the detection it runs on connect, which needs
//! a live SSH session, so only a real run proves they reach module arguments and templates.

mod common;

use assert_cmd::Command;
use std::path::Path;

/// One reference through module arguments and one through a `file` template: the two go
/// through different interpolation paths, and both must see the facts. Each writes what it
/// saw to the host, because a real run keeps task stdout out of the console.
const PLAN: &str = r#"
plan "facts" {
    step "Write facts from args" {
        shell "echo facts=${@os.id}/${@os.version}/${@os.family}/${@os.pkg-manager}/${@os.init}/rt=${@os.container-runtime}/nix=${@os.nix-installed} > /root/os-args.txt"
    }
    step "Render facts" {
        file "/root/os-facts.txt" src="os-facts.tmpl" template=#true
    }
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn os_facts_reach_module_args_and_templates() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    let key = container.write_key_file(dir.path());
    write_fixtures(dir.path(), container.port, &key);

    run(dir.path());

    // The test image installs neither a container runtime nor Nix.
    assert_eq!(
        read(&ssh, "/root/os-args.txt").await,
        "facts=ubuntu/22.04/debian/apt/systemd/rt=/nix=false",
        "module args must see the detected facts"
    );
    assert_eq!(
        read(&ssh, "/root/os-facts.txt").await,
        "rendered=debian",
        "a file template must see the detected facts"
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

    run(dir.path());

    let facts = read(&ssh, "/root/os-args.txt").await;
    assert!(
        facts.contains("/rt=docker/"),
        "a host with docker must expose it: {facts}"
    );
}

const FACTS_PLAN: &str = r#"
plan "facts" {
    step "Write host facts" {
        shell "printf '%s|%s|%s|%s|%s|%s' '${@fact.hostname}' '${@fact.kernel}' '${@fact.arch}' '${@fact.cpu.count}' '${@fact.mem.total-mb}' '${@fact.ip.default}' > /root/facts.txt"
    }
}
"#;

/// Each fact against what the container itself reports over a separate exec.
#[tokio::test(flavor = "multi_thread")]
async fn host_facts_match_the_host() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    let key = container.write_key_file(dir.path());
    write_fixtures(dir.path(), container.port, &key);
    std::fs::write(dir.path().join("plan.kdl"), FACTS_PLAN).unwrap();

    run(dir.path());

    let written = read(&ssh, "/root/facts.txt").await;
    let fields: Vec<&str> = written.split('|').collect();
    let [hostname, kernel, arch, cpus, mem, ip] = fields[..] else {
        panic!("unexpected facts line: {written}");
    };

    assert_eq!(hostname, sh(&ssh, "hostname 2>/dev/null || uname -n").await);
    assert_eq!(kernel, sh(&ssh, "uname -r").await);
    assert_eq!(arch, sh(&ssh, "uname -m").await);
    assert!(!arch.is_empty(), "arch must be reported");
    assert!(
        cpus.parse::<u32>().is_ok_and(|n| n > 0),
        "cpu.count must be a positive integer: {cpus:?}"
    );
    assert!(
        mem.parse::<u64>().is_ok_and(|n| n > 0),
        "mem.total-mb must be a positive integer: {mem:?}"
    );
    // The image may not ship iproute2; then the fact is empty rather than an error.
    assert!(
        ip.is_empty() || ip.parse::<std::net::IpAddr>().is_ok(),
        "ip.default must be empty or an address: {ip:?}"
    );
}

async fn sh(ssh: &glidesh::ssh::SshSession, command: &str) -> String {
    let out = ssh.exec(command).await.unwrap();
    assert_eq!(out.exit_code, 0, "`{command}` failed: {}", out.stderr);
    out.stdout.trim().to_string()
}

fn run(dir: &Path) {
    let mut cmd = Command::cargo_bin("glidesh").unwrap();
    cmd.current_dir(dir)
        .args(["run", "-i", "inventory.kdl", "-p", "plan.kdl"])
        .args(["--no-tui", "--no-host-key-check"]);
    cmd.assert().success();
}

async fn read(ssh: &glidesh::ssh::SshSession, path: &str) -> String {
    let out = ssh.exec(&format!("cat {path}")).await.unwrap();
    assert_eq!(out.exit_code, 0, "{path} was not written: {}", out.stderr);
    out.stdout.trim().to_string()
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
