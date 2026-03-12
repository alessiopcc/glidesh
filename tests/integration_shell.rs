mod common;

use glidesh::config::types::ParamValue;
use glidesh::modules::shell::ShellModule;
use glidesh::modules::{Module, ModuleParams, ModuleStatus};
use std::collections::HashMap;

#[tokio::test]
async fn test_shell_check_always_pending() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let params = ModuleParams {
        resource_name: "echo hello".to_string(),
        args: HashMap::new(),
    };

    let status = ShellModule.check(&ctx, &params).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Pending { .. }),
        "shell check should always return Pending"
    );
}

#[tokio::test]
async fn test_shell_apply_echo() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let params = ModuleParams {
        resource_name: "echo hello".to_string(),
        args: HashMap::new(),
    };

    let result = ShellModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);
    assert!(
        result.output.contains("hello"),
        "output should contain 'hello', got: {}",
        result.output
    );
}

#[tokio::test]
async fn test_shell_apply_failure() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let params = ModuleParams {
        resource_name: "false".to_string(),
        args: HashMap::new(),
    };

    let result = ShellModule.apply(&ctx, &params).await;
    assert!(result.is_err(), "running 'false' should return an error");
}

#[tokio::test]
async fn test_shell_dry_run() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, true);

    let params = ModuleParams {
        resource_name: "echo should-not-run".to_string(),
        args: HashMap::new(),
    };

    let result = ShellModule.apply(&ctx, &params).await.unwrap();
    assert!(!result.changed);
    assert!(result.output.contains("dry-run"));
}

#[tokio::test]
async fn test_shell_retries() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let mut args = HashMap::new();
    args.insert("retries".to_string(), ParamValue::Integer(2));

    let params = ModuleParams {
        resource_name: "false".to_string(),
        args,
    };

    let result = ShellModule.apply(&ctx, &params).await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("2 attempt"),
        "error should mention 2 attempts, got: {}",
        err_msg
    );
}
