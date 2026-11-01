//! Logging in through a real `ssh-agent`: with no `--key` and no key file, `run` reaches the
//! host, directly and through a jump host, with a key the agent holds, past one the server
//! rejects and with an RSA key; `--no-agent` refuses it.
//!
//! `HOME` points at an empty directory, so there is no default key to fall back on.
#![cfg(unix)]

mod common;

use assert_cmd::Command;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::Duration;

/// An `ssh-agent` holding the given keys in that order, stopped on drop.
struct Agent {
    child: Child,
    socket: PathBuf,
}

impl Agent {
    fn start(dir: &Path, keys: &[&Path]) -> Self {
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
        for key in keys {
            std::fs::set_permissions(key, std::fs::Permissions::from_mode(0o600)).unwrap();
            let added = std::process::Command::new("ssh-add")
                .arg(key)
                .env("SSH_AUTH_SOCK", &agent.socket)
                .output()
                .expect("ssh-add");
            assert!(added.status.success(), "{added:?}");
        }
        agent
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A new passphrase-less key at `dir/name`, its public half beside it as `name.pub`.
fn keygen(dir: &Path, name: &str, kind: &str) -> PathBuf {
    let path = dir.join(name);
    let made = std::process::Command::new("ssh-keygen")
        .args(["-q", "-N", "", "-t", kind, "-f"])
        .arg(&path)
        .output()
        .expect("ssh-keygen");
    assert!(made.status.success(), "{made:?}");
    path
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

/// Runs a plan through the agent, directly and through a jump host, and checks it reached the
/// container both ways.
async fn logs_in_both_ways(container: &common::TestContainer, dir: &Path, agent: &Agent) {
    let ssh = container.ssh_session().await;

    std::fs::write(
        dir.join("inventory.kdl"),
        format!(
            "host \"target\" \"127.0.0.1\" user=\"root\" port={}\n",
            container.port
        ),
    )
    .unwrap();
    write_plan(dir, "agent-direct");
    let (ok, out) = run(dir, agent, &[]);
    assert!(ok, "{out}");
    let written = ssh.exec("cat /root/agent-direct").await.unwrap().stdout;
    assert_eq!(written.trim(), "agent");

    // The container is its own bastion: the jump reaches its sshd on port 22 from inside.
    std::fs::write(
        dir.join("inventory.kdl"),
        format!(
            "host \"inner\" \"127.0.0.1\" user=\"root\" port=22 {{\n    jump \"127.0.0.1\" user=\"root\" port={}\n}}\n",
            container.port
        ),
    )
    .unwrap();
    write_plan(dir, "agent-jump");
    let (ok, out) = run(dir, agent, &[]);
    assert!(ok, "{out}");
    let written = ssh.exec("cat /root/agent-jump").await.unwrap().stdout;
    assert_eq!(written.trim(), "agent");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agent_logs_in_past_a_key_the_server_rejects() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let dir = tempfile::tempdir().unwrap();
    let key = container.write_key_file(dir.path());
    // The agent lists keys in the order they were added, so this one is offered first.
    let rejected = keygen(dir.path(), "rejected", "ed25519");
    let agent = Agent::start(dir.path(), &[&rejected, &key]);
    std::fs::remove_file(&key).unwrap();

    logs_in_both_ways(&container, dir.path(), &agent).await;

    let (ok, out) = run(dir.path(), &agent, &["--no-agent"]);
    assert!(!ok);
    assert!(out.contains("--no-agent is set"), "{out}");
}

/// The container's OpenSSH 8.9 refuses SHA-1 RSA signatures, so a login shows SHA-256 was used.
#[tokio::test(flavor = "multi_thread")]
async fn the_agent_logs_in_with_an_rsa_key() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let dir = tempfile::tempdir().unwrap();
    let key = keygen(dir.path(), "rsa", "rsa");
    let public = std::fs::read_to_string(key.with_extension("pub")).unwrap();
    let ssh = container.ssh_session().await;
    let added = ssh
        .exec(&format!(
            "echo '{}' >> /root/.ssh/authorized_keys",
            public.trim()
        ))
        .await
        .unwrap();
    assert_eq!(added.exit_code, 0, "{}", added.stderr);
    let agent = Agent::start(dir.path(), &[&key]);
    std::fs::remove_file(&key).unwrap();

    logs_in_both_ways(&container, dir.path(), &agent).await;
}
