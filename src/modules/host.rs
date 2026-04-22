//! The `host` module runs a command once per task and broadcasts the result
//! to every target host's var map (via `register`). By default it runs on the
//! controller (local machine); with `on="<name>"` it runs against that
//! specific inventory host's SSH session. Coordination across NodeRunners is
//! performed by `executor::host_coordinator::HostCoordinator`; this file only
//! provides the execution primitive.

use crate::config::types::ResolvedHost;
use crate::error::GlideshError;
use crate::modules::ModuleParams;
use crate::modules::shell::{login_enabled, resolve_cmd_from_params, wrap_login};
use crate::ssh::{HostKeyPolicy, SshSession};
use russh_keys::key::PrivateKeyWithHashAlg;

pub const MODULE_NAME: &str = "host";

#[derive(Debug, Clone)]
pub struct HostOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Execute a `host` task. Intended to be invoked exactly once per TaskKey
/// through `HostCoordinator::get_or_run`; this function does not itself
/// deduplicate across hosts.
pub async fn run_host_task(
    params: &ModuleParams,
    all_targets: &[ResolvedHost],
    key: &PrivateKeyWithHashAlg,
    host_key_policy: HostKeyPolicy,
    dry_run: bool,
) -> Result<HostOutput, GlideshError> {
    let raw = resolve_cmd_from_params(params, MODULE_NAME)?;
    let command = if login_enabled(params) {
        wrap_login(&raw)
    } else {
        raw
    };

    let on = params
        .args
        .get("on")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    if dry_run {
        let where_ = on.as_deref().unwrap_or("<local>");
        return Ok(HostOutput {
            stdout: format!("[dry-run] Would run on {}: {}", where_, command),
            stderr: String::new(),
            exit_code: 0,
        });
    }

    match on {
        Some(target) => run_on_named_host(&target, &command, all_targets, key, host_key_policy)
            .await
            .map_err(|e| match e {
                GlideshError::Module { .. } => e,
                other => GlideshError::Module {
                    module: MODULE_NAME.to_string(),
                    message: other.to_string(),
                },
            }),
        None => run_local(&command).await,
    }
}

async fn run_local(command: &str) -> Result<HostOutput, GlideshError> {
    let output = if cfg!(windows) {
        tokio::process::Command::new("cmd")
            .arg("/C")
            .arg(command)
            .output()
            .await
    } else {
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
            .await
    }
    .map_err(|e| GlideshError::Module {
        module: MODULE_NAME.to_string(),
        message: format!("Failed to spawn local command '{}': {}", command, e),
    })?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

    if exit_code != 0 {
        return Err(GlideshError::Module {
            module: MODULE_NAME.to_string(),
            message: format!(
                "Local command '{}' failed with exit code {}.\nstdout: {}\nstderr: {}",
                command, exit_code, stdout, stderr
            ),
        });
    }

    Ok(HostOutput {
        stdout,
        stderr,
        exit_code,
    })
}

async fn run_on_named_host(
    target_name: &str,
    command: &str,
    all_targets: &[ResolvedHost],
    key: &PrivateKeyWithHashAlg,
    host_key_policy: HostKeyPolicy,
) -> Result<HostOutput, GlideshError> {
    let target = all_targets
        .iter()
        .find(|h| h.name == target_name)
        .ok_or_else(|| GlideshError::Module {
            module: MODULE_NAME.to_string(),
            message: format!(
                "on=\"{}\" does not match any host in the current run's target set",
                target_name
            ),
        })?;

    let session = match &target.jump {
        Some(jump) => {
            SshSession::connect_via_jump(
                &target.address,
                target.port,
                &target.user,
                key,
                host_key_policy,
                jump,
            )
            .await
        }
        None => {
            SshSession::connect(
                &target.address,
                target.port,
                &target.user,
                key,
                host_key_policy,
            )
            .await
        }
    }?;

    let output = session.exec(command).await?;
    let _ = session.close().await;

    let stdout = output.stdout;
    let stderr = output.stderr;
    let exit_code = output.exit_code as i32;

    if exit_code != 0 {
        return Err(GlideshError::Module {
            module: MODULE_NAME.to_string(),
            message: format!(
                "Command '{}' on host '{}' failed with exit code {}.\nstdout: {}\nstderr: {}",
                command, target_name, exit_code, stdout, stderr
            ),
        });
    }

    Ok(HostOutput {
        stdout,
        stderr,
        exit_code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::ParamValue;
    use std::collections::HashMap;

    fn params(positional: &str, args: &[(&str, ParamValue)]) -> ModuleParams {
        let mut map = HashMap::new();
        for (k, v) in args {
            map.insert((*k).to_string(), v.clone());
        }
        ModuleParams {
            resource_name: positional.to_string(),
            args: map,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_exec_captures_stdout() {
        let p = params(
            "label",
            &[("cmd", ParamValue::String("echo hello".to_string()))],
        );
        let out = run_local("echo hello").await.expect("ok");
        assert_eq!(out.stdout.trim(), "hello");
        assert_eq!(out.exit_code, 0);
        let cmd = resolve_cmd_from_params(&p, MODULE_NAME).unwrap();
        assert_eq!(cmd, "echo hello");
    }

    #[test]
    fn cmd_list_joins_with_and_and() {
        let p = params(
            "label",
            &[(
                "cmd",
                ParamValue::List(vec!["a".into(), "b".into(), "c".into()]),
            )],
        );
        let cmd = resolve_cmd_from_params(&p, MODULE_NAME).unwrap();
        assert_eq!(cmd, "a && b && c");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_exec_runs_list_as_joined_commands() {
        let p = params(
            "chain",
            &[(
                "cmd",
                ParamValue::List(vec!["echo one".into(), "echo two".into()]),
            )],
        );
        let out = run_host_task(&p, &[], &dummy_key(), dummy_policy(), false)
            .await
            .expect("ok");
        assert!(out.stdout.contains("one"));
        assert!(out.stdout.contains("two"));
        assert_eq!(out.exit_code, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_exec_nonzero_is_error() {
        let err = run_local("false").await.unwrap_err();
        match err {
            GlideshError::Module { module, message } => {
                assert_eq!(module, "host");
                assert!(message.contains("exit code"));
            }
            _ => panic!("expected Module error"),
        }
    }

    #[tokio::test]
    async fn dry_run_does_not_execute() {
        let p = params(
            "gen",
            &[("cmd", ParamValue::String("echo dry".to_string()))],
        );
        let out = run_host_task(&p, &[], &dummy_key(), dummy_policy(), true)
            .await
            .expect("dry-run ok");
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.contains("[dry-run]"));
        assert!(out.stdout.contains("echo dry"));
    }

    #[tokio::test]
    async fn unknown_on_target_errors() {
        let p = params(
            "gen",
            &[
                ("cmd", ParamValue::String("echo x".to_string())),
                ("on", ParamValue::String("nope".to_string())),
            ],
        );
        let err = run_host_task(&p, &[], &dummy_key(), dummy_policy(), false)
            .await
            .unwrap_err();
        match err {
            GlideshError::Module { module, message } => {
                assert_eq!(module, "host");
                assert!(message.contains("nope"));
            }
            _ => panic!("expected Module error"),
        }
    }

    fn dummy_policy() -> HostKeyPolicy {
        HostKeyPolicy {
            verify: false,
            accept_new: false,
        }
    }

    fn dummy_key() -> PrivateKeyWithHashAlg {
        let private =
            ssh_key::PrivateKey::random(&mut rand::thread_rng(), ssh_key::Algorithm::Ed25519)
                .expect("ed25519");
        PrivateKeyWithHashAlg::new(std::sync::Arc::new(private), None).unwrap()
    }
}
