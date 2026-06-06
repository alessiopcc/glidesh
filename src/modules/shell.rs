use crate::error::GlideshError;
use crate::modules::context::ModuleContext;
use crate::modules::{Module, ModuleParams, ModuleResult, ModuleStatus};
use crate::ssh::connection::CommandOutput;
use async_trait::async_trait;
use std::collections::HashSet;
use std::time::Duration;

pub struct ShellModule;

pub(crate) fn resolve_cmd_from_params(
    params: &ModuleParams,
    module_name: &str,
) -> Result<String, GlideshError> {
    if let Some(cmd_val) = params.args.get("cmd") {
        if let Some(cmd_list) = cmd_val.as_list() {
            if cmd_list.is_empty() {
                return Err(GlideshError::Module {
                    module: module_name.to_string(),
                    message: "cmd list must not be empty".to_string(),
                });
            }
            Ok(cmd_list.join(" && "))
        } else if let Some(cmd_str) = cmd_val.as_str() {
            if cmd_str.is_empty() {
                return Err(GlideshError::Module {
                    module: module_name.to_string(),
                    message: "cmd must not be empty".to_string(),
                });
            }
            Ok(cmd_str.to_string())
        } else {
            Err(GlideshError::Module {
                module: module_name.to_string(),
                message: "cmd must be a string or a list of strings".to_string(),
            })
        }
    } else if !params.resource_name.is_empty() {
        Ok(params.resource_name.clone())
    } else {
        Err(GlideshError::Module {
            module: module_name.to_string(),
            message: format!(
                "{module_name} requires a command (positional argument, cmd string, or cmd list)"
            ),
        })
    }
}

pub(crate) fn login_enabled(params: &ModuleParams) -> bool {
    params
        .args
        .get("login")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

// A login shell sources /etc/profile and ~/.profile, which is where Nix,
// asdf, nvm, rustup, etc. inject their PATH entries.
pub(crate) fn wrap_login(cmd: &str) -> String {
    let escaped = cmd.replace('\'', "'\\''");
    format!("sh -l -c '{}'", escaped)
}

/// Parse the optional `timeout` argument (seconds). `None` (absent or `<= 0`)
/// means run with no time limit.
pub(crate) fn parse_timeout(params: &ModuleParams) -> Result<Option<u64>, GlideshError> {
    match params.args.get("timeout") {
        None => Ok(None),
        Some(value) => {
            let secs = value.as_i64().ok_or_else(|| GlideshError::Module {
                module: "shell".to_string(),
                message: "timeout must be an integer number of seconds".to_string(),
            })?;
            Ok(if secs > 0 { Some(secs as u64) } else { None })
        }
    }
}

/// Parse the optional `success_codes` argument. `None` means only exit `0` is
/// accepted (the default); `Some(set)` accepts exactly those codes.
/// Accepts a string (`"0,2"`), a single integer (`2`), or a list.
pub(crate) fn parse_success_codes(
    params: &ModuleParams,
) -> Result<Option<HashSet<i32>>, GlideshError> {
    let Some(value) = params.args.get("success_codes") else {
        return Ok(None);
    };

    let mut codes = HashSet::new();
    let add_tokens = |s: &str, codes: &mut HashSet<i32>| -> Result<(), GlideshError> {
        for tok in s
            .split([',', ' ', '\t', '\n'])
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            let code: i32 = tok.parse().map_err(|_| GlideshError::Module {
                module: "shell".to_string(),
                message: format!("invalid success_codes entry: '{}'", tok),
            })?;
            codes.insert(code);
        }
        Ok(())
    };

    if let Some(s) = value.as_str() {
        add_tokens(s, &mut codes)?;
    } else if let Some(i) = value.as_i64() {
        codes.insert(i as i32);
    } else if let Some(list) = value.as_list() {
        for item in list {
            add_tokens(item, &mut codes)?;
        }
    } else {
        return Err(GlideshError::Module {
            module: "shell".to_string(),
            message: "success_codes must be a string, integer, or list".to_string(),
        });
    }

    Ok(if codes.is_empty() { None } else { Some(codes) })
}

/// Run a command, optionally bounded by a timeout. `Ok(None)` means the command
/// did not finish within the limit (the in-flight exec is dropped, which closes
/// the channel; the remote command may keep running).
pub(crate) async fn exec_timed(
    ctx: &ModuleContext<'_>,
    command: &str,
    timeout: Option<u64>,
) -> Result<Option<CommandOutput>, GlideshError> {
    match timeout {
        Some(secs) => {
            match tokio::time::timeout(Duration::from_secs(secs), ctx.exec(command)).await {
                Ok(result) => result.map(Some),
                Err(_) => Ok(None),
            }
        }
        None => ctx.exec(command).await.map(Some),
    }
}

fn accepted(exit_code: i32, success_codes: &Option<HashSet<i32>>) -> bool {
    match success_codes {
        // Absent success_codes accepts only the conventional success code, 0.
        None => exit_code == 0,
        Some(codes) => codes.contains(&exit_code),
    }
}

fn describe_success_codes(success_codes: &Option<HashSet<i32>>) -> String {
    match success_codes {
        None => "0".to_string(),
        Some(codes) => {
            let mut sorted: Vec<i32> = codes.iter().copied().collect();
            sorted.sort_unstable();
            sorted
                .iter()
                .map(i32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        }
    }
}

impl ShellModule {
    fn resolve_command(params: &ModuleParams) -> Result<String, GlideshError> {
        resolve_cmd_from_params(params, "shell")
    }

    fn login_enabled(params: &ModuleParams) -> bool {
        self::login_enabled(params)
    }

    fn wrap_login(cmd: &str) -> String {
        self::wrap_login(cmd)
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
                let timeout = parse_timeout(params)?;
                // A timed-out gate is treated as "not satisfied" so `apply` runs.
                match exec_timed(ctx, &gate_cmd, timeout).await? {
                    Some(output) if output.exit_code == 0 => Ok(ModuleStatus::Satisfied),
                    _ => Ok(ModuleStatus::Pending {
                        plan: format!("Run: {}", command),
                    }),
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
        let timeout = parse_timeout(params)?;
        // Absent `success_codes` accepts only exit 0 (the default).
        let success_codes = parse_success_codes(params)?;

        let mut last_output: Option<CommandOutput> = None;
        let mut timed_out = false;

        for attempt in 1..=max_retries {
            match exec_timed(ctx, &command, timeout).await? {
                Some(output) => {
                    if accepted(output.exit_code as i32, &success_codes) {
                        return Ok(ModuleResult {
                            changed: true,
                            output: output.stdout,
                            stderr: output.stderr,
                            exit_code: output.exit_code as i32,
                        });
                    }
                    timed_out = false;
                    last_output = Some(output);
                }
                None => {
                    timed_out = true;
                    last_output = None;
                }
            }

            if attempt < max_retries && delay_secs > 0 {
                tokio::time::sleep(Duration::from_secs(delay_secs)).await;
            }
        }

        let message = if timed_out {
            format!(
                "Command '{}' timed out after {}s ({} attempt(s)).",
                command,
                timeout.unwrap_or(0),
                max_retries
            )
        } else if let Some(output) = last_output {
            let accepted_desc = describe_success_codes(&success_codes);
            format!(
                "Command '{}' failed: exit code {} is not in the accepted set ({}) after {} attempt(s).\nstdout: {}\nstderr: {}",
                command, output.exit_code, accepted_desc, max_retries, output.stdout, output.stderr
            )
        } else {
            format!(
                "Command '{}' failed after {} attempt(s).",
                command, max_retries
            )
        };
        Err(GlideshError::Module {
            module: "shell".to_string(),
            message,
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

    #[test]
    fn timeout_absent_is_none() {
        assert_eq!(parse_timeout(&params_with(&[])).unwrap(), None);
    }

    #[test]
    fn timeout_parses_positive_seconds() {
        let p = params_with(&[("timeout", ParamValue::Integer(60))]);
        assert_eq!(parse_timeout(&p).unwrap(), Some(60));
    }

    #[test]
    fn timeout_zero_or_negative_is_none() {
        let p = params_with(&[("timeout", ParamValue::Integer(0))]);
        assert_eq!(parse_timeout(&p).unwrap(), None);
        let p = params_with(&[("timeout", ParamValue::Integer(-5))]);
        assert_eq!(parse_timeout(&p).unwrap(), None);
    }

    #[test]
    fn success_codes_absent_means_zero_only() {
        assert_eq!(parse_success_codes(&params_with(&[])).unwrap(), None);
        // Absent success_codes accepts only exit 0.
        assert!(accepted(0, &None));
        assert!(!accepted(2, &None));
        assert!(!accepted(137, &None));
    }

    #[test]
    fn success_codes_parses_comma_string() {
        let p = params_with(&[("success_codes", ParamValue::String("0, 2".to_string()))]);
        let set = parse_success_codes(&p).unwrap().unwrap();
        assert!(set.contains(&0) && set.contains(&2) && set.len() == 2);
        assert!(accepted(2, &Some(set.clone())));
        assert!(!accepted(1, &Some(set)));
    }

    #[test]
    fn success_codes_parses_single_integer() {
        let p = params_with(&[("success_codes", ParamValue::Integer(2))]);
        let set = parse_success_codes(&p).unwrap().unwrap();
        assert_eq!(set.len(), 1);
        assert!(set.contains(&2));
    }

    #[test]
    fn success_codes_parses_list() {
        let p = params_with(&[(
            "success_codes",
            ParamValue::List(vec!["0".to_string(), "2".to_string()]),
        )]);
        let set = parse_success_codes(&p).unwrap().unwrap();
        assert!(set.contains(&0) && set.contains(&2));
    }

    #[test]
    fn success_codes_rejects_garbage() {
        let p = params_with(&[("success_codes", ParamValue::String("0,nope".to_string()))]);
        assert!(parse_success_codes(&p).is_err());
    }

    #[test]
    fn describe_success_codes_is_sorted_and_human_readable() {
        assert_eq!(describe_success_codes(&None), "0");
        let set: HashSet<i32> = [2, 0, 137].into_iter().collect();
        assert_eq!(describe_success_codes(&Some(set)), "0, 2, 137");
    }
}
