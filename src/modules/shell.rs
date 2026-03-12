use crate::error::GlideshError;
use crate::modules::context::ModuleContext;
use crate::modules::{Module, ModuleParams, ModuleResult, ModuleStatus};
use async_trait::async_trait;

pub struct ShellModule;

#[async_trait]
impl Module for ShellModule {
    fn name(&self) -> &str {
        "shell"
    }

    async fn check(
        &self,
        _ctx: &ModuleContext<'_>,
        _params: &ModuleParams,
    ) -> Result<ModuleStatus, GlideshError> {
        // Shell commands are always pending — we can't know if they need to run
        // without running them (they are inherently non-idempotent).
        Ok(ModuleStatus::Pending {
            plan: format!("Run: {}", _params.resource_name),
        })
    }

    async fn apply(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleResult, GlideshError> {
        if ctx.dry_run {
            return Ok(ModuleResult {
                changed: false,
                output: format!("[dry-run] Would run: {}", params.resource_name),
                stderr: String::new(),
                exit_code: 0,
            });
        }

        let command = &params.resource_name;

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
            let output = ctx.ssh.exec(command).await?;

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
