mod common;

use glidesh::config::types::{ParamValue, ResolvedRunAs, RunAsMethod};
use glidesh::modules::file::FileModule;
use glidesh::modules::shell::ShellModule;
use glidesh::modules::{Module, ModuleParams, ModuleStatus};
use std::collections::HashMap;

/// Escalate to root via passwordless sudo (the test container's `deploy` user has a
/// NOPASSWD sudoers entry).
fn run_as_root() -> ResolvedRunAs {
    ResolvedRunAs {
        user: "root".to_string(),
        method: RunAsMethod::Sudo,
        password: None,
    }
}

#[tokio::test]
async fn test_run_as_sudo_runs_as_root() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&ssh, &os_info, &vars, false, run_as_root());

    let params = ModuleParams {
        resource_name: "id -un".to_string(),
        args: HashMap::new(),
    };

    let result = ShellModule.apply(&ctx, &params).await.unwrap();
    assert!(
        result.output.contains("root"),
        "escalated command should run as root, got: {}",
        result.output
    );
}

#[tokio::test]
async fn test_no_run_as_is_login_user() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let params = ModuleParams {
        resource_name: "id -un".to_string(),
        args: HashMap::new(),
    };

    let result = ShellModule.apply(&ctx, &params).await.unwrap();
    assert!(
        result.output.contains("deploy"),
        "without run-as the command should run as the login user, got: {}",
        result.output
    );
}

#[tokio::test]
async fn test_run_as_file_upload_to_root_owned_dir() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"managed by glidesh").unwrap();

    let mut args = HashMap::new();
    args.insert(
        "src".to_string(),
        ParamValue::String(tmp.path().to_string_lossy().to_string()),
    );
    let params = ModuleParams {
        resource_name: "/etc/glidesh-runas-test.conf".to_string(),
        args,
    };

    // Validates the temp-upload + sudo-mv path for a destination the login user
    // cannot write directly.
    let result = FileModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);

    let root = container.ssh_session().await;
    let content = root.exec("cat /etc/glidesh-runas-test.conf").await.unwrap();
    assert_eq!(content.stdout, "managed by glidesh");
    let owner = root
        .exec("stat -c %U /etc/glidesh-runas-test.conf")
        .await
        .unwrap();
    assert_eq!(
        owner.stdout.trim(),
        "root",
        "escalated upload should be owned by the run-as user"
    );
}

#[tokio::test]
async fn test_no_run_as_cannot_write_root_owned_dir() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&deploy, &os_info, &vars, false);

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"should fail").unwrap();
    let mut args = HashMap::new();
    args.insert(
        "src".to_string(),
        ParamValue::String(tmp.path().to_string_lossy().to_string()),
    );
    let params = ModuleParams {
        resource_name: "/etc/glidesh-runas-denied.conf".to_string(),
        args,
    };

    let result = FileModule.apply(&ctx, &params).await;
    assert!(
        result.is_err(),
        "writing to a root-owned dir as deploy without run-as should fail"
    );
}

fn upload_params(src: &std::path::Path, dest: &str, extra: &[(&str, ParamValue)]) -> ModuleParams {
    let mut args = HashMap::new();
    args.insert(
        "src".to_string(),
        ParamValue::String(src.to_string_lossy().to_string()),
    );
    for (key, value) in extra {
        args.insert(key.to_string(), value.clone());
    }
    ModuleParams {
        resource_name: dest.to_string(),
        args,
    }
}

async fn stat(session: &glidesh::ssh::SshSession, path: &str) -> String {
    let out = session
        .exec(&format!("stat -c '%a %U:%G' {path}"))
        .await
        .unwrap();
    out.stdout.trim().to_string()
}

#[tokio::test]
async fn test_run_as_upload_of_a_new_file_gets_the_plain_upload_mode() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"new file").unwrap();
    let params = upload_params(tmp.path(), "/etc/glidesh-runas-new.conf", &[]);
    FileModule.apply(&ctx, &params).await.unwrap();

    // Staging goes through a `mktemp` file, which is 0600 and owned by the login
    // user's group; neither may leak into the result.
    let root = container.ssh_session().await;
    assert_eq!(
        stat(&root, "/etc/glidesh-runas-new.conf").await,
        "644 root:root"
    );
}

#[tokio::test]
async fn test_run_as_upload_keeps_the_replaced_files_owner_and_mode() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let root = container.ssh_session().await;
    root.exec(
        "printf old > /etc/glidesh-runas-kept.sh && \
         chown nobody:nogroup /etc/glidesh-runas-kept.sh && \
         chmod 0755 /etc/glidesh-runas-kept.sh",
    )
    .await
    .unwrap();

    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"new").unwrap();
    let params = upload_params(tmp.path(), "/etc/glidesh-runas-kept.sh", &[]);
    let result = FileModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);

    let content = root.exec("cat /etc/glidesh-runas-kept.sh").await.unwrap();
    assert_eq!(content.stdout, "new");
    assert_eq!(
        stat(&root, "/etc/glidesh-runas-kept.sh").await,
        "755 nobody:nogroup"
    );
}

#[tokio::test]
async fn test_run_as_upload_mode_still_wins_and_a_second_run_is_ok() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let root = container.ssh_session().await;
    root.exec(
        "printf old > /etc/glidesh-runas-mode.conf && chmod 0755 /etc/glidesh-runas-mode.conf",
    )
    .await
    .unwrap();

    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"new").unwrap();
    let params = upload_params(
        tmp.path(),
        "/etc/glidesh-runas-mode.conf",
        &[("mode", ParamValue::String("0640".to_string()))],
    );
    FileModule.apply(&ctx, &params).await.unwrap();
    assert_eq!(
        stat(&root, "/etc/glidesh-runas-mode.conf").await,
        "640 root:root"
    );

    let status = FileModule.check(&ctx, &params).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Satisfied),
        "a second run should be ok, got {status:?}"
    );
}

#[tokio::test]
async fn test_run_as_recursive_upload_gets_the_plain_upload_modes() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let src = tempfile::tempdir().unwrap();
    std::fs::create_dir(src.path().join("sub")).unwrap();
    std::fs::write(src.path().join("top.conf"), b"top").unwrap();
    std::fs::write(src.path().join("sub").join("inner.conf"), b"inner").unwrap();
    let params = upload_params(
        src.path(),
        "/etc/glidesh-runas-tree",
        &[("recurse", ParamValue::Bool(true))],
    );
    FileModule.apply(&ctx, &params).await.unwrap();

    let root = container.ssh_session().await;
    assert_eq!(
        stat(&root, "/etc/glidesh-runas-tree/top.conf").await,
        "644 root:root"
    );
    assert_eq!(
        stat(&root, "/etc/glidesh-runas-tree/sub/inner.conf").await,
        "644 root:root"
    );
    assert_eq!(
        stat(&root, "/etc/glidesh-runas-tree/sub").await,
        "755 root:root"
    );
}

#[tokio::test]
async fn test_run_as_upload_writes_through_a_symlink_destination() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let root = container.ssh_session().await;
    root.exec(
        "printf old > /etc/glidesh-runas-target.conf && \
         chmod 0640 /etc/glidesh-runas-target.conf && \
         ln -s /etc/glidesh-runas-target.conf /etc/glidesh-runas-link.conf",
    )
    .await
    .unwrap();

    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"new").unwrap();
    let params = upload_params(tmp.path(), "/etc/glidesh-runas-link.conf", &[]);
    FileModule.apply(&ctx, &params).await.unwrap();

    // Moving over the link would leave a regular file with the link's own 0777 mode.
    let link = root
        .exec("test -L /etc/glidesh-runas-link.conf && echo link")
        .await
        .unwrap();
    assert_eq!(link.stdout.trim(), "link");
    let content = root
        .exec("cat /etc/glidesh-runas-target.conf")
        .await
        .unwrap();
    assert_eq!(content.stdout, "new");
    assert_eq!(
        stat(&root, "/etc/glidesh-runas-target.conf").await,
        "640 root:root"
    );
}

#[tokio::test]
async fn test_run_as_upload_of_a_new_file_takes_a_setgid_directorys_group() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let root = container.ssh_session().await;
    root.exec("mkdir -p /srv/glidesh-shared && chgrp nogroup /srv/glidesh-shared && chmod 2755 /srv/glidesh-shared")
        .await
        .unwrap();

    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"shared").unwrap();
    let params = upload_params(tmp.path(), "/srv/glidesh-shared/new.conf", &[]);
    FileModule.apply(&ctx, &params).await.unwrap();

    assert_eq!(
        stat(&root, "/srv/glidesh-shared/new.conf").await,
        "644 root:nogroup"
    );
}

#[tokio::test]
async fn test_run_as_upload_keeps_a_setuid_mode_with_an_owner() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"#!/bin/sh\n").unwrap();
    let params = upload_params(
        tmp.path(),
        "/usr/local/bin/glidesh-setuid",
        &[
            ("owner", ParamValue::String("nobody".to_string())),
            ("mode", ParamValue::String("4755".to_string())),
        ],
    );
    FileModule.apply(&ctx, &params).await.unwrap();

    // chown clears setuid, so a mode applied before the owner would not survive.
    let root = container.ssh_session().await;
    assert_eq!(
        stat(&root, "/usr/local/bin/glidesh-setuid").await,
        "4755 nobody:root"
    );
}

#[tokio::test]
async fn test_run_as_upload_through_a_symlink_with_a_mode_is_ok_on_the_next_run() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let root = container.ssh_session().await;
    root.exec(
        "printf old > /etc/glidesh-runas-mtarget.conf && \
         ln -s /etc/glidesh-runas-mtarget.conf /etc/glidesh-runas-mlink.conf",
    )
    .await
    .unwrap();

    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"new").unwrap();
    let params = upload_params(
        tmp.path(),
        "/etc/glidesh-runas-mlink.conf",
        &[
            ("owner", ParamValue::String("nobody".to_string())),
            ("mode", ParamValue::String("0640".to_string())),
        ],
    );
    FileModule.apply(&ctx, &params).await.unwrap();
    assert_eq!(
        stat(&root, "/etc/glidesh-runas-mtarget.conf").await,
        "640 nobody:root"
    );

    // The check must read the target, not the link's own 777 root:root.
    let status = FileModule.check(&ctx, &params).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Satisfied),
        "a second run should be ok, got {status:?}"
    );
}

#[tokio::test]
async fn test_run_as_upload_refuses_a_symlink_another_user_planted() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let root = container.ssh_session().await;
    root.exec(
        "printf secret > /etc/glidesh-runas-victim && chmod 0600 /etc/glidesh-runas-victim && \
         mkdir -p /srv/glidesh-links && \
         ln -s /etc/glidesh-runas-victim /srv/glidesh-links/app.conf && \
         chown -h nobody /srv/glidesh-links/app.conf",
    )
    .await
    .unwrap();

    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"overwritten").unwrap();
    let params = upload_params(tmp.path(), "/srv/glidesh-links/app.conf", &[]);
    let err = FileModule.apply(&ctx, &params).await.unwrap_err();
    assert!(
        err.to_string().contains("nor the owner of its directory"),
        "unexpected error: {err}"
    );

    let content = root.exec("cat /etc/glidesh-runas-victim").await.unwrap();
    assert_eq!(content.stdout, "secret");
}

#[tokio::test]
async fn test_run_as_root_upload_without_a_mode_keeps_setuid() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let root = container.ssh_session().await;
    root.exec("printf old > /usr/local/bin/glidesh-suid && chmod 4755 /usr/local/bin/glidesh-suid")
        .await
        .unwrap();

    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"new").unwrap();
    let params = upload_params(tmp.path(), "/usr/local/bin/glidesh-suid", &[]);
    FileModule.apply(&ctx, &params).await.unwrap();

    // The kernel clears setuid/setgid on a write only by a writer without CAP_FSETID;
    // root has it, so the in-place write keeps them.
    assert_eq!(
        stat(&root, "/usr/local/bin/glidesh-suid").await,
        "4755 root:root"
    );
}

#[tokio::test]
async fn test_run_as_upload_into_a_service_users_directory_works() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let root = container.ssh_session().await;
    root.exec("mkdir -p /srv/glidesh-www && chown www-data:www-data /srv/glidesh-www")
        .await
        .unwrap();

    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    // Its owner is trusted, as the kernel trusts a directory's owner with its links.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"<h1>ok</h1>").unwrap();
    let params = upload_params(tmp.path(), "/srv/glidesh-www/index.html", &[]);
    FileModule.apply(&ctx, &params).await.unwrap();

    let content = root.exec("cat /srv/glidesh-www/index.html").await.unwrap();
    assert_eq!(content.stdout, "<h1>ok</h1>");
}

#[tokio::test]
async fn test_run_as_upload_into_a_group_writable_directory_is_refused() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let root = container.ssh_session().await;
    root.exec("mkdir -p /srv/glidesh-team && chgrp nogroup /srv/glidesh-team && chmod 0775 /srv/glidesh-team")
        .await
        .unwrap();

    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"x").unwrap();
    let params = upload_params(tmp.path(), "/srv/glidesh-team/app.conf", &[]);
    let err = FileModule.apply(&ctx, &params).await.unwrap_err();
    assert!(
        err.to_string()
            .contains("other users can write to /srv/glidesh-team"),
        "unexpected error: {err}"
    );
    let exists = root
        .exec("test -e /srv/glidesh-team/app.conf && echo yes")
        .await
        .unwrap();
    assert_eq!(exists.stdout.trim(), "");
}

#[tokio::test]
async fn test_run_as_recursive_owner_does_not_follow_a_link_in_the_tree() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let root = container.ssh_session().await;
    root.exec(
        "printf secret > /etc/glidesh-rvictim && \
         mkdir -p /srv/glidesh-rtree/uploads && chmod 0777 /srv/glidesh-rtree/uploads && \
         ln -s /etc/glidesh-rvictim /srv/glidesh-rtree/uploads/evil && \
         chown -h nobody /srv/glidesh-rtree/uploads/evil",
    )
    .await
    .unwrap();

    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let src = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("app.conf"), b"app").unwrap();
    let params = upload_params(
        src.path(),
        "/srv/glidesh-rtree",
        &[
            ("recurse", ParamValue::Bool(true)),
            ("owner", ParamValue::String("nobody".to_string())),
        ],
    );
    FileModule.apply(&ctx, &params).await.unwrap();

    // GNU `chown -R` already spares the target; busybox needs `-h`.
    assert_eq!(stat(&root, "/etc/glidesh-rvictim").await, "644 root:root");
    assert_eq!(
        stat(&root, "/srv/glidesh-rtree/app.conf").await,
        "644 nobody:root"
    );
}

#[tokio::test]
async fn test_run_as_diff_is_not_shown_for_a_path_others_could_redirect() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let root = container.ssh_session().await;
    root.exec(
        "mkdir -p /srv/glidesh-dteam && chgrp nogroup /srv/glidesh-dteam && \
         chmod 0775 /srv/glidesh-dteam && printf 'port=80\n' > /srv/glidesh-dteam/app.conf && \
         chmod 0644 /srv/glidesh-dteam/app.conf",
    )
    .await
    .unwrap();

    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let mut ctx = container.module_context_run_as(&deploy, &os_info, &vars, true, run_as_root());
    ctx.diff = true;

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"port=8080\n").unwrap();
    let params = upload_params(tmp.path(), "/srv/glidesh-dteam/app.conf", &[]);
    let status = FileModule.check(&ctx, &params).await.unwrap();
    let ModuleStatus::Pending { diff, .. } = status else {
        panic!("expected Pending, got {status:?}");
    };
    // A group member could swap the file for a link to a private one between the
    // world-readable check and the read.
    let diff = diff.unwrap();
    assert!(diff.contains("diff not shown"), "{diff}");
    assert!(diff.contains("other users can write"), "{diff}");
}

#[tokio::test]
async fn test_run_as_fetch_reads_a_root_only_file() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let root = container.ssh_session().await;
    root.exec("printf 'root only' > /etc/glidesh-fetch.conf && chmod 0600 /etc/glidesh-fetch.conf")
        .await
        .unwrap();

    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    // Staged in /tmp, where fs.protected_regular (enabled by default under systemd)
    // refuses root an O_CREAT open of the login user's file; staging must not depend on it.
    let local = tempfile::tempdir().unwrap();
    let dest = local.path().join("fetched.conf");
    let mut args = HashMap::new();
    args.insert(
        "src".to_string(),
        ParamValue::String("/etc/glidesh-fetch.conf".to_string()),
    );
    args.insert("fetch".to_string(), ParamValue::Bool(true));
    let params = ModuleParams {
        resource_name: dest.to_string_lossy().to_string(),
        args,
    };
    FileModule.apply(&ctx, &params).await.unwrap();
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "root only");

    let leftovers = deploy
        .exec("ls -d /tmp/glidesh.* 2>/dev/null | wc -l")
        .await
        .unwrap();
    assert_eq!(
        leftovers.stdout.trim(),
        "0",
        "the staging directory is removed"
    );
}

#[tokio::test]
async fn test_run_as_diff_of_a_root_owned_file_shows_its_content() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let root = container.ssh_session().await;
    root.exec("printf 'port=80\n' > /etc/glidesh-diff.conf && chmod 0644 /etc/glidesh-diff.conf")
        .await
        .unwrap();

    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let mut ctx = container.module_context_run_as(&deploy, &os_info, &vars, true, run_as_root());
    ctx.diff = true;

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"port=8080\n").unwrap();
    let params = upload_params(tmp.path(), "/etc/glidesh-diff.conf", &[]);
    let status = FileModule.check(&ctx, &params).await.unwrap();
    let ModuleStatus::Pending { diff, .. } = status else {
        panic!("expected Pending, got {status:?}");
    };
    let diff = diff.unwrap();
    assert!(diff.contains("-port=80"), "{diff}");
    assert!(diff.contains("+port=8080"), "{diff}");
}

#[tokio::test]
async fn test_run_as_recursive_owner_through_a_symlinked_destination() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let root = container.ssh_session().await;
    root.exec(
        "mkdir -p /srv/glidesh-realtree && ln -s /srv/glidesh-realtree /srv/glidesh-linktree",
    )
    .await
    .unwrap();

    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let src = tempfile::tempdir().unwrap();
    std::fs::create_dir(src.path().join("sub")).unwrap();
    std::fs::write(src.path().join("sub").join("app.conf"), b"app").unwrap();
    let params = upload_params(
        src.path(),
        "/srv/glidesh-linktree",
        &[
            ("recurse", ParamValue::Bool(true)),
            ("owner", ParamValue::String("nobody".to_string())),
        ],
    );
    FileModule.apply(&ctx, &params).await.unwrap();

    assert_eq!(
        stat(&root, "/srv/glidesh-realtree/sub/app.conf").await,
        "644 nobody:root"
    );
    let status = FileModule.check(&ctx, &params).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Satisfied),
        "a second run should be ok, got {status:?}"
    );
}

#[tokio::test]
async fn test_run_as_recursive_attributes_on_root_are_refused_before_uploading() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let src = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("glidesh-root.conf"), b"x").unwrap();
    let params = upload_params(
        src.path(),
        "/",
        &[
            ("recurse", ParamValue::Bool(true)),
            ("owner", ParamValue::String("nobody".to_string())),
        ],
    );
    let err = FileModule.apply(&ctx, &params).await.unwrap_err();
    assert!(
        err.to_string().contains("recursively on /"),
        "unexpected error: {err}"
    );

    let root = container.ssh_session().await;
    let owner = root.exec("stat -c %U /").await.unwrap();
    assert_eq!(owner.stdout.trim(), "root");
    let uploaded = root
        .exec("test -e /glidesh-root.conf && echo yes")
        .await
        .unwrap();
    assert_eq!(uploaded.stdout.trim(), "");
}

#[tokio::test]
async fn test_run_as_recursive_attributes_on_an_alias_of_root_are_refused() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let root = container.ssh_session().await;
    root.exec("ln -s / /srv/glidesh-rootlink").await.unwrap();

    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let src = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("glidesh-alias.conf"), b"x").unwrap();
    for dest in ["/tmp/..", "/.", "/srv/glidesh-rootlink"] {
        let params = upload_params(
            src.path(),
            dest,
            &[
                ("recurse", ParamValue::Bool(true)),
                ("mode", ParamValue::String("0700".to_string())),
            ],
        );
        let err = FileModule.apply(&ctx, &params).await.unwrap_err();
        assert!(
            err.to_string().contains("recursively on /"),
            "{dest}: unexpected error: {err}"
        );
    }

    assert_eq!(stat(&root, "/").await, "755 root:root");
    let uploaded = root
        .exec("test -e /glidesh-alias.conf && echo yes")
        .await
        .unwrap();
    assert_eq!(uploaded.stdout.trim(), "", "refused before uploading");
}

#[tokio::test]
async fn test_run_as_recursive_copy_to_root_without_attributes_works() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let deploy = container.ssh_session_as("deploy").await;
    let os_info = container.detect_os(&deploy).await;
    let vars = HashMap::new();
    let ctx = container.module_context_run_as(&deploy, &os_info, &vars, false, run_as_root());

    let src = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("glidesh-top.conf"), b"top").unwrap();
    let params = upload_params(src.path(), "/", &[("recurse", ParamValue::Bool(true))]);
    FileModule.apply(&ctx, &params).await.unwrap();

    let root = container.ssh_session().await;
    let content = root.exec("cat /glidesh-top.conf").await.unwrap();
    assert_eq!(content.stdout, "top");
}
