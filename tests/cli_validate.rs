//! `glidesh validate` must reject what `run` would reject before connecting anywhere.
//!
//! Drives the binary; no host or container needed.

use assert_cmd::Command;
use std::path::Path;

fn validate(dir: &Path) -> (bool, String) {
    let out = Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir)
        .args(["validate", "-p", "plan.kdl"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    (out.status.success(), text)
}

fn write(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

#[test]
fn a_valid_plan_passes() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "app.conf", "x");
    write(
        dir.path(),
        "plan.kdl",
        r#"plan "p" { step "s" { file "/etc/app.conf" src="app.conf" } }"#,
    );
    let (ok, out) = validate(dir.path());
    assert!(ok, "{out}");
    assert!(out.contains("OK (1 steps)"), "{out}");
}

/// The template is only rendered when the task runs; validate reads it now.
#[test]
fn a_template_reading_a_variable_its_block_never_has_fails() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "failure.txt", "${@error.msg}");
    write(
        dir.path(),
        "plan.kdl",
        r#"plan "p" { step "s" { file "/var/log/f" src="failure.txt" template=#true } }"#,
    );
    let (ok, out) = validate(dir.path());
    assert!(!ok, "{out}");
    assert!(out.contains("uses ${@error.msg}"), "{out}");

    write(
        dir.path(),
        "plan.kdl",
        r#"plan "p" { step "s" { shell "false"; rescue { file "/var/log/f" src="failure.txt" template=#true } } }"#,
    );
    let (ok, out) = validate(dir.path());
    assert!(ok, "a rescue may read the failure:\n{out}");
}

#[test]
fn an_unknown_module_fails() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "plan.kdl",
        r#"plan "p" { step "s" { pakage "nginx" state="present" } }"#,
    );
    let (ok, out) = validate(dir.path());
    assert!(!ok);
    assert!(out.contains("Unknown module(s): pakage"), "{out}");
}

#[test]
fn a_subscription_to_a_missing_step_fails() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "plan.kdl",
        r#"plan "p" { step "Restart" subscribe="Deploy" { shell "true" } }"#,
    );
    let (ok, out) = validate(dir.path());
    assert!(!ok);
    assert!(out.contains("not a preceding step"), "{out}");
}

/// Includes are resolved, so a problem inside an included plan is reported.
#[test]
fn a_problem_in_an_included_plan_is_found() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "common/base.kdl",
        r#"plan "base" { step "Base" { sytemd "nginx" state="started" } }"#,
    );
    write(
        dir.path(),
        "plan.kdl",
        r#"plan "p" { include "common/base.kdl" }"#,
    );
    let (ok, out) = validate(dir.path());
    assert!(!ok);
    assert!(out.contains("Unknown module(s): sytemd"), "{out}");
}

#[test]
fn a_rollout_plan_passes() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "plan.kdl",
        r#"plan "p" {
            mode "sync"
            serial 1 "25%"
            max-fail "10%"
            step "s" { shell "true" }
        }"#,
    );
    let (ok, out) = validate(dir.path());
    assert!(ok, "{out}");
}

#[test]
fn bad_rollout_or_mode_values_fail() {
    for (setting, expect) in [
        ("serial 0", "serial must be"),
        (r#"serial "150%""#, "serial must be"),
        (r#"max-fail "abc""#, "max-fail must be"),
        (r#"mode "asinc""#, r#""sync" or "async""#),
    ] {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "plan.kdl",
            &format!("plan \"p\" {{\n    {setting}\n    step \"s\" {{ shell \"true\" }}\n}}"),
        );
        let (ok, out) = validate(dir.path());
        assert!(!ok, "{setting} must fail:\n{out}");
        assert!(out.contains(expect), "{setting}:\n{out}");
    }
}

#[test]
fn a_missing_include_fails() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "plan.kdl", r#"plan "p" { include "nope.kdl" }"#);
    let (ok, out) = validate(dir.path());
    assert!(!ok, "{out}");
}

/// The field case: a `.env` uploaded without `template #true` shipped `${cuda-devices}`
/// literally. A warning, not a failure — the plan is valid, just probably wrong.
#[test]
fn a_defined_variable_in_an_untemplated_file_warns_without_failing() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "vllm.env",
        "CUDA_VISIBLE_DEVICES=${cuda-devices}\nHOME_DIR=${HOME}\n",
    );
    write(
        dir.path(),
        "plan.kdl",
        r#"plan "p" {
            vars { cuda-devices "0,1" }
            step "Env" { file "/etc/vllm/vllm.env" src="vllm.env" }
        }"#,
    );
    let (ok, out) = validate(dir.path());
    assert!(ok, "a warning must not fail validation:\n{out}");
    assert!(
        out.contains("warning: step 'Env': vllm.env contains ${cuda-devices}"),
        "{out}"
    );
    assert!(
        !out.contains("${HOME}"),
        "a shell variable is not glidesh's:\n{out}"
    );
}

/// A prompted variable is defined at run time, so it counts; validate itself never asks.
#[test]
fn a_prompted_variable_counts_as_defined_without_asking() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "app.env", "RELEASE=${release}\n");
    write(
        dir.path(),
        "plan.kdl",
        r#"plan "p" {
            vars-prompt { release "Release to deploy" }
            step "Env" { file "/etc/app.env" src="app.env" }
        }"#,
    );
    let (ok, out) = validate(dir.path());
    assert!(ok, "{out}");
    assert!(
        out.contains("warning: step 'Env': app.env contains ${release}"),
        "{out}"
    );
}

#[test]
fn a_vars_prompt_in_an_included_plan_fails() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "child.kdl",
        r#"plan "child" { vars-prompt { release "Release" } }"#,
    );
    write(
        dir.path(),
        "plan.kdl",
        r#"plan "p" { include "child.kdl" }"#,
    );
    let (ok, out) = validate(dir.path());
    assert!(!ok, "{out}");
    assert!(out.contains("only the plan you run may ask"), "{out}");
}

/// An inventory variable counts too, when `-i` is given.
#[test]
fn an_inventory_variable_counts_as_defined() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "app.conf", "db=${db-host}\n");
    write(
        dir.path(),
        "inventory.kdl",
        "host \"web\" \"10.0.0.1\" {\n    vars {\n        db-host \"10.0.0.2\"\n    }\n}\n",
    );
    write(
        dir.path(),
        "plan.kdl",
        r#"plan "p" { step "s" { file "/etc/app.conf" src="app.conf" } }"#,
    );
    let out = Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir.path())
        .args(["validate", "-p", "plan.kdl", "-i", "inventory.kdl"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("contains ${db-host}"), "{text}");
}

#[test]
fn a_missing_file_source_fails_and_every_problem_is_listed() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "plan.kdl",
        r#"plan "p" {
            step "s" {
                file "/etc/a.conf" src="a.conf"
                file "/etc/b.conf" src="b.conf"
                pakage "nginx"
            }
        }"#,
    );
    let (ok, out) = validate(dir.path());
    assert!(!ok);
    for expect in ["src 'a.conf' not found", "src 'b.conf' not found", "pakage"] {
        assert!(out.contains(expect), "missing {expect:?} in:\n{out}");
    }
}

fn validate_inventory(dir: &Path) -> (bool, String) {
    let out = Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir)
        .args(["validate", "-i", "inventory.kdl"])
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
    )
}

/// `run -i` runs the plans the inventory names, so `validate -i` checks them: each file
/// once, from the inventory's directory, whoever names it.
#[test]
fn validate_inventory_checks_each_plan_it_names_once() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "inventory.kdl",
        r#"group "web" plan="plans/web.kdl" {
    host "web-1" "10.0.0.1"
    host "web-2" "10.0.0.2"
}
group "db" {
    host "db-1" "10.0.0.3" plan="plans/web.kdl"
    host "db-2" "10.0.0.4" plan="plans/db.kdl"
}
host "lone" "10.0.0.5"
"#,
    );
    write(
        dir.path(),
        "plans/web.kdl",
        r#"plan "web" { step "s" { shell "true" } }"#,
    );
    write(
        dir.path(),
        "plans/db.kdl",
        r#"plan "db" { step "a" { shell "true" }
        step "b" { shell "true" } }"#,
    );
    let (ok, out) = validate_inventory(dir.path());
    assert!(ok, "{out}");
    assert_eq!(out.matches("web.kdl").count(), 1, "{out}");
    assert!(out.contains("web.kdl' (3 hosts)... OK (1 steps)"), "{out}");
    assert!(out.contains("db.kdl' (1 host)... OK (2 steps)"), "{out}");
}

#[test]
fn a_broken_or_missing_inventory_plan_fails() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "inventory.kdl",
        r#"host "a" "10.0.0.1" plan="broken.kdl"
host "b" "10.0.0.2" plan="missing.kdl"
host "c" "10.0.0.3" plan="good.kdl"
"#,
    );
    write(
        dir.path(),
        "broken.kdl",
        r#"plan "p" { step "s" { pakage "nginx" } }"#,
    );
    write(
        dir.path(),
        "good.kdl",
        r#"plan "p" { step "s" { shell "true" } }"#,
    );
    let (ok, out) = validate_inventory(dir.path());
    assert!(!ok, "{out}");
    assert!(out.contains("Unknown module(s): pakage"), "{out}");
    assert!(out.contains("missing.kdl' (1 host)... FAILED"), "{out}");
    assert!(
        out.contains("good.kdl' (1 host)... OK"),
        "every plan is checked:\n{out}"
    );
}

/// A plan's hosts define its variables; another group's hosts do not.
#[test]
fn an_inventory_plan_knows_only_its_own_hosts_variables() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "app.conf",
        "db=${db-host} cache=${cache-host}\n",
    );
    write(
        dir.path(),
        "inventory.kdl",
        r#"group "app" plan="plan.kdl" {
    vars { db-host "10.0.0.9"; }
    host "app-1" "10.0.0.1"
}
group "cache" {
    vars { cache-host "10.0.0.8"; }
    host "cache-1" "10.0.0.2"
}
"#,
    );
    write(
        dir.path(),
        "plan.kdl",
        r#"plan "p" { step "s" { file "/etc/app.conf" src="app.conf" } }"#,
    );
    let (ok, out) = validate_inventory(dir.path());
    assert!(ok, "{out}");
    assert!(out.contains("contains ${db-host}"), "{out}");
    assert!(!out.contains("${cache-host}"), "{out}");
}

/// With `-p`, the inventory's own plans are not checked: the run would not use them.
#[test]
fn an_explicit_plan_replaces_the_inventory_plans() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "inventory.kdl",
        r#"host "a" "10.0.0.1" plan="missing.kdl""#,
    );
    write(
        dir.path(),
        "plan.kdl",
        r#"plan "p" { step "s" { shell "true" } }"#,
    );
    let out = Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir.path())
        .args(["validate", "-p", "plan.kdl", "-i", "inventory.kdl"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(!text.contains("missing.kdl"), "{text}");
}

const SHADOW_INVENTORY: &str = r#"group "web" plan="plan.kdl" {
    vars {
        customer "acme"
    }
    host "web-1" "10.0.0.1"
}
host "db-1" "10.0.0.2" {
    vars {
        tier "gold"
    }
}
"#;

const SHADOW_PLAN: &str = r#"plan "deploy" {
    vars {
        customer "default-customer"
        region "eu"
    }
    step "s" { shell "echo ${customer} ${region}" }
}"#;

/// A plan var beats what the inventory sets for a host: `validate` names the variable and
/// the scopes, never a value, and still passes.
#[test]
fn a_plan_variable_the_inventory_also_sets_warns_without_failing() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "inventory.kdl", SHADOW_INVENTORY);
    write(dir.path(), "plan.kdl", SHADOW_PLAN);
    let out = Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir.path())
        .args(["validate", "-p", "plan.kdl", "-i", "inventory.kdl"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains(
            "warning: plan 'deploy' overrides 'customer', which is also set by group 'web'"
        ),
        "{text}"
    );
    assert!(!text.contains("'region'"), "{text}");
    assert!(
        !text.contains("acme") && !text.contains("default-customer"),
        "{text}"
    );
}

/// For a plan the inventory names, only the hosts that run it count.
#[test]
fn validate_inventory_warns_only_for_the_plans_own_hosts() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "inventory.kdl", SHADOW_INVENTORY);
    write(
        dir.path(),
        "plan.kdl",
        r#"plan "deploy" {
    vars {
        customer "default-customer"
        tier "silver"
    }
    step "s" { shell "true" }
}"#,
    );
    let (ok, text) = validate_inventory(dir.path());
    assert!(ok, "{text}");
    assert!(text.contains("overrides 'customer'"), "{text}");
    assert!(
        !text.contains("overrides 'tier'"),
        "db-1 does not run the plan:\n{text}"
    );
}

/// A list variable the plan and the secrets file both define: the plan's wins.
#[test]
fn a_structured_plan_variable_the_secrets_file_also_defines_warns() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "inventory.kdl", "host \"web-1\" \"10.0.0.1\"\n");
    write(
        dir.path(),
        "secrets.kdl",
        "api-keys {\n    - name=\"a\" value=\"secret:v1:one\"\n}\n",
    );
    write(
        dir.path(),
        "plan.kdl",
        r#"plan "deploy" {
    vars {
        api-keys {
            - name="placeholder" value="x"
        }
    }
    step "s" { shell "true" }
}"#,
    );
    let out = Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir.path())
        .args(["validate", "-p", "plan.kdl", "-i", "inventory.kdl"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains("overrides 'api-keys', which is also set by the secrets file"),
        "{text}"
    );
}

fn validate_with_inventory(dir: &Path) -> (bool, String) {
    let out = Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir)
        .args(["validate", "-p", "plan.kdl", "-i", "inventory.kdl"])
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
    )
}

/// With the inventory given, a templated file reading a name nothing defines fails
/// validation, naming the file, the line and the name; the same name escaped passes.
#[test]
fn an_undefined_name_in_a_template_fails_and_an_escaped_one_passes() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "run.sh",
        "#!/bin/sh\ncd ${app-dir}\necho ${port} ${HOME}\n",
    );
    write(
        dir.path(),
        "inventory.kdl",
        "host \"web-1\" \"10.0.0.1\" {\n    vars {\n        port \"80\"\n    }\n}\n",
    );
    write(
        dir.path(),
        "plan.kdl",
        r#"plan "p" {
    vars {
        app-dir "/srv/app"
    }
    step "s" { file "/usr/local/bin/run.sh" src="run.sh" template=#true }
}"#,
    );
    let (ok, out) = validate_with_inventory(dir.path());
    assert!(!ok, "{out}");
    assert!(
        out.contains("template run.sh, line 3: ${HOME} is not defined"),
        "{out}"
    );
    assert!(out.contains("write $${HOME}"), "{out}");
    assert!(!out.contains("${port} is not defined"), "{out}");

    write(
        dir.path(),
        "run.sh",
        "#!/bin/sh\ncd ${app-dir}\necho ${port} $${HOME}\n",
    );
    let (ok, out) = validate_with_inventory(dir.path());
    assert!(ok, "{out}");
}

/// Without the inventory, a name it might set only warns: `validate -p` alone must not
/// fail a plan that runs.
#[test]
fn without_an_inventory_an_unknown_template_name_only_warns() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "app.conf", "port=${port}\n");
    write(
        dir.path(),
        "plan.kdl",
        r#"plan "p" { step "s" { file "/etc/app.conf" src="app.conf" template=#true } }"#,
    );
    let (ok, out) = validate(dir.path());
    assert!(ok, "{out}");
    assert!(
        out.contains(
            "warning: step 's': file '/etc/app.conf': template app.conf, line 1: ${port} is \
             not defined, unless the inventory sets it (pass -i)"
        ),
        "{out}"
    );
}
