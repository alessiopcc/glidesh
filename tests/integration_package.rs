mod common;

use glidesh::config::types::ParamValue;
use glidesh::modules::package::PackageModule;
use glidesh::modules::{Module, ModuleParams, ModuleStatus};
use std::collections::HashMap;

#[tokio::test]
async fn test_package_install_and_check() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    // First ensure curl is removed so we can test installing it
    let _ = ssh.exec("apt-get remove -y curl 2>/dev/null").await;

    let params = ModuleParams {
        resource_name: "curl".to_string(),
        args: HashMap::new(), // state defaults to "present"
    };

    // Check should show Pending (not installed)
    let status = PackageModule.check(&ctx, &params).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Pending { .. }),
        "curl should need installing"
    );

    // Apply should install it
    let result = PackageModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);

    // Check again — should be Satisfied
    let status = PackageModule.check(&ctx, &params).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Satisfied),
        "curl should be installed now"
    );
}

#[tokio::test]
async fn test_package_remove() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    // Ensure curl is installed first
    let _ = ssh.exec("apt-get install -y curl 2>/dev/null").await;

    let mut args = HashMap::new();
    args.insert(
        "state".to_string(),
        ParamValue::String("absent".to_string()),
    );

    let params = ModuleParams {
        resource_name: "curl".to_string(),
        args,
    };

    // Apply removal
    let result = PackageModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);

    // Check for present — should be Pending (needs install)
    let check_params = ModuleParams {
        resource_name: "curl".to_string(),
        args: HashMap::new(),
    };
    let status = PackageModule.check(&ctx, &check_params).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Pending { .. }),
        "curl should be removed"
    );
}

#[tokio::test]
async fn test_package_idempotent() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let params = ModuleParams {
        resource_name: "curl".to_string(),
        args: HashMap::new(),
    };

    // Install twice — second apply should still succeed
    let result1 = PackageModule.apply(&ctx, &params).await.unwrap();
    assert!(result1.changed);

    let result2 = PackageModule.apply(&ctx, &params).await.unwrap();
    assert!(result2.changed); // apt-get install -y always "changes"

    // But check should return Satisfied
    let status = PackageModule.check(&ctx, &params).await.unwrap();
    assert!(matches!(status, ModuleStatus::Satisfied));
}

#[tokio::test]
async fn test_package_absent_already() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    // Ensure nonexistent-package is not installed
    let mut args = HashMap::new();
    args.insert(
        "state".to_string(),
        ParamValue::String("absent".to_string()),
    );

    let params = ModuleParams {
        resource_name: "nonexistent-package-xyz".to_string(),
        args,
    };

    let status = PackageModule.check(&ctx, &params).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Satisfied),
        "absent check on non-installed package should be Satisfied"
    );
}
