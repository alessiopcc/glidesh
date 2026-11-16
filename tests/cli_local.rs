//! `run --local` and `local=#true` hosts: a plan applied to the machine running the test, with
//! no SSH server, key or agent. On Windows, where modules cannot run, `--local` refuses.
//!
//! `HOME` is an empty directory and `SSH_AUTH_SOCK` is unset, so nothing could log in over SSH.

use assert_cmd::Command;
use std::path::Path;
use std::time::Duration;

fn run(dir: &Path, args: &[&str]) -> (bool, String) {
    run_with(dir, args, &[])
}

fn run_with(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> (bool, String) {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let out = Command::cargo_bin("glidesh")
        .unwrap()
        .current_dir(dir)
        .arg("run")
        .args(args)
        .arg("--no-tui")
        .env("HOME", &home)
        .env_remove("SSH_AUTH_SOCK")
        .envs(envs.iter().copied())
        .timeout(Duration::from_secs(60))
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// A plan touching only `dir`: a registered command, a templated upload with a mode, and a
/// shell step guarded by `check=`, so a second run changes nothing.
#[cfg(unix)]
fn write_plan(dir: &Path) {
    let root = dir.to_str().unwrap();
    std::fs::write(dir.join("app.conf"), "host=${@host.name} said=${said}\n").unwrap();
    std::fs::write(
        dir.join("plan.kdl"),
        format!(
            r#"plan "local" {{
    step "Ask" {{
        shell "echo hi" register="said"
    }}
    step "Config" {{
        file "{root}/out/app.conf" src="app.conf" template=#true mode="0640"
    }}
    step "Mark" {{
        shell "echo once >> {root}/out/marks" check="test -e {root}/out/marks"
    }}
}}
"#
        ),
    )
    .unwrap();
}

#[cfg(unix)]
#[test]
fn run_local_applies_a_plan_here_and_a_second_run_changes_nothing() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    write_plan(dir.path());

    let (ok, out) = run(dir.path(), &["--local", "-p", "plan.kdl"]);
    assert!(ok, "{out}");
    let conf = dir.path().join("out/app.conf");
    assert_eq!(
        std::fs::read_to_string(&conf).unwrap(),
        "host=local said=hi\n"
    );
    let mode = std::fs::metadata(&conf).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o640);

    let (ok, out) = run(dir.path(), &["--local", "-p", "plan.kdl"]);
    assert!(ok, "{out}");
    assert!(out.contains(" 1 changed"), "only `Ask` reruns: {out}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out/marks")).unwrap(),
        "once\n"
    );
}

#[cfg(unix)]
#[test]
fn an_inventory_host_marked_local_runs_here_under_its_own_name() {
    let dir = tempfile::tempdir().unwrap();
    write_plan(dir.path());
    std::fs::write(
        dir.path().join("inventory.kdl"),
        "host \"builder\" local=#true\n",
    )
    .unwrap();

    let (ok, out) = run(dir.path(), &["-i", "inventory.kdl", "-p", "plan.kdl"]);
    assert!(ok, "{out}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out/app.conf")).unwrap(),
        "host=builder said=hi\n"
    );
}

#[cfg(unix)]
#[test]
fn su_is_refused_on_a_local_host_with_what_to_use_instead() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("plan.kdl"),
        "plan \"p\" {\n    step \"s\" {\n        shell \"true\"\n    }\n}\n",
    )
    .unwrap();
    // With a password, as `su` needs one: what is refused is asking for it.
    let (ok, out) = run_with(
        dir.path(),
        &[
            "--local",
            "-p",
            "plan.kdl",
            "--run-as",
            "root",
            "--run-as-method",
            "su",
        ],
        &[("GLIDESH_RUNAS_PASS", "not-used")],
    );
    assert!(!ok);
    assert!(out.contains("run-as-method=\"sudo\""), "{out}");
}

#[cfg(windows)]
#[test]
fn run_local_needs_a_unix_controller() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("plan.kdl"),
        "plan \"p\" {\n    step \"s\" {\n        shell \"true\"\n    }\n}\n",
    )
    .unwrap();
    let (ok, out) = run(dir.path(), &["--local", "-p", "plan.kdl"]);
    assert!(!ok);
    assert!(out.contains("needs a Linux or macOS controller"), "{out}");
}
