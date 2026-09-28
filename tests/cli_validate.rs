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
