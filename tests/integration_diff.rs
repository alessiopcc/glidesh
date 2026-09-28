//! `--diff`: the content diff `file` produces and the parameters `container` names.
//!
//! Module-level tests call `check` with `diff` on, over real SSH. The binary tests cover
//! where a diff surfaces: on the console for a preview, in the run log for a real run.

mod common;

use assert_cmd::Command;
use glidesh::config::types::ParamValue;
use glidesh::modules::container::ContainerModule;
use glidesh::modules::file::FileModule;
use glidesh::modules::{Module, ModuleParams, ModuleStatus};
use glidesh::secrets::config::{Provider, SecretsConfig};
use glidesh::secrets::passphrase::{PassphraseProvider, generate_dek};
use glidesh::secrets::{Identity, Secrets, token};
use glidesh::ssh::SshSession;
use std::collections::HashMap;
use std::path::Path;

const FAKE_DOCKER: &str = include_str!("fake-docker.sh");

fn params(resource: &str, args: &[(&str, ParamValue)]) -> ModuleParams {
    ModuleParams {
        resource_name: resource.to_string(),
        args: args
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    }
}

fn s(value: &str) -> ParamValue {
    ParamValue::String(value.to_string())
}

fn pending_diff(status: ModuleStatus) -> Option<String> {
    match status {
        ModuleStatus::Pending { diff, .. } => diff,
        other => panic!("expected Pending, got {other:?}"),
    }
}

async fn put(ssh: &SshSession, path: &str, content: &str) {
    ssh.upload_file(content.as_bytes(), path).await.unwrap();
}

#[tokio::test]
async fn file_diff_shows_the_changed_lines_only_when_asked() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.conf"), "name=app\nport=8080\n").unwrap();
    put(&ssh, "/root/diff-app.conf", "name=app\nport=80\n").await;

    let vars = HashMap::new();
    let mut ctx = container.module_context(&ssh, &os_info, &vars, true);
    ctx.plan_base_dir = dir.path();
    let spec = params("/root/diff-app.conf", &[("src", s("app.conf"))]);

    assert_eq!(
        pending_diff(FileModule.check(&ctx, &spec).await.unwrap()),
        None,
        "without --diff nothing is downloaded or shown"
    );

    ctx.diff = true;
    let diff = pending_diff(FileModule.check(&ctx, &spec).await.unwrap()).unwrap();
    assert!(diff.contains("--- /root/diff-app.conf (host)"), "{diff}");
    assert!(diff.contains("-port=80\n"), "{diff}");
    assert!(diff.contains("+port=8080\n"), "{diff}");
    assert!(diff.contains(" name=app\n"), "{diff}");
}

#[tokio::test]
async fn file_diff_of_a_new_file_and_a_directory() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("new.conf"), "fresh\n").unwrap();
    let tree = dir.path().join("site");
    std::fs::create_dir(&tree).unwrap();
    std::fs::write(tree.join("same.txt"), "same\n").unwrap();
    std::fs::write(tree.join("edit.txt"), "after\n").unwrap();
    ssh.exec("mkdir -p /root/diff-site").await.unwrap();
    put(&ssh, "/root/diff-site/same.txt", "same\n").await;
    put(&ssh, "/root/diff-site/edit.txt", "before\n").await;

    let vars = HashMap::new();
    let mut ctx = container.module_context(&ssh, &os_info, &vars, true);
    ctx.plan_base_dir = dir.path();
    ctx.diff = true;

    let new = params("/root/diff-new.conf", &[("src", s("new.conf"))]);
    let diff = pending_diff(FileModule.check(&ctx, &new).await.unwrap()).unwrap();
    assert!(diff.contains("--- /dev/null"), "{diff}");
    assert!(diff.contains("+fresh"), "{diff}");

    let site = params(
        "/root/diff-site",
        &[("src", s("site")), ("recurse", ParamValue::Bool(true))],
    );
    let diff = pending_diff(FileModule.check(&ctx, &site).await.unwrap()).unwrap();
    assert!(diff.contains("/root/diff-site/edit.txt (host)"), "{diff}");
    assert!(
        diff.contains("-before") && diff.contains("+after"),
        "{diff}"
    );
    assert!(
        !diff.contains("same.txt"),
        "an unchanged file has no diff: {diff}"
    );
}

#[tokio::test]
async fn file_diff_is_hidden_when_the_content_holds_a_secret() {
    skip_unless_integration!();

    let dek = generate_dek();
    let cfg = SecretsConfig {
        provider: Provider::Passphrase,
        encryptedkey: PassphraseProvider::new("pw".into()).wrap_dek(&dek).unwrap(),
        recipients: Vec::new(),
    };
    let secrets = Secrets::open(Some(&cfg), Some(&Identity::Passphrase("pw".into()))).unwrap();
    let mut vars = HashMap::from([(
        "db-password".to_string(),
        token::encrypt_value(&dek, b"rotated-s3cret").unwrap(),
    )]);
    secrets.decrypt_vars(&mut vars).unwrap();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("db.conf"), "password=${db-password}\n").unwrap();
    put(&ssh, "/root/diff-db.conf", "password=previous-s3cret\n").await;

    let mut ctx = container.module_context(&ssh, &os_info, &vars, true);
    ctx.plan_base_dir = dir.path();
    ctx.diff = true;
    ctx.secrets = Some(secrets.registry());
    let spec = params(
        "/root/diff-db.conf",
        &[("src", s("db.conf")), ("template", ParamValue::Bool(true))],
    );

    let diff = pending_diff(FileModule.check(&ctx, &spec).await.unwrap()).unwrap();
    assert_eq!(
        diff,
        "/root/diff-db.conf: diff hidden (content contains a secret)"
    );
}

/// A secret the plan no longer uses is not registered, but the host's copy still holds it,
/// so a file kept from other users is hidden whatever it holds.
#[tokio::test]
async fn file_diff_is_hidden_for_a_file_other_users_cannot_read() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("db.conf"), "password=now-in-plain-text\n").unwrap();
    put(&ssh, "/root/diff-private.conf", "password=retired-s3cret\n").await;
    ssh.exec("chmod 600 /root/diff-private.conf").await.unwrap();

    let vars = HashMap::new();
    let mut ctx = container.module_context(&ssh, &os_info, &vars, true);
    ctx.plan_base_dir = dir.path();
    ctx.diff = true;

    let on_host = params("/root/diff-private.conf", &[("src", s("db.conf"))]);
    let diff = pending_diff(FileModule.check(&ctx, &on_host).await.unwrap()).unwrap();
    assert_eq!(
        diff,
        "/root/diff-private.conf: diff hidden (not readable by other users)"
    );

    let by_plan = params(
        "/root/diff-new-private.conf",
        &[("src", s("db.conf")), ("mode", s("0640"))],
    );
    let diff = pending_diff(FileModule.check(&ctx, &by_plan).await.unwrap()).unwrap();
    assert_eq!(
        diff,
        "/root/diff-new-private.conf: diff hidden (not readable by other users)"
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
    assert_eq!(out.exit_code, 0, "installing fake docker: {}", out.stderr);
}

#[tokio::test]
async fn container_diff_names_the_parameters_that_drifted() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    install_fake_docker(&ssh).await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let mut ctx = container.module_context(&ssh, &os_info, &vars, false);

    let created = params(
        "web",
        &[
            ("image", s("nginx:1.27")),
            ("ports", ParamValue::List(vec!["80:80".into()])),
        ],
    );
    ContainerModule.apply(&ctx, &created).await.unwrap();

    let secret_env = ParamValue::Map(HashMap::from([(
        "API_TOKEN".to_string(),
        "tok-very-secret".to_string(),
    )]));
    let wanted = params(
        "web",
        &[("image", s("nginx:1.28")), ("environment", secret_env)],
    );

    assert_eq!(
        pending_diff(ContainerModule.check(&ctx, &wanted).await.unwrap()),
        None
    );

    ctx.diff = true;
    let diff = pending_diff(ContainerModule.check(&ctx, &wanted).await.unwrap()).unwrap();
    assert_eq!(diff, "changed: image; added: environment; removed: ports");
    assert!(!diff.contains("tok-very-secret"));

    // A container created before field hashes existed carries only the whole-spec hash.
    ssh.exec("rm -f /var/lib/fakedocker/c/web/fields")
        .await
        .unwrap();
    let diff = pending_diff(ContainerModule.check(&ctx, &wanted).await.unwrap()).unwrap();
    assert_eq!(
        diff,
        "created without per-parameter hashes (by an older glidesh or by hand); recreating it records them"
    );
}

fn write_fixtures(dir: &Path, port: u16, key: &Path) {
    std::fs::write(dir.join("app.conf"), "name=app\nport=8080\n").unwrap();
    std::fs::write(
        dir.join("plan.kdl"),
        r#"
plan "diff" {
    step "Config" {
        file "/root/diff-cli.conf" src="app.conf"
    }
}
"#,
    )
    .unwrap();
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

/// Run logs go under `$HOME/.glidesh/runs`, so each run gets its own home.
fn run(dir: &Path, home: &Path, extra: &[&str]) -> String {
    let out = Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir)
        .env("HOME", home)
        .args(["run", "-i", "inventory.kdl", "-p", "plan.kdl"])
        .args(["--no-tui", "--no-host-key-check"])
        .args(extra)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out).unwrap()
}

fn run_logs(home: &Path) -> String {
    let runs = home.join(".glidesh").join("runs");
    let mut all = String::new();
    for run in std::fs::read_dir(runs).unwrap() {
        for entry in std::fs::read_dir(run.unwrap().path()).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "log") {
                all.push_str(&std::fs::read_to_string(path).unwrap());
            }
        }
    }
    all
}

#[tokio::test(flavor = "multi_thread")]
async fn a_preview_prints_the_diff_and_a_real_run_logs_it() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    let key = container.write_key_file(dir.path());
    write_fixtures(dir.path(), container.port, &key);
    put(&ssh, "/root/diff-cli.conf", "name=app\nport=80\n").await;

    let home = tempfile::tempdir().unwrap();
    let plain = run(dir.path(), home.path(), &["--dry-run"]);
    assert!(!plain.contains("+port=8080"), "{plain}");

    let preview = run(dir.path(), home.path(), &["--dry-run", "--diff"]);
    assert!(preview.contains("-port=80"), "{preview}");
    assert!(preview.contains("+port=8080"), "{preview}");

    let home = tempfile::tempdir().unwrap();
    run(dir.path(), home.path(), &["--diff"]);
    let logs = run_logs(home.path());
    assert!(logs.contains("+port=8080"), "{logs}");
    let applied = ssh.exec("cat /root/diff-cli.conf").await.unwrap().stdout;
    assert_eq!(applied, "name=app\nport=8080\n");
}
