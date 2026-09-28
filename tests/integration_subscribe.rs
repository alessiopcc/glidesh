//! End-to-end `subscribe` tests that drive the `glidesh` binary.
//!
//! A triggered subscriber must redo its work — restart, recreate — and an untriggered one
//! must not. Each effect is read from the host: the service's main PID for a restart, the
//! stand-in runtime's generation counter for a recreate.

mod common;

use assert_cmd::Command;
use glidesh::ssh::SshSession;
use std::path::Path;

const FAKE_DOCKER: &str = include_str!("fake-docker.sh");

const PLAN: &str = r#"
plan "handlers" {
    step "Deploy config" {
        file "/root/sub-app.conf" src="app.conf"
    }
    step "Restart cron" subscribe="Deploy config" {
        systemd "cron" state="restarted"
    }
    step "Recreate app" subscribe="Deploy config" {
        container "subapp" image="nginx:alpine"
    }
    step "Migrate" subscribe="Deploy config" {
        container "submigrate" image="alpine" state="run-once" check="true" remove=#false
    }
    step "Static file" subscribe="Deploy config" {
        file "/root/sub-static.txt" src="static.txt"
    }
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_subscriber_redoes_its_work_only_when_triggered() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    install_fake_docker(&ssh).await;
    let dir = tempfile::tempdir().unwrap();
    write_fixtures(dir.path(), &container);

    let first = run(dir.path(), &[]);
    assert!(first.contains("5 changed"), "{first}");
    let pid = cron_pid(&ssh).await;
    let (app, job) = (
        generation(&ssh, "subapp").await,
        generation(&ssh, "submigrate").await,
    );

    let settled = run(dir.path(), &[]);
    assert!(
        settled.contains("0 changed"),
        "nothing changed, so no handler may run:\n{settled}"
    );
    assert_eq!(
        cron_pid(&ssh).await,
        pid,
        "an untriggered `restarted` handler must not restart"
    );
    assert_eq!(generation(&ssh, "subapp").await, app);
    assert_eq!(
        generation(&ssh, "submigrate").await,
        job,
        "an untriggered run-once job whose check passes must not run"
    );

    std::fs::write(dir.path().join("app.conf"), b"version 2\n").unwrap();
    // Config, restart, recreate and rerun; the static file has nothing to redo and stays `ok`.
    let preview = run(dir.path(), &["--dry-run"]);
    assert!(preview.contains("4 would change"), "{preview}");
    assert!(preview.contains("(triggered)"), "{preview}");
    assert_eq!(cron_pid(&ssh).await, pid, "a preview must not restart");

    let triggered = run(dir.path(), &[]);
    assert!(triggered.contains("4 changed"), "{triggered}");
    assert_ne!(
        cron_pid(&ssh).await,
        pid,
        "a triggered handler must restart"
    );
    assert_ne!(
        generation(&ssh, "subapp").await,
        app,
        "a triggered container must be recreated"
    );
    assert_ne!(
        generation(&ssh, "submigrate").await,
        job,
        "a triggered run-once job must run despite its check"
    );
}

async fn install_fake_docker(ssh: &SshSession) {
    let out = ssh
        .exec(&format!(
            "cat > /usr/local/bin/docker <<'GLIDESH_FAKE_DOCKER_EOF'\n{}\nGLIDESH_FAKE_DOCKER_EOF\nchmod +x /usr/local/bin/docker",
            FAKE_DOCKER
        ))
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0, "{}", out.stderr);
}

async fn cron_pid(ssh: &SshSession) -> String {
    let out = ssh
        .exec("systemctl show -p MainPID --value cron")
        .await
        .unwrap();
    let pid = out.stdout.trim().to_string();
    assert!(pid != "0" && !pid.is_empty(), "cron is not running");
    pid
}

async fn generation(ssh: &SshSession, name: &str) -> String {
    let out = ssh
        .exec(&format!("cat /var/lib/fakedocker/c/{name}/generation"))
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0, "container {name} was never created");
    out.stdout.trim().to_string()
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

fn write_fixtures(dir: &Path, container: &common::TestContainer) {
    let key = container.write_key_file(dir);
    std::fs::write(dir.join("app.conf"), b"version 1\n").unwrap();
    std::fs::write(dir.join("static.txt"), b"unchanging\n").unwrap();
    std::fs::write(dir.join("plan.kdl"), PLAN).unwrap();
    std::fs::write(
        dir.join("inventory.kdl"),
        format!(
            "host \"target\" \"127.0.0.1\" user=\"root\" port={} {{\n    vars {{\n        ssh-key {:?}\n    }}\n}}\n",
            container.port,
            key.to_string_lossy()
        ),
    )
    .unwrap();
}
