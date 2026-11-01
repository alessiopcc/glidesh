//! Logging in through a real `ssh-agent`: with no `--key` and no key file, `run` reaches the
//! host, directly and through a jump host, with the key the agent holds; `--no-agent`
//! refuses it.
//!
//! `HOME` points at an empty directory, so there is no default key to fall back on.
#![cfg(unix)]

mod common;

use assert_cmd::Command;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::Duration;

/// An `ssh-agent` holding the container's key, stopped on drop.
struct Agent {
    child: Child,
    socket: PathBuf,
}

impl Agent {
    fn start(dir: &Path, key: &Path) -> Self {
        let socket = dir.join("agent.sock");
        let child = std::process::Command::new("ssh-agent")
            .arg("-D")
            .arg("-a")
            .arg(&socket)
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("ssh-agent");
        // Built before anything can fail, so a failed setup still stops the agent.
        let agent = Agent { child, socket };
        for _ in 0..50 {
            if agent.socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        std::fs::set_permissions(key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let added = std::process::Command::new("ssh-add")
            .arg(key)
            .env("SSH_AUTH_SOCK", &agent.socket)
            .output()
            .expect("ssh-add");
        assert!(added.status.success(), "{added:?}");
        agent
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run(dir: &Path, agent: &Agent, extra: &[&str]) -> (bool, String) {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let out = Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir)
        .args(["run", "-i", "inventory.kdl", "-p", "plan.kdl"])
        .args(["--no-tui", "--no-host-key-check"])
        .args(extra)
        .env("HOME", &home)
        .env("SSH_AUTH_SOCK", &agent.socket)
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

fn write_plan(dir: &Path, marker: &str) {
    std::fs::write(
        dir.join("plan.kdl"),
        format!("plan \"p\" {{\n    step \"s\" {{\n        shell \"echo agent > /root/{marker}\"\n    }}\n}}\n"),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agent_logs_in_directly_and_through_a_jump_host() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    let key = container.write_key_file(dir.path());
    let agent = Agent::start(dir.path(), &key);
    std::fs::remove_file(&key).unwrap();

    std::fs::write(
        dir.path().join("inventory.kdl"),
        format!(
            "host \"target\" \"127.0.0.1\" user=\"root\" port={}\n",
            container.port
        ),
    )
    .unwrap();
    write_plan(dir.path(), "agent-direct");
    let (ok, out) = run(dir.path(), &agent, &[]);
    assert!(ok, "{out}");
    let written = ssh.exec("cat /root/agent-direct").await.unwrap().stdout;
    assert_eq!(written.trim(), "agent");

    // The container is its own bastion: the jump reaches its sshd on port 22 from inside.
    std::fs::write(
        dir.path().join("inventory.kdl"),
        format!(
            "host \"inner\" \"127.0.0.1\" user=\"root\" port=22 {{\n    jump \"127.0.0.1\" user=\"root\" port={}\n}}\n",
            container.port
        ),
    )
    .unwrap();
    write_plan(dir.path(), "agent-jump");
    let (ok, out) = run(dir.path(), &agent, &[]);
    assert!(ok, "{out}");
    let written = ssh.exec("cat /root/agent-jump").await.unwrap().stdout;
    assert_eq!(written.trim(), "agent");

    let (ok, out) = run(dir.path(), &agent, &["--no-agent"]);
    assert!(!ok);
    assert!(out.contains("--no-agent is set"), "{out}");
}
