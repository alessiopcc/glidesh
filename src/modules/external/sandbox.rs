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

/// Sandbox a std::process::Command for the describe probe (discovery phase).
pub fn apply_probe_sandbox(cmd: &mut std::process::Command) {
    apply_common_std(cmd);
}

/// Sandbox a tokio::process::Command for check/apply runtime.
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
            cmd.pre_exec(|| {
                // New session so plugin can't signal glidesh's process group
                libc::setsid();
                apply_landlock();
                Ok(())
            });
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
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                apply_landlock();
                Ok(())
            });
        }
    }
}

/// Apply landlock filesystem restrictions in the child process (Linux 5.13+).
/// Best-effort: silently continues if kernel doesn't support landlock.
#[cfg(target_os = "linux")]
fn apply_landlock() {
    use landlock::{
        ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
    };

    let abi = match ABI::new_current() {
        Ok(abi) => abi,
        Err(_) => return,
    };

    let temp = std::env::temp_dir();
    let read_exec = AccessFs::from_read(abi) | AccessFs::Execute;
    let read_write = AccessFs::from_all(abi);

    let result = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .expect("landlock ruleset creation")
        .create()
        .expect("landlock ruleset activation")
        .add_rule(PathBeneath::new(PathFd::new(&temp).unwrap(), read_write))
        .and_then(|r| r.add_rule(PathBeneath::new(PathFd::new("/usr").unwrap(), read_exec)))
        .and_then(|r| r.add_rule(PathBeneath::new(PathFd::new("/lib").unwrap(), read_exec)))
        .and_then(|r| r.add_rule(PathBeneath::new(PathFd::new("/lib64").unwrap(), read_exec)))
        .and_then(|r| r.restrict_self());

    if let Err(e) = result {
        // Best-effort: log but don't fail
        eprintln!("landlock: failed to apply restrictions: {e}");
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
        // PATH should always be present (it's set on virtually all systems)
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
        // Temporarily set a secret-like var and verify it's excluded
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
        // We can't directly inspect env_clear, but we can verify the command builds
        // without panicking — the real test is the integration behavior
    }
}
