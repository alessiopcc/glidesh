mod common;

use glidesh::config::types::ParamValue;
use glidesh::modules::disk::DiskModule;
use glidesh::modules::{Module, ModuleParams, ModuleStatus};
use std::collections::HashMap;

fn disk_params(device: &str, fs: &str, mount: &str, state: &str, force: bool) -> ModuleParams {
    let mut args = HashMap::new();
    args.insert("fs".to_string(), ParamValue::String(fs.to_string()));
    args.insert("mount".to_string(), ParamValue::String(mount.to_string()));
    args.insert("state".to_string(), ParamValue::String(state.to_string()));
    if force {
        args.insert("force".to_string(), ParamValue::Bool(true));
    }
    ModuleParams {
        resource_name: device.to_string(),
        args,
    }
}

#[tokio::test]
async fn test_disk_format_and_mount() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let device = common::setup_loopback(&ssh).await;

    let params = disk_params(&device, "ext4", "/mnt/test-disk", "mounted", false);

    let result = DiskModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);

    // Verify mounted
    let findmnt = ssh.exec("findmnt -n /mnt/test-disk").await.unwrap();
    assert!(
        !findmnt.stdout.trim().is_empty(),
        "should be mounted at /mnt/test-disk"
    );

    // Verify fstab entry
    let fstab = ssh.exec("grep '/mnt/test-disk' /etc/fstab").await.unwrap();
    assert!(
        !fstab.stdout.trim().is_empty(),
        "fstab should have entry for /mnt/test-disk"
    );

    // Cleanup
    let _ = ssh.exec("umount /mnt/test-disk 2>/dev/null").await;
    common::teardown_loopback(&ssh, &device).await;
}

#[tokio::test]
async fn test_disk_idempotent() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let device = common::setup_loopback(&ssh).await;

    let params = disk_params(&device, "ext4", "/mnt/test-idemp", "mounted", false);

    // Apply once
    DiskModule.apply(&ctx, &params).await.unwrap();

    // Check — should be Satisfied
    let status = DiskModule.check(&ctx, &params).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Satisfied),
        "already formatted and mounted should be Satisfied"
    );

    // Cleanup
    let _ = ssh.exec("umount /mnt/test-idemp 2>/dev/null").await;
    common::teardown_loopback(&ssh, &device).await;
}

#[tokio::test]
async fn test_disk_unmount() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let device = common::setup_loopback(&ssh).await;

    // First mount
    let mount_params = disk_params(&device, "ext4", "/mnt/test-unmount", "mounted", false);
    DiskModule.apply(&ctx, &mount_params).await.unwrap();

    // Now unmount
    let unmount_params = disk_params(&device, "ext4", "/mnt/test-unmount", "unmounted", false);
    let result = DiskModule.apply(&ctx, &unmount_params).await.unwrap();
    assert!(result.changed);

    // Verify unmounted
    let findmnt = ssh
        .exec("findmnt -n /mnt/test-unmount 2>/dev/null")
        .await
        .unwrap();
    assert!(findmnt.stdout.trim().is_empty(), "should be unmounted");

    // Cleanup
    common::teardown_loopback(&ssh, &device).await;
}

#[tokio::test]
async fn test_disk_refuse_reformat() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let device = common::setup_loopback(&ssh).await;

    // Format as ext4 first
    let _ = ssh.exec(&format!("mkfs.ext4 {}", device)).await;

    // Try to mount as xfs without force — should error
    let params = disk_params(&device, "xfs", "/mnt/test-refuse", "mounted", false);
    let result = DiskModule.check(&ctx, &params).await;
    assert!(result.is_err(), "should refuse to reformat without force");

    // Cleanup
    common::teardown_loopback(&ssh, &device).await;
}

#[tokio::test]
async fn test_disk_force_reformat() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let device = common::setup_loopback(&ssh).await;

    // Format as ext4 first
    let _ = ssh.exec(&format!("mkfs.ext4 {}", device)).await;

    // Force reformat as xfs
    let params = disk_params(&device, "xfs", "/mnt/test-force", "mounted", true);
    let result = DiskModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);

    // Verify it's xfs
    let blkid = ssh
        .exec(&format!("blkid -o value -s TYPE {}", device))
        .await
        .unwrap();
    assert_eq!(blkid.stdout.trim(), "xfs");

    // Cleanup
    let _ = ssh.exec("umount /mnt/test-force 2>/dev/null").await;
    common::teardown_loopback(&ssh, &device).await;
}

#[tokio::test]
async fn test_disk_absent() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let device = common::setup_loopback(&ssh).await;

    // First mount
    let mount_params = disk_params(&device, "ext4", "/mnt/test-absent", "mounted", false);
    DiskModule.apply(&ctx, &mount_params).await.unwrap();

    // Set absent
    let absent_params = disk_params(&device, "ext4", "/mnt/test-absent", "absent", false);
    let result = DiskModule.apply(&ctx, &absent_params).await.unwrap();
    assert!(result.changed);

    // Verify unmounted and fstab cleaned
    let findmnt = ssh
        .exec("findmnt -n /mnt/test-absent 2>/dev/null")
        .await
        .unwrap();
    assert!(findmnt.stdout.trim().is_empty());

    let fstab = ssh
        .exec("grep '/mnt/test-absent' /etc/fstab 2>/dev/null || echo ''")
        .await
        .unwrap();
    assert!(
        fstab.stdout.trim().is_empty(),
        "fstab should not have entry"
    );

    // Cleanup
    common::teardown_loopback(&ssh, &device).await;
}
