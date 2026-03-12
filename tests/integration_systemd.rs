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

#[tokio::test]
async fn test_systemd_create_service() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    // Clean up any previous test service
    let _ = ssh
        .exec("systemctl stop glidesh-test 2>/dev/null; rm -f /etc/systemd/system/glidesh-test.service; systemctl daemon-reload")
        .await;

    let mut args = HashMap::new();
    args.insert(
        "command".to_string(),
        ParamValue::String("/bin/sleep 3600".to_string()),
    );
    args.insert(
        "description".to_string(),
        ParamValue::String("Glidesh Test Service".to_string()),
    );
    args.insert(
        "state".to_string(),
        ParamValue::String("started".to_string()),
    );

    let params = ModuleParams {
        resource_name: "glidesh-test".to_string(),
        args,
    };

    // Check should be Pending (unit file doesn't exist)
    let status = SystemdModule.check(&ctx, &params).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Pending { .. }),
        "new service should be Pending"
    );

    // Apply should create the service and start it
    let result = SystemdModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);

    // Verify unit file exists
    let cat = ssh
        .exec("cat /etc/systemd/system/glidesh-test.service")
        .await
        .unwrap();
    assert_eq!(cat.exit_code, 0);
    assert!(cat.stdout.contains("ExecStart=/bin/sleep 3600"));
    assert!(cat.stdout.contains("Description=Glidesh Test Service"));

    // Verify service is running
    let active = ssh.exec("systemctl is-active glidesh-test").await.unwrap();
    assert_eq!(active.stdout.trim(), "active");

    // Cleanup
    let _ = ssh
        .exec("systemctl stop glidesh-test; rm -f /etc/systemd/system/glidesh-test.service; systemctl daemon-reload")
        .await;
}

#[tokio::test]
async fn test_systemd_create_service_idempotent() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    // Clean up
    let _ = ssh
        .exec("systemctl stop glidesh-idem 2>/dev/null; rm -f /etc/systemd/system/glidesh-idem.service; systemctl daemon-reload")
        .await;

    let mut args = HashMap::new();
    args.insert(
        "command".to_string(),
        ParamValue::String("/bin/sleep 3600".to_string()),
    );
    args.insert(
        "state".to_string(),
        ParamValue::String("started".to_string()),
    );

    let params = ModuleParams {
        resource_name: "glidesh-idem".to_string(),
        args,
    };

    // First apply
    let result = SystemdModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);

    // Second check should be Satisfied
    let status = SystemdModule.check(&ctx, &params).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Satisfied),
        "second check should be Satisfied, got {:?}",
        status
    );

    // Cleanup
    let _ = ssh
        .exec("systemctl stop glidesh-idem; rm -f /etc/systemd/system/glidesh-idem.service; systemctl daemon-reload")
        .await;
}

#[tokio::test]
async fn test_systemd_create_service_update() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    // Clean up
    let _ = ssh
        .exec("systemctl stop glidesh-upd 2>/dev/null; rm -f /etc/systemd/system/glidesh-upd.service; systemctl daemon-reload")
        .await;

    // Create initial service
    let mut args = HashMap::new();
    args.insert(
        "command".to_string(),
        ParamValue::String("/bin/sleep 3600".to_string()),
    );
    args.insert(
        "state".to_string(),
        ParamValue::String("started".to_string()),
    );

    let params = ModuleParams {
        resource_name: "glidesh-upd".to_string(),
        args,
    };
    SystemdModule.apply(&ctx, &params).await.unwrap();

    // Update command
    let mut args2 = HashMap::new();
    args2.insert(
        "command".to_string(),
        ParamValue::String("/bin/sleep 7200".to_string()),
    );
    args2.insert(
        "state".to_string(),
        ParamValue::String("started".to_string()),
    );

    let params2 = ModuleParams {
        resource_name: "glidesh-upd".to_string(),
        args: args2,
    };

    // Check should detect change
    let status = SystemdModule.check(&ctx, &params2).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Pending { .. }),
        "updated service should be Pending"
    );

    // Apply update
    let result = SystemdModule.apply(&ctx, &params2).await.unwrap();
    assert!(result.changed);

    // Verify updated content
    let cat = ssh
        .exec("cat /etc/systemd/system/glidesh-upd.service")
        .await
        .unwrap();
    assert!(cat.stdout.contains("ExecStart=/bin/sleep 7200"));

    // Cleanup
    let _ = ssh
        .exec("systemctl stop glidesh-upd; rm -f /etc/systemd/system/glidesh-upd.service; systemctl daemon-reload")
        .await;
}

#[tokio::test]
async fn test_systemd_create_with_environment() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    // Clean up
    let _ = ssh
        .exec("systemctl stop glidesh-env 2>/dev/null; rm -f /etc/systemd/system/glidesh-env.service; systemctl daemon-reload")
        .await;

    let mut env = HashMap::new();
    env.insert("PORT".to_string(), "8080".to_string());
    env.insert("NODE_ENV".to_string(), "production".to_string());

    let mut args = HashMap::new();
    args.insert(
        "command".to_string(),
        ParamValue::String("/bin/sleep 3600".to_string()),
    );
    args.insert("environment".to_string(), ParamValue::Map(env));
    args.insert(
        "state".to_string(),
        ParamValue::String("started".to_string()),
    );

    let params = ModuleParams {
        resource_name: "glidesh-env".to_string(),
        args,
    };

    let result = SystemdModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);

    // Verify environment lines in unit file
    let cat = ssh
        .exec("cat /etc/systemd/system/glidesh-env.service")
        .await
        .unwrap();
    assert!(cat.stdout.contains(r#"Environment="PORT=8080""#));
    assert!(cat.stdout.contains(r#"Environment="NODE_ENV=production""#));

    // Cleanup
    let _ = ssh
        .exec("systemctl stop glidesh-env; rm -f /etc/systemd/system/glidesh-env.service; systemctl daemon-reload")
        .await;
}
