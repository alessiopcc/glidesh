mod common;

use glidesh::config::plan::{parse_plan, resolve_includes};
use glidesh::config::template::interpolate;
use glidesh::config::types::{LoopSource, PlanItem};
use glidesh::modules::shell::ShellModule;
use glidesh::modules::{Module, ModuleParams};
use std::collections::HashMap;

/// Test that register captures shell output into a variable.
#[tokio::test]
async fn test_register_captures_output() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let mut vars: HashMap<String, String> = HashMap::new();
    let ctx = container.module_context(&ssh, &os_info, &vars, false);

    // Simulate: shell "echo hello-world" register="output"
    let params = ModuleParams {
        resource_name: "echo hello-world".to_string(),
        args: HashMap::new(),
    };

    let result = ShellModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);

    // Register: store trimmed output
    let registered = result.output.trim().to_string();
    vars.insert("output".to_string(), registered);

    assert_eq!(vars.get("output").unwrap(), "hello-world");
}

/// Test register + loop: capture multiline output, then iterate with ${item}.
#[tokio::test]
async fn test_register_then_loop() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let mut vars: HashMap<String, String> = HashMap::new();

    // Step 1: register multiline output
    {
        let ctx = container.module_context(&ssh, &os_info, &vars, false);
        let params = ModuleParams {
            resource_name: "printf 'alpha\\nbeta\\ngamma'".to_string(),
            args: HashMap::new(),
        };
        let result = ShellModule.apply(&ctx, &params).await.unwrap();
        vars.insert("items".to_string(), result.output.trim().to_string());
    }

    // Simulate loop: split by newlines, filter empty, run shell "echo ${@item}" per item
    let items: Vec<String> = vars
        .get("items")
        .unwrap()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    assert_eq!(items, vec!["alpha", "beta", "gamma"]);

    let mut outputs = Vec::new();
    for item in &items {
        vars.insert("@item".to_string(), item.clone());
        let cmd = interpolate("echo ${@item}", &vars).unwrap();
        let ctx = container.module_context(&ssh, &os_info, &vars, false);
        let params = ModuleParams {
            resource_name: cmd,
            args: HashMap::new(),
        };
        let result = ShellModule.apply(&ctx, &params).await.unwrap();
        outputs.push(result.output.trim().to_string());
    }
    vars.remove("item");

    assert_eq!(outputs, vec!["alpha", "beta", "gamma"]);
    assert!(!vars.contains_key("item"));
}

/// Test that empty registered output yields zero loop iterations.
#[tokio::test]
async fn test_loop_empty_register() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;
    let mut vars: HashMap<String, String> = HashMap::new();

    // Register output that's only whitespace/newlines
    {
        let ctx = container.module_context(&ssh, &os_info, &vars, false);
        let params = ModuleParams {
            resource_name: "echo ''".to_string(),
            args: HashMap::new(),
        };
        let result = ShellModule.apply(&ctx, &params).await.unwrap();
        vars.insert("empty".to_string(), result.output.trim().to_string());
    }

    let items: Vec<String> = vars
        .get("empty")
        .unwrap()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    assert!(items.is_empty(), "empty output should yield no loop items");
}

/// Test include resolution with actual files on disk.
#[test]
fn test_include_resolves_relative_to_plan() {
    use std::io::Write;
    let dir = std::env::temp_dir().join("glidesh_integ_include");
    let sub = dir.join("sub");
    let _ = std::fs::create_dir_all(&sub);

    // sub/base.kdl
    let base = r#"
plan "base" {
    step "Base setup" {
        shell "echo base"
    }
}
"#;
    std::fs::File::create(sub.join("base.kdl"))
        .unwrap()
        .write_all(base.as_bytes())
        .unwrap();

    // main.kdl includes sub/base.kdl
    let main = r#"
plan "main" {
    step "Init" {
        shell "echo init"
    }
    include "sub/base.kdl"
    step "Finish" {
        shell "echo done"
    }
}
"#;
    let mut plan = parse_plan(main).unwrap();
    resolve_includes(&mut plan, &dir).unwrap();

    let steps = plan.steps();
    assert_eq!(steps.len(), 3);
    assert_eq!(steps[0].name, "Init");
    assert_eq!(steps[1].name, "Base setup");
    assert_eq!(steps[2].name, "Finish");

    // All items should now be Step (no Include left)
    assert!(plan.items.iter().all(|i| matches!(i, PlanItem::Step(_))));

    let _ = std::fs::remove_dir_all(&dir);
}

/// Test parsing a full plan with register + loop + include all together.
#[test]
fn test_parse_full_plan_with_all_features() {
    let input = r#"
plan "full" {
    vars {
        fs-type "ext4"
    }

    step "Discover disks" {
        shell "lsblk -dn -o NAME" register="disks"
    }

    step "Format each disk" loop="${disks}" {
        disk "${@item}" fs="${fs-type}"
    }

    include "monitoring.kdl"
}
"#;
    let fp = parse_plan(input).unwrap();

    // 2 steps + 1 include
    assert_eq!(fp.items.len(), 3);
    assert_eq!(fp.steps().len(), 2);

    // register
    assert_eq!(fp.steps()[0].tasks[0].register, Some("disks".to_string()));

    // loop
    assert_eq!(
        fp.steps()[1].loop_source,
        Some(LoopSource::Variable("disks".to_string()))
    );

    // include
    assert!(matches!(&fp.items[2], PlanItem::Include(i) if i.path == "monitoring.kdl"));
}

/// Test that host-level vars override group vars and flow through to module execution.
#[tokio::test]
async fn test_host_level_vars_override_and_interpolate() {
    skip_unless_integration!();

    use glidesh::config::inventory::parse_inventory;
    use glidesh::config::template::interpolate;
    use glidesh::modules::shell::ShellModule;

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let os_info = container.detect_os(&ssh).await;

    let inv_input = r#"
vars {
    greeting "global-hello"
    shared "from-global"
}
group "app" {
    vars {
        shared "from-group"
        group-only "group-val"
    }
    host "test-host" "127.0.0.1" user="root" {
        vars {
            shared "from-host"
            host-only "host-val"
        }
    }
}
"#;
    let inv = parse_inventory(inv_input).unwrap();
    let resolved = inv.resolve_targets(Some("app"));
    assert_eq!(resolved.len(), 1);

    let host = &resolved[0];
    assert_eq!(host.vars.get("shared").unwrap(), "from-host");
    assert_eq!(host.vars.get("greeting").unwrap(), "global-hello");
    assert_eq!(host.vars.get("group-only").unwrap(), "group-val");
    assert_eq!(host.vars.get("host-only").unwrap(), "host-val");

    let cmd_template = "echo ${shared}-${greeting}-${host-only}-${group-only}";
    let cmd = interpolate(cmd_template, &host.vars).unwrap();
    assert_eq!(cmd, "echo from-host-global-hello-host-val-group-val");

    let ctx = container.module_context(&ssh, &os_info, &host.vars, false);
    let params = ModuleParams {
        resource_name: cmd,
        args: HashMap::new(),
    };
    let result = ShellModule.apply(&ctx, &params).await.unwrap();
    assert!(result.changed);
    assert_eq!(
        result.output.trim(),
        "from-host-global-hello-host-val-group-val"
    );
}

/// An included plan carries its own files and vars: its `src` and `vars-file` resolve from
/// its directory, and its variables reach its templates.
#[tokio::test(flavor = "multi_thread")]
async fn an_included_plan_uploads_from_its_own_directory_with_its_own_vars() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    let key = container.write_key_file(dir.path());
    let role = dir.path().join("roles").join("web");
    std::fs::create_dir_all(&role).unwrap();

    std::fs::write(dir.path().join("app.conf"), "top-level copy\n").unwrap();
    std::fs::write(role.join("app.conf"), "port=${port} name=${name}\n").unwrap();
    std::fs::write(role.join("defaults.kdl"), "port \"8080\"\n").unwrap();
    std::fs::write(
        role.join("plan.kdl"),
        r#"
plan "web" {
    vars-file "defaults.kdl"
    vars {
        name "web"
    }
    step "Config" {
        file "/root/include-app.conf" src="app.conf" template=#true
    }
}
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("plan.kdl"),
        "plan \"main\" {\n    include \"roles/web/plan.kdl\"\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("inventory.kdl"),
        format!(
            "host \"target\" \"127.0.0.1\" user=\"root\" port={} {{\n    vars {{\n        ssh-key {:?}\n    }}\n}}\n",
            container.port,
            key.to_string_lossy()
        ),
    )
    .unwrap();

    let home = tempfile::tempdir().unwrap();
    assert_cmd::Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir.path())
        .env("HOME", home.path())
        .args(["run", "-i", "inventory.kdl", "-p", "plan.kdl"])
        .args(["--no-tui", "--no-host-key-check"])
        .assert()
        .success();

    let uploaded = ssh.exec("cat /root/include-app.conf").await.unwrap().stdout;
    assert_eq!(uploaded, "port=8080 name=web\n");
}

/// Output cut at the limit has lost its middle: registering it would hand later steps a
/// wrong value, so the task fails instead.
#[tokio::test(flavor = "multi_thread")]
async fn register_refuses_output_cut_at_the_limit() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    let key = container.write_key_file(dir.path());
    std::fs::write(
        dir.path().join("plan.kdl"),
        r#"
plan "big" {
    step "Capture" {
        shell "seq 1 3000000" register="lines"
    }
    step "After" {
        shell "touch /root/register-cut-after"
    }
}
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("inventory.kdl"),
        format!(
            "host \"target\" \"127.0.0.1\" user=\"root\" port={} {{\n    vars {{\n        ssh-key {:?}\n    }}\n}}\n",
            container.port,
            key.to_string_lossy()
        ),
    )
    .unwrap();

    let home = tempfile::tempdir().unwrap();
    let out = assert_cmd::Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir.path())
        .env("HOME", home.path())
        .args(["run", "-i", "inventory.kdl", "-p", "plan.kdl"])
        .args(["--no-tui", "--no-host-key-check"])
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("too long for register="), "{text}");
    let after = ssh.exec("test -e /root/register-cut-after").await.unwrap();
    assert_ne!(
        after.exit_code, 0,
        "the host must stop at the failed register"
    );
}

/// A plugin that runs a command over the limit is told its `stdout` was cut, and a plugin
/// that reports its own output as cut has it refused by `register=`.
///
/// The plugin reads the 8 MiB `exec` response with `head -n 1`: `read` goes byte by byte on a
/// pipe, and glidesh sends nothing more until the plugin answers, so `head` cannot swallow a
/// later message.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_learns_its_exec_output_was_cut_and_can_refuse_register() {
    use std::os::unix::fs::PermissionsExt;
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    let key = container.write_key_file(dir.path());
    let modules = dir.path().join("modules");
    std::fs::create_dir_all(&modules).unwrap();
    let plugin = modules.join("glidesh-module-flood");
    std::fs::write(
        &plugin,
        r#"#!/bin/sh
while IFS= read -r line; do
    case "$line" in
    *'"method":"describe"'*)
        echo '{"name":"test/flood","version":"1.0.0","protocol_version":1}' ;;
    *'"method":"check"'*)
        echo '{"status":"pending","plan":"flood"}' ;;
    *'"method":"apply"'*)
        echo '{"ssh":"exec","command":"yes x | head -c 9000000"}'
        result=$(head -n 1)
        case "$result" in
        *'"stdout_cut":true'*) cut=true ;;
        *) cut=false ;;
        esac
        echo "{\"changed\":true,\"output\":\"x\",\"stderr\":\"\",\"exit_code\":0,\"output_cut\":$cut}" ;;
    *'"method":"shutdown"'*)
        exit 0 ;;
    esac
done
"#,
    )
    .unwrap();
    std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        dir.path().join("plan.kdl"),
        r#"
plan "plugin" {
    step "Flood" {
        external "test/flood" "x" register="out"
    }
    step "After" {
        shell "touch /root/plugin-cut-after"
    }
}
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("inventory.kdl"),
        format!(
            "host \"target\" \"127.0.0.1\" user=\"root\" port={} {{\n    vars {{\n        ssh-key {:?}\n    }}\n}}\n",
            container.port,
            key.to_string_lossy()
        ),
    )
    .unwrap();

    let home = tempfile::tempdir().unwrap();
    let out = assert_cmd::Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir.path())
        .env("HOME", home.path())
        .args(["run", "-i", "inventory.kdl", "-p", "plan.kdl"])
        .args(["--no-tui", "--no-host-key-check"])
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("too long for register="), "{text}");
    let after = ssh.exec("test -e /root/plugin-cut-after").await.unwrap();
    assert_ne!(
        after.exit_code, 0,
        "the host must stop at the refused register"
    );
}

/// Plan vars are defaults: a group's value reaches the host over the plan's, a name only the
/// plan sets keeps its value, and a prompt's answer beats the inventory. The run says which
/// plan values the inventory overrides.
#[tokio::test(flavor = "multi_thread")]
async fn the_inventory_overrides_a_plan_var_and_an_answer_overrides_the_inventory() {
    skip_unless_integration!();

    let container = common::TestContainer::start();
    let ssh = container.ssh_session().await;
    let dir = tempfile::tempdir().unwrap();
    let key = container.write_key_file(dir.path());
    std::fs::write(
        dir.path().join("plan.kdl"),
        r#"
plan "enroll" {
    vars-prompt { release "Release" }
    vars {
        customer "default"
        port "8080"
    }
    step "Record" {
        shell "echo ${customer} ${port} ${release} > /root/precedence.txt"
    }
}
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("inventory.kdl"),
        format!(
            "group \"web\" {{\n    vars {{\n        customer \"acme\"\n        release \"pinned\"\n    }}\n    host \"target\" \"127.0.0.1\" user=\"root\" port={} {{\n        vars {{\n            ssh-key {:?}\n        }}\n    }}\n}}\n",
            container.port,
            key.to_string_lossy()
        ),
    )
    .unwrap();

    let home = tempfile::tempdir().unwrap();
    let out = assert_cmd::Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir.path())
        .env("HOME", home.path())
        .args([
            "run",
            "-i",
            "inventory.kdl",
            "-p",
            "plan.kdl",
            "--var",
            "release=v3",
        ])
        .args(["--no-tui", "--no-host-key-check"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("plan 'enroll' sets 'customer' as a default, which group 'web' overrides"),
        "{stderr}"
    );
    assert!(
        stderr.contains("plan 'enroll' asks for 'release', which group 'web' also sets: an answer"),
        "{stderr}"
    );

    let recorded = ssh.exec("cat /root/precedence.txt").await.unwrap().stdout;
    assert_eq!(recorded, "acme 8080 v3\n");
}
