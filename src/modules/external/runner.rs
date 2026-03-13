use crate::error::GlideshError;
use crate::modules::context::ModuleContext;
use crate::modules::external::protocol::{
    CheckResponse, ModuleRequest, PluginMessage, ShutdownRequest, SshRequest, SshResponse,
};
use crate::modules::{ModuleParams, ModuleResult, ModuleStatus};
use crate::ssh::SshSession;
use async_trait::async_trait;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;

use super::discovery::ExternalModuleInfo;

const PLUGIN_TIMEOUT: Duration = Duration::from_secs(60);

pub struct ExternalModule {
    info: ExternalModuleInfo,
}

impl ExternalModule {
    pub fn new(info: ExternalModuleInfo) -> Self {
        Self { info }
    }

    fn spawn_plugin(&self) -> Result<Child, GlideshError> {
        super::discovery::build_tokio_command(&self.info)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| GlideshError::Module {
                module: self.info.name.clone(),
                message: format!(
                    "Failed to spawn plugin '{}': {}",
                    self.info.path.display(),
                    e
                ),
            })
    }

    async fn run_method(
        &self,
        method: &str,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<PluginMessage, GlideshError> {
        let mut child = self.spawn_plugin()?;

        let stdin = child.stdin.take().ok_or_else(|| GlideshError::Module {
            module: self.info.name.clone(),
            message: "Failed to open plugin stdin".to_string(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| GlideshError::Module {
            module: self.info.name.clone(),
            message: "Failed to open plugin stdout".to_string(),
        })?;

        let mut writer = tokio::io::BufWriter::new(stdin);
        let mut reader = BufReader::new(stdout);

        let result = tokio::time::timeout(
            PLUGIN_TIMEOUT,
            self.run_method_inner(method, ctx, params, &mut writer, &mut reader),
        )
        .await;

        // Send shutdown and clean up
        let _ = send_line(&mut writer, &ShutdownRequest::new()).await;
        let _ = child.kill().await;

        // Collect stderr for logging
        if let Some(mut stderr) = child.stderr.take() {
            let mut stderr_buf = String::new();
            let _ = tokio::io::AsyncReadExt::read_to_string(&mut stderr, &mut stderr_buf).await;
            if !stderr_buf.is_empty() {
                tracing::debug!("Plugin '{}' stderr: {}", self.info.name, stderr_buf.trim());
            }
        }

        match result {
            Ok(inner) => inner,
            Err(_) => Err(GlideshError::Module {
                module: self.info.name.clone(),
                message: "Plugin timed out".to_string(),
            }),
        }
    }

    async fn run_method_inner<W, R>(
        &self,
        method: &str,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
        writer: &mut tokio::io::BufWriter<W>,
        reader: &mut BufReader<R>,
    ) -> Result<PluginMessage, GlideshError>
    where
        W: tokio::io::AsyncWrite + Unpin,
        R: tokio::io::AsyncRead + Unpin,
    {
        let request = ModuleRequest {
            method,
            resource_name: &params.resource_name,
            args: &params.args,
            os_info: ctx.os_info,
            vars: ctx.vars,
            dry_run: ctx.dry_run,
        };

        send_line(writer, &request).await?;

        // Read lines until we get a terminal response (not an SSH request)
        loop {
            let line = read_line(reader).await.map_err(|e| GlideshError::Module {
                module: self.info.name.clone(),
                message: format!("Failed to read from plugin: {}", e),
            })?;

            let msg: PluginMessage =
                serde_json::from_str(&line).map_err(|e| GlideshError::Module {
                    module: self.info.name.clone(),
                    message: format!("Malformed JSON from plugin: {} (line: {})", e, line),
                })?;

            match msg {
                PluginMessage::SshRequest(ssh_req) => {
                    let response = handle_ssh_request(ctx.ssh, ssh_req).await;
                    send_line(writer, &response).await?;
                }
                terminal => return Ok(terminal),
            }
        }
    }
}

#[async_trait]
impl crate::modules::Module for ExternalModule {
    fn name(&self) -> &str {
        &self.info.name
    }

    async fn check(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleStatus, GlideshError> {
        let msg = self.run_method("check", ctx, params).await?;
        match msg {
            PluginMessage::CheckResponse(resp) => match resp {
                CheckResponse::Satisfied => Ok(ModuleStatus::Satisfied),
                CheckResponse::Pending { plan } => Ok(ModuleStatus::Pending { plan }),
                CheckResponse::Unknown { reason } => Ok(ModuleStatus::Unknown { reason }),
            },
            PluginMessage::Error(e) => Err(GlideshError::Module {
                module: self.info.name.clone(),
                message: e.error,
            }),
            PluginMessage::ApplyResponse(_) => Err(GlideshError::Module {
                module: self.info.name.clone(),
                message: "Plugin returned apply response for check method".to_string(),
            }),
            PluginMessage::SshRequest(_) => unreachable!("SSH requests handled in loop"),
        }
    }

    async fn apply(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleResult, GlideshError> {
        let msg = self.run_method("apply", ctx, params).await?;
        match msg {
            PluginMessage::ApplyResponse(resp) => Ok(ModuleResult {
                changed: resp.changed,
                output: resp.output,
                stderr: resp.stderr,
                exit_code: resp.exit_code,
            }),
            PluginMessage::Error(e) => Err(GlideshError::Module {
                module: self.info.name.clone(),
                message: e.error,
            }),
            PluginMessage::CheckResponse(_) => Err(GlideshError::Module {
                module: self.info.name.clone(),
                message: "Plugin returned check response for apply method".to_string(),
            }),
            PluginMessage::SshRequest(_) => unreachable!("SSH requests handled in loop"),
        }
    }
}

async fn handle_ssh_request(ssh: &SshSession, req: SshRequest) -> SshResponse {
    match req {
        SshRequest::Exec { command } => match ssh.exec(&command).await {
            Ok(output) => SshResponse::Exec {
                exit_code: output.exit_code,
                stdout: output.stdout,
                stderr: output.stderr,
            },
            Err(e) => SshResponse::Exec {
                exit_code: 255,
                stdout: String::new(),
                stderr: e.to_string(),
            },
        },
        SshRequest::Upload {
            path,
            content_base64,
        } => {
            use base64::Engine as _;
            let decoded = match base64::engine::general_purpose::STANDARD.decode(&content_base64) {
                Ok(d) => d,
                Err(e) => {
                    return SshResponse::Upload {
                        ok: false,
                        error: Some(format!("Invalid base64: {}", e)),
                    };
                }
            };
            match ssh.upload_file(&decoded, &path).await {
                Ok(()) => SshResponse::Upload {
                    ok: true,
                    error: None,
                },
                Err(e) => SshResponse::Upload {
                    ok: false,
                    error: Some(e.to_string()),
                },
            }
        }
        SshRequest::Download { path } => match ssh.download_file(&path).await {
            Ok(data) => {
                use base64::Engine as _;
                SshResponse::Download {
                    content_base64: base64::engine::general_purpose::STANDARD.encode(&data),
                    exists: true,
                }
            }
            Err(_) => SshResponse::Download {
                content_base64: String::new(),
                exists: false,
            },
        },
        SshRequest::Checksum { path } => match ssh.checksum_remote(&path).await {
            Ok(Some(hash)) => SshResponse::Checksum { hash, exists: true },
            Ok(None) => SshResponse::Checksum {
                hash: String::new(),
                exists: false,
            },
            Err(_) => SshResponse::Checksum {
                hash: String::new(),
                exists: false,
            },
        },
        SshRequest::SetAttrs {
            path,
            owner,
            group,
            mode,
        } => {
            match ssh
                .set_file_attrs(&path, owner.as_deref(), group.as_deref(), mode.as_deref())
                .await
            {
                Ok(()) => SshResponse::SetAttrs {
                    ok: true,
                    error: None,
                },
                Err(e) => SshResponse::SetAttrs {
                    ok: false,
                    error: Some(e.to_string()),
                },
            }
        }
    }
}

async fn send_line<T: serde::Serialize, W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut tokio::io::BufWriter<W>,
    msg: &T,
) -> Result<(), GlideshError> {
    let json = serde_json::to_string(msg).map_err(|e| GlideshError::Module {
        module: "external".to_string(),
        message: format!("Failed to serialize message: {}", e),
    })?;
    writer
        .write_all(json.as_bytes())
        .await
        .map_err(|e| GlideshError::Module {
            module: "external".to_string(),
            message: format!("Failed to write to plugin: {}", e),
        })?;
    writer
        .write_all(b"\n")
        .await
        .map_err(|e| GlideshError::Module {
            module: "external".to_string(),
            message: format!("Failed to write newline to plugin: {}", e),
        })?;
    writer.flush().await.map_err(|e| GlideshError::Module {
        module: "external".to_string(),
        message: format!("Failed to flush plugin stdin: {}", e),
    })?;
    Ok(())
}

async fn read_line<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> std::io::Result<String> {
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "Plugin process closed stdout",
        ));
    }
    Ok(line)
}

#[cfg(test)]
mod tests {
    use crate::modules::external::protocol::*;

    #[test]
    fn test_check_response_satisfied_deserialize() {
        let json = r#"{"status":"satisfied"}"#;
        let msg: PluginMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(
            msg,
            PluginMessage::CheckResponse(CheckResponse::Satisfied)
        ));
    }

    #[test]
    fn test_check_response_pending_deserialize() {
        let json = r#"{"status":"pending","plan":"Install nginx"}"#;
        let msg: PluginMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(
            msg,
            PluginMessage::CheckResponse(CheckResponse::Pending { .. })
        ));
    }

    #[test]
    fn test_apply_response_deserialize() {
        let json = r#"{"changed":true,"output":"done","stderr":"","exit_code":0}"#;
        let msg: PluginMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, PluginMessage::ApplyResponse(_)));
    }

    #[test]
    fn test_ssh_exec_request_deserialize() {
        let json = r#"{"ssh":"exec","command":"whoami"}"#;
        let msg: PluginMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(
            msg,
            PluginMessage::SshRequest(SshRequest::Exec { .. })
        ));
    }

    #[test]
    fn test_ssh_upload_request_deserialize() {
        let json = r#"{"ssh":"upload","path":"/tmp/f","content_base64":"aGVsbG8="}"#;
        let msg: PluginMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(
            msg,
            PluginMessage::SshRequest(SshRequest::Upload { .. })
        ));
    }

    #[test]
    fn test_error_response_deserialize() {
        let json = r#"{"error":"something broke"}"#;
        let msg: PluginMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, PluginMessage::Error(_)));
    }

    #[test]
    fn test_ssh_response_exec_serialize() {
        let resp = SshResponse::Exec {
            exit_code: 0,
            stdout: "root".to_string(),
            stderr: String::new(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("ssh_result"));
        assert!(json.contains("exec"));
    }

    #[test]
    fn test_module_request_serialize() {
        let req = ModuleRequest {
            method: "check",
            resource_name: "test",
            args: &std::collections::HashMap::new(),
            os_info: &crate::modules::detect::OsInfo {
                id: "ubuntu".to_string(),
                version: "22.04".to_string(),
                family: crate::modules::detect::OsFamily::Debian,
                pkg_manager: crate::modules::detect::PkgManager::Apt,
                init_system: crate::modules::detect::InitSystem::Systemd,
                container_runtime: None,
            },
            vars: &std::collections::HashMap::new(),
            dry_run: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"method\":\"check\""));
        assert!(json.contains("\"resource_name\":\"test\""));
        assert!(json.contains("\"family\":\"debian\""));
    }
}
