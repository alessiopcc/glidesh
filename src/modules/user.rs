use crate::config::types::ParamValue;
use crate::error::GlideshError;
use crate::modules::context::ModuleContext;
use crate::modules::{Module, ModuleParams, ModuleResult, ModuleStatus};
use async_trait::async_trait;

pub struct UserModule;

/// `groups` as a list (`groups "docker" "sudo"`, or a `-` block), a single name, or a
/// comma-separated attribute (`groups="docker,sudo"`).
fn desired_groups(params: &ModuleParams) -> Option<Vec<String>> {
    match params.args.get("groups")? {
        ParamValue::List(groups) => Some(groups.clone()),
        ParamValue::String(groups) => Some(
            groups
                .split(',')
                .map(str::trim)
                .filter(|g| !g.is_empty())
                .map(str::to_string)
                .collect(),
        ),
        _ => None,
    }
    .filter(|groups| !groups.is_empty())
}

#[async_trait]
impl Module for UserModule {
    fn name(&self) -> &str {
        "user"
    }

    async fn check(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleStatus, GlideshError> {
        let username = &params.resource_name;
        let desired_state = params
            .args
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("present");

        let output = ctx.exec(&format!("id {} 2>/dev/null", username)).await?;
        let exists = output.exit_code == 0;

        match (desired_state, exists) {
            ("present", true) => {
                let changes = self.compute_changes(ctx, params).await?;
                if changes.is_empty() {
                    Ok(ModuleStatus::Satisfied)
                } else {
                    Ok(ModuleStatus::pending(format!(
                        "Modify user {}: {}",
                        username,
                        changes.join(", ")
                    )))
                }
            }
            ("present", false) => Ok(ModuleStatus::pending(format!("Create user {}", username))),
            ("absent", true) => Ok(ModuleStatus::pending(format!("Delete user {}", username))),
            ("absent", false) => Ok(ModuleStatus::Satisfied),
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
        let username = &params.resource_name;
        let desired_state = params
            .args
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("present");

        if ctx.dry_run {
            return Ok(ModuleResult {
                changed: false,
                output: format!("[dry-run] Would {} user {}", desired_state, username),
                stderr: String::new(),
                exit_code: 0,
                output_cut: false,
            });
        }

        match desired_state {
            "present" => self.ensure_present(ctx, params).await,
            "absent" => self.ensure_absent(ctx, params).await,
            _ => Err(GlideshError::Module {
                module: "user".to_string(),
                message: format!("Unknown state: {}", desired_state),
            }),
        }
    }
}

impl UserModule {
    async fn compute_changes(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<Vec<String>, GlideshError> {
        let username = &params.resource_name;
        let mut changes = Vec::new();

        if let Some(desired_shell) = params.args.get("shell").and_then(|v| v.as_str()) {
            let output = ctx
                .exec(&format!("getent passwd {} | cut -d: -f7", username))
                .await?;
            let current_shell = output.stdout.trim();
            if current_shell != desired_shell {
                changes.push(format!("shell {} -> {}", current_shell, desired_shell));
            }
        }

        if let Some(desired_groups) = desired_groups(params) {
            let output = ctx.exec(&format!("id -nG {}", username)).await?;
            let current_groups: Vec<&str> = output.stdout.split_whitespace().collect();
            for g in desired_groups {
                if !current_groups.contains(&g.as_str()) {
                    changes.push(format!("add to group {}", g));
                }
            }
        }

        if let Some(desired_uid) = params.args.get("uid").and_then(|v| v.as_i64()) {
            let output = ctx.exec(&format!("id -u {}", username)).await?;
            let current_uid: i64 = output.stdout.trim().parse().unwrap_or(-1);
            if current_uid != desired_uid {
                changes.push(format!("uid {} -> {}", current_uid, desired_uid));
            }
        }

        Ok(changes)
    }

    async fn ensure_present(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleResult, GlideshError> {
        let username = &params.resource_name;

        let exists = ctx
            .exec(&format!("id {} 2>/dev/null", username))
            .await?
            .exit_code
            == 0;

        let mut cmd_parts = Vec::new();

        if exists {
            cmd_parts.push("usermod".to_string());
        } else {
            cmd_parts.push("useradd".to_string());
        }

        if let Some(uid) = params.args.get("uid").and_then(|v| v.as_i64()) {
            cmd_parts.push(format!("-u {}", uid));
        }

        if let Some(shell) = params.args.get("shell").and_then(|v| v.as_str()) {
            cmd_parts.push(format!("-s {}", shell));
        }

        if let Some(groups) = desired_groups(params) {
            cmd_parts.push(format!("-G {}", groups.join(",")));
        }

        if !exists {
            cmd_parts.push("-m".to_string()); // create home directory
        }

        cmd_parts.push(username.clone());

        let cmd = cmd_parts.join(" ");
        let output = ctx.exec(&cmd).await?;

        if output.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "user".to_string(),
                message: format!(
                    "Failed to {} user '{}' (exit {}): {}",
                    if exists { "modify" } else { "create" },
                    username,
                    output.exit_code,
                    output.stderr
                ),
            });
        }

        Ok(ModuleResult {
            changed: true,
            output: output.stdout,
            stderr: output.stderr,
            exit_code: output.exit_code as i32,
            output_cut: output.stdout_cut,
        })
    }

    async fn ensure_absent(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleResult, GlideshError> {
        let username = &params.resource_name;
        let cmd = format!(
            "userdel -r {} 2>/dev/null || userdel {}",
            username, username
        );
        let output = ctx.exec(&cmd).await?;

        if output.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "user".to_string(),
                message: format!(
                    "Failed to delete user '{}' (exit {}): {}",
                    username, output.exit_code, output.stderr
                ),
            });
        }

        Ok(ModuleResult {
            changed: true,
            output: output.stdout,
            stderr: output.stderr,
            exit_code: output.exit_code as i32,
            output_cut: output.stdout_cut,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn groups_of(value: ParamValue) -> Option<Vec<String>> {
        desired_groups(&ModuleParams {
            resource_name: "deploy".to_string(),
            args: HashMap::from([("groups".to_string(), value)]),
        })
    }

    #[test]
    fn groups_are_read_from_a_list_a_name_or_a_comma_list() {
        let both = Some(vec!["docker".to_string(), "sudo".to_string()]);
        assert_eq!(
            groups_of(ParamValue::List(vec!["docker".into(), "sudo".into()])),
            both
        );
        assert_eq!(groups_of(ParamValue::String("docker, sudo".into())), both);
        assert_eq!(
            groups_of(ParamValue::String("docker".into())),
            Some(vec!["docker".to_string()])
        );
        assert_eq!(groups_of(ParamValue::String(String::new())), None);
    }
}
