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
