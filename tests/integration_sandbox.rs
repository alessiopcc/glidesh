#![cfg(target_os = "linux")]

mod common;

use glidesh::modules::external::sandbox::apply_probe_sandbox;
use std::process::{Command, Stdio};

/// Check if the running kernel supports landlock by attempting to create a minimal ruleset.
fn landlock_supported() -> bool {
    use landlock::{ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr};
    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_read(ABI::V5))
        .and_then(|r| r.set_compatibility(CompatLevel::HardRequirement).create())
        .is_ok()
}

/// Landlock blocks reading files outside /tmp, /usr, /lib, /lib64.
/// Skips if kernel doesn't support landlock.
#[test]
fn test_sandbox_blocks_read_outside_tmpdir() {
    skip_unless_integration!();

    if !landlock_supported() {
        eprintln!("Skipping: landlock not supported on this kernel");
        return;
    }

    let sentinel =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".sandbox_test_sentinel");
    std::fs::write(&sentinel, "secret").expect("failed to write sentinel file");

    let mut cmd = Command::new("cat");
    cmd.arg(&sentinel);
    apply_probe_sandbox(&mut cmd);

    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to spawn sandboxed process");

    let _ = std::fs::remove_file(&sentinel);

    assert!(
        !output.status.success(),
        "sandboxed process should NOT be able to read outside /tmp"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Permission denied"),
        "expected 'Permission denied', got: {stderr}"
    );
}

/// Sandboxed processes can still read/write inside /tmp.
#[test]
fn test_sandbox_allows_read_inside_tmpdir() {
    skip_unless_integration!();

    let tmp_file = std::env::temp_dir().join("glidesh_sandbox_test_ok");
    std::fs::write(&tmp_file, "allowed").expect("failed to write tmp file");

    let mut cmd = Command::new("cat");
    cmd.arg(&tmp_file);
    apply_probe_sandbox(&mut cmd);

    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to spawn sandboxed process");

    let _ = std::fs::remove_file(&tmp_file);

    assert!(
        output.status.success(),
        "sandboxed process should read inside /tmp, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "allowed");
}

/// Sandboxed process cannot read ~/.ssh/id_rsa (home dir blocked by landlock).
/// Skips if landlock not supported or file doesn't exist.
#[test]
fn test_sandbox_blocks_ssh_key_access() {
    skip_unless_integration!();

    if !landlock_supported() {
        eprintln!("Skipping: landlock not supported on this kernel");
        return;
    }

    let ssh_key = dirs::home_dir()
        .expect("no home dir")
        .join(".ssh")
        .join("id_rsa");

    if !ssh_key.exists() {
        eprintln!("Skipping: ~/.ssh/id_rsa does not exist");
        return;
    }

    let mut cmd = Command::new("cat");
    cmd.arg(&ssh_key);
    apply_probe_sandbox(&mut cmd);

    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to spawn sandboxed process");

    assert!(
        !output.status.success(),
        "sandboxed process should NOT be able to read ~/.ssh/id_rsa"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Permission denied"),
        "expected 'Permission denied', got: {stderr}"
    );
}

/// Environment secrets are scrubbed from sandboxed processes.
#[test]
fn test_sandbox_env_scrubbed() {
    skip_unless_integration!();

    unsafe { std::env::set_var("AWS_SECRET_ACCESS_KEY", "supersecret") };

    let mut cmd = Command::new("env");
    apply_probe_sandbox(&mut cmd);

    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to spawn sandboxed process");

    unsafe { std::env::remove_var("AWS_SECRET_ACCESS_KEY") };

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("AWS_SECRET_ACCESS_KEY"),
        "sandboxed process should not see AWS_SECRET_ACCESS_KEY, env:\n{stdout}"
    );
    assert!(
        !stdout.contains("supersecret"),
        "sandboxed process should not see secret value"
    );
}

/// Sandboxed process working directory is /tmp, not glidesh's project dir.
#[test]
fn test_sandbox_workdir_is_tmpdir() {
    skip_unless_integration!();

    let mut cmd = Command::new("pwd");
    apply_probe_sandbox(&mut cmd);

    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to spawn sandboxed process");

    assert!(output.status.success());

    let cwd = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let expected = std::env::temp_dir()
        .to_string_lossy()
        .trim_end_matches('/')
        .to_string();
    assert_eq!(cwd, expected, "working dir should be temp dir, got: {cwd}");
}
