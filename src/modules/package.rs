use crate::error::GlideshError;
use crate::modules::context::ModuleContext;
use crate::modules::{Module, ModuleParams, ModuleResult, ModuleStatus};
use async_trait::async_trait;

pub struct PackageModule;

/// Every parameter the module reads; any other is rejected before connecting.
const PARAMS: &[&str] = &["state"];

#[async_trait]
impl Module for PackageModule {
    fn name(&self) -> &str {
        "package"
    }

    fn params(&self) -> Option<&'static [&'static str]> {
        Some(PARAMS)
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
        let output = ctx.exec(&check_cmd).await?;
        let is_installed = output.exit_code == 0;

        match (desired_state, is_installed) {
            ("present", true) | ("absent", false) => Ok(ModuleStatus::Satisfied),
            ("present", false) => Ok(ModuleStatus::pending(format!(
                "Install package {}",
                package
            ))),
            ("absent", true) => Ok(ModuleStatus::pending(format!("Remove package {}", package))),
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
                output_cut: false,
            });
        }

        let mut refresh_failed = None;
        let cmd = match desired_state {
            "present" => {
                let update_cmd = ctx.os_info.pkg_manager.update_index_cmd();
                let refresh = ctx.exec(update_cmd).await?;
                // One broken repository fails the whole refresh, while the package may come
                // from one that works, so the install still runs.
                if refresh.exit_code != 0 {
                    refresh_failed = Some(refresh_note(
                        update_cmd,
                        refresh.exit_code,
                        &refresh.stdout,
                        &refresh.stderr,
                    ));
                }
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

        let output = ctx.exec(&cmd).await?;

        if output.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "package".to_string(),
                message: failure_message(
                    package,
                    refresh_failed.as_deref(),
                    output.exit_code,
                    &output.stderr,
                ),
            });
        }

        Ok(ModuleResult {
            changed: true,
            output: output.stdout,
            stderr: with_warning(refresh_failed.as_deref(), output.stderr),
            exit_code: output.exit_code as i32,
            output_cut: output.stdout_cut,
        })
    }
}

/// Describes a failed index refresh. Some managers report errors on stdout, so it falls back
/// to that when stderr is empty.
fn refresh_note(cmd: &str, exit_code: u32, stdout: &str, stderr: &str) -> String {
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    format!("the package index refresh failed ({cmd}, exit {exit_code}): {detail}")
}

/// The refresh's failure comes first: it is the likely cause of an install that then fails.
fn failure_message(package: &str, refresh: Option<&str>, exit_code: u32, stderr: &str) -> String {
    let failed = format!("Package operation failed for '{package}' (exit {exit_code}): {stderr}");
    match refresh {
        Some(note) => format!("{note}\n{failed}"),
        None => failed,
    }
}

/// Keeps a failed refresh visible when the install succeeds anyway.
fn with_warning(refresh: Option<&str>, stderr: String) -> String {
    match refresh {
        Some(note) => format!("warning: {note}\n{stderr}"),
        None => stderr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_refresh_is_named_before_the_install_error() {
        let refresh = "E: The repository 'http://example.invalid stable Release' does not have a Release file.\n";
        let note = refresh_note("apt-get update -qq", 100, "", refresh);
        let message = failure_message(
            "nginx",
            Some(&note),
            100,
            "E: Unable to locate package nginx",
        );
        assert_eq!(
            message,
            "the package index refresh failed (apt-get update -qq, exit 100): E: The repository \
             'http://example.invalid stable Release' does not have a Release file.\n\
             Package operation failed for 'nginx' (exit 100): E: Unable to locate package nginx"
        );
    }

    #[test]
    fn without_a_failed_refresh_only_the_install_error_is_reported() {
        assert_eq!(
            failure_message("nginx", None, 100, "E: Unable to locate package nginx"),
            "Package operation failed for 'nginx' (exit 100): E: Unable to locate package nginx"
        );
    }

    #[test]
    fn a_refresh_that_reports_on_stdout_is_still_described() {
        let note = refresh_note(
            "pacman -Sy",
            1,
            "error: failed to synchronize all databases\n",
            "",
        );
        assert!(
            note.ends_with("exit 1): error: failed to synchronize all databases"),
            "{note}"
        );
    }

    #[test]
    fn a_successful_install_keeps_the_refresh_failure_as_a_warning() {
        let note = "the package index refresh failed (apk update -q, exit 1): ERROR: bad repo";
        assert_eq!(
            with_warning(Some(note), "done\n".to_string()),
            format!("warning: {note}\ndone\n")
        );
        assert_eq!(with_warning(None, "done\n".to_string()), "done\n");
    }
}
