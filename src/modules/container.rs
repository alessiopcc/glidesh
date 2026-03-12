use crate::error::GlideshError;
use crate::modules::context::ModuleContext;
use crate::modules::detect::ContainerRuntime;
use crate::modules::detect::PkgManager;
use crate::modules::{Module, ModuleParams, ModuleResult, ModuleStatus};
use async_trait::async_trait;

pub struct ContainerModule;

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
        let container_name = &params.resource_name;
        let desired_state = params
            .args
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("running");

        let runtime = self.resolve_runtime(ctx, params).await?;

        let output = ctx
            .ssh
            .exec(&format!(
                "{} inspect --format '{{{{.State.Status}}}}' {} 2>/dev/null",
                runtime, container_name
            ))
            .await?;

        let current_state = output.stdout.trim().to_string();
        let exists = output.exit_code == 0;

        match desired_state {
            "running" => {
                if exists && current_state == "running" {
                    if let Some(raw_image) = params.args.get("image").and_then(|v| v.as_str()) {
                        let desired_image = qualify_image(raw_image, &runtime);
                        let img_output = ctx
                            .ssh
                            .exec(&format!(
                                "{} inspect --format '{{{{.Config.Image}}}}' {} 2>/dev/null",
                                runtime, container_name
                            ))
                            .await?;
                        let current_image = img_output.stdout.trim();
                        if current_image != desired_image.as_str() {
                            return Ok(ModuleStatus::Pending {
                                plan: format!(
                                    "Recreate container {} (image {} -> {})",
                                    container_name, current_image, desired_image
                                ),
                            });
                        }
                    }
                    Ok(ModuleStatus::Satisfied)
                } else {
                    Ok(ModuleStatus::Pending {
                        plan: format!("Start container {}", container_name),
                    })
                }
            }
            "stopped" => {
                if exists && current_state != "running" {
                    Ok(ModuleStatus::Satisfied)
                } else if exists {
                    Ok(ModuleStatus::Pending {
                        plan: format!("Stop container {}", container_name),
                    })
                } else {
                    Ok(ModuleStatus::Satisfied)
                }
            }
            "absent" => {
                if exists {
                    Ok(ModuleStatus::Pending {
                        plan: format!("Remove container {}", container_name),
                    })
                } else {
                    Ok(ModuleStatus::Satisfied)
                }
            }
            _ => Ok(ModuleStatus::Unknown {
                reason: format!("Unknown container state: {}", desired_state),
            }),
        }
    }

    async fn apply(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleResult, GlideshError> {
        let container_name = &params.resource_name;
        let desired_state = params
            .args
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("running");

        if ctx.dry_run {
            return Ok(ModuleResult {
                changed: false,
                output: format!("[dry-run] Would manage container {}", container_name),
                stderr: String::new(),
                exit_code: 0,
            });
        }

        let runtime = self.resolve_runtime(ctx, params).await?;

        match desired_state {
            "running" => self.ensure_running(ctx, params, &runtime).await,
            "stopped" => self.ensure_stopped(ctx, container_name, &runtime).await,
            "absent" => self.ensure_absent(ctx, container_name, &runtime).await,
            _ => Err(GlideshError::Module {
                module: "container".to_string(),
                message: format!("Unknown state: {}", desired_state),
            }),
        }
    }
}

impl ContainerModule {
    fn detected_runtime(ctx: &ModuleContext<'_>) -> Option<&'static str> {
        match &ctx.os_info.container_runtime {
            Some(ContainerRuntime::Podman) => Some("podman"),
            Some(ContainerRuntime::Docker) => Some("docker"),
            None => None,
        }
    }

    /// Resolve which runtime to use. If `install-runtime` is set and no runtime
    /// is detected, install the requested one (podman or docker) via the host's
    /// package manager.
    async fn resolve_runtime(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<String, GlideshError> {
        let preferred = params
            .args
            .get("runtime")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if let Some(detected) = Self::detected_runtime(ctx) {
            if !preferred.is_empty() && preferred != detected {
                let check = ctx
                    .ssh
                    .exec(&format!("which {} 2>/dev/null", preferred))
                    .await?;
                if check.exit_code == 0 {
                    return Ok(preferred.to_string());
                }
                let should_install = params
                    .args
                    .get("install-runtime")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if !should_install {
                    return Ok(detected.to_string());
                }
            } else {
                return Ok(detected.to_string());
            }
        }

        let should_install = params
            .args
            .get("install-runtime")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if !should_install {
            return Err(GlideshError::Module {
                module: "container".to_string(),
                message: "No container runtime found. Set install-runtime=true to auto-install."
                    .to_string(),
            });
        }

        let runtime_to_install = if !preferred.is_empty() {
            preferred.to_string()
        } else {
            "docker".to_string()
        };

        self.install_runtime(ctx, &runtime_to_install).await?;
        Ok(runtime_to_install)
    }

    async fn install_runtime(
        &self,
        ctx: &ModuleContext<'_>,
        runtime: &str,
    ) -> Result<(), GlideshError> {
        if ctx.dry_run {
            return Ok(());
        }

        let packages = runtime_packages(&ctx.os_info.pkg_manager, runtime);
        let install_cmd = ctx.os_info.pkg_manager.install_cmd(&packages);

        tracing::info!(
            "Installing container runtime '{}' via: {}",
            runtime,
            install_cmd
        );

        let output = ctx.ssh.exec(&install_cmd).await?;
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
            .ssh
            .exec(&format!("systemctl enable --now {} 2>/dev/null", service))
            .await;

        Ok(())
    }

    async fn ensure_running(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
        runtime: &str,
    ) -> Result<ModuleResult, GlideshError> {
        let container_name = &params.resource_name;
        let raw_image = params
            .args
            .get("image")
            .and_then(|v| v.as_str())
            .ok_or_else(|| GlideshError::Module {
                module: "container".to_string(),
                message: "Container module requires 'image' parameter".to_string(),
            })?;

        // Podman doesn't default to Docker Hub for short names — qualify them.
        let image = qualify_image(raw_image, runtime);

        let _ = ctx
            .ssh
            .exec(&format!(
                "{} stop {} 2>/dev/null; {} rm {} 2>/dev/null",
                runtime, container_name, runtime, container_name
            ))
            .await;

        let mut cmd = format!("{} run -d --name {}", runtime, container_name);

        if let Some(restart) = params.args.get("restart").and_then(|v| v.as_str()) {
            cmd.push_str(&format!(" --restart={}", restart));
        }

        if let Some(ports) = params.args.get("ports").and_then(|v| v.as_list()) {
            for port in ports {
                cmd.push_str(&format!(" -p '{}'", port));
            }
        }

        if let Some(env) = params.args.get("environment").and_then(|v| v.as_map()) {
            for (key, value) in env {
                cmd.push_str(&format!(" -e '{}={}'", key, value));
            }
        }

        if let Some(volumes) = params.args.get("volumes").and_then(|v| v.as_list()) {
            for vol in volumes {
                cmd.push_str(&format!(" -v '{}'", vol));
            }
        }

        cmd.push_str(&format!(" '{}'", image));

        let output = ctx.ssh.exec(&cmd).await?;

        if output.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "container".to_string(),
                message: format!(
                    "Failed to run container '{}' (exit {}): {}",
                    container_name, output.exit_code, output.stderr
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

    async fn ensure_stopped(
        &self,
        ctx: &ModuleContext<'_>,
        container_name: &str,
        runtime: &str,
    ) -> Result<ModuleResult, GlideshError> {
        let output = ctx
            .ssh
            .exec(&format!("{} stop {}", runtime, container_name))
            .await?;

        if output.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "container".to_string(),
                message: format!(
                    "Failed to stop container '{}' (exit {}): {}",
                    container_name, output.exit_code, output.stderr
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

    async fn ensure_absent(
        &self,
        ctx: &ModuleContext<'_>,
        container_name: &str,
        runtime: &str,
    ) -> Result<ModuleResult, GlideshError> {
        let output = ctx
            .ssh
            .exec(&format!(
                "{} stop {} 2>/dev/null; {} rm -f {}",
                runtime, container_name, runtime, container_name
            ))
            .await?;

        Ok(ModuleResult {
            changed: true,
            output: output.stdout,
            stderr: output.stderr,
            exit_code: output.exit_code as i32,
        })
    }
}

/// Qualify a short image name for Podman which doesn't default to Docker Hub.
/// If the image has no registry prefix (no `.` or `localhost` before the first `/`),
/// prepend `docker.io/`. For Docker this is a no-op since Docker already defaults
/// to Docker Hub, but the explicit prefix doesn't hurt.
fn qualify_image(image: &str, runtime: &str) -> String {
    if runtime != "podman" {
        return image.to_string();
    }
    // Already fully qualified (contains a domain-like prefix)
    if let Some(slash_pos) = image.find('/') {
        let prefix = &image[..slash_pos];
        if prefix.contains('.') || prefix == "localhost" {
            return image.to_string();
        }
    }
    format!("docker.io/{}", image)
}

/// Return the package names to install for a given runtime and package manager.
fn runtime_packages(pkg: &PkgManager, runtime: &str) -> Vec<String> {
    match runtime {
        "podman" => vec!["podman".to_string()],
        _ => match pkg {
            // Debian/Ubuntu use docker.io from distro repos
            PkgManager::Apt => vec!["docker.io".to_string()],
            // Arch uses docker
            PkgManager::Pacman => vec!["docker".to_string()],
            // Alpine
            PkgManager::Apk => vec!["docker".to_string()],
            // RPM-based and SUSE
            _ => vec!["docker-ce".to_string()],
        },
    }
}
