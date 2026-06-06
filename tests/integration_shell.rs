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

    // The default accepts only exit 0, so `false` (exit 1) fails on its own.
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

#[tokio::test]
async fn test_shell_success_codes_accepts_nonzero() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let mut args = HashMap::new();
    args.insert(
        "success_codes".to_string(),
        ParamValue::String("0,2".to_string()),
    );
    let params = ModuleParams {
        resource_name: "exit 2".to_string(),
        args,
    };

    // Exit 2 is in the accepted set, so the task succeeds and reports the code.
    let result = ShellModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);
    assert_eq!(
        result.exit_code, 2,
        "the real remote exit code should be captured, got: {}",
        result.exit_code
    );
}

#[tokio::test]
async fn test_shell_default_rejects_nonzero() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    // No success_codes: the default accepts only exit 0, so a non-zero exit
    // fails the task on its own.
    let params = ModuleParams {
        resource_name: "exit 3".to_string(),
        args: HashMap::new(),
    };

    let result = ShellModule.apply(&ctx, &params).await;
    assert!(
        result.is_err(),
        "a non-zero exit should fail by default (only 0 is accepted)"
    );
}

#[tokio::test]
async fn test_shell_success_codes_rejects_unlisted() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let mut args = HashMap::new();
    args.insert(
        "success_codes".to_string(),
        ParamValue::String("0,2".to_string()),
    );
    let params = ModuleParams {
        resource_name: "exit 1".to_string(),
        args,
    };

    let result = ShellModule.apply(&ctx, &params).await;
    assert!(
        result.is_err(),
        "exit 1 is not in the accepted set {{0,2}} and should fail"
    );
}

#[tokio::test]
async fn test_shell_timeout_aborts_stuck_command() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let mut args = HashMap::new();
    args.insert("timeout".to_string(), ParamValue::Integer(1));
    let params = ModuleParams {
        resource_name: "sleep 30".to_string(),
        args,
    };

    // The whole call must finish well under the command's 30s sleep: the 1s
    // timeout aborts the attempt and surfaces a timeout failure.
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        ShellModule.apply(&ctx, &params),
    )
    .await
    .expect("apply should return long before `sleep 30` finishes");

    let err = result.expect_err("a timed-out command should fail");
    assert!(
        err.to_string().contains("timed out"),
        "error should mention the timeout, got: {}",
        err
    );
}

#[tokio::test]
async fn test_shell_backgrounded_child_does_not_hang() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    // A backgrounded child inherits the command's stdout/stderr and holds the
    // channel open after the foreground command exits. The exec read loop must
    // return once the exit status arrives instead of waiting for the channel to
    // close (which the lingering `sleep` would delay for 30s). Regression test
    // for the exec-hang fix.
    let params = ModuleParams {
        resource_name: "sh -c 'sleep 30 & echo started'".to_string(),
        args: HashMap::new(),
    };

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        ShellModule.apply(&ctx, &params),
    )
    .await
    .expect("exec must not hang waiting for the backgrounded child to exit")
    .unwrap();

    assert!(
        result.output.contains("started"),
        "foreground output should be captured, got: {}",
        result.output
    );
}
