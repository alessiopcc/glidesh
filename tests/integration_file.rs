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
    let parts: Vec<&str> = stat.stdout.split_whitespace().collect();
    assert_eq!(parts[0], "644", "mode should be 644");
    assert_eq!(parts[1], "root", "owner should be root");
}

#[tokio::test]
async fn test_file_check_detects_permission_drift() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"drift test").unwrap();

    let mut args = HashMap::new();
    args.insert(
        "src".to_string(),
        ParamValue::String(tmp.path().to_string_lossy().to_string()),
    );
    args.insert("mode".to_string(), ParamValue::String("0600".to_string()));

    let params = ModuleParams {
        resource_name: "/root/glidesh-test-drift.txt".to_string(),
        args,
    };

    // Upload with mode 0600
    FileModule.apply(&ctx, &params).await.unwrap();

    // Check should be Satisfied
    let status = FileModule.check(&ctx, &params).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Satisfied),
        "expected Satisfied after apply, got {:?}",
        status
    );

    // Simulate drift: change mode on remote
    ssh.exec("chmod 644 /root/glidesh-test-drift.txt")
        .await
        .unwrap();

    // Check should now detect the drift
    let status = FileModule.check(&ctx, &params).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Pending { .. }),
        "expected Pending after permission drift, got {:?}",
        status
    );
}

#[tokio::test]
async fn test_file_check_detects_owner_drift() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    // Create a non-root user for the test
    ssh.exec("useradd -m testdrift 2>/dev/null || true")
        .await
        .unwrap();

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"owner drift test").unwrap();

    let mut args = HashMap::new();
    args.insert(
        "src".to_string(),
        ParamValue::String(tmp.path().to_string_lossy().to_string()),
    );
    args.insert("owner".to_string(), ParamValue::String("root".to_string()));

    let params = ModuleParams {
        resource_name: "/root/glidesh-test-owner-drift.txt".to_string(),
        args,
    };

    FileModule.apply(&ctx, &params).await.unwrap();

    let status = FileModule.check(&ctx, &params).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Satisfied),
        "expected Satisfied after apply, got {:?}",
        status
    );

    // Simulate drift: change owner
    ssh.exec("chown testdrift /root/glidesh-test-owner-drift.txt")
        .await
        .unwrap();

    let status = FileModule.check(&ctx, &params).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Pending { .. }),
        "expected Pending after owner drift, got {:?}",
        status
    );
}

#[tokio::test]
async fn test_file_apply_fixes_attrs_without_reupload() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"attrs only test").unwrap();

    let mut args = HashMap::new();
    args.insert(
        "src".to_string(),
        ParamValue::String(tmp.path().to_string_lossy().to_string()),
    );
    args.insert("mode".to_string(), ParamValue::String("0600".to_string()));

    let params = ModuleParams {
        resource_name: "/root/glidesh-test-attrs-only.txt".to_string(),
        args,
    };

    // Upload with mode 0600
    FileModule.apply(&ctx, &params).await.unwrap();

    // Drift the mode
    ssh.exec("chmod 644 /root/glidesh-test-attrs-only.txt")
        .await
        .unwrap();

    // Apply again — should fix attrs without re-uploading
    let result = FileModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);
    assert!(
        result.output.contains("attrs") && result.output.contains("content unchanged"),
        "expected attrs-only message, got: {}",
        result.output
    );

    // Verify mode is fixed
    let stat = ssh
        .exec("stat -c '%a' /root/glidesh-test-attrs-only.txt")
        .await
        .unwrap();
    assert_eq!(stat.stdout.trim(), "600", "mode should be restored to 600");
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

/// The field case: an upload without `template #true` shipped `${cuda-devices}` literally
/// and nothing said so. It still ships as-is, but the result now carries a warning — on a
/// preview too, which is where it is cheapest to catch.
#[tokio::test]
async fn an_untemplated_upload_warns_about_a_defined_variable() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let mut vars = HashMap::new();
    vars.insert("cuda-devices".to_string(), "0,1".to_string());

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"CUDA=${cuda-devices}\nHOME_DIR=${HOME}\n").unwrap();
    let params = |template: bool| {
        let mut args = HashMap::new();
        args.insert(
            "src".to_string(),
            ParamValue::String(tmp.path().to_string_lossy().to_string()),
        );
        args.insert("template".to_string(), ParamValue::Bool(template));
        ModuleParams {
            resource_name: "/root/glidesh-test-literal.env".to_string(),
            args,
        }
    };

    for dry_run in [true, false] {
        let ctx = container.module_context(&ssh, &os_info, &vars, dry_run);
        let result = FileModule.apply(&ctx, &params(false)).await.unwrap();
        assert!(
            result.stderr.contains("contains ${cuda-devices}")
                && result.stderr.contains("template #true"),
            "dry_run={dry_run}: {}",
            result.stderr
        );
        assert!(
            !result.stderr.contains("${HOME}"),
            "a shell variable is not glidesh's: {}",
            result.stderr
        );
    }

    let shipped = ssh
        .exec("cat /root/glidesh-test-literal.env")
        .await
        .unwrap();
    assert!(
        shipped.stdout.contains("CUDA=${cuda-devices}"),
        "the warning must not change what is uploaded: {}",
        shipped.stdout
    );

    let ctx = container.module_context(&ssh, &os_info, &vars, false);
    let templated = FileModule.apply(&ctx, &params(true)).await.unwrap();
    assert!(templated.stderr.is_empty(), "{}", templated.stderr);
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

/// A file whose remote content has drifted must preview as pending while the host stays
/// untouched. The executor derives "would change" from `check` precisely because a
/// dry-run `apply` reports `changed: false`.
#[tokio::test]
async fn test_file_dry_run_sees_drift_and_changes_nothing() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let dest = "/root/glidesh-dry-run-drift.txt";

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"desired content").unwrap();

    let mut args = HashMap::new();
    args.insert(
        "src".to_string(),
        ParamValue::String(tmp.path().to_string_lossy().to_string()),
    );
    let params = ModuleParams {
        resource_name: dest.to_string(),
        args,
    };

    let applied = container.module_context(&ssh, &os_info, &vars, false);
    FileModule.apply(&applied, &params).await.unwrap();
    assert!(matches!(
        FileModule.check(&applied, &params).await.unwrap(),
        ModuleStatus::Satisfied
    ));

    ssh.exec(&format!("echo 'drifted out of band' > {dest}"))
        .await
        .unwrap();

    let preview = container.module_context(&ssh, &os_info, &vars, true);
    match FileModule.check(&preview, &params).await.unwrap() {
        ModuleStatus::Pending { plan, .. } => {
            assert!(
                plan.contains(dest),
                "plan should name the file, got: {plan}"
            )
        }
        other => panic!("drifted file must be Pending, got {other:?}"),
    }

    let result = FileModule.apply(&preview, &params).await.unwrap();
    assert!(
        !result.changed,
        "a dry-run apply must not report having changed anything"
    );
    let after = ssh.exec(&format!("cat {dest}")).await.unwrap();
    assert_eq!(
        after.stdout.trim(),
        "drifted out of band",
        "dry-run must leave the remote file untouched"
    );
}
