mod common;

use glidesh::config::types::{ParamValue, ResolvedRunAs, RunAsMethod};
use glidesh::modules::file::FileModule;
use glidesh::modules::shell::ShellModule;
use glidesh::modules::{Module, ModuleParams};
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
