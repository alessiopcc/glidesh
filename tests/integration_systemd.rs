mod common;

use glidesh::config::types::ParamValue;
use glidesh::modules::systemd::SystemdModule;
use glidesh::modules::{Module, ModuleParams, ModuleStatus};
use std::collections::HashMap;

#[tokio::test]
async fn test_systemd_start() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    // Stop cron first
    let _ = ssh.exec("systemctl stop cron 2>/dev/null").await;

    let mut args = HashMap::new();
    args.insert(
        "state".to_string(),
        ParamValue::String("started".to_string()),
    );

    let params = ModuleParams {
        resource_name: "cron".to_string(),
        args,
    };

    let result = SystemdModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);

    // Verify active
    let status = ssh.exec("systemctl is-active cron").await.unwrap();
    assert_eq!(status.stdout.trim(), "active");
}

#[tokio::test]
async fn test_systemd_stop() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    // Start cron first
    let _ = ssh.exec("systemctl start cron 2>/dev/null").await;

    let mut args = HashMap::new();
    args.insert(
        "state".to_string(),
        ParamValue::String("stopped".to_string()),
    );

    let params = ModuleParams {
        resource_name: "cron".to_string(),
        args,
    };

    let result = SystemdModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);

    // Verify stopped
    let status = ssh
        .exec("systemctl is-active cron 2>/dev/null")
        .await
        .unwrap();
    assert_ne!(status.stdout.trim(), "active");
}

#[tokio::test]
async fn test_systemd_enable() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    // Disable first
    let _ = ssh.exec("systemctl disable cron 2>/dev/null").await;

    let mut args = HashMap::new();
    args.insert(
        "state".to_string(),
        ParamValue::String("started".to_string()),
    );
    args.insert("enabled".to_string(), ParamValue::Bool(true));

    let params = ModuleParams {
        resource_name: "cron".to_string(),
        args,
    };

    let result = SystemdModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);

    // Verify enabled
    let enabled = ssh.exec("systemctl is-enabled cron").await.unwrap();
    assert_eq!(enabled.stdout.trim(), "enabled");
}

#[tokio::test]
async fn test_systemd_restart() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    // Start cron
    let _ = ssh.exec("systemctl start cron 2>/dev/null").await;

    let mut args = HashMap::new();
    args.insert(
        "state".to_string(),
        ParamValue::String("restarted".to_string()),
    );

    let params = ModuleParams {
        resource_name: "cron".to_string(),
        args,
    };

    // Restart check should always be Pending
    let status = SystemdModule.check(&ctx, &params).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Pending { .. }),
        "restart should always be Pending"
    );

    // Apply restart
    let result = SystemdModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);
}

#[tokio::test]
async fn test_systemd_idempotent() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    // Start cron
    let _ = ssh.exec("systemctl start cron 2>/dev/null").await;

    let mut args = HashMap::new();
    args.insert(
        "state".to_string(),
        ParamValue::String("started".to_string()),
    );

    let params = ModuleParams {
        resource_name: "cron".to_string(),
        args,
    };

    // Check should be Satisfied since already started
    let status = SystemdModule.check(&ctx, &params).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Satisfied),
        "already-started service should be Satisfied"
    );
}
