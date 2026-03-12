mod common;

use glidesh::modules::detect::{InitSystem, OsFamily, PkgManager};

#[tokio::test]
async fn test_detect_os_ubuntu() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;

    assert_eq!(os_info.family, OsFamily::Debian);
    assert_eq!(os_info.pkg_manager, PkgManager::Apt);
    assert_eq!(os_info.init_system, InitSystem::Systemd);
}

#[tokio::test]
async fn test_detect_os_has_fields() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;

    assert_eq!(os_info.id, "ubuntu");
    assert!(!os_info.version.is_empty(), "version should be populated");
}
