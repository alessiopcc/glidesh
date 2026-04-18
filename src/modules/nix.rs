use crate::error::GlideshError;
use crate::modules::context::ModuleContext;
use crate::modules::{Module, ModuleParams, ModuleResult, ModuleStatus};
use async_trait::async_trait;

pub struct NixModule;

const NIX_INSTALLER_URL: &str = "https://install.determinate.systems/nix";

impl NixModule {
    // `OsInfo` is detected once at connection time, so `nix_installed` can be
    // stale after an earlier task in the same run auto-installs Nix. Re-probe
    // live when the cached flag is false.
    async fn nix_available(ctx: &ModuleContext<'_>) -> Result<bool, GlideshError> {
        if ctx.os_info.nix_installed {
            return Ok(true);
        }
        let output = ctx
            .ssh
            .exec(
                "[ -x /nix/var/nix/profiles/default/bin/nix ] \
                 || [ -x \"$HOME/.nix-profile/bin/nix\" ] \
                 || command -v nix >/dev/null 2>&1",
            )
            .await?;
        Ok(output.exit_code == 0)
    }

    fn install_requested(params: &ModuleParams) -> bool {
        params
            .args
            .get("install")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    fn nix_missing_error() -> GlideshError {
        GlideshError::Module {
            module: "nix".to_string(),
            message: "Nix is not installed on the target. Set install=#true to auto-install."
                .to_string(),
        }
    }

    // Call only from `apply()` — `check()` must remain side-effect-free.
    async fn install_nix(ctx: &ModuleContext<'_>) -> Result<(), GlideshError> {
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

    fn shell_escape(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }

    /// Resolves `profile=`: `"user"` (default) → empty; `"default"`/`"system"`
    /// → `/nix/var/nix/profiles/default`; any other string → explicit path.
    fn profile_arg(params: &ModuleParams) -> String {
        let profile = params
            .args
            .get("profile")
            .and_then(|v| v.as_str())
            .unwrap_or("user");
        match profile {
            "user" => String::new(),
            "default" | "system" => " --profile /nix/var/nix/profiles/default".to_string(),
            other => format!(" --profile {}", Self::shell_escape(other)),
        }
    }

    // Matches both `nix profile` and legacy `nix-env` installs for the user
    // profile (either is a satisfactory "installed" state). `nix-env` does not
    // accept `--profile`, so non-user profiles only check `nix profile`.
    fn build_check_install_cmd(package: &str, profile_arg: &str) -> String {
        let pkg_q = Self::shell_escape(package);
        if profile_arg.is_empty() {
            format!(
                "nix profile list 2>/dev/null | grep -qw {pkg} || nix-env -q {pkg} 2>/dev/null | grep -qw {pkg}",
                pkg = pkg_q
            )
        } else {
            format!(
                "nix profile list{profile} 2>/dev/null | grep -qw {pkg}",
                profile = profile_arg,
                pkg = pkg_q
            )
        }
    }

    // Non-user profiles skip the `nix-env` fallback: `nix-env` doesn't accept
    // `--profile` as `nix profile` does.
    fn build_apply_install_cmd(package: &str, desired_state: &str, profile_arg: &str) -> String {
        let pkg_q = Self::shell_escape(package);
        let flake_ref = Self::shell_escape(&format!("nixpkgs#{}", package));
        let nix_env_attr = Self::shell_escape(&format!("nixpkgs.{}", package));
        let is_user = profile_arg.is_empty();

        match desired_state {
            "present" => {
                if is_user {
                    format!(
                        "nix profile install {flake} 2>/dev/null || nix-env -iA {attr}",
                        flake = flake_ref,
                        attr = nix_env_attr
                    )
                } else {
                    format!(
                        "nix profile install{profile} {flake}",
                        profile = profile_arg,
                        flake = flake_ref
                    )
                }
            }
            "absent" => {
                if is_user {
                    format!(
                        "nix profile remove {flake} 2>/dev/null || nix-env -e {pkg}",
                        flake = flake_ref,
                        pkg = pkg_q
                    )
                } else {
                    format!(
                        "nix profile remove{profile} {flake}",
                        profile = profile_arg,
                        flake = flake_ref
                    )
                }
            }
            _ => String::new(),
        }
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

        let profile_arg = Self::profile_arg(params);
        let check_cmd = Self::build_check_install_cmd(package, &profile_arg);
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

        if !matches!(desired_state, "present" | "absent") {
            return Err(GlideshError::Module {
                module: "nix".to_string(),
                message: format!("Unknown state: {}", desired_state),
            });
        }

        if ctx.dry_run {
            return Ok(ModuleResult {
                changed: false,
                output: format!("[dry-run] Would {} Nix package {}", desired_state, package),
                stderr: String::new(),
                exit_code: 0,
            });
        }

        let profile_arg = Self::profile_arg(params);
        let cmd = Self::build_apply_install_cmd(package, desired_state, &profile_arg);

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

    // Wrap in `bash -c '<escaped>'` so pipes, redirects, and quoted args in
    // the user command are preserved (rather than tokenized by `nix shell`).
    fn build_shell_cmd(command: &str, packages: &[String]) -> String {
        let pkg_args: Vec<String> = packages
            .iter()
            .map(|p| {
                let flake_ref = if p.contains('#') {
                    p.clone()
                } else {
                    format!("nixpkgs#{}", p)
                };
                Self::shell_escape(&flake_ref)
            })
            .collect();

        format!(
            "nix shell {} --command bash -c {}",
            pkg_args.join(" "),
            Self::shell_escape(command)
        )
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

        let cmd = Self::build_shell_cmd(command, packages);

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

        let check_cmd = format!("test -L {}", Self::shell_escape(out_link));
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

        let cmd = format!(
            "nix build {} -o {}",
            Self::shell_escape(derivation),
            Self::shell_escape(out_link)
        );
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
            "nix-channel --list 2>/dev/null | grep -qw {}",
            Self::shell_escape(channel_name)
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
                format!(
                    "nix-channel --add {} {}",
                    Self::shell_escape(url),
                    Self::shell_escape(channel_name)
                )
            }
            "absent" => format!("nix-channel --remove {}", Self::shell_escape(channel_name)),
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
            format!(
                "cd {} && nix flake update {}",
                Self::shell_escape(flake_dir),
                Self::shell_escape(input)
            )
        } else {
            format!("cd {} && nix flake update", Self::shell_escape(flake_dir))
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
            format!(
                "nix-collect-garbage --delete-older-than {}",
                Self::shell_escape(older_than)
            )
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
        if !Self::nix_available(ctx).await? {
            if !Self::install_requested(params) {
                return Err(Self::nix_missing_error());
            }
            return Ok(ModuleStatus::Pending {
                plan: format!(
                    "Install Nix, then perform {} action on {}",
                    Self::get_action(params),
                    params.resource_name
                ),
            });
        }

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
        if !Self::nix_available(ctx).await? {
            if !Self::install_requested(params) {
                return Err(Self::nix_missing_error());
            }
            Self::install_nix(ctx).await?;
        }

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

    fn params_with(args: &[(&str, crate::config::types::ParamValue)]) -> ModuleParams {
        let mut map = std::collections::HashMap::new();
        for (k, v) in args {
            map.insert((*k).to_string(), v.clone());
        }
        ModuleParams {
            resource_name: String::new(),
            args: map,
        }
    }

    #[test]
    fn shell_escape_wraps_in_single_quotes() {
        assert_eq!(NixModule::shell_escape("ripgrep"), "'ripgrep'");
    }

    #[test]
    fn shell_escape_escapes_embedded_quotes() {
        assert_eq!(NixModule::shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn profile_arg_default_is_user_profile() {
        let p = params_with(&[]);
        assert_eq!(NixModule::profile_arg(&p), "");
    }

    #[test]
    fn profile_arg_user_is_empty() {
        let p = params_with(&[(
            "profile",
            crate::config::types::ParamValue::String("user".to_string()),
        )]);
        assert_eq!(NixModule::profile_arg(&p), "");
    }

    #[test]
    fn profile_arg_default_maps_to_system_path() {
        let p = params_with(&[(
            "profile",
            crate::config::types::ParamValue::String("default".to_string()),
        )]);
        assert_eq!(
            NixModule::profile_arg(&p),
            " --profile /nix/var/nix/profiles/default"
        );
    }

    #[test]
    fn profile_arg_system_alias() {
        let p = params_with(&[(
            "profile",
            crate::config::types::ParamValue::String("system".to_string()),
        )]);
        assert_eq!(
            NixModule::profile_arg(&p),
            " --profile /nix/var/nix/profiles/default"
        );
    }

    #[test]
    fn profile_arg_custom_path_is_shell_escaped() {
        let p = params_with(&[(
            "profile",
            crate::config::types::ParamValue::String("/opt/my profile".to_string()),
        )]);
        assert_eq!(NixModule::profile_arg(&p), " --profile '/opt/my profile'");
    }

    #[test]
    fn check_install_user_profile_matches_both_mechanisms() {
        let cmd = NixModule::build_check_install_cmd("ripgrep", "");
        assert!(cmd.contains("nix profile list"));
        assert!(cmd.contains("nix-env -q"));
        assert!(cmd.contains("'ripgrep'"));
    }

    #[test]
    fn check_install_system_profile_only_uses_nix_profile() {
        let cmd = NixModule::build_check_install_cmd(
            "ripgrep",
            " --profile /nix/var/nix/profiles/default",
        );
        assert!(cmd.contains("nix profile list --profile /nix/var/nix/profiles/default"));
        assert!(!cmd.contains("nix-env"));
    }

    #[test]
    fn apply_install_user_profile_tries_profile_then_env() {
        let cmd = NixModule::build_apply_install_cmd("htop", "present", "");
        assert!(cmd.contains("nix profile install 'nixpkgs#htop'"));
        assert!(cmd.contains("nix-env -iA 'nixpkgs.htop'"));
    }

    #[test]
    fn apply_install_system_profile_uses_profile_flag() {
        let cmd = NixModule::build_apply_install_cmd(
            "htop",
            "present",
            " --profile /nix/var/nix/profiles/default",
        );
        assert_eq!(
            cmd,
            "nix profile install --profile /nix/var/nix/profiles/default 'nixpkgs#htop'"
        );
        assert!(!cmd.contains("nix-env"));
    }

    #[test]
    fn apply_install_absent_user_profile() {
        let cmd = NixModule::build_apply_install_cmd("htop", "absent", "");
        assert!(cmd.contains("nix profile remove 'nixpkgs#htop'"));
        assert!(cmd.contains("nix-env -e 'htop'"));
    }

    #[test]
    fn build_shell_cmd_wraps_in_bash_c() {
        let cmd = NixModule::build_shell_cmd(
            "echo hello world",
            &["python3".to_string(), "jq".to_string()],
        );
        assert_eq!(
            cmd,
            "nix shell 'nixpkgs#python3' 'nixpkgs#jq' --command bash -c 'echo hello world'"
        );
    }

    #[test]
    fn build_shell_cmd_escapes_single_quotes_in_command() {
        let cmd = NixModule::build_shell_cmd("echo 'hi'", &["coreutils".to_string()]);
        assert_eq!(
            cmd,
            "nix shell 'nixpkgs#coreutils' --command bash -c 'echo '\\''hi'\\'''"
        );
    }

    #[test]
    fn build_shell_cmd_preserves_explicit_flake_refs() {
        let cmd = NixModule::build_shell_cmd("true", &["github:user/repo#tool".to_string()]);
        assert!(cmd.contains("'github:user/repo#tool'"));
        assert!(!cmd.contains("nixpkgs#github:"));
    }

    #[test]
    fn build_shell_cmd_preserves_pipes_and_redirects() {
        let cmd = NixModule::build_shell_cmd(
            "curl -sf http://x | jq . > /tmp/out",
            &["curl".to_string(), "jq".to_string()],
        );
        // The full command including pipes/redirects lives inside the single-quoted
        // bash -c argument, so bash (not the outer shell) tokenizes it.
        assert!(cmd.contains("bash -c 'curl -sf http://x | jq . > /tmp/out'"));
    }
}
