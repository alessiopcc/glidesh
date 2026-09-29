mod common;

use glidesh::config::types::{ParamValue, ResolvedRunAs, RunAsMethod};
use glidesh::modules::file::FileModule;
use glidesh::modules::shell::ShellModule;
use glidesh::modules::{Module, ModuleParams, ModuleStatus};
use std::collections::HashMap;

/// Escalate to root via passwordless sudo (the test container's `deploy` user has a
/// NOPASSWD sudoers entry).
fn run_as_root() -> ResolvedRunAs {
    ResolvedRunAs {
        user: "root".to_string(),
        method: RunAsMethod::Sudo,
        password: None,
    }
}

#[tokio::test]
async fn test_run_as_sudo_runs_as_root() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&ssh, &os_info, &vars, false, run_as_root());

    let params = ModuleParams {
        resource_name: "id -un".to_string(),
        args: HashMap::new(),
    };

    let result = ShellModule.apply(&ctx, &params).await.unwrap();
    assert!(
        result.output.contains("root"),
        "escalated command should run as root, got: {}",
        result.output
    );
}

#[tokio::test]
async fn test_no_run_as_is_login_user() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let params = ModuleParams {
        resource_name: "id -un".to_string(),
        args: HashMap::new(),
    };

    let result = ShellModule.apply(&ctx, &params).await.unwrap();
    assert!(
        result.output.contains("deploy"),
        "without run-as the command should run as the login user, got: {}",
        result.output
    );
}

#[tokio::test]
async fn test_run_as_file_upload_to_root_owned_dir() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"managed by glidesh").unwrap();

    let mut args = HashMap::new();
    args.insert(
        "src".to_string(),
        ParamValue::String(tmp.path().to_string_lossy().to_string()),
    );
    let params = ModuleParams {
        resource_name: "/etc/glidesh-runas-test.conf".to_string(),
        args,
    };

    // Validates the temp-upload + sudo-mv path for a destination the login user
    // cannot write directly.
    let result = FileModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);

    let root = container.ssh_session().await;
    let content = root.exec("cat /etc/glidesh-runas-test.conf").await.unwrap();
    assert_eq!(content.stdout, "managed by glidesh");
    let owner = root
        .exec("stat -c %U /etc/glidesh-runas-test.conf")
        .await
        .unwrap();
    assert_eq!(
        owner.stdout.trim(),
        "root",
        "escalated upload should be owned by the run-as user"
    );
}

#[tokio::test]
async fn test_no_run_as_cannot_write_root_owned_dir() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&deploy, &os_info, &vars, false);

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"should fail").unwrap();
    let mut args = HashMap::new();
    args.insert(
        "src".to_string(),
        ParamValue::String(tmp.path().to_string_lossy().to_string()),
    );
    let params = ModuleParams {
        resource_name: "/etc/glidesh-runas-denied.conf".to_string(),
        args,
    };

    let result = FileModule.apply(&ctx, &params).await;
    assert!(
        result.is_err(),
        "writing to a root-owned dir as deploy without run-as should fail"
    );
}

fn upload_params(src: &std::path::Path, dest: &str, extra: &[(&str, ParamValue)]) -> ModuleParams {
    let mut args = HashMap::new();
    args.insert(
        "src".to_string(),
        ParamValue::String(src.to_string_lossy().to_string()),
    );
    for (key, value) in extra {
        args.insert(key.to_string(), value.clone());
    }
    ModuleParams {
        resource_name: dest.to_string(),
        args,
    }
}

async fn stat(session: &glidesh::ssh::SshSession, path: &str) -> String {
    let out = session
        .exec(&format!("stat -c '%a %U:%G' {path}"))
        .await
        .unwrap();
    out.stdout.trim().to_string()
}

#[tokio::test]
async fn test_run_as_upload_of_a_new_file_gets_the_plain_upload_mode() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"new file").unwrap();
    let params = upload_params(tmp.path(), "/etc/glidesh-runas-new.conf", &[]);
    FileModule.apply(&ctx, &params).await.unwrap();

    // Staging goes through a `mktemp` file, which is 0600 and owned by the login
    // user's group; neither may leak into the result.
    let root = container.ssh_session().await;
    assert_eq!(
        stat(&root, "/etc/glidesh-runas-new.conf").await,
        "644 root:root"
    );
}

#[tokio::test]
async fn test_run_as_upload_keeps_the_replaced_files_owner_and_mode() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let root = container.ssh_session().await;
    root.exec(
        "printf old > /etc/glidesh-runas-kept.sh &&          chown nobody:nogroup /etc/glidesh-runas-kept.sh &&          chmod 0755 /etc/glidesh-runas-kept.sh",
    )
    .await
    .unwrap();

    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"new").unwrap();
    let params = upload_params(tmp.path(), "/etc/glidesh-runas-kept.sh", &[]);
    let result = FileModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);

    let content = root.exec("cat /etc/glidesh-runas-kept.sh").await.unwrap();
    assert_eq!(content.stdout, "new");
    assert_eq!(
        stat(&root, "/etc/glidesh-runas-kept.sh").await,
        "755 nobody:nogroup"
    );
}

#[tokio::test]
async fn test_run_as_upload_mode_still_wins_and_a_second_run_is_ok() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let root = container.ssh_session().await;
    root.exec(
        "printf old > /etc/glidesh-runas-mode.conf && chmod 0755 /etc/glidesh-runas-mode.conf",
    )
    .await
    .unwrap();

    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"new").unwrap();
    let params = upload_params(
        tmp.path(),
        "/etc/glidesh-runas-mode.conf",
        &[("mode", ParamValue::String("0640".to_string()))],
    );
    FileModule.apply(&ctx, &params).await.unwrap();
    assert_eq!(
        stat(&root, "/etc/glidesh-runas-mode.conf").await,
        "640 root:root"
    );

    let status = FileModule.check(&ctx, &params).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Satisfied),
        "a second run should be ok, got {status:?}"
    );
}

#[tokio::test]
async fn test_run_as_recursive_upload_gets_the_plain_upload_modes() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let src = tempfile::tempdir().unwrap();
    std::fs::create_dir(src.path().join("sub")).unwrap();
    std::fs::write(src.path().join("top.conf"), b"top").unwrap();
    std::fs::write(src.path().join("sub").join("inner.conf"), b"inner").unwrap();
    let params = upload_params(
        src.path(),
        "/etc/glidesh-runas-tree",
        &[("recurse", ParamValue::Bool(true))],
    );
    FileModule.apply(&ctx, &params).await.unwrap();

    let root = container.ssh_session().await;
    assert_eq!(
        stat(&root, "/etc/glidesh-runas-tree/top.conf").await,
        "644 root:root"
    );
    assert_eq!(
        stat(&root, "/etc/glidesh-runas-tree/sub/inner.conf").await,
        "644 root:root"
    );
    assert_eq!(
        stat(&root, "/etc/glidesh-runas-tree/sub").await,
        "755 root:root"
    );
}

#[tokio::test]
async fn test_run_as_upload_writes_through_a_symlink_destination() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let root = container.ssh_session().await;
    root.exec(
        "printf old > /etc/glidesh-runas-target.conf && \
         chmod 0640 /etc/glidesh-runas-target.conf && \
         ln -s /etc/glidesh-runas-target.conf /etc/glidesh-runas-link.conf",
    )
    .await
    .unwrap();

    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"new").unwrap();
    let params = upload_params(tmp.path(), "/etc/glidesh-runas-link.conf", &[]);
    FileModule.apply(&ctx, &params).await.unwrap();

    // Moving over the link would leave a regular file with the link's own 0777 mode.
    let link = root
        .exec("test -L /etc/glidesh-runas-link.conf && echo link")
        .await
        .unwrap();
    assert_eq!(link.stdout.trim(), "link");
    let content = root
        .exec("cat /etc/glidesh-runas-target.conf")
        .await
        .unwrap();
    assert_eq!(content.stdout, "new");
    assert_eq!(
        stat(&root, "/etc/glidesh-runas-target.conf").await,
        "640 root:root"
    );
}

#[tokio::test]
async fn test_run_as_upload_of_a_new_file_takes_a_setgid_directorys_group() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let root = container.ssh_session().await;
    root.exec("mkdir -p /srv/glidesh-shared && chgrp nogroup /srv/glidesh-shared && chmod 2775 /srv/glidesh-shared")
        .await
        .unwrap();

    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"shared").unwrap();
    let params = upload_params(tmp.path(), "/srv/glidesh-shared/new.conf", &[]);
    FileModule.apply(&ctx, &params).await.unwrap();

    assert_eq!(
        stat(&root, "/srv/glidesh-shared/new.conf").await,
        "644 root:nogroup"
    );
}
