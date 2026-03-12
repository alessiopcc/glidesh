mod common;

use glidesh::config::types::ParamValue;
use glidesh::modules::file::FileModule;
use glidesh::modules::{Module, ModuleParams, ModuleStatus};
use std::collections::HashMap;

#[tokio::test]
async fn test_file_upload() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    // Create a temp local file
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"hello from glidesh").unwrap();

    let mut args = HashMap::new();
    args.insert(
        "src".to_string(),
        ParamValue::String(tmp.path().to_string_lossy().to_string()),
    );

    let params = ModuleParams {
        resource_name: "/root/glidesh-test-upload.txt".to_string(),
        args,
    };

    let result = FileModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);

    // Verify content on remote
    let output = ssh.exec("cat /root/glidesh-test-upload.txt").await.unwrap();
    assert_eq!(output.stdout, "hello from glidesh");
}

#[tokio::test]
async fn test_file_upload_idempotent() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"idempotent content").unwrap();

    let mut args = HashMap::new();
    args.insert(
        "src".to_string(),
        ParamValue::String(tmp.path().to_string_lossy().to_string()),
    );

    let params = ModuleParams {
        resource_name: "/root/glidesh-test-idemp.txt".to_string(),
        args,
    };

    // Upload once
    FileModule.apply(&ctx, &params).await.unwrap();

    // Check — should be Satisfied (same content)
    let status = FileModule.check(&ctx, &params).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Satisfied),
        "same content should be Satisfied"
    );
}

#[tokio::test]
async fn test_file_upload_permissions() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"perms test").unwrap();

    let mut args = HashMap::new();
    args.insert(
        "src".to_string(),
        ParamValue::String(tmp.path().to_string_lossy().to_string()),
    );
    args.insert("owner".to_string(), ParamValue::String("root".to_string()));
    args.insert("mode".to_string(), ParamValue::String("0644".to_string()));

    let params = ModuleParams {
        resource_name: "/root/glidesh-test-perms.txt".to_string(),
        args,
    };

    FileModule.apply(&ctx, &params).await.unwrap();

    // Verify permissions
    let stat = ssh
        .exec("stat -c '%a %U' /root/glidesh-test-perms.txt")
        .await
        .unwrap();
    let parts: Vec<&str> = stat.stdout.trim().split_whitespace().collect();
    assert_eq!(parts[0], "644", "mode should be 644");
    assert_eq!(parts[1], "root", "owner should be root");
}

#[tokio::test]
async fn test_file_template() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let mut vars = HashMap::new();
    vars.insert("greeting".to_string(), "world".to_string());
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"hello ${greeting}!").unwrap();

    let mut args = HashMap::new();
    args.insert(
        "src".to_string(),
        ParamValue::String(tmp.path().to_string_lossy().to_string()),
    );
    args.insert("template".to_string(), ParamValue::Bool(true));

    let params = ModuleParams {
        resource_name: "/root/glidesh-test-template.txt".to_string(),
        args,
    };

    FileModule.apply(&ctx, &params).await.unwrap();

    let output = ssh
        .exec("cat /root/glidesh-test-template.txt")
        .await
        .unwrap();
    assert_eq!(output.stdout, "hello world!");
}

#[tokio::test]
async fn test_file_fetch() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    // Create a file on remote
    ssh.exec("echo 'fetched content' > /root/glidesh-fetch-src.txt")
        .await
        .unwrap();

    let local_dest = tempfile::NamedTempFile::new().unwrap();
    let local_path = local_dest.path().to_string_lossy().to_string();

    let mut args = HashMap::new();
    args.insert(
        "src".to_string(),
        ParamValue::String("/root/glidesh-fetch-src.txt".to_string()),
    );
    args.insert("fetch".to_string(), ParamValue::Bool(true));

    let params = ModuleParams {
        resource_name: local_path.clone(),
        args,
    };

    let result = FileModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);

    // Verify local content
    let content = std::fs::read_to_string(&local_path).unwrap();
    assert_eq!(content.trim(), "fetched content");
}
