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

/// A shell script templated for one value keeps its own `${…}`, written `$${…}`; the
/// second run sees the same content, so it changes nothing.
#[tokio::test]
async fn test_file_template_keeps_an_escaped_reference_literal() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::from([("app".to_string(), "api".to_string())]);
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        tmp.path(),
        b"#!/bin/sh\n# runs ${app} from $${HOME}\ncd \"$${HOME}/${app}\" && exec ./${app} \"$${1:-serve}\"\n",
    )
    .unwrap();
    let mut args = HashMap::new();
    args.insert(
        "src".to_string(),
        ParamValue::String(tmp.path().to_string_lossy().to_string()),
    );
    args.insert("template".to_string(), ParamValue::Bool(true));
    let params = ModuleParams {
        resource_name: "/root/glidesh-escaped.sh".to_string(),
        args,
    };

    FileModule.apply(&ctx, &params).await.unwrap();
    let output = ssh.exec("cat /root/glidesh-escaped.sh").await.unwrap();
    assert_eq!(
        output.stdout,
        "#!/bin/sh\n# runs api from ${HOME}\ncd \"${HOME}/api\" && exec ./api \"${1:-serve}\"\n"
    );
    let status = FileModule.check(&ctx, &params).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Satisfied),
        "a second run is ok, got {status:?}"
    );
}

/// A template reading a name nothing defines fails naming the file and the line.
#[tokio::test]
async fn test_file_template_names_the_line_of_an_undefined_variable() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("run.sh");
    std::fs::write(&src, "#!/bin/sh\necho ${HOME}\n").unwrap();
    let mut args = HashMap::new();
    args.insert(
        "src".to_string(),
        ParamValue::String(src.to_string_lossy().to_string()),
    );
    args.insert("template".to_string(), ParamValue::Bool(true));
    let params = ModuleParams {
        resource_name: "/root/glidesh-undefined.sh".to_string(),
        args,
    };

    let err = FileModule
        .apply(&ctx, &params)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("line 2: undefined variable HOME"), "{err}");
    assert!(err.contains("write $${HOME}"), "{err}");
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

    // Its own file: templating the one above would fail on `${HOME}`, which glidesh does not
    // define.
    std::fs::write(tmp.path(), b"CUDA=${cuda-devices}\n").unwrap();
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

#[tokio::test]
async fn test_file_fetch_to_a_relative_path_lands_beside_the_plan() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    // The test runs from the crate root; the plan lives elsewhere.
    let plan_dir = tempfile::tempdir().unwrap();
    let mut ctx = container.module_context(&ssh, &os_info, &vars, false);
    ctx.plan_base_dir = plan_dir.path();

    ssh.exec("printf 'db dump' > /root/glidesh-fetch-rel.sql")
        .await
        .unwrap();
    let mut args = HashMap::new();
    args.insert(
        "src".to_string(),
        ParamValue::String("/root/glidesh-fetch-rel.sql".to_string()),
    );
    args.insert("fetch".to_string(), ParamValue::Bool(true));
    let params = ModuleParams {
        resource_name: "backups/db.sql".to_string(),
        args,
    };

    let expected = plan_dir.path().join("backups/db.sql");
    let status = FileModule.check(&ctx, &params).await.unwrap();
    assert!(
        format!("{status:?}").contains(&expected.display().to_string()),
        "the preview names the absolute path: {status:?}"
    );
    let result = FileModule.apply(&ctx, &params).await.unwrap();
    assert!(
        result.output.contains(&expected.display().to_string()),
        "the output names the absolute path: {}",
        result.output
    );
    assert_eq!(std::fs::read_to_string(&expected).unwrap(), "db dump");
    assert!(
        !std::path::Path::new("backups/db.sql").exists(),
        "nothing is written relative to the working directory"
    );
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

fn tree_params(src: &std::path::Path, dest: &str, extra: Vec<(&str, ParamValue)>) -> ModuleParams {
    let mut args = HashMap::new();
    args.insert(
        "src".to_string(),
        ParamValue::String(src.to_string_lossy().to_string()),
    );
    args.insert("recurse".to_string(), ParamValue::Bool(true));
    for (key, value) in extra {
        args.insert(key.to_string(), value);
    }
    ModuleParams {
        resource_name: dest.to_string(),
        args,
    }
}

fn source_tree(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, content) in files {
        let path = dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    dir
}

async fn exists(ssh: &glidesh::ssh::SshSession, path: &str) -> bool {
    ssh.exec(&format!("test -e {path} && echo yes"))
        .await
        .unwrap()
        .stdout
        .trim()
        == "yes"
}

/// A stray file and directory keep a recursive upload pending with `prune`, named in the
/// plan, and are removed by the apply; without `prune` they stay and the upload is `ok`.
#[tokio::test]
async fn prune_reports_then_removes_what_the_source_lacks() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);
    let src = source_tree(&[("a.conf", "a"), ("lib/x", "x")]);
    let dest = "/srv/glidesh-prune";
    let plain = tree_params(src.path(), dest, vec![]);
    FileModule.apply(&ctx, &plain).await.unwrap();
    ssh.exec("mkdir -p /srv/glidesh-prune/old/deep && touch /srv/glidesh-prune/old/deep/f /srv/glidesh-prune/lib/stale")
        .await
        .unwrap();

    let status = FileModule.check(&ctx, &plain).await.unwrap();
    assert!(
        matches!(status, ModuleStatus::Satisfied),
        "without prune strays are left alone: {status:?}"
    );

    let prune = tree_params(src.path(), dest, vec![("prune", ParamValue::Bool(true))]);
    let ModuleStatus::Pending { plan, .. } = FileModule.check(&ctx, &prune).await.unwrap() else {
        panic!("strays should be pending");
    };
    assert!(plan.contains("remove 4"), "{plan}");
    assert!(plan.contains("/srv/glidesh-prune/lib/stale"), "{plan}");

    let result = FileModule.apply(&ctx, &prune).await.unwrap();
    assert!(result.output.contains("4 removed"), "{}", result.output);
    for gone in ["old", "lib/stale"] {
        assert!(!exists(&ssh, &format!("{dest}/{gone}")).await, "{gone}");
    }
    for kept in ["a.conf", "lib/x"] {
        assert!(exists(&ssh, &format!("{dest}/{kept}")).await, "{kept}");
    }
    let status = FileModule.check(&ctx, &prune).await.unwrap();
    assert!(matches!(status, ModuleStatus::Satisfied), "{status:?}");
}

/// An excluded path is neither uploaded nor pruned, on either side.
#[tokio::test]
async fn an_excluded_path_is_neither_uploaded_nor_pruned() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);
    let src = source_tree(&[("app.conf", "a"), (".git/HEAD", "ref"), ("debug.log", "l")]);
    let dest = "/srv/glidesh-exclude";
    ssh.exec("mkdir -p /srv/glidesh-exclude/data && touch /srv/glidesh-exclude/data/keep.log /srv/glidesh-exclude/stray")
        .await
        .unwrap();
    let params = tree_params(
        src.path(),
        dest,
        vec![
            ("prune", ParamValue::Bool(true)),
            (
                "exclude",
                ParamValue::List(vec![".git".to_string(), "*.log".to_string()]),
            ),
        ],
    );
    FileModule.apply(&ctx, &params).await.unwrap();

    assert!(exists(&ssh, &format!("{dest}/app.conf")).await);
    assert!(!exists(&ssh, &format!("{dest}/.git")).await, "not uploaded");
    assert!(
        !exists(&ssh, &format!("{dest}/debug.log")).await,
        "not uploaded"
    );
    assert!(
        exists(&ssh, &format!("{dest}/data/keep.log")).await,
        "an excluded host file and its directory stay"
    );
    assert!(!exists(&ssh, &format!("{dest}/stray")).await, "pruned");
    let status = FileModule.check(&ctx, &params).await.unwrap();
    assert!(matches!(status, ModuleStatus::Satisfied), "{status:?}");
}

/// `dir-mode` and `file-mode` set each kind, `mode` the other; only the source's paths
/// change, and an empty source directory is created.
#[tokio::test]
async fn dir_mode_and_file_mode_apply_to_the_sources_paths_only() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);
    let src = source_tree(&[("bin/run", "#!/bin/sh"), ("app.conf", "a")]);
    std::fs::create_dir_all(src.path().join("empty")).unwrap();
    let dest = "/srv/glidesh-modes";
    ssh.exec("mkdir -p /srv/glidesh-modes && touch /srv/glidesh-modes/local.conf && chmod 0600 /srv/glidesh-modes/local.conf")
        .await
        .unwrap();
    let params = tree_params(
        src.path(),
        dest,
        vec![
            ("mode", ParamValue::String("0640".to_string())),
            ("dir-mode", ParamValue::String("0750".to_string())),
        ],
    );
    FileModule.apply(&ctx, &params).await.unwrap();

    let modes = ssh
        .exec("cd /srv/glidesh-modes && stat -c '%n %a' . bin empty bin/run app.conf local.conf")
        .await
        .unwrap()
        .stdout;
    assert_eq!(
        modes,
        ". 750\nbin 750\nempty 750\nbin/run 640\napp.conf 640\nlocal.conf 600\n"
    );
    let status = FileModule.check(&ctx, &params).await.unwrap();
    assert!(matches!(status, ModuleStatus::Satisfied), "{status:?}");

    ssh.exec("chmod 0755 /srv/glidesh-modes/bin").await.unwrap();
    let ModuleStatus::Pending { plan, .. } = FileModule.check(&ctx, &params).await.unwrap() else {
        panic!("a directory's mode drifted");
    };
    assert!(plan.contains("1 attrs"), "{plan}");
}

/// `prune` deletes, so it refuses a destination that is a symlink.
#[tokio::test]
async fn prune_refuses_a_symlinked_destination() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);
    ssh.exec("mkdir -p /srv/glidesh-real && touch /srv/glidesh-real/stray && ln -s /srv/glidesh-real /srv/glidesh-link")
        .await
        .unwrap();
    let src = source_tree(&[("a", "a")]);
    let params = tree_params(
        src.path(),
        "/srv/glidesh-link",
        vec![("prune", ParamValue::Bool(true))],
    );
    let err = FileModule.check(&ctx, &params).await.unwrap_err();
    assert!(err.to_string().contains("is a symlink"), "{err}");
    assert!(exists(&ssh, "/srv/glidesh-real/stray").await);
}

/// A preview names what prune would remove and removes nothing; `--diff` lists each path; a
/// symlink under the destination goes as a link, what it points to stays.
#[tokio::test]
async fn prune_previews_then_removes_a_link_as_a_link() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    ssh.exec("mkdir -p /srv/glidesh-links && echo keep > /root/glidesh-target && ln -s /root/glidesh-target /srv/glidesh-links/link && mkdir /srv/glidesh-links/linked-dir-target && ln -s /root /srv/glidesh-links/rootlink")
        .await
        .unwrap();
    let src = source_tree(&[("a", "a")]);
    let params = tree_params(
        src.path(),
        "/srv/glidesh-links",
        vec![("prune", ParamValue::Bool(true))],
    );

    let mut preview = container.module_context(&ssh, &os_info, &vars, true);
    preview.diff = true;
    let ModuleStatus::Pending { diff, .. } = FileModule.check(&preview, &params).await.unwrap()
    else {
        panic!("strays should be pending");
    };
    let diff = diff.unwrap();
    for path in ["link", "rootlink", "linked-dir-target"] {
        assert!(
            diff.contains(&format!("remove /srv/glidesh-links/{path}")),
            "{diff}"
        );
    }
    let dry = FileModule.apply(&preview, &params).await.unwrap();
    assert!(dry.output.contains("and remove 3"), "{}", dry.output);
    assert!(
        exists(&ssh, "/srv/glidesh-links/link").await,
        "a preview removes nothing"
    );

    let ctx = container.module_context(&ssh, &os_info, &vars, false);
    FileModule.apply(&ctx, &params).await.unwrap();
    for gone in ["link", "rootlink", "linked-dir-target"] {
        let path = format!("/srv/glidesh-links/{gone}");
        let left = ssh
            .exec(&format!("test -e {path} || test -L {path} && echo yes"))
            .await
            .unwrap();
        assert_eq!(left.stdout.trim(), "", "{gone}");
    }
    let target = ssh.exec("cat /root/glidesh-target").await.unwrap();
    assert_eq!(target.stdout, "keep\n", "the link's target stays");
    assert!(exists(&ssh, "/root").await);
}

/// With nothing to upload, prune would empty the destination: refused.
#[tokio::test]
async fn prune_refuses_an_empty_source() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);
    ssh.exec("mkdir -p /srv/glidesh-empty && touch /srv/glidesh-empty/data")
        .await
        .unwrap();
    let src = source_tree(&[("only.log", "x")]);
    let params = tree_params(
        src.path(),
        "/srv/glidesh-empty",
        vec![
            ("prune", ParamValue::Bool(true)),
            ("exclude", ParamValue::List(vec!["*.log".to_string()])),
        ],
    );
    let err = FileModule.apply(&ctx, &params).await.unwrap_err();
    assert!(err.to_string().contains("nothing to upload"), "{err}");
    assert!(exists(&ssh, "/srv/glidesh-empty/data").await);
}

/// A host entry of the other kind than the source's is in the way: without `prune` the
/// task says so and changes nothing; with it the entry is replaced, and a second run is `ok`.
#[tokio::test]
async fn an_entry_of_the_other_kind_is_replaced_only_with_prune() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let vars = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);
    ssh.exec("mkdir -p /srv/glidesh-kinds/conf.d && touch /srv/glidesh-kinds/conf.d/old /srv/glidesh-kinds/cache")
        .await
        .unwrap();
    let src = source_tree(&[("conf.d", "a file now")]);
    std::fs::create_dir_all(src.path().join("cache")).unwrap();
    let dest = "/srv/glidesh-kinds";

    let plain = tree_params(src.path(), dest, vec![]);
    let ModuleStatus::Pending { plan, .. } = FileModule.check(&ctx, &plain).await.unwrap() else {
        panic!("entries of the other kind should be pending");
    };
    assert!(plan.contains("2 of the other kind"), "{plan}");
    let err = FileModule.apply(&ctx, &plain).await.unwrap_err();
    assert!(err.to_string().contains("set prune=#true"), "{err}");
    assert!(
        exists(&ssh, "/srv/glidesh-kinds/conf.d/old").await,
        "nothing changed"
    );

    let prune = tree_params(src.path(), dest, vec![("prune", ParamValue::Bool(true))]);
    FileModule.apply(&ctx, &prune).await.unwrap();
    let kinds = ssh
        .exec("cd /srv/glidesh-kinds && stat -c '%n %F' cache conf.d")
        .await
        .unwrap()
        .stdout;
    assert_eq!(kinds, "cache directory\nconf.d regular file\n");
    let status = FileModule.check(&ctx, &prune).await.unwrap();
    assert!(matches!(status, ModuleStatus::Satisfied), "{status:?}");
}
