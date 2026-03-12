use crate::error::GlideshError;
use crate::modules::context::ModuleContext;
use crate::modules::{Module, ModuleParams, ModuleResult, ModuleStatus};
use async_trait::async_trait;

pub struct PackageModule;

#[async_trait]
impl Module for PackageModule {
    fn name(&self) -> &str {
        "package"
    }

    async fn check(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleStatus, GlideshError> {
        let package = &params.resource_name;
        let desired_state = params
            .args
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("present");

        let check_cmd = ctx.os_info.pkg_manager.check_installed_cmd(package);
        let output = ctx.ssh.exec(&check_cmd).await?;
        let is_installed = output.exit_code == 0;

        match (desired_state, is_installed) {
            ("present", true) | ("absent", false) => Ok(ModuleStatus::Satisfied),
            ("present", false) => Ok(ModuleStatus::Pending {
                plan: format!("Install package {}", package),
            }),
            ("absent", true) => Ok(ModuleStatus::Pending {
                plan: format!("Remove package {}", package),
            }),
            _ => Ok(ModuleStatus::Unknown {
                reason: format!("Unknown state: {}", desired_state),
            }),
        }
    }

    async fn apply(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleResult, GlideshError> {
        let package = &params.resource_name;
        let desired_state = params
            .args
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("present");

        if ctx.dry_run {
            return Ok(ModuleResult {
                changed: false,
                output: format!("[dry-run] Would {} package {}", desired_state, package),
                stderr: String::new(),
                exit_code: 0,
            });
        }

        let cmd = match desired_state {
            "present" => {
                let update_cmd = ctx.os_info.pkg_manager.update_index_cmd();
                ctx.ssh.exec(update_cmd).await?;
                ctx.os_info
                    .pkg_manager
                    .install_cmd(std::slice::from_ref(package))
            }
            "absent" => ctx
                .os_info
                .pkg_manager
                .remove_cmd(std::slice::from_ref(package)),
            _ => {
                return Err(GlideshError::Module {
                    module: "package".to_string(),
                    message: format!("Unknown state: {}", desired_state),
                });
            }
        };

        let output = ctx.ssh.exec(&cmd).await?;

        if output.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "package".to_string(),
                message: format!(
                    "Package operation failed for '{}' (exit {}): {}",
                    package, output.exit_code, output.stderr
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
