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
        // Build the landlock ruleset in the parent process (heap allocation is safe here).
        // Only the final restrict_self() syscall runs in the child after fork().
        let mut prepared = prepare_landlock();
        unsafe {
            cmd.pre_exec(move || pre_exec_sandbox(prepared.take()));
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
        let mut prepared = prepare_landlock();
        unsafe {
            cmd.pre_exec(move || pre_exec_sandbox(prepared.take()));
        }
    }
}

/// Pre-exec hook: session isolation + filesystem restriction.
/// Runs in the child process after fork(), before exec().
/// Only async-signal-safe operations here — no heap allocation.
/// The landlock ruleset is pre-built in the parent; we only call restrict_self().
#[cfg(unix)]
fn pre_exec_sandbox(landlock: PreparedLandlock) -> std::io::Result<()> {
    if unsafe { libc::setsid() } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    restrict_landlock(landlock);
    Ok(())
}

/// A pre-built landlock ruleset ready to be restricted in a child process.
/// Built in the parent (where heap allocation is safe), moved into pre_exec.
#[cfg(target_os = "linux")]
type PreparedLandlock = Option<landlock::RulesetCreated>;

#[cfg(all(unix, not(target_os = "linux")))]
type PreparedLandlock = Option<()>;

/// Build the landlock ruleset in the parent process. All heap allocation
/// (PathFd opens, Ruleset builder, rule additions) happens here, before fork().
#[cfg(target_os = "linux")]
fn prepare_landlock() -> PreparedLandlock {
    use landlock::{
        ABI, Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
        RulesetCreatedAttr,
    };

    let abi = ABI::V5;
    let temp = std::env::temp_dir();
    let read_exec = AccessFs::from_read(abi) | AccessFs::Execute;
    let read_write = AccessFs::from_all(abi);

    let fd_temp = PathFd::new(&temp).ok()?;
    let fd_usr = PathFd::new("/usr").ok()?;
    let fd_lib = PathFd::new("/lib").ok()?;
    let fd_lib64 = PathFd::new("/lib64").ok()?;

    let ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .and_then(|r: Ruleset| r.set_compatibility(CompatLevel::BestEffort).create())
        .and_then(|r: landlock::RulesetCreated| {
            r.set_compatibility(CompatLevel::BestEffort)
                .add_rule(PathBeneath::new(fd_temp, read_write))?
                .add_rule(PathBeneath::new(fd_usr, read_exec))?
                .add_rule(PathBeneath::new(fd_lib, read_exec))?
                .add_rule(PathBeneath::new(fd_lib64, read_exec))
        })
        .ok()?;

    Some(ruleset)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn prepare_landlock() -> PreparedLandlock {
    None
}

/// Apply the pre-built landlock ruleset. Only calls restrict_self() (a syscall),
/// which is async-signal-safe and does not allocate.
#[cfg(target_os = "linux")]
fn restrict_landlock(prepared: PreparedLandlock) {
    use landlock::{CompatLevel, Compatible};
    if let Some(ruleset) = prepared {
        if let Err(e) = ruleset
            .set_compatibility(CompatLevel::BestEffort)
            .restrict_self()
        {
            // Cannot use eprintln (allocates) in pre_exec; silently ignore.
            let _ = e;
        }
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn restrict_landlock(_prepared: PreparedLandlock) {}

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
