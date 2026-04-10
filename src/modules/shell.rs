use crate::error::GlideshError;
use crate::modules::context::ModuleContext;
use crate::modules::{Module, ModuleParams, ModuleResult, ModuleStatus};
use async_trait::async_trait;

pub struct ShellModule;

impl ShellModule {
    fn resolve_command(params: &ModuleParams) -> Result<String, GlideshError> {
        if let Some(cmd_list) = params.args.get("cmd").and_then(|v| v.as_list()) {
            if cmd_list.is_empty() {
                return Err(GlideshError::Module {
                    module: "shell".to_string(),
                    message: "cmd list must not be empty".to_string(),
                });
            }
            Ok(cmd_list.join(" && "))
        } else if !params.resource_name.is_empty() {
            Ok(params.resource_name.clone())
        } else {
            Err(GlideshError::Module {
                module: "shell".to_string(),
                message: "shell requires a command (positional argument or cmd list)".to_string(),
            })
        }
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
                let output = ctx.ssh.exec(check_cmd).await?;
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
        let command = Self::resolve_command(params)?;

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
