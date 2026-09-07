mod health;
mod run_args;

use crate::error::GlideshError;
use crate::modules::context::ModuleContext;
use crate::modules::detect::ContainerRuntime;
use crate::modules::detect::PkgManager;
use crate::modules::shell::{
    accepted, describe_success_codes, exec_timed, parse_success_codes, parse_timeout,
};
use crate::modules::{Module, ModuleParams, ModuleResult, ModuleStatus};
use crate::util::shell_escape;
use async_trait::async_trait;
use std::time::Duration;

pub struct ContainerModule;

/// What has to happen to an existing container whose spec still matches the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartAction {
    /// Already up (or the runtime is bringing it up) — leave it alone.
    None,
    Start,
    Unpause,
    /// Not recoverable by starting; tear it down and run a fresh one.
    Recreate,
}

fn start_action(state: &str) -> StartAction {
    match state {
        "running" | "restarting" => StartAction::None,
        "paused" => StartAction::Unpause,
        "created" | "exited" | "stopped" | "configured" => StartAction::Start,
        // "dead", "removing", and anything unrecognised: starting won't fix it.
        _ => StartAction::Recreate,
    }
}

fn desired_state(params: &ModuleParams) -> &str {
    params
        .args
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("running")
}

const KNOWN_STATES: &[&str] = &["running", "run-once", "stopped", "absent"];

/// Which runtime to drive, and whether it is already on the host.
///
/// Kept separate from installing it so `check` can stay read-only: a check that
/// installs packages mutates a host the operator may never have applied to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RuntimeResolution {
    Present(String),
    NeedsInstall(String),
}

impl RuntimeResolution {
    fn runtime(&self) -> &str {
        match self {
            RuntimeResolution::Present(rt) | RuntimeResolution::NeedsInstall(rt) => rt,
        }
    }
}

#[async_trait]
impl Module for ContainerModule {
    fn name(&self) -> &str {
        "container"
    }

    async fn check(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleStatus, GlideshError> {
        run_args::validate_params(params)?;

        let name = &params.resource_name;
        let state = desired_state(params);
        if !KNOWN_STATES.contains(&state) {
            return Ok(ModuleStatus::Unknown {
                reason: format!("Unknown container state: {}", state),
            });
        }

        let runtime = match self.resolve_runtime(ctx, params).await? {
            RuntimeResolution::Present(rt) => rt,
            // Nothing can exist on a host with no runtime, so there is nothing to
            // inspect — and installing one is `apply`'s job, not this one's.
            RuntimeResolution::NeedsInstall(rt) => {
                return Ok(match state {
                    "stopped" | "absent" => ModuleStatus::Satisfied,
                    "run-once" => ModuleStatus::Pending {
                        plan: format!("Install {} and run container {} to completion", rt, name),
                    },
                    _ => ModuleStatus::Pending {
                        plan: format!("Install {} and create container {}", rt, name),
                    },
                });
            }
        };

        match state {
            "running" => self.check_running(ctx, params, &runtime).await,
            "run-once" => Self::check_run_once(ctx, params).await,
            "stopped" => match inspect_state(ctx, &runtime, name).await? {
                Some(current) if current == "running" || current == "restarting" => {
                    Ok(ModuleStatus::Pending {
                        plan: format!("Stop container {}", name),
                    })
                }
                _ => Ok(ModuleStatus::Satisfied),
            },
            "absent" => match inspect_state(ctx, &runtime, name).await? {
                Some(_) => Ok(ModuleStatus::Pending {
                    plan: format!("Remove container {}", name),
                }),
                None => Ok(ModuleStatus::Satisfied),
            },
            other => unreachable!("state {} rejected above", other),
        }
    }

    async fn apply(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleResult, GlideshError> {
        run_args::validate_params(params)?;

        let name = &params.resource_name;
        let state = desired_state(params);
        if !KNOWN_STATES.contains(&state) {
            return Err(GlideshError::Module {
                module: "container".to_string(),
                message: format!("Unknown state: {}", state),
            });
        }

        let resolution = self.resolve_runtime(ctx, params).await?;
        let runtime = resolution.runtime().to_string();
        let needs_install = matches!(resolution, RuntimeResolution::NeedsInstall(_));

        if ctx.dry_run {
            let planned = match state {
                "running" => run_args::build_run_command(&runtime, name, params)?,
                "run-once" => run_args::build_run_once_command(&runtime, name, params)?,
                "stopped" => format!("{} stop {}", runtime, shell_escape(name)),
                _ => format!("{} rm -f {}", runtime, shell_escape(name)),
            };
            let prefix = if needs_install {
                format!("install {}; ", runtime)
            } else {
                String::new()
            };
            return Ok(ModuleResult {
                changed: false,
                output: format!("[dry-run] {}{}", prefix, planned),
                stderr: String::new(),
                exit_code: 0,
            });
        }

        if needs_install {
            // Tearing down a container on a host with no runtime is a no-op;
            // installing one to discover that would be worse than pointless.
            if matches!(state, "stopped" | "absent") {
                return Ok(ModuleResult {
                    changed: false,
                    output: String::new(),
                    stderr: String::new(),
                    exit_code: 0,
                });
            }
            self.install_runtime(ctx, &runtime).await?;
        }

        match state {
            "running" => self.ensure_running(ctx, params, &runtime).await,
            "run-once" => self.run_once(ctx, params, &runtime).await,
            "stopped" => Self::ensure_stopped(ctx, name, &runtime).await,
            "absent" => Self::ensure_absent(ctx, name, &runtime).await,
            other => unreachable!("state {} rejected above", other),
        }
    }
}

impl ContainerModule {
    async fn check_running(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
        runtime: &str,
    ) -> Result<ModuleStatus, GlideshError> {
        let name = &params.resource_name;

        let Some(state) = inspect_state(ctx, runtime, name).await? else {
            return Ok(ModuleStatus::Pending {
                plan: format!("Create and start container {}", name),
            });
        };

        let desired_hash = run_args::spec_hash(runtime, params)?;
        if inspect_spec_hash(ctx, runtime, name).await? != desired_hash {
            return Ok(ModuleStatus::Pending {
                plan: format!("Recreate container {} (configuration changed)", name),
            });
        }

        match start_action(&state) {
            StartAction::None => {}
            StartAction::Recreate => {
                return Ok(ModuleStatus::Pending {
                    plan: format!("Recreate container {} (currently {})", name, state),
                });
            }
            _ => {
                return Ok(ModuleStatus::Pending {
                    plan: format!("Start container {} (currently {})", name, state),
                });
            }
        }

        if let Some(spec) = health::parse_wait(params)? {
            match health::probe(ctx, runtime, name, &spec).await? {
                health::Readiness::Ready => {}
                // It was running a moment ago and is not now; apply will start or
                // rebuild it as its state warrants.
                health::Readiness::Failed(reason) => {
                    return Ok(ModuleStatus::Pending {
                        plan: format!("Recover container {} ({})", name, reason),
                    });
                }
                health::Readiness::NotReady(_) => {
                    return Ok(ModuleStatus::Pending {
                        plan: format!("Wait for container {} to {}", name, health::describe(&spec)),
                    });
                }
            }
        }

        Ok(ModuleStatus::Satisfied)
    }

    /// A one-shot job has no lasting state to compare against, so it re-runs on
    /// every pass unless a `check` guard says the work is already done.
    async fn check_run_once(
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleStatus, GlideshError> {
        let pending = ModuleStatus::Pending {
            plan: format!("Run container {} to completion", params.resource_name),
        };

        let Some(gate) = params.args.get("check").and_then(|v| v.as_str()) else {
            return Ok(pending);
        };

        // A timed-out guard counts as "not satisfied" so the job still runs.
        match exec_timed(ctx, gate, parse_timeout(params)?).await? {
            Some(output) if output.exit_code == 0 => Ok(ModuleStatus::Satisfied),
            _ => Ok(pending),
        }
    }

    fn detected_runtime(ctx: &ModuleContext<'_>) -> Option<&'static str> {
        match &ctx.os_info.container_runtime {
            Some(ContainerRuntime::Podman) => Some("podman"),
            Some(ContainerRuntime::Docker) => Some("docker"),
            None => None,
        }
    }

    /// Decide which runtime to drive, without changing anything on the host.
    /// `runtime` is validated against [`run_args::SUPPORTED_RUNTIMES`] before this runs.
    async fn resolve_runtime(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<RuntimeResolution, GlideshError> {
        let preferred = params
            .args
            .get("runtime")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let should_install = params
            .args
            .get("install-runtime")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if let Some(detected) = Self::detected_runtime(ctx) {
            if preferred.is_empty() || preferred == detected {
                return Ok(RuntimeResolution::Present(detected.to_string()));
            }
            // Detection found one runtime and the plan asks for another: prefer the
            // plan's if it happens to be installed too.
            let present = ctx
                .exec(&format!("which {} 2>/dev/null", shell_escape(preferred)))
                .await?;
            if present.exit_code == 0 {
                return Ok(RuntimeResolution::Present(preferred.to_string()));
            }
            if !should_install {
                return Ok(RuntimeResolution::Present(detected.to_string()));
            }
            return Ok(RuntimeResolution::NeedsInstall(preferred.to_string()));
        }

        if !should_install {
            return Err(GlideshError::Module {
                module: "container".to_string(),
                message: "No container runtime found. Set install-runtime=true to auto-install."
                    .to_string(),
            });
        }

        let target = if preferred.is_empty() {
            "docker"
        } else {
            preferred
        };
        Ok(RuntimeResolution::NeedsInstall(target.to_string()))
    }

    /// Install a runtime. Never reached under `--dry-run`; `apply` gates the call.
    async fn install_runtime(
        &self,
        ctx: &ModuleContext<'_>,
        runtime: &str,
    ) -> Result<(), GlideshError> {
        let packages = runtime_packages(&ctx.os_info.pkg_manager, runtime);
        let install_cmd = ctx.os_info.pkg_manager.install_cmd(&packages);

        tracing::info!(
            "Installing container runtime '{}' via: {}",
            runtime,
            install_cmd
        );

        let output = ctx.exec(&install_cmd).await?;
        if output.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "container".to_string(),
                message: format!(
                    "Failed to install {} (exit {}): {}",
                    runtime, output.exit_code, output.stderr
                ),
            });
        }

        let service = match runtime {
            "podman" => "podman.socket",
            _ => "docker",
        };
        let _ = ctx
            .exec(&format!("systemctl enable --now {} 2>/dev/null", service))
            .await;

        Ok(())
    }

    async fn ensure_network(
        ctx: &ModuleContext<'_>,
        runtime: &str,
        network: &str,
    ) -> Result<(), GlideshError> {
        let inspect = ctx
            .exec(&format!(
                "{} network inspect {} 2>/dev/null",
                runtime,
                shell_escape(network)
            ))
            .await?;

        if inspect.exit_code == 0 {
            return Ok(());
        }

        let create = ctx
            .exec(&format!(
                "{} network create {}",
                runtime,
                shell_escape(network)
            ))
            .await?;

        if create.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "container".to_string(),
                message: format!(
                    "Failed to create network '{}' (exit {}): {}",
                    network, create.exit_code, create.stderr
                ),
            });
        }

        Ok(())
    }

    async fn ensure_declared_network(
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
        runtime: &str,
    ) -> Result<(), GlideshError> {
        if let Some(network) = params.args.get("network").and_then(|v| v.as_str()) {
            if !run_args::is_builtin_network(network) {
                Self::ensure_network(ctx, runtime, network).await?;
            }
        }
        Ok(())
    }

    async fn ensure_running(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
        runtime: &str,
    ) -> Result<ModuleResult, GlideshError> {
        let name = &params.resource_name;
        Self::ensure_declared_network(ctx, params, runtime).await?;

        let state = inspect_state(ctx, runtime, name).await?;
        let action = match &state {
            Some(current) => start_action(current),
            None => StartAction::Recreate,
        };

        let spec_matches = state.is_some()
            && inspect_spec_hash(ctx, runtime, name).await?
                == run_args::spec_hash(runtime, params)?;

        let (changed, output) = if spec_matches && action != StartAction::Recreate {
            match action {
                StartAction::None => (false, String::new()),
                StartAction::Start => (true, Self::lifecycle(ctx, runtime, "start", name).await?),
                StartAction::Unpause => {
                    (true, Self::lifecycle(ctx, runtime, "unpause", name).await?)
                }
                StartAction::Recreate => unreachable!("guarded above"),
            }
        } else {
            if state.is_some() {
                Self::remove_container(ctx, runtime, name).await?;
            }
            let cmd = run_args::build_run_command(runtime, name, params)?;
            let out = ctx.exec(&cmd).await?;
            if out.exit_code != 0 {
                return Err(GlideshError::Module {
                    module: "container".to_string(),
                    message: format!(
                        "Failed to run container '{}' (exit {}): {}",
                        name,
                        out.exit_code,
                        out.stderr.trim()
                    ),
                });
            }
            (true, out.stdout)
        };

        if let Some(spec) = health::parse_wait(params)? {
            health::wait_until_ready(ctx, runtime, name, &spec).await?;
        }

        Ok(ModuleResult {
            changed,
            output,
            stderr: String::new(),
            exit_code: 0,
        })
    }

    /// Run a container in the foreground to completion — a job, not a service.
    async fn run_once(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
        runtime: &str,
    ) -> Result<ModuleResult, GlideshError> {
        let name = &params.resource_name;
        Self::ensure_declared_network(ctx, params, runtime).await?;

        // A previous run that kept its container (`remove #false`, or a crash
        // before `--rm` fired) would otherwise collide on the name.
        if inspect_state(ctx, runtime, name).await?.is_some() {
            Self::remove_container(ctx, runtime, name).await?;
        }

        let cmd = run_args::build_run_once_command(runtime, name, params)?;
        let timeout = parse_timeout(params)?;
        let success_codes = parse_success_codes(params)?;
        let max_attempts = params
            .args
            .get("retries")
            .and_then(|v| v.as_i64())
            .unwrap_or(1)
            .max(1) as u32;
        let delay_secs = params
            .args
            .get("delay")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            .max(0) as u64;

        let mut last_failure = String::new();

        for attempt in 1..=max_attempts {
            match exec_timed(ctx, &cmd, timeout).await? {
                Some(out) if accepted(out.exit_code as i32, &success_codes) => {
                    return Ok(ModuleResult {
                        changed: true,
                        output: out.stdout,
                        stderr: out.stderr,
                        exit_code: out.exit_code as i32,
                    });
                }
                Some(out) => {
                    last_failure = format!(
                        "exit code {} is not in the accepted set ({})\nstdout: {}\nstderr: {}",
                        out.exit_code,
                        describe_success_codes(&success_codes),
                        out.stdout.trim(),
                        out.stderr.trim()
                    );
                }
                None => {
                    last_failure = format!("timed out after {}s", timeout.unwrap_or(0));
                }
            }

            // The failed attempt owns the name until it is cleared.
            let _ = Self::remove_container(ctx, runtime, name).await;

            if attempt < max_attempts && delay_secs > 0 {
                tokio::time::sleep(Duration::from_secs(delay_secs)).await;
            }
        }

        Err(GlideshError::Module {
            module: "container".to_string(),
            message: format!(
                "container '{}' failed after {} attempt(s): {}",
                name, max_attempts, last_failure
            ),
        })
    }

    async fn lifecycle(
        ctx: &ModuleContext<'_>,
        runtime: &str,
        verb: &str,
        name: &str,
    ) -> Result<String, GlideshError> {
        let out = ctx
            .exec(&format!("{} {} {}", runtime, verb, shell_escape(name)))
            .await?;
        if out.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "container".to_string(),
                message: format!(
                    "Failed to {} container '{}' (exit {}): {}",
                    verb,
                    name,
                    out.exit_code,
                    out.stderr.trim()
                ),
            });
        }
        Ok(out.stdout)
    }

    /// Remove a container and verify it is actually gone. Silently ignoring a
    /// failed removal leaves the follow-up `run` to fail with the runtime's
    /// opaque "name is already in use" instead of the real reason.
    async fn remove_container(
        ctx: &ModuleContext<'_>,
        runtime: &str,
        name: &str,
    ) -> Result<(), GlideshError> {
        let escaped = shell_escape(name);
        // Stop first so the workload gets its SIGTERM; `rm -f` then handles
        // whatever state it ended up in (including a stop that timed out).
        let out = ctx
            .exec(&format!(
                "{runtime} stop {escaped} >/dev/null 2>&1; {runtime} rm -f {escaped}"
            ))
            .await?;

        // `rm -f` on an already-absent container is a no-op on current Docker and
        // Podman but non-zero on older ones, so trust the follow-up inspect.
        if inspect_state(ctx, runtime, name).await?.is_some() {
            return Err(GlideshError::Module {
                module: "container".to_string(),
                message: format!(
                    "could not remove the existing container '{}' (exit {}): {}",
                    name,
                    out.exit_code,
                    out.stderr.trim()
                ),
            });
        }

        Ok(())
    }

    async fn ensure_stopped(
        ctx: &ModuleContext<'_>,
        name: &str,
        runtime: &str,
    ) -> Result<ModuleResult, GlideshError> {
        if inspect_state(ctx, runtime, name).await?.is_none() {
            return Ok(ModuleResult {
                changed: false,
                output: String::new(),
                stderr: String::new(),
                exit_code: 0,
            });
        }

        let output = Self::lifecycle(ctx, runtime, "stop", name).await?;
        Ok(ModuleResult {
            changed: true,
            output,
            stderr: String::new(),
            exit_code: 0,
        })
    }

    async fn ensure_absent(
        ctx: &ModuleContext<'_>,
        name: &str,
        runtime: &str,
    ) -> Result<ModuleResult, GlideshError> {
        if inspect_state(ctx, runtime, name).await?.is_none() {
            return Ok(ModuleResult {
                changed: false,
                output: String::new(),
                stderr: String::new(),
                exit_code: 0,
            });
        }

        Self::remove_container(ctx, runtime, name).await?;
        Ok(ModuleResult {
            changed: true,
            output: String::new(),
            stderr: String::new(),
            exit_code: 0,
        })
    }
}

/// The container's runtime status, or `None` when no container by that name exists.
pub(super) async fn inspect_state(
    ctx: &ModuleContext<'_>,
    runtime: &str,
    name: &str,
) -> Result<Option<String>, GlideshError> {
    let out = ctx
        .exec(&format!(
            "{} container inspect --format '{{{{.State.Status}}}}' {} 2>/dev/null",
            runtime,
            shell_escape(name)
        ))
        .await?;
    if out.exit_code != 0 {
        return Ok(None);
    }
    let status = out.stdout.trim();
    if status.is_empty() {
        Ok(None)
    } else {
        Ok(Some(status.to_string()))
    }
}

/// The spec hash recorded on a live container, or an empty string if it carries none.
async fn inspect_spec_hash(
    ctx: &ModuleContext<'_>,
    runtime: &str,
    name: &str,
) -> Result<String, GlideshError> {
    let out = ctx
        .exec(&format!(
            "{} container inspect --format '{{{{index .Config.Labels \"{}\"}}}}' {} 2>/dev/null",
            runtime,
            run_args::PARAM_HASH_LABEL,
            shell_escape(name)
        ))
        .await?;
    Ok(out.stdout.trim().to_string())
}

/// Return the package names to install for a given runtime and package manager.
fn runtime_packages(pkg: &PkgManager, runtime: &str) -> Vec<String> {
    match runtime {
        "podman" => vec!["podman".to_string()],
        _ => match pkg {
            // Debian/Ubuntu use docker.io from distro repos
            PkgManager::Apt => vec!["docker.io".to_string()],
            PkgManager::Pacman => vec!["docker".to_string()],
            PkgManager::Apk => vec!["docker".to_string()],
            _ => vec!["docker-ce".to_string()],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_container_needs_no_action() {
        assert_eq!(start_action("running"), StartAction::None);
        assert_eq!(start_action("restarting"), StartAction::None);
    }

    #[test]
    fn stopped_container_is_started_not_left_behind() {
        assert_eq!(start_action("exited"), StartAction::Start);
        assert_eq!(start_action("created"), StartAction::Start);
    }

    #[test]
    fn paused_container_is_unpaused() {
        assert_eq!(start_action("paused"), StartAction::Unpause);
    }

    #[test]
    fn unsalvageable_states_force_a_rebuild() {
        assert_eq!(start_action("dead"), StartAction::Recreate);
        assert_eq!(start_action("removing"), StartAction::Recreate);
        assert_eq!(start_action("something-new"), StartAction::Recreate);
    }

    #[test]
    fn state_defaults_to_running() {
        let params = ModuleParams {
            resource_name: "c".to_string(),
            args: Default::default(),
        };
        assert_eq!(desired_state(&params), "running");
    }

    #[test]
    fn runtime_packages_per_manager() {
        assert_eq!(runtime_packages(&PkgManager::Apt, "podman"), ["podman"]);
        assert_eq!(runtime_packages(&PkgManager::Apt, "docker"), ["docker.io"]);
        assert_eq!(runtime_packages(&PkgManager::Pacman, "docker"), ["docker"]);
    }
}
