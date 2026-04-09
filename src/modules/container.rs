use crate::error::GlideshError;
use crate::modules::context::ModuleContext;
use crate::modules::detect::ContainerRuntime;
use crate::modules::detect::PkgManager;
use crate::modules::{Module, ModuleParams, ModuleResult, ModuleStatus};
use async_trait::async_trait;
use sha2::{Digest, Sha256};

pub struct ContainerModule;

const PARAM_HASH_LABEL: &str = "sh.glide.param-hash";

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
                    let desired_hash = param_hash(&runtime, params);
                    let label_output = ctx
                        .ssh
                        .exec(&format!(
                            "{} inspect --format '{{{{index .Config.Labels \"{}\"}}}}' {} 2>/dev/null",
                            runtime, PARAM_HASH_LABEL, container_name
                        ))
                        .await?;
                    let current_hash = label_output.stdout.trim();

                    if current_hash != desired_hash {
                        return Ok(ModuleStatus::Pending {
                            plan: format!(
                                "Recreate container {} (configuration changed)",
                                container_name
                            ),
                        });
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

    async fn ensure_network(
        ctx: &ModuleContext<'_>,
        runtime: &str,
        network: &str,
    ) -> Result<(), GlideshError> {
        let inspect = ctx
            .ssh
            .exec(&format!(
                "{} network inspect {} 2>/dev/null",
                runtime, network
            ))
            .await?;

        if inspect.exit_code == 0 {
            return Ok(());
        }

        let create = ctx
            .ssh
            .exec(&format!("{} network create {}", runtime, network))
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

    async fn ensure_running(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
        runtime: &str,
    ) -> Result<ModuleResult, GlideshError> {
        let container_name = &params.resource_name;

        if let Some(network) = params.args.get("network").and_then(|v| v.as_str()) {
            if !is_builtin_network(network) {
                Self::ensure_network(ctx, runtime, network).await?;
            }
        }

        let _ = ctx
            .ssh
            .exec(&format!(
                "{} stop {} 2>/dev/null; {} rm {} 2>/dev/null",
                runtime, container_name, runtime, container_name
            ))
            .await;

        let cmd = build_run_command(runtime, container_name, params)?;

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

/// Compute a stable hash of all container parameters that affect runtime behavior.
/// Used to detect configuration drift via a label stored on the container.
fn param_hash(runtime: &str, params: &ModuleParams) -> String {
    let mut hasher = Sha256::new();

    // Image (qualified, so runtime switch also triggers recreate)
    if let Some(img) = params.args.get("image").and_then(|v| v.as_str()) {
        hasher.update(b"image:");
        hasher.update(qualify_image(img, runtime).as_bytes());
        hasher.update(b"\n");
    }

    if let Some(network) = params.args.get("network").and_then(|v| v.as_str()) {
        hasher.update(b"network:");
        hasher.update(network.as_bytes());
        hasher.update(b"\n");
    }

    if let Some(restart) = params.args.get("restart").and_then(|v| v.as_str()) {
        hasher.update(b"restart:");
        hasher.update(restart.as_bytes());
        hasher.update(b"\n");
    }

    if let Some(ports) = params.args.get("ports").and_then(|v| v.as_list()) {
        hasher.update(b"ports:");
        let mut sorted: Vec<&str> = ports.iter().map(|s| s.as_str()).collect();
        sorted.sort_unstable();
        for port in sorted {
            hasher.update(port.as_bytes());
            hasher.update(b",");
        }
        hasher.update(b"\n");
    }

    if let Some(env) = params.args.get("environment").and_then(|v| v.as_map()) {
        hasher.update(b"env:");
        let mut pairs: Vec<_> = env.iter().collect();
        pairs.sort_by_key(|(k, _)| *k);
        for (key, value) in pairs {
            hasher.update(key.as_bytes());
            hasher.update(b"=");
            hasher.update(value.as_bytes());
            hasher.update(b",");
        }
        hasher.update(b"\n");
    }

    if let Some(volumes) = params.args.get("volumes").and_then(|v| v.as_list()) {
        hasher.update(b"volumes:");
        let mut sorted: Vec<&str> = volumes.iter().map(|s| s.as_str()).collect();
        sorted.sort_unstable();
        for vol in sorted {
            hasher.update(vol.as_bytes());
            hasher.update(b",");
        }
        hasher.update(b"\n");
    }

    if let Some(command) = params.args.get("command").and_then(|v| v.as_str()) {
        hasher.update(b"command:");
        hasher.update(command.as_bytes());
        hasher.update(b"\n");
    }

    let result = hasher.finalize();
    result.iter().map(|b| format!("{:02x}", b)).collect()
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

/// Returns true for Docker/Podman built-in network modes that should not be auto-created.
fn is_builtin_network(name: &str) -> bool {
    matches!(name, "host" | "bridge" | "none" | "default")
}

/// Build the `<runtime> run` command string from parameters (extracted for testability).
fn build_run_command(
    runtime: &str,
    container_name: &str,
    params: &ModuleParams,
) -> Result<String, GlideshError> {
    let raw_image = params
        .args
        .get("image")
        .and_then(|v| v.as_str())
        .ok_or_else(|| GlideshError::Module {
            module: "container".to_string(),
            message: "Container module requires 'image' parameter".to_string(),
        })?;

    let image = qualify_image(raw_image, runtime);
    let hash = param_hash(runtime, params);
    let mut cmd = format!(
        "{} run -d --name {} --label {}={}",
        runtime, container_name, PARAM_HASH_LABEL, hash
    );

    if let Some(network) = params.args.get("network").and_then(|v| v.as_str()) {
        cmd.push_str(&format!(" --network {}", network));
    }

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

    if let Some(command) = params.args.get("command").and_then(|v| v.as_str()) {
        cmd.push_str(&format!(" {}", command));
    }

    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::ParamValue;
    use crate::modules::ModuleParams;

    fn make_params(args: Vec<(&str, ParamValue)>) -> ModuleParams {
        ModuleParams {
            resource_name: "testcontainer".to_string(),
            args: args
                .into_iter()
                .map(|(k, v): (&str, ParamValue)| (k.to_string(), v))
                .collect(),
        }
    }

    #[test]
    fn test_build_run_command_basic() {
        let params = make_params(vec![("image", ParamValue::String("nginx:latest".into()))]);
        let cmd = build_run_command("docker", "testcontainer", &params).unwrap();
        assert!(cmd.starts_with("docker run -d --name testcontainer --label sh.glide.param-hash="));
        assert!(cmd.ends_with("'nginx:latest'"));
    }

    #[test]
    fn test_build_run_command_with_command() {
        let params = make_params(vec![
            ("image", ParamValue::String("python:3.12".into())),
            (
                "command",
                ParamValue::String("python -m http.server 8000".into()),
            ),
        ]);
        let cmd = build_run_command("docker", "testcontainer", &params).unwrap();
        assert!(cmd.starts_with("docker run -d --name testcontainer --label sh.glide.param-hash="));
        assert!(cmd.ends_with("'python:3.12' python -m http.server 8000"));
    }

    #[test]
    fn test_build_run_command_with_all_options_and_command() {
        let params = make_params(vec![
            ("image", ParamValue::String("myapp:v1".into())),
            ("restart", ParamValue::String("always".into())),
            (
                "ports",
                ParamValue::List(vec!["8080:80".into(), "8443:443".into()]),
            ),
            ("volumes", ParamValue::List(vec!["/data:/app/data".into()])),
            (
                "command",
                ParamValue::String("./start.sh --config /etc/app.conf".into()),
            ),
        ]);
        let cmd = build_run_command("docker", "testcontainer", &params).unwrap();
        assert!(cmd.starts_with("docker run -d --name testcontainer"));
        assert!(cmd.contains("--restart=always"));
        assert!(cmd.contains("-p '8080:80'"));
        assert!(cmd.contains("-p '8443:443'"));
        assert!(cmd.contains("-v '/data:/app/data'"));
        assert!(cmd.ends_with("'myapp:v1' ./start.sh --config /etc/app.conf"));
    }

    #[test]
    fn test_build_run_command_no_image_error() {
        let params = make_params(vec![]);
        let result = build_run_command("docker", "testcontainer", &params);
        assert!(result.is_err());
    }

    #[test]
    fn test_build_run_command_podman_qualifies_image() {
        let params = make_params(vec![
            ("image", ParamValue::String("nginx:latest".into())),
            (
                "command",
                ParamValue::String("nginx -g 'daemon off;'".into()),
            ),
        ]);
        let cmd = build_run_command("podman", "testcontainer", &params).unwrap();
        assert!(cmd.contains("'docker.io/nginx:latest'"));
        assert!(cmd.ends_with("nginx -g 'daemon off;'"));
    }

    #[test]
    fn test_is_builtin_network() {
        assert!(is_builtin_network("host"));
        assert!(is_builtin_network("bridge"));
        assert!(is_builtin_network("none"));
        assert!(is_builtin_network("default"));
        assert!(!is_builtin_network("mynet"));
        assert!(!is_builtin_network("app-network"));
    }

    #[test]
    fn test_build_run_command_with_custom_network() {
        let params = make_params(vec![
            ("image", ParamValue::String("nginx:latest".into())),
            ("network", ParamValue::String("mynet".into())),
        ]);
        let cmd = build_run_command("docker", "testcontainer", &params).unwrap();
        assert!(cmd.contains("--network mynet"));
        assert!(cmd.ends_with("'nginx:latest'"));
    }

    #[test]
    fn test_build_run_command_with_host_network() {
        let params = make_params(vec![
            ("image", ParamValue::String("nginx:latest".into())),
            ("network", ParamValue::String("host".into())),
        ]);
        let cmd = build_run_command("docker", "testcontainer", &params).unwrap();
        assert!(cmd.contains("--network host"));
        assert!(cmd.ends_with("'nginx:latest'"));
    }

    #[test]
    fn test_param_hash_stable() {
        let params = make_params(vec![
            ("image", ParamValue::String("nginx:latest".into())),
            ("network", ParamValue::String("mynet".into())),
        ]);
        let h1 = param_hash("docker", &params);
        let h2 = param_hash("docker", &params);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // 32 bytes = 64 hex chars
    }

    #[test]
    fn test_param_hash_differs_on_change() {
        let params_a = make_params(vec![
            ("image", ParamValue::String("nginx:latest".into())),
            ("ports", ParamValue::List(vec!["8080:80".into()])),
        ]);
        let params_b = make_params(vec![
            ("image", ParamValue::String("nginx:latest".into())),
            ("ports", ParamValue::List(vec!["9090:80".into()])),
        ]);
        assert_ne!(
            param_hash("docker", &params_a),
            param_hash("docker", &params_b)
        );
    }

    #[test]
    fn test_qualify_image_docker_noop() {
        assert_eq!(qualify_image("nginx:latest", "docker"), "nginx:latest");
    }

    #[test]
    fn test_qualify_image_podman_short_name() {
        assert_eq!(
            qualify_image("nginx:latest", "podman"),
            "docker.io/nginx:latest"
        );
    }

    #[test]
    fn test_qualify_image_podman_already_qualified() {
        assert_eq!(
            qualify_image("ghcr.io/org/app:v1", "podman"),
            "ghcr.io/org/app:v1"
        );
    }
}
