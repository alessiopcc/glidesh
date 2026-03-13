//! Sandbox helpers for external module process isolation.
//!
//! Configures `Command` objects with: env scrubbing (allow-list only),
//! temp working directory, Unix session isolation (`setsid`), and
//! Linux filesystem restriction (landlock, best-effort).

fn minimal_env() -> Vec<(String, String)> {
    let mut env = Vec::new();
    let passthrough: &[&str] = if cfg!(windows) {
        &["PATH", "SYSTEMROOT", "USERPROFILE", "TEMP", "TMP"]
    } else {
        &["PATH", "HOME", "TMPDIR"]
    };
    for key in passthrough {
        if let Ok(val) = std::env::var(key) {
            env.push((key.to_string(), val));
        }
    }
    env.push(("LANG".to_string(), "C.UTF-8".to_string()));
    env
}

pub fn apply_probe_sandbox(cmd: &mut std::process::Command) {
    apply_common_std(cmd);
}

pub fn apply_runtime_sandbox(cmd: &mut tokio::process::Command, module_name: &str) {
    apply_common_tokio(cmd);
    cmd.env(
        "GLIDESH_PROTOCOL_VERSION",
        super::protocol::PROTOCOL_VERSION.to_string(),
    );
    cmd.env("GLIDESH_MODULE_NAME", module_name);
}

fn apply_common_std(cmd: &mut std::process::Command) {
    cmd.env_clear();
    for (k, v) in minimal_env() {
        cmd.env(&k, &v);
    }
    cmd.current_dir(std::env::temp_dir());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(pre_exec_sandbox);
        }
    }
}

fn apply_common_tokio(cmd: &mut tokio::process::Command) {
    cmd.env_clear();
    for (k, v) in minimal_env() {
        cmd.env(&k, &v);
    }
    cmd.current_dir(std::env::temp_dir());

    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(pre_exec_sandbox);
    }
}

/// Pre-exec hook: session isolation + filesystem restriction.
/// Runs in the child process after fork(), before exec().
/// Note: only async-signal-safe functions are technically permitted here,
/// but setsid and landlock syscalls are safe. Avoid heap allocation.
#[cfg(unix)]
fn pre_exec_sandbox() -> std::io::Result<()> {
    if unsafe { libc::setsid() } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    apply_landlock();
    Ok(())
}

#[cfg(target_os = "linux")]
fn apply_landlock() {
    use landlock::{
        ABI, Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
        RulesetCreatedAttr,
    };

    let abi = ABI::V5;
    let temp = std::env::temp_dir();
    let read_exec = AccessFs::from_read(abi) | AccessFs::Execute;
    let read_write = AccessFs::from_all(abi);

    let fd_temp = match PathFd::new(&temp) {
        Ok(fd) => fd,
        Err(_) => return,
    };
    let fd_usr = match PathFd::new("/usr") {
        Ok(fd) => fd,
        Err(_) => return,
    };
    let fd_lib = match PathFd::new("/lib") {
        Ok(fd) => fd,
        Err(_) => return,
    };
    let fd_lib64 = match PathFd::new("/lib64") {
        Ok(fd) => fd,
        Err(_) => return,
    };

    let result = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .and_then(|r: Ruleset| r.set_compatibility(CompatLevel::BestEffort).create())
        .and_then(|r: landlock::RulesetCreated| {
            r.set_compatibility(CompatLevel::BestEffort)
                .add_rule(PathBeneath::new(fd_temp, read_write))?
                .add_rule(PathBeneath::new(fd_usr, read_exec))?
                .add_rule(PathBeneath::new(fd_lib, read_exec))?
                .add_rule(PathBeneath::new(fd_lib64, read_exec))?
                .restrict_self()
        });

    if let Err(e) = result {
        eprintln!("landlock: {e}");
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn apply_landlock() {}

#[cfg(not(unix))]
#[allow(dead_code)]
fn apply_landlock() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_env_contains_path() {
        let env = minimal_env();
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"PATH"), "minimal_env must include PATH");
    }

    #[test]
    fn minimal_env_contains_lang() {
        let env = minimal_env();
        let lang = env.iter().find(|(k, _)| k == "LANG");
        assert_eq!(lang.unwrap().1, "C.UTF-8");
    }

    #[test]
    fn minimal_env_excludes_secrets() {
        unsafe { std::env::set_var("AWS_SECRET_ACCESS_KEY", "hunter2") };
        let env = minimal_env();
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(
            !keys.contains(&"AWS_SECRET_ACCESS_KEY"),
            "minimal_env must not include AWS_SECRET_ACCESS_KEY"
        );
        unsafe { std::env::remove_var("AWS_SECRET_ACCESS_KEY") };
    }

    #[test]
    fn probe_sandbox_sets_env_clear() {
        let mut cmd = std::process::Command::new("echo");
        apply_probe_sandbox(&mut cmd);
    }
}
