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

/// Adds a source whose suite does not exist, so `apt-get update` fails while the image's own
/// sources still work.
async fn break_one_source(ssh: &glidesh::ssh::connection::SshSession) {
    let added = ssh
        .exec(
            "mirror=$(grep -m1 '^deb ' /etc/apt/sources.list | awk '{print $2}') && \
             echo \"deb $mirror glidesh-no-such-suite main\" > /etc/apt/sources.list.d/broken.list",
        )
        .await
        .unwrap();
    assert_eq!(added.exit_code, 0, "{}", added.stderr);
}

#[tokio::test]
async fn a_failed_refresh_is_reported_when_the_install_fails() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);
    break_one_source(&ssh).await;

    let params = ModuleParams {
        resource_name: "glidesh-no-such-package".to_string(),
        args: HashMap::new(),
    };
    let error = PackageModule.apply(&ctx, &params).await.err().unwrap();
    let error = error.to_string();
    assert!(
        error.contains("the package index refresh failed (apt-get update -qq, exit 100)")
            && error.contains("glidesh-no-such-suite")
            && error.contains("Package operation failed for 'glidesh-no-such-package'"),
        "{error}"
    );
}

#[tokio::test]
async fn a_failed_refresh_does_not_stop_an_install_that_works() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);
    let _ = ssh.exec("apt-get remove -y curl 2>/dev/null").await;
    break_one_source(&ssh).await;

    let params = ModuleParams {
        resource_name: "curl".to_string(),
        args: HashMap::new(),
    };
    let result = PackageModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);
    assert!(
        result.stderr.starts_with(
            "warning: the package index refresh failed (apt-get update -qq, exit 100)"
        ),
        "{}",
        result.stderr
    );
    let status = PackageModule.check(&ctx, &params).await.unwrap();
    assert!(matches!(status, ModuleStatus::Satisfied));
}
