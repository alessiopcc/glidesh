//! Readiness gating for containers.
//!
//! `subscribe` orders steps but says nothing about whether the service a step
//! started is actually usable yet. A container task can declare what "ready"
//! means — the runtime health status, or a probe run from the control host — and
//! the step then blocks until that holds, so the next step really can talk to it.

use crate::error::GlideshError;
use crate::modules::ModuleParams;
use crate::modules::context::ModuleContext;
use crate::util::shell_escape;
use std::time::{Duration, Instant};

pub(super) const DEFAULT_WAIT_TIMEOUT: u64 = 300;
pub(super) const DEFAULT_WAIT_INTERVAL: u64 = 3;
const LOG_TAIL_LINES: u32 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WaitTarget {
    /// The container process is up. Catches crash-on-boot, nothing more.
    Running,
    /// The runtime's own healthcheck reports `healthy`.
    Healthy,
}

#[derive(Debug, Clone)]
pub(super) struct WaitSpec {
    pub target: WaitTarget,
    pub timeout: u64,
    pub interval: u64,
    /// Probe executed on the target host (not inside the container), ready on
    /// exit code 0. Useful when the image has no shell or no HTTP client.
    pub ready_cmd: Option<String>,
}

#[derive(Debug)]
pub(super) enum Readiness {
    Ready,
    /// Not there yet, but still plausibly on its way.
    NotReady(String),
    /// The container stopped. Terminal for this wait; the caller decides whether
    /// to recover it. A readiness condition that can never hold is not this — it
    /// is a plan error, and [`probe`] returns `Err` for that instead.
    Failed(String),
}

/// Parse the readiness parameters. Returns `None` when the task declares no
/// readiness condition, in which case starting the container is the whole job.
pub(super) fn parse_wait(params: &ModuleParams) -> Result<Option<WaitSpec>, GlideshError> {
    let ready_cmd = match params.args.get("ready-cmd") {
        None => None,
        Some(value) => {
            let cmd = value
                .as_str()
                .ok_or_else(|| invalid("ready-cmd must be a string"))?;
            let cmd = cmd.trim();
            if cmd.is_empty() {
                None
            } else {
                Some(cmd.to_string())
            }
        }
    };

    let declared = match params.args.get("wait") {
        None => None,
        Some(value) => {
            let raw = value
                .as_str()
                .ok_or_else(|| invalid("wait must be one of \"healthy\", \"running\", \"none\""))?;
            match raw.trim() {
                "healthy" => Some(WaitTarget::Healthy),
                "running" => Some(WaitTarget::Running),
                "none" | "" => return Ok(None),
                other => {
                    return Err(invalid(&format!(
                        "unknown wait target '{}' (expected \"healthy\", \"running\" or \"none\")",
                        other
                    )));
                }
            }
        }
    };

    let target = match (declared, &ready_cmd) {
        (Some(t), _) => t,
        // A host-side probe implies the container must at least be up.
        (None, Some(_)) => WaitTarget::Running,
        (None, None) => return Ok(None),
    };

    Ok(Some(WaitSpec {
        target,
        timeout: positive_secs(params, "wait-timeout", DEFAULT_WAIT_TIMEOUT)?,
        interval: positive_secs(params, "wait-interval", DEFAULT_WAIT_INTERVAL)?,
        ready_cmd,
    }))
}

fn positive_secs(params: &ModuleParams, key: &str, default: u64) -> Result<u64, GlideshError> {
    match params.args.get(key) {
        None => Ok(default),
        Some(value) => {
            let secs = value
                .as_i64()
                .ok_or_else(|| invalid(&format!("{} must be an integer number of seconds", key)))?;
            if secs <= 0 {
                return Err(invalid(&format!("{} must be greater than 0", key)));
            }
            Ok(secs as u64)
        }
    }
}

fn invalid(message: &str) -> GlideshError {
    GlideshError::Module {
        module: "container".to_string(),
        message: message.to_string(),
    }
}

/// Evaluate the readiness condition once.
pub(super) async fn probe(
    ctx: &ModuleContext<'_>,
    runtime: &str,
    name: &str,
    spec: &WaitSpec,
) -> Result<Readiness, GlideshError> {
    let state = super::inspect_state(ctx, runtime, name)
        .await?
        .unwrap_or_default();
    match state.as_str() {
        "running" => {}
        "exited" | "dead" | "removing" => {
            return Ok(Readiness::Failed(format!("container is {}", state)));
        }
        "" => return Ok(Readiness::Failed("container does not exist".to_string())),
        other => return Ok(Readiness::NotReady(format!("container is {}", other))),
    }

    if spec.target == WaitTarget::Healthy {
        match health_status(ctx, runtime, name).await? {
            // Not a state to wait out: no amount of polling gives a container a
            // healthcheck it was never given. Failing here rather than reporting
            // "waiting" keeps `check` honest, including under --dry-run.
            None => {
                return Err(invalid(&format!(
                    "container '{}' declares wait=\"healthy\" but has no healthcheck — \
                     add a `healthcheck` block, or use `ready-cmd` to probe from the host",
                    name
                )));
            }
            Some(status) if status == "healthy" => {}
            Some(status) => return Ok(Readiness::NotReady(format!("health is {}", status))),
        }
    }

    if let Some(cmd) = &spec.ready_cmd {
        let out = ctx.exec(cmd).await?;
        if out.exit_code != 0 {
            return Ok(Readiness::NotReady(format!(
                "ready-cmd exited {}",
                out.exit_code
            )));
        }
    }

    Ok(Readiness::Ready)
}

/// Poll until the readiness condition holds, or fail with the container's own
/// log tail attached — the log is nearly always where the real answer is.
pub(super) async fn wait_until_ready(
    ctx: &ModuleContext<'_>,
    runtime: &str,
    name: &str,
    spec: &WaitSpec,
) -> Result<(), GlideshError> {
    let deadline = Instant::now() + Duration::from_secs(spec.timeout);

    let reason = loop {
        match probe(ctx, runtime, name, spec).await? {
            Readiness::Ready => return Ok(()),
            Readiness::Failed(reason) => {
                return Err(readiness_error(ctx, runtime, name, &reason).await);
            }
            Readiness::NotReady(reason) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break format!("timed out after {}s waiting: {}", spec.timeout, reason);
                }
                tokio::time::sleep(remaining.min(Duration::from_secs(spec.interval))).await;
            }
        }
    };

    Err(readiness_error(ctx, runtime, name, &reason).await)
}

/// Short description of what the task is waiting for, for `check`'s plan line.
pub(super) fn describe(spec: &WaitSpec) -> &'static str {
    match (spec.target, spec.ready_cmd.is_some()) {
        (WaitTarget::Healthy, _) => "become healthy",
        (WaitTarget::Running, true) => "pass its readiness probe",
        (WaitTarget::Running, false) => "be running",
    }
}

/// Docker exposes the healthcheck result at `.State.Health`; older Podman uses
/// `.State.Healthcheck`. Try both before concluding there is no healthcheck.
async fn health_status(
    ctx: &ModuleContext<'_>,
    runtime: &str,
    name: &str,
) -> Result<Option<String>, GlideshError> {
    for field in ["Health", "Healthcheck"] {
        let out = ctx
            .exec(&format!(
                "{} container inspect --format '{{{{.State.{}.Status}}}}' {} 2>/dev/null",
                runtime,
                field,
                shell_escape(name)
            ))
            .await?;
        if out.exit_code != 0 {
            continue;
        }
        let status = out.stdout.trim();
        if !status.is_empty() && status != "<no value>" && status != "<nil>" {
            return Ok(Some(status.to_string()));
        }
    }
    Ok(None)
}

async fn readiness_error(
    ctx: &ModuleContext<'_>,
    runtime: &str,
    name: &str,
    reason: &str,
) -> GlideshError {
    let logs = ctx
        .exec(&format!(
            "{} logs --tail {} {} 2>&1",
            runtime,
            LOG_TAIL_LINES,
            shell_escape(name)
        ))
        .await
        .map(|out| out.stdout.trim().to_string())
        .unwrap_or_default();

    let message = if logs.is_empty() {
        format!("container '{}' never became ready: {}", name, reason)
    } else {
        format!(
            "container '{}' never became ready: {}\nlast {} log lines:\n{}",
            name, reason, LOG_TAIL_LINES, logs
        )
    };
    GlideshError::Module {
        module: "container".to_string(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::ParamValue;

    fn make_params(args: Vec<(&str, ParamValue)>) -> ModuleParams {
        ModuleParams {
            resource_name: "c".to_string(),
            args: args.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        }
    }

    #[test]
    fn absent_wait_means_no_gating() {
        assert!(parse_wait(&make_params(vec![])).unwrap().is_none());
    }

    #[test]
    fn wait_none_disables_gating() {
        let params = make_params(vec![("wait", ParamValue::String("none".into()))]);
        assert!(parse_wait(&params).unwrap().is_none());
    }

    #[test]
    fn wait_healthy_uses_defaults() {
        let params = make_params(vec![("wait", ParamValue::String("healthy".into()))]);
        let spec = parse_wait(&params).unwrap().unwrap();
        assert_eq!(spec.target, WaitTarget::Healthy);
        assert_eq!(spec.timeout, DEFAULT_WAIT_TIMEOUT);
        assert_eq!(spec.interval, DEFAULT_WAIT_INTERVAL);
        assert!(spec.ready_cmd.is_none());
    }

    #[test]
    fn ready_cmd_alone_implies_running() {
        let params = make_params(vec![(
            "ready-cmd",
            ParamValue::String("curl -sf localhost:8000/health".into()),
        )]);
        let spec = parse_wait(&params).unwrap().unwrap();
        assert_eq!(spec.target, WaitTarget::Running);
        assert_eq!(
            spec.ready_cmd.as_deref(),
            Some("curl -sf localhost:8000/health")
        );
    }

    #[test]
    fn timeout_and_interval_are_overridable() {
        let params = make_params(vec![
            ("wait", ParamValue::String("healthy".into())),
            ("wait-timeout", ParamValue::Integer(900)),
            ("wait-interval", ParamValue::Integer(10)),
        ]);
        let spec = parse_wait(&params).unwrap().unwrap();
        assert_eq!(spec.timeout, 900);
        assert_eq!(spec.interval, 10);
    }

    #[test]
    fn unknown_wait_target_is_rejected() {
        let params = make_params(vec![("wait", ParamValue::String("ready".into()))]);
        assert!(parse_wait(&params).is_err());
    }

    #[test]
    fn non_string_wait_is_rejected_rather_than_ignored() {
        let params = make_params(vec![("wait", ParamValue::Bool(true))]);
        assert!(parse_wait(&params).is_err());
    }

    #[test]
    fn non_positive_timeout_is_rejected() {
        let params = make_params(vec![
            ("wait", ParamValue::String("running".into())),
            ("wait-timeout", ParamValue::Integer(0)),
        ]);
        assert!(parse_wait(&params).is_err());
    }

    #[test]
    fn describe_covers_each_shape() {
        let healthy = WaitSpec {
            target: WaitTarget::Healthy,
            timeout: 1,
            interval: 1,
            ready_cmd: None,
        };
        let probe = WaitSpec {
            target: WaitTarget::Running,
            timeout: 1,
            interval: 1,
            ready_cmd: Some("true".into()),
        };
        let running = WaitSpec {
            target: WaitTarget::Running,
            timeout: 1,
            interval: 1,
            ready_cmd: None,
        };
        assert_eq!(describe(&healthy), "become healthy");
        assert_eq!(describe(&probe), "pass its readiness probe");
        assert_eq!(describe(&running), "be running");
    }
}
