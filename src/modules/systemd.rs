use crate::error::GlideshError;
use crate::modules::context::ModuleContext;
use crate::modules::{Module, ModuleParams, ModuleResult, ModuleStatus};
use async_trait::async_trait;
use sha2::{Digest, Sha256};

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

pub struct SystemdModule;

impl SystemdModule {
    fn has_command(params: &ModuleParams) -> bool {
        params.args.contains_key("command")
    }

    fn unit_name(resource_name: &str) -> String {
        if let Some((_base, ext)) = resource_name.rsplit_once('.') {
            if !ext.is_empty() {
                return resource_name.to_string();
            }
        }
        format!("{}.service", resource_name)
    }

    fn validate_unit_name_for_creation(resource_name: &str) -> Result<(), GlideshError> {
        let unit = Self::unit_name(resource_name);
        if !unit.ends_with(".service") {
            return Err(GlideshError::Module {
                module: "systemd".to_string(),
                message: format!(
                    "unit file creation requires a .service unit, got '{}'",
                    unit
                ),
            });
        }
        Ok(())
    }

    fn validate_unit_name(name: &str) -> Result<(), GlideshError> {
        let unit = Self::unit_name(name);
        if unit.contains('/') || unit.contains("..") {
            return Err(GlideshError::Module {
                module: "systemd".to_string(),
                message: format!(
                    "unit name '{}' contains path separators or dot-segments",
                    unit
                ),
            });
        }
        Ok(())
    }

    fn unit_path(resource_name: &str) -> String {
        format!("/etc/systemd/system/{}", Self::unit_name(resource_name))
    }

    fn sha256_hex(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex_encode(hasher.finalize().as_slice())
    }

    fn validate_env_value(key: &str, val: &str) -> Result<(), GlideshError> {
        if key.contains('\n') || key.contains('"') || key.contains('\\') {
            return Err(GlideshError::Module {
                module: "systemd".to_string(),
                message: format!(
                    "environment key '{}' contains invalid characters (newlines, quotes, or backslashes)",
                    key
                ),
            });
        }
        if val.contains('\n') {
            return Err(GlideshError::Module {
                module: "systemd".to_string(),
                message: format!(
                    "environment value for '{}' contains newlines, which are not supported in systemd Environment= directives",
                    key
                ),
            });
        }
        Ok(())
    }

    fn validate_desired_state(state: &str) -> Result<(), GlideshError> {
        match state {
            "started" | "stopped" | "restarted" => Ok(()),
            other => Err(GlideshError::Module {
                module: "systemd".to_string(),
                message: format!(
                    "invalid state '{}'; expected one of: started, stopped, restarted",
                    other
                ),
            }),
        }
    }

    fn validate_directive_value(name: &str, value: &str) -> Result<(), GlideshError> {
        if value.contains('\n') || value.contains('\r') {
            return Err(GlideshError::Module {
                module: "systemd".to_string(),
                message: format!(
                    "'{}' value contains newline characters, which would inject additional unit-file directives",
                    name
                ),
            });
        }
        Ok(())
    }

    fn generate_unit_file(
        resource_name: &str,
        params: &ModuleParams,
    ) -> Result<String, GlideshError> {
        Self::validate_unit_name_for_creation(resource_name)?;

        let command = params
            .args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| GlideshError::Module {
                module: "systemd".to_string(),
                message: "command parameter must be a non-empty string".to_string(),
            })?;

        if command.is_empty() {
            return Err(GlideshError::Module {
                module: "systemd".to_string(),
                message: "command parameter must be a non-empty string".to_string(),
            });
        }
        Self::validate_directive_value("command", command)?;

        let description = params
            .args
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("{} service", resource_name));
        Self::validate_directive_value("description", &description)?;

        let after = params
            .args
            .get("after")
            .and_then(|v| v.as_str())
            .unwrap_or("network.target");
        Self::validate_directive_value("after", after)?;

        let service_type = params
            .args
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("simple");
        Self::validate_directive_value("type", service_type)?;

        let restart_policy = params
            .args
            .get("restart-policy")
            .and_then(|v| v.as_str())
            .unwrap_or("on-failure");
        Self::validate_directive_value("restart-policy", restart_policy)?;

        let wanted_by = params
            .args
            .get("wanted-by")
            .and_then(|v| v.as_str())
            .unwrap_or("multi-user.target");
        Self::validate_directive_value("wanted-by", wanted_by)?;

        let mut unit_content = String::new();

        unit_content.push_str("[Unit]\n");
        unit_content.push_str(&format!("Description={}\n", description));
        unit_content.push_str(&format!("After={}\n", after));

        unit_content.push_str("\n[Service]\n");
        unit_content.push_str(&format!("Type={}\n", service_type));
        unit_content.push_str(&format!("ExecStart={}\n", command));
        unit_content.push_str(&format!("Restart={}\n", restart_policy));

        if let Some(user) = params.args.get("user").and_then(|v| v.as_str()) {
            Self::validate_directive_value("user", user)?;
            unit_content.push_str(&format!("User={}\n", user));
        }
        if let Some(group) = params.args.get("group").and_then(|v| v.as_str()) {
            Self::validate_directive_value("group", group)?;
            unit_content.push_str(&format!("Group={}\n", group));
        }
        if let Some(working_dir) = params.args.get("working-dir").and_then(|v| v.as_str()) {
            Self::validate_directive_value("working-dir", working_dir)?;
            unit_content.push_str(&format!("WorkingDirectory={}\n", working_dir));
        }

        if let Some(env_value) = params.args.get("environment") {
            match env_value.as_map() {
                Some(env_map) => {
                    let mut keys: Vec<&String> = env_map.keys().collect();
                    keys.sort();
                    for key in keys {
                        let val = &env_map[key];
                        Self::validate_env_value(key, val)?;
                        let escaped_val = val.replace('\\', "\\\\").replace('"', "\\\"");
                        unit_content
                            .push_str(&format!("Environment=\"{}={}\"\n", key, escaped_val));
                    }
                }
                None => {
                    return Err(GlideshError::Module {
                        module: "systemd".to_string(),
                        message: "environment must be a map".to_string(),
                    });
                }
            }
        }

        unit_content.push_str("\n[Install]\n");
        unit_content.push_str(&format!("WantedBy={}\n", wanted_by));

        Ok(unit_content)
    }

    async fn check_unit_file(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<Option<String>, GlideshError> {
        if !Self::has_command(params) {
            return Ok(None);
        }

        let content = Self::generate_unit_file(&params.resource_name, params)?;
        let local_hash = Self::sha256_hex(content.as_bytes());
        let unit_path = Self::unit_path(&params.resource_name);

        match ctx.ssh.checksum_remote(&unit_path).await? {
            Some(remote_hash) if remote_hash == local_hash => Ok(None),
            _ => Ok(Some(content)),
        }
    }

    async fn apply_unit_file(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<bool, GlideshError> {
        if !Self::has_command(params) {
            return Ok(false);
        }

        let content = Self::generate_unit_file(&params.resource_name, params)?;
        let local_hash = Self::sha256_hex(content.as_bytes());
        let unit_path = Self::unit_path(&params.resource_name);

        let needs_upload = !matches!(
            ctx.ssh.checksum_remote(&unit_path).await?,
            Some(remote_hash) if remote_hash == local_hash
        );

        if !needs_upload {
            return Ok(false);
        }

        if ctx.dry_run {
            return Ok(true);
        }

        ctx.ssh.upload_file(content.as_bytes(), &unit_path).await?;

        let reload = ctx.ssh.exec("systemctl daemon-reload").await?;
        if reload.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "systemd".to_string(),
                message: format!("daemon-reload failed: {}", reload.stderr),
            });
        }

        Ok(true)
    }
}

#[async_trait]
impl Module for SystemdModule {
    fn name(&self) -> &str {
        "systemd"
    }

    async fn check(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleStatus, GlideshError> {
        Self::validate_unit_name(&params.resource_name)?;
        let unit = Self::unit_name(&params.resource_name);
        let desired_state = params
            .args
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("started");
        Self::validate_desired_state(desired_state)?;
        let desired_enabled = params.args.get("enabled").and_then(|v| v.as_bool());

        let mut needs_change = Vec::new();

        let unit_file_changed = self.check_unit_file(ctx, params).await?.is_some();
        if unit_file_changed {
            needs_change.push("upload unit file + daemon-reload".to_string());
        }

        let is_active = ctx
            .ssh
            .exec(&format!("systemctl is-active {} 2>/dev/null", unit))
            .await?;
        let active = is_active.stdout.trim() == "active";

        match desired_state {
            "started" if !active => needs_change.push("start".to_string()),
            "started" if active && unit_file_changed => needs_change.push("restart".to_string()),
            "stopped" if active => needs_change.push("stop".to_string()),
            "restarted" => needs_change.push("restart".to_string()),
            _ => {}
        }

        if let Some(want_enabled) = desired_enabled {
            let is_enabled = ctx
                .ssh
                .exec(&format!("systemctl is-enabled {} 2>/dev/null", unit))
                .await?;
            let enabled = is_enabled.stdout.trim() == "enabled";
            if want_enabled && !enabled {
                needs_change.push("enable".to_string());
            } else if !want_enabled && enabled {
                needs_change.push("disable".to_string());
            }
        }

        if needs_change.is_empty() {
            Ok(ModuleStatus::Satisfied)
        } else {
            Ok(ModuleStatus::Pending {
                plan: format!("systemd {}: {}", unit, needs_change.join(", ")),
            })
        }
    }

    async fn apply(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleResult, GlideshError> {
        Self::validate_unit_name(&params.resource_name)?;
        let unit = Self::unit_name(&params.resource_name);
        let desired_state = params
            .args
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("started");
        Self::validate_desired_state(desired_state)?;
        let desired_enabled = params.args.get("enabled").and_then(|v| v.as_bool());

        if ctx.dry_run {
            let mut actions = Vec::new();
            if Self::has_command(params) {
                actions.push(format!(
                    "upload unit file to {}",
                    Self::unit_path(&params.resource_name)
                ));
            }
            actions.push(format!("manage systemd unit {}", unit));
            return Ok(ModuleResult {
                changed: false,
                output: format!("[dry-run] Would {}", actions.join(", ")),
                stderr: String::new(),
                exit_code: 0,
            });
        }

        let file_changed = self.apply_unit_file(ctx, params).await?;

        let mut commands = Vec::new();

        if let Some(want_enabled) = desired_enabled {
            if want_enabled {
                commands.push(format!("systemctl enable {}", unit));
            } else {
                commands.push(format!("systemctl disable {}", unit));
            }
        }

        match desired_state {
            "started" if file_changed => {
                commands.push(format!("systemctl restart {}", unit));
            }
            "started" => commands.push(format!("systemctl start {}", unit)),
            "stopped" => commands.push(format!("systemctl stop {}", unit)),
            "restarted" => commands.push(format!("systemctl restart {}", unit)),
            _ => unreachable!(),
        }

        let combined = commands.join(" && ");
        let output = ctx.ssh.exec(&combined).await?;

        if output.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "systemd".to_string(),
                message: format!(
                    "systemd operation failed for '{}' (exit {}): {}",
                    unit, output.exit_code, output.stderr
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::ParamValue;
    use std::collections::HashMap;

    #[test]
    fn test_unit_name_no_suffix() {
        assert_eq!(SystemdModule::unit_name("my-app"), "my-app.service");
    }

    #[test]
    fn test_unit_name_with_service_suffix() {
        assert_eq!(SystemdModule::unit_name("my-app.service"), "my-app.service");
    }

    #[test]
    fn test_unit_name_with_timer_suffix() {
        assert_eq!(SystemdModule::unit_name("backup.timer"), "backup.timer");
    }

    #[test]
    fn test_unit_path() {
        assert_eq!(
            SystemdModule::unit_path("my-app"),
            "/etc/systemd/system/my-app.service"
        );
    }

    #[test]
    fn test_sha256_hex() {
        let hash = SystemdModule::sha256_hex(b"hello world\n");
        assert_eq!(
            hash,
            "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447"
        );
    }

    #[test]
    fn test_generate_unit_file_minimal() {
        let mut args = HashMap::new();
        args.insert(
            "command".to_string(),
            ParamValue::String("/usr/bin/my-app".to_string()),
        );
        let params = ModuleParams {
            resource_name: "my-app".to_string(),
            args,
        };

        let content = SystemdModule::generate_unit_file("my-app", &params).unwrap();
        assert!(content.contains("[Unit]"));
        assert!(content.contains("Description=my-app service"));
        assert!(content.contains("After=network.target"));
        assert!(content.contains("[Service]"));
        assert!(content.contains("Type=simple"));
        assert!(content.contains("ExecStart=/usr/bin/my-app"));
        assert!(content.contains("Restart=on-failure"));
        assert!(content.contains("[Install]"));
        assert!(content.contains("WantedBy=multi-user.target"));
        assert!(!content.contains("User="));
        assert!(!content.contains("Group="));
        assert!(!content.contains("WorkingDirectory="));
        assert!(!content.contains("Environment="));
    }

    #[test]
    fn test_generate_unit_file_full() {
        let mut env = HashMap::new();
        env.insert("PORT".to_string(), "8080".to_string());
        env.insert("NODE_ENV".to_string(), "production".to_string());

        let mut args = HashMap::new();
        args.insert(
            "command".to_string(),
            ParamValue::String("/usr/bin/my-app --port 8080".to_string()),
        );
        args.insert(
            "description".to_string(),
            ParamValue::String("My Application".to_string()),
        );
        args.insert(
            "user".to_string(),
            ParamValue::String("www-data".to_string()),
        );
        args.insert(
            "group".to_string(),
            ParamValue::String("www-data".to_string()),
        );
        args.insert(
            "working-dir".to_string(),
            ParamValue::String("/opt/my-app".to_string()),
        );
        args.insert(
            "restart-policy".to_string(),
            ParamValue::String("always".to_string()),
        );
        args.insert("type".to_string(), ParamValue::String("simple".to_string()));
        args.insert(
            "after".to_string(),
            ParamValue::String("network.target".to_string()),
        );
        args.insert(
            "wanted-by".to_string(),
            ParamValue::String("multi-user.target".to_string()),
        );
        args.insert("environment".to_string(), ParamValue::Map(env));

        let params = ModuleParams {
            resource_name: "my-app".to_string(),
            args,
        };

        let content = SystemdModule::generate_unit_file("my-app", &params).unwrap();
        assert!(content.contains("Description=My Application"));
        assert!(content.contains("ExecStart=/usr/bin/my-app --port 8080"));
        assert!(content.contains("User=www-data"));
        assert!(content.contains("Group=www-data"));
        assert!(content.contains("WorkingDirectory=/opt/my-app"));
        assert!(content.contains("Restart=always"));
        assert!(content.contains("Environment=\"NODE_ENV=production\""));
        assert!(content.contains("Environment=\"PORT=8080\""));
    }

    #[test]
    fn test_generate_unit_file_empty_command() {
        let mut args = HashMap::new();
        args.insert("command".to_string(), ParamValue::String(String::new()));
        let params = ModuleParams {
            resource_name: "test".to_string(),
            args,
        };

        let result = SystemdModule::generate_unit_file("test", &params);
        assert!(result.is_err());
    }

    #[test]
    fn test_generate_unit_file_environment_not_map() {
        let mut args = HashMap::new();
        args.insert(
            "command".to_string(),
            ParamValue::String("/usr/bin/test".to_string()),
        );
        args.insert(
            "environment".to_string(),
            ParamValue::String("not a map".to_string()),
        );
        let params = ModuleParams {
            resource_name: "test".to_string(),
            args,
        };

        let result = SystemdModule::generate_unit_file("test", &params);
        assert!(result.is_err());
    }

    #[test]
    fn test_has_command_true() {
        let mut args = HashMap::new();
        args.insert(
            "command".to_string(),
            ParamValue::String("/usr/bin/app".to_string()),
        );
        let params = ModuleParams {
            resource_name: "test".to_string(),
            args,
        };
        assert!(SystemdModule::has_command(&params));
    }

    #[test]
    fn test_has_command_false() {
        let params = ModuleParams {
            resource_name: "test".to_string(),
            args: HashMap::new(),
        };
        assert!(!SystemdModule::has_command(&params));
    }

    #[test]
    fn test_has_command_empty_string() {
        let mut args = HashMap::new();
        args.insert("command".to_string(), ParamValue::String(String::new()));
        let params = ModuleParams {
            resource_name: "test".to_string(),
            args,
        };
        assert!(SystemdModule::has_command(&params));
    }

    #[test]
    fn test_validate_desired_state_valid() {
        assert!(SystemdModule::validate_desired_state("started").is_ok());
        assert!(SystemdModule::validate_desired_state("stopped").is_ok());
        assert!(SystemdModule::validate_desired_state("restarted").is_ok());
    }

    #[test]
    fn test_validate_desired_state_invalid() {
        assert!(SystemdModule::validate_desired_state("running").is_err());
        assert!(SystemdModule::validate_desired_state("").is_err());
    }

    #[test]
    fn test_validate_unit_name_for_creation_service() {
        assert!(SystemdModule::validate_unit_name_for_creation("my-app").is_ok());
        assert!(SystemdModule::validate_unit_name_for_creation("my-app.service").is_ok());
    }

    #[test]
    fn test_validate_unit_name_for_creation_non_service() {
        assert!(SystemdModule::validate_unit_name_for_creation("backup.timer").is_err());
        assert!(SystemdModule::validate_unit_name_for_creation("my.socket").is_err());
    }

    #[test]
    fn test_validate_env_value_valid() {
        assert!(SystemdModule::validate_env_value("PORT", "8080").is_ok());
        assert!(SystemdModule::validate_env_value("PATH", "/usr/bin:/bin").is_ok());
    }

    #[test]
    fn test_validate_env_value_newline_in_value() {
        assert!(SystemdModule::validate_env_value("KEY", "line1\nline2").is_err());
    }

    #[test]
    fn test_validate_env_value_bad_key() {
        assert!(SystemdModule::validate_env_value("KEY\"BAD", "val").is_err());
        assert!(SystemdModule::validate_env_value("KEY\nBAD", "val").is_err());
    }

    #[test]
    fn test_generate_unit_file_non_service_suffix() {
        let mut args = HashMap::new();
        args.insert(
            "command".to_string(),
            ParamValue::String("/usr/bin/app".to_string()),
        );
        let params = ModuleParams {
            resource_name: "backup.timer".to_string(),
            args,
        };
        let result = SystemdModule::generate_unit_file("backup.timer", &params);
        assert!(result.is_err());
    }

    #[test]
    fn test_environment_escaping() {
        let mut env = HashMap::new();
        env.insert("GREETING".to_string(), "hello \"world\"".to_string());

        let mut args = HashMap::new();
        args.insert(
            "command".to_string(),
            ParamValue::String("/usr/bin/app".to_string()),
        );
        args.insert("environment".to_string(), ParamValue::Map(env));
        let params = ModuleParams {
            resource_name: "test".to_string(),
            args,
        };

        let content = SystemdModule::generate_unit_file("test", &params).unwrap();
        assert!(content.contains(r#"Environment="GREETING=hello \"world\"""#));
    }

    #[test]
    fn test_unit_name_with_path_suffix() {
        assert_eq!(SystemdModule::unit_name("monitor.path"), "monitor.path");
    }

    #[test]
    fn test_unit_name_with_slice_suffix() {
        assert_eq!(SystemdModule::unit_name("user.slice"), "user.slice");
    }

    #[test]
    fn test_validate_unit_name_valid() {
        assert!(SystemdModule::validate_unit_name("my-app").is_ok());
        assert!(SystemdModule::validate_unit_name("backup.timer").is_ok());
    }

    #[test]
    fn test_validate_unit_name_path_traversal() {
        assert!(SystemdModule::validate_unit_name("../etc/passwd").is_err());
        assert!(SystemdModule::validate_unit_name("foo/bar").is_err());
    }

    #[test]
    fn test_validate_directive_value_valid() {
        assert!(SystemdModule::validate_directive_value("command", "/usr/bin/app").is_ok());
    }

    #[test]
    fn test_validate_directive_value_newline() {
        assert!(
            SystemdModule::validate_directive_value("command", "/bin/app\nExecStop=/bin/bad")
                .is_err()
        );
        assert!(SystemdModule::validate_directive_value("description", "line1\rline2").is_err());
    }

    #[test]
    fn test_generate_unit_file_command_with_newline() {
        let mut args = HashMap::new();
        args.insert(
            "command".to_string(),
            ParamValue::String("/usr/bin/app\nExecStop=/bin/evil".to_string()),
        );
        let params = ModuleParams {
            resource_name: "test".to_string(),
            args,
        };
        assert!(SystemdModule::generate_unit_file("test", &params).is_err());
    }
}
