mod common;

use glidesh::config::types::ParamValue;
use glidesh::modules::user::UserModule;
use glidesh::modules::{Module, ModuleParams, ModuleStatus};
use std::collections::HashMap;

#[tokio::test]
async fn test_user_create() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let params = ModuleParams {
        resource_name: "testuser".to_string(),
        args: HashMap::new(),
    };

    let result = UserModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);

    // Verify user exists
    let output = ssh.exec("id testuser").await.unwrap();
    assert_eq!(output.exit_code, 0, "testuser should exist");
}

#[tokio::test]
async fn test_user_with_uid_shell() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let mut args = HashMap::new();
    args.insert("uid".to_string(), ParamValue::Integer(5000));
    args.insert(
        "shell".to_string(),
        ParamValue::String("/bin/sh".to_string()),
    );

    let params = ModuleParams {
        resource_name: "uiduser".to_string(),
        args,
    };

    let result = UserModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);

    // Verify UID
    let uid_output = ssh.exec("id -u uiduser").await.unwrap();
    assert_eq!(uid_output.stdout.trim(), "5000");

    // Verify shell
    let shell_output = ssh
        .exec("getent passwd uiduser | cut -d: -f7")
        .await
        .unwrap();
    assert_eq!(shell_output.stdout.trim(), "/bin/sh");
}

#[tokio::test]
async fn test_user_modify_groups() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    // Create user first
    let create_params = ModuleParams {
        resource_name: "grpuser".to_string(),
        args: HashMap::new(),
    };
    UserModule.apply(&ctx, &create_params).await.unwrap();

    // Create a group
    let _ = ssh.exec("groupadd testgrp 2>/dev/null").await;

    // Modify user to add group
    let mut args = HashMap::new();
    args.insert(
        "groups".to_string(),
        ParamValue::List(vec!["testgrp".to_string()]),
    );

    let modify_params = ModuleParams {
        resource_name: "grpuser".to_string(),
        args,
    };

    let result = UserModule.apply(&ctx, &modify_params).await.unwrap();
    assert!(result.changed);

    // Verify membership
    let groups = ssh.exec("id -nG grpuser").await.unwrap();
    assert!(
        groups.stdout.contains("testgrp"),
        "user should be in testgrp, got: {}",
        groups.stdout
    );
}

#[tokio::test]
async fn test_user_delete() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    // Create user
    let create_params = ModuleParams {
        resource_name: "deluser".to_string(),
        args: HashMap::new(),
    };
    UserModule.apply(&ctx, &create_params).await.unwrap();

    // Delete user
    let mut args = HashMap::new();
    args.insert(
        "state".to_string(),
        ParamValue::String("absent".to_string()),
    );
    let delete_params = ModuleParams {
        resource_name: "deluser".to_string(),
        args,
    };
    let result = UserModule.apply(&ctx, &delete_params).await.unwrap();
    assert!(result.changed);

    // Verify gone
    let output = ssh.exec("id deluser 2>/dev/null").await.unwrap();
    assert_ne!(output.exit_code, 0, "user should no longer exist");
}

#[tokio::test]
async fn test_user_idempotent() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let params = ModuleParams {
        resource_name: "idempuser".to_string(),
        args: HashMap::new(),
    };

    // Create user
    UserModule.apply(&ctx, &params).await.unwrap();

    // Check — should be Satisfied
    let status = UserModule.check(&ctx, &params).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Satisfied),
        "existing user with no changes should be Satisfied"
    );
}
