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
fn a_missing_include_fails() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "plan.kdl", r#"plan "p" { include "nope.kdl" }"#);
    let (ok, out) = validate(dir.path());
    assert!(!ok, "{out}");
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
