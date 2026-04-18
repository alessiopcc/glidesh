use crate::error::GlideshError;
use crate::modules::context::ModuleContext;
use crate::modules::{Module, ModuleParams, ModuleResult, ModuleStatus};
use async_trait::async_trait;

pub struct ShellModule;

impl ShellModule {
    fn resolve_command(params: &ModuleParams) -> Result<String, GlideshError> {
        if let Some(cmd_val) = params.args.get("cmd") {
            if let Some(cmd_list) = cmd_val.as_list() {
                if cmd_list.is_empty() {
                    return Err(GlideshError::Module {
                        module: "shell".to_string(),
                        message: "cmd list must not be empty".to_string(),
                    });
                }
                Ok(cmd_list.join(" && "))
            } else if let Some(cmd_str) = cmd_val.as_str() {
                if cmd_str.is_empty() {
                    return Err(GlideshError::Module {
                        module: "shell".to_string(),
                        message: "cmd must not be empty".to_string(),
                    });
                }
                Ok(cmd_str.to_string())
            } else {
                Err(GlideshError::Module {
                    module: "shell".to_string(),
                    message: "cmd must be a string or a list of strings".to_string(),
                })
            }
        } else if !params.resource_name.is_empty() {
            Ok(params.resource_name.clone())
        } else {
            Err(GlideshError::Module {
                module: "shell".to_string(),
                message: "shell requires a command (positional argument, cmd string, or cmd list)"
                    .to_string(),
            })
        }
    }

    fn login_enabled(params: &ModuleParams) -> bool {
        params
            .args
            .get("login")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    // A login shell sources /etc/profile and ~/.profile, which is where Nix,
    // asdf, nvm, rustup, etc. inject their PATH entries.
    fn wrap_login(cmd: &str) -> String {
        let escaped = cmd.replace('\'', "'\\''");
        format!("sh -l -c '{}'", escaped)
    }
}

#[async_trait]
impl Module for ShellModule {
    fn name(&self) -> &str {
        "shell"
    }

    async fn check(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleStatus, GlideshError> {
        let command = Self::resolve_command(params)?;

        let gate = params.args.get("check").and_then(|v| v.as_str());

        match gate {
            Some(check_cmd) => {
                let gate_cmd = if Self::login_enabled(params) {
                    Self::wrap_login(check_cmd)
                } else {
                    check_cmd.to_string()
                };
                let output = ctx.ssh.exec(&gate_cmd).await?;
                if output.exit_code == 0 {
                    Ok(ModuleStatus::Satisfied)
                } else {
                    Ok(ModuleStatus::Pending {
                        plan: format!("Run: {}", command),
                    })
                }
            }
            None => Ok(ModuleStatus::Pending {
                plan: format!("Run: {}", command),
            }),
        }
    }

    async fn apply(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleResult, GlideshError> {
        let raw = Self::resolve_command(params)?;
        let command = if Self::login_enabled(params) {
            Self::wrap_login(&raw)
        } else {
            raw
        };

        if ctx.dry_run {
            return Ok(ModuleResult {
                changed: false,
                output: format!("[dry-run] Would run: {}", command),
                stderr: String::new(),
                exit_code: 0,
            });
        }

        let max_retries = params
            .args
            .get("retries")
            .and_then(|v| v.as_i64())
            .unwrap_or(1) as u32;
        let delay_secs = params
            .args
            .get("delay")
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as u64;

        let mut last_output = None;

        for attempt in 1..=max_retries {
            let output = ctx.ssh.exec(&command).await?;

            if output.exit_code == 0 {
                return Ok(ModuleResult {
                    changed: true,
                    output: output.stdout,
                    stderr: output.stderr,
                    exit_code: output.exit_code as i32,
                });
            }

            last_output = Some(output);

            if attempt < max_retries && delay_secs > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
            }
        }

        let output = last_output.unwrap();
        Err(GlideshError::Module {
            module: "shell".to_string(),
            message: format!(
                "Command '{}' failed with exit code {} after {} attempt(s).\nstdout: {}\nstderr: {}",
                command, output.exit_code, max_retries, output.stdout, output.stderr
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::ParamValue;

    fn params_with(args: &[(&str, ParamValue)]) -> ModuleParams {
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
    fn login_disabled_by_default() {
        let p = params_with(&[]);
        assert!(!ShellModule::login_enabled(&p));
    }

    #[test]
    fn login_enabled_when_true() {
        let p = params_with(&[("login", ParamValue::Bool(true))]);
        assert!(ShellModule::login_enabled(&p));
    }

    #[test]
    fn wrap_login_wraps_in_sh_l_c() {
        assert_eq!(ShellModule::wrap_login("rg foo"), "sh -l -c 'rg foo'");
    }

    #[test]
    fn wrap_login_escapes_single_quotes() {
        assert_eq!(
            ShellModule::wrap_login("echo 'hi'"),
            "sh -l -c 'echo '\\''hi'\\'''"
        );
    }
}
