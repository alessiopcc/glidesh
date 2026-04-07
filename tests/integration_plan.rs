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

    // Simulate loop: split by newlines, filter empty, run shell "echo ${item}" per item
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
        vars.insert("item".to_string(), item.clone());
        let cmd = interpolate("echo ${item}", &vars).unwrap();
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
    // item var should be cleaned up
    assert!(vars.get("item").is_none());
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
        disk "${item}" fs="${fs-type}"
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
    assert!(matches!(&fp.items[2], PlanItem::Include(p) if p == "monitoring.kdl"));
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
