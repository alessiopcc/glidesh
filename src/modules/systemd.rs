use crate::error::GlideshError;
use crate::modules::context::ModuleContext;
use crate::modules::{Module, ModuleParams, ModuleResult, ModuleStatus};
use async_trait::async_trait;

pub struct SystemdModule;

#[async_trait]
impl Module for SystemdModule {
    fn name(&self) -> &str {
        "systemd"
    }

    async fn check(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleStatus, GlideshError> {
        let unit = &params.resource_name;
        let desired_state = params
            .args
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("started");
        let desired_enabled = params.args.get("enabled").and_then(|v| v.as_bool());

        let mut needs_change = Vec::new();

        let is_active = ctx
            .ssh
            .exec(&format!("systemctl is-active {} 2>/dev/null", unit))
            .await?;
        let active = is_active.stdout.trim() == "active";

        match desired_state {
            "started" if !active => needs_change.push("start"),
            "stopped" if active => needs_change.push("stop"),
            "restarted" => needs_change.push("restart"),
            _ => {}
        }

        if let Some(want_enabled) = desired_enabled {
            let is_enabled = ctx
                .ssh
                .exec(&format!("systemctl is-enabled {} 2>/dev/null", unit))
                .await?;
            let enabled = is_enabled.stdout.trim() == "enabled";
            if want_enabled && !enabled {
                needs_change.push("enable");
            } else if !want_enabled && enabled {
                needs_change.push("disable");
            }
        }

        if needs_change.is_empty() {
            Ok(ModuleStatus::Satisfied)
        } else {
            Ok(ModuleStatus::Pending {
                plan: format!("systemd {}: {}", unit, needs_change.join(", ")),
            })
        }
    }

    async fn apply(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleResult, GlideshError> {
        let unit = &params.resource_name;
        let desired_state = params
            .args
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("started");
        let desired_enabled = params.args.get("enabled").and_then(|v| v.as_bool());

        if ctx.dry_run {
            return Ok(ModuleResult {
                changed: false,
                output: format!("[dry-run] Would manage systemd unit {}", unit),
                stderr: String::new(),
                exit_code: 0,
            });
        }

        let mut commands = Vec::new();

        if let Some(want_enabled) = desired_enabled {
            if want_enabled {
                commands.push(format!("systemctl enable {}", unit));
            } else {
                commands.push(format!("systemctl disable {}", unit));
            }
        }

        match desired_state {
            "started" => commands.push(format!("systemctl start {}", unit)),
            "stopped" => commands.push(format!("systemctl stop {}", unit)),
            "restarted" => commands.push(format!("systemctl restart {}", unit)),
            _ => {}
        }

        let combined = commands.join(" && ");
        let output = ctx.ssh.exec(&combined).await?;

        if output.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "systemd".to_string(),
                message: format!(
                    "systemd operation failed for '{}' (exit {}): {}",
                    unit, output.exit_code, output.stderr
                ),
            });
        }

        Ok(ModuleResult {
            changed: true,
            output: output.stdout,
            stderr: output.stderr,
            exit_code: output.exit_code as i32,
        })
    }
}
