//! What `run` settles about variables before it connects anywhere: `vars-prompt` answers —
//! without a terminal a missing answer must fail at once, never wait for input — and the
//! warning for a plan variable that overrides the inventory.
//!
//! Drives the binary with stdin not a terminal; no host or container needed. The inventory
//! host is a TEST-NET address, so reaching the connection stage would show up as a hang or
//! a connection error rather than the expected message.

use assert_cmd::Command;
use std::path::Path;
use std::time::Duration;

const PLAN: &str = r#"
plan "deploy" {
    vars-prompt {
        release "Release to deploy"
        region "Region" default="eu"
        db-password "Database password" secret=#true
    }
    step "Deploy" {
        shell "echo ${release} ${region} ${db-password}"
    }
}
"#;

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("plan.kdl"), PLAN).unwrap();
    std::fs::write(
        dir.path().join("inventory.kdl"),
        "host \"target\" \"192.0.2.1\" user=\"root\"\n",
    )
    .unwrap();
    dir
}

fn run(dir: &Path, extra: &[&str]) -> (bool, String) {
    let out = Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir)
        .args(["run", "-i", "inventory.kdl", "-p", "plan.kdl"])
        .args(["--no-tui", "--no-host-key-check", "--key", "absent-key"])
        .args(extra)
        .timeout(Duration::from_secs(60))
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), squash(&text))
}

/// The error report is wrapped to the terminal width with a gutter and colors, so compare
/// without them: every whitespace, gutter and escape sequence removed.
fn squash(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            '│' => {}
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out
}

fn has(text: &str, needle: &str) -> bool {
    text.contains(&squash(needle))
}

/// Unlocking the secrets file needs a passphrase file that does not exist, and the SSH key
/// does not exist either: the prompt error must come before both.
#[test]
fn a_missing_answer_without_a_terminal_fails_before_connecting() {
    let dir = fixture();
    Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir.path())
        .env("GLIDESH_SECRET_PASS", "pw")
        .args(["secret", "init", "--file", "secrets.kdl"])
        .assert()
        .success();
    let out = Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir.path())
        .env_remove("GLIDESH_SECRET_PASS")
        .env("GLIDESH_SECRET_PASS_FILE", dir.path().join("absent.txt"))
        .args(["run", "-i", "inventory.kdl", "-p", "plan.kdl", "--no-tui"])
        .timeout(Duration::from_secs(60))
        .output()
        .unwrap();
    let err = squash(&String::from_utf8_lossy(&out.stderr));
    assert!(!out.status.success(), "{err}");
    assert!(
        has(&err, "command line: release, db-password")
            && has(&err, "--var release=<value> --var db-password=<value>"),
        "{err}"
    );
    assert!(
        !err.contains("region"),
        "a default answers region:
{err}"
    );
    assert!(!err.contains("Connecting"), "{err}");
}

/// With every prompt answered or defaulted, the run gets past the prompts and stops at the
/// next thing it needs: the SSH key.
#[test]
fn var_flags_answer_the_prompts() {
    let dir = fixture();
    let (ok, out) = run(
        dir.path(),
        &["--var", "release=v1.4", "--var", "db-password=s3cret-pw"],
    );
    assert!(!ok, "{out}");
    assert!(
        !out.contains("--var"),
        "the prompts were answered:
{out}"
    );
    assert!(has(&out, "Key loading failed"), "{out}");
}

#[test]
fn a_var_flag_the_plan_does_not_ask_for_fails() {
    let dir = fixture();
    let (ok, out) = run(
        dir.path(),
        &[
            "--var",
            "release=v1",
            "--var",
            "db-password=x",
            "--var",
            "relase=v1",
        ],
    );
    assert!(!ok, "{out}");
    assert!(
        has(
            &out,
            "--var #3 (did you mean release?) names a variable the plan does not ask for"
        ) && has(&out, "declares: release, region, db-password")
            && !out.contains("relase"),
        "{out}"
    );
}

/// Each group's `plan=` may prompt; a name several of them declare is asked once.
#[test]
fn inventory_plans_share_one_answer_per_name() {
    let dir = tempfile::tempdir().unwrap();
    let plan = |name: &str, extra: &str| {
        format!(
            "plan \"{name}\" {{\n    vars-prompt {{\n        release \"Release\"\n{extra}    }}\n}}\n"
        )
    };
    std::fs::write(dir.path().join("web.kdl"), plan("web", "")).unwrap();
    std::fs::write(
        dir.path().join("db.kdl"),
        plan("db", "        schema \"Schema version\"\n"),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("inventory.kdl"),
        "group \"web\" plan=\"web.kdl\" {\n    host \"w\" \"192.0.2.1\"\n}\n\
         group \"db\" plan=\"db.kdl\" {\n    host \"d\" \"192.0.2.2\"\n}\n",
    )
    .unwrap();
    let out = Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir.path())
        .args(["run", "-i", "inventory.kdl", "--no-tui"])
        .timeout(Duration::from_secs(60))
        .output()
        .unwrap();
    let err = squash(&String::from_utf8_lossy(&out.stderr));
    assert!(!out.status.success(), "{err}");
    assert!(
        has(
            &err,
            "command line: release, schema. Pass --var release=<value> --var schema=<value>"
        ),
        "{err}"
    );
}

#[test]
fn a_malformed_var_flag_fails() {
    let dir = fixture();
    let (ok, out) = run(dir.path(), &["--var", "release"]);
    assert!(!ok, "{out}");
    assert!(has(&out, "must be name=value"), "{out}");
}

/// Before connecting, a run warns about each plan variable the inventory overrides for a
/// host it targets: before glidesh 2.0 the plan's value won.
#[test]
fn a_run_warns_when_the_inventory_overrides_a_plan_variable() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("plan.kdl"),
        r#"plan "deploy" {
    vars {
        customer "default-customer"
    }
    step "s" { shell "echo ${customer}" }
}"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("inventory.kdl"),
        "host \"target\" \"192.0.2.1\" user=\"root\" {\n    vars {\n        customer \"acme\"\n    }\n}\n",
    )
    .unwrap();
    let (_, text) = run(dir.path(), &[]);
    assert!(
        text.contains(&squash(
            "warning: plan 'deploy' sets 'customer' as a default, which host 'target' overrides"
        )),
        "{text}"
    );
    assert!(!text.contains("acme"), "{text}");
}

/// Two groups running one plan give one warning with the scopes of both.
#[test]
fn a_plan_several_groups_run_warns_once_per_variable() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("plan.kdl"),
        r#"plan "deploy" {
    vars {
        customer "default-customer"
    }
    step "s" { shell "echo ${customer}" }
}"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("inventory.kdl"),
        r#"group "web" plan="plan.kdl" {
    vars {
        customer "acme"
    }
    host "web-1" "192.0.2.1" user="root"
}
group "db" plan="plan.kdl" {
    host "db-1" "192.0.2.2" user="root" {
        vars {
            customer "globex"
        }
    }
}
"#,
    )
    .unwrap();
    let out = Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir.path())
        .args([
            "run",
            "-i",
            "inventory.kdl",
            "--no-tui",
            "--no-host-key-check",
        ])
        .args(["--key", "absent-key"])
        .timeout(Duration::from_secs(60))
        .output()
        .unwrap();
    let text = squash(&String::from_utf8_lossy(&out.stderr));
    let warning = squash(
        "warning: plan 'deploy' sets 'customer' as a default, which group 'web' and host \
         'db-1' override",
    );
    assert_eq!(text.matches(&warning).count(), 1, "{text}");
}
