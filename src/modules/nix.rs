use crate::error::GlideshError;
use crate::modules::context::ModuleContext;
use crate::modules::{Module, ModuleParams, ModuleResult, ModuleStatus};
use async_trait::async_trait;

pub struct NixModule;

const NIX_INSTALLER_URL: &str = "https://install.determinate.systems/nix";

impl NixModule {
    /// Ensure Nix is available on the target. If not installed and `install=#true`,
    /// auto-install via the Determinate Systems installer.
    async fn ensure_nix(
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<(), GlideshError> {
        if ctx.os_info.nix_installed {
            return Ok(());
        }

        let should_install = params
            .args
            .get("install")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if !should_install {
            return Err(GlideshError::Module {
                module: "nix".to_string(),
                message: "Nix is not installed on the target. Set install=#true to auto-install."
                    .to_string(),
            });
        }

        if ctx.dry_run {
            return Ok(());
        }

        tracing::info!("Installing Nix via Determinate Systems installer");

        let cmd = format!(
            "curl -sSf -L {} | sh -s -- install --no-confirm",
            NIX_INSTALLER_URL
        );
        let output = ctx.ssh.exec(&cmd).await?;

        if output.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "nix".to_string(),
                message: format!(
                    "Nix installation failed (exit {}): {}",
                    output.exit_code, output.stderr
                ),
            });
        }

        Ok(())
    }

    fn get_action(params: &ModuleParams) -> &str {
        params
            .args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("install")
    }

    async fn check_install(
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleStatus, GlideshError> {
        let package = &params.resource_name;
        let desired_state = params
            .args
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("present");

        let check_cmd = format!(
            "nix-env -q '{}' 2>/dev/null | grep -qw '{}'",
            package, package
        );
        let output = ctx.ssh.exec(&check_cmd).await?;
        let is_installed = output.exit_code == 0;

        match (desired_state, is_installed) {
            ("present", true) | ("absent", false) => Ok(ModuleStatus::Satisfied),
            ("present", false) => Ok(ModuleStatus::Pending {
                plan: format!("Install Nix package {}", package),
            }),
            ("absent", true) => Ok(ModuleStatus::Pending {
                plan: format!("Remove Nix package {}", package),
            }),
            _ => Ok(ModuleStatus::Unknown {
                reason: format!("Unknown state: {}", desired_state),
            }),
        }
    }

    async fn apply_install(
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
                output: format!("[dry-run] Would {} Nix package {}", desired_state, package),
                stderr: String::new(),
                exit_code: 0,
            });
        }

        let cmd = match desired_state {
            "present" => {
                // Try nix profile first, fall back to nix-env
                format!(
                    "nix profile install 'nixpkgs#{}' 2>/dev/null || nix-env -iA nixpkgs.{}",
                    package, package
                )
            }
            "absent" => {
                format!(
                    "nix profile remove 'nixpkgs#{}' 2>/dev/null || nix-env -e '{}'",
                    package, package
                )
            }
            _ => {
                return Err(GlideshError::Module {
                    module: "nix".to_string(),
                    message: format!("Unknown state: {}", desired_state),
                });
            }
        };

        let output = ctx.ssh.exec(&cmd).await?;

        if output.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "nix".to_string(),
                message: format!(
                    "Nix package operation failed for '{}' (exit {}): {}",
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

    async fn check_shell(
        _ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleStatus, GlideshError> {
        Ok(ModuleStatus::Pending {
            plan: format!("Run command in Nix shell: {}", params.resource_name),
        })
    }

    async fn apply_shell(
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleResult, GlideshError> {
        let command = &params.resource_name;

        let packages = params
            .args
            .get("packages")
            .and_then(|v| v.as_list())
            .ok_or_else(|| GlideshError::Module {
                module: "nix".to_string(),
                message: "action=\"shell\" requires a 'packages' list".to_string(),
            })?;

        if ctx.dry_run {
            return Ok(ModuleResult {
                changed: false,
                output: format!("[dry-run] Would run '{}' in Nix shell", command),
                stderr: String::new(),
                exit_code: 0,
            });
        }

        let pkg_args: Vec<String> = packages
            .iter()
            .map(|p| {
                if p.contains('#') {
                    p.clone()
                } else {
                    format!("nixpkgs#{}", p)
                }
            })
            .collect();

        let cmd = format!("nix shell {} --command {}", pkg_args.join(" "), command);

        let output = ctx.ssh.exec(&cmd).await?;

        if output.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "nix".to_string(),
                message: format!(
                    "Nix shell command failed (exit {}): {}",
                    output.exit_code, output.stderr
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

    async fn check_build(
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleStatus, GlideshError> {
        let out_link = params
            .args
            .get("out-link")
            .and_then(|v| v.as_str())
            .unwrap_or("result");

        let check_cmd = format!("test -L '{}'", out_link);
        let output = ctx.ssh.exec(&check_cmd).await?;

        if output.exit_code == 0 {
            // Symlink exists — but we can't cheaply check if the derivation changed
            // without building, so we still report Pending for safety
            Ok(ModuleStatus::Pending {
                plan: format!("Rebuild Nix derivation {}", params.resource_name),
            })
        } else {
            Ok(ModuleStatus::Pending {
                plan: format!("Build Nix derivation {}", params.resource_name),
            })
        }
    }

    async fn apply_build(
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleResult, GlideshError> {
        let derivation = &params.resource_name;
        let out_link = params
            .args
            .get("out-link")
            .and_then(|v| v.as_str())
            .unwrap_or("result");

        if ctx.dry_run {
            return Ok(ModuleResult {
                changed: false,
                output: format!("[dry-run] Would build {}", derivation),
                stderr: String::new(),
                exit_code: 0,
            });
        }

        let cmd = format!("nix build '{}' -o '{}'", derivation, out_link);
        let output = ctx.ssh.exec(&cmd).await?;

        if output.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "nix".to_string(),
                message: format!(
                    "Nix build failed for '{}' (exit {}): {}",
                    derivation, output.exit_code, output.stderr
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

    async fn check_channel(
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleStatus, GlideshError> {
        let channel_name = &params.resource_name;
        let desired_state = params
            .args
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("present");

        let check_cmd = format!(
            "nix-channel --list 2>/dev/null | grep -qw '{}'",
            channel_name
        );
        let output = ctx.ssh.exec(&check_cmd).await?;
        let exists = output.exit_code == 0;

        match (desired_state, exists) {
            ("present", true) | ("absent", false) => Ok(ModuleStatus::Satisfied),
            ("present", false) => Ok(ModuleStatus::Pending {
                plan: format!("Add Nix channel {}", channel_name),
            }),
            ("absent", true) => Ok(ModuleStatus::Pending {
                plan: format!("Remove Nix channel {}", channel_name),
            }),
            _ => Ok(ModuleStatus::Unknown {
                reason: format!("Unknown state: {}", desired_state),
            }),
        }
    }

    async fn apply_channel(
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleResult, GlideshError> {
        let channel_name = &params.resource_name;
        let desired_state = params
            .args
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("present");

        if ctx.dry_run {
            return Ok(ModuleResult {
                changed: false,
                output: format!("[dry-run] Would {} channel {}", desired_state, channel_name),
                stderr: String::new(),
                exit_code: 0,
            });
        }

        let cmd = match desired_state {
            "present" => {
                let url = params
                    .args
                    .get("url")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| GlideshError::Module {
                        module: "nix".to_string(),
                        message: "Channel action with state=\"present\" requires 'url' parameter"
                            .to_string(),
                    })?;
                format!("nix-channel --add '{}' '{}'", url, channel_name)
            }
            "absent" => format!("nix-channel --remove '{}'", channel_name),
            _ => {
                return Err(GlideshError::Module {
                    module: "nix".to_string(),
                    message: format!("Unknown state: {}", desired_state),
                });
            }
        };

        let output = ctx.ssh.exec(&cmd).await?;

        if output.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "nix".to_string(),
                message: format!(
                    "Channel operation failed for '{}' (exit {}): {}",
                    channel_name, output.exit_code, output.stderr
                ),
            });
        }

        let should_update = params
            .args
            .get("update")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        if should_update {
            let update_output = ctx.ssh.exec("nix-channel --update").await?;
            if update_output.exit_code != 0 {
                return Err(GlideshError::Module {
                    module: "nix".to_string(),
                    message: format!(
                        "Channel update failed (exit {}): {}",
                        update_output.exit_code, update_output.stderr
                    ),
                });
            }
        }

        Ok(ModuleResult {
            changed: true,
            output: output.stdout,
            stderr: output.stderr,
            exit_code: output.exit_code as i32,
        })
    }

    async fn check_flake_update(
        _ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleStatus, GlideshError> {
        Ok(ModuleStatus::Pending {
            plan: format!("Update flake inputs in {}", params.resource_name),
        })
    }

    async fn apply_flake_update(
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleResult, GlideshError> {
        let flake_dir = &params.resource_name;

        if ctx.dry_run {
            return Ok(ModuleResult {
                changed: false,
                output: format!("[dry-run] Would update flake in {}", flake_dir),
                stderr: String::new(),
                exit_code: 0,
            });
        }

        let cmd = if let Some(input) = params.args.get("input").and_then(|v| v.as_str()) {
            format!("cd '{}' && nix flake update '{}'", flake_dir, input)
        } else {
            format!("cd '{}' && nix flake update", flake_dir)
        };

        let output = ctx.ssh.exec(&cmd).await?;

        if output.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "nix".to_string(),
                message: format!(
                    "Flake update failed in '{}' (exit {}): {}",
                    flake_dir, output.exit_code, output.stderr
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

    async fn check_gc(
        _ctx: &ModuleContext<'_>,
        _params: &ModuleParams,
    ) -> Result<ModuleStatus, GlideshError> {
        Ok(ModuleStatus::Pending {
            plan: "Garbage collect Nix store".to_string(),
        })
    }

    async fn apply_gc(
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleResult, GlideshError> {
        if ctx.dry_run {
            return Ok(ModuleResult {
                changed: false,
                output: "[dry-run] Would garbage collect Nix store".to_string(),
                stderr: String::new(),
                exit_code: 0,
            });
        }

        let cmd = if let Some(older_than) = params.args.get("older-than").and_then(|v| v.as_str()) {
            format!("nix-collect-garbage --delete-older-than '{}'", older_than)
        } else {
            "nix-collect-garbage -d".to_string()
        };

        let output = ctx.ssh.exec(&cmd).await?;

        if output.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "nix".to_string(),
                message: format!(
                    "Nix garbage collection failed (exit {}): {}",
                    output.exit_code, output.stderr
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

#[async_trait]
impl Module for NixModule {
    fn name(&self) -> &str {
        "nix"
    }

    async fn check(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleStatus, GlideshError> {
        Self::ensure_nix(ctx, params).await?;

        match Self::get_action(params) {
            "install" => Self::check_install(ctx, params).await,
            "shell" => Self::check_shell(ctx, params).await,
            "build" => Self::check_build(ctx, params).await,
            "channel" => Self::check_channel(ctx, params).await,
            "flake-update" => Self::check_flake_update(ctx, params).await,
            "gc" => Self::check_gc(ctx, params).await,
            other => Err(GlideshError::Module {
                module: "nix".to_string(),
                message: format!("Unknown action: {}", other),
            }),
        }
    }

    async fn apply(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleResult, GlideshError> {
        Self::ensure_nix(ctx, params).await?;

        match Self::get_action(params) {
            "install" => Self::apply_install(ctx, params).await,
            "shell" => Self::apply_shell(ctx, params).await,
            "build" => Self::apply_build(ctx, params).await,
            "channel" => Self::apply_channel(ctx, params).await,
            "flake-update" => Self::apply_flake_update(ctx, params).await,
            "gc" => Self::apply_gc(ctx, params).await,
            other => Err(GlideshError::Module {
                module: "nix".to_string(),
                message: format!("Unknown action: {}", other),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_action_default() {
        let params = ModuleParams {
            resource_name: "htop".to_string(),
            args: std::collections::HashMap::new(),
        };
        assert_eq!(NixModule::get_action(&params), "install");
    }

    #[test]
    fn test_get_action_explicit() {
        let mut args = std::collections::HashMap::new();
        args.insert(
            "action".to_string(),
            crate::config::types::ParamValue::String("shell".to_string()),
        );
        let params = ModuleParams {
            resource_name: "echo hello".to_string(),
            args,
        };
        assert_eq!(NixModule::get_action(&params), "shell");
    }
}
