use crate::config::types::ResolvedJumpHost;
use crate::error::GlideshError;
use crate::ssh::HostKeyPolicy;
use crate::ssh::handler::SshHandler;
use russh::client;
use russh_keys::key::PrivateKeyWithHashAlg;
use russh_sftp::client::SftpSession;
use russh_sftp::client::fs::File as SftpFile;
use russh_sftp::protocol::OpenFlags;
use std::sync::Arc;

pub struct CommandOutput {
    pub exit_code: u32,
    pub stdout: String,
    pub stderr: String,
}

pub struct SshSession {
    handle: client::Handle<SshHandler>,
    host: String,
    /// Hold the jump host handle alive for the lifetime of the tunneled session.
    _jump_handle: Option<client::Handle<SshHandler>>,
}

impl SshSession {
    pub async fn connect(
        host: &str,
        port: u16,
        user: &str,
        key: &PrivateKeyWithHashAlg,
        host_key_policy: HostKeyPolicy,
    ) -> Result<Self, GlideshError> {
        let config = Arc::new(client::Config::default());
        let handler = SshHandler {
            host: host.to_string(),
            port,
            host_key_policy,
        };

        tracing::debug!("Connecting to {}:{}", host, port);
        let mut handle = client::connect(config, (host, port), handler)
            .await
            .map_err(|e| GlideshError::SshConnection {
                message: format!("Failed to connect to {}:{}: {}", host, port, e),
            })?;

        tracing::debug!("TCP connected, authenticating as '{}' with pubkey", user);
        let auth_result = handle
            .authenticate_publickey(user, key.clone())
            .await
            .map_err(|e| GlideshError::SshAuth {
                host: host.to_string(),
                user: user.to_string(),
                message: e.to_string(),
            })?;

        tracing::debug!("Auth result: {}", auth_result);

        if !auth_result {
            return Err(GlideshError::SshAuth {
                host: host.to_string(),
                user: user.to_string(),
                message: "Authentication rejected by server".to_string(),
            });
        }

        Ok(SshSession {
            handle,
            host: host.to_string(),
            _jump_handle: None,
        })
    }

    /// Connect to a target host through a jump (bastion) host.
    ///
    /// 1. Establishes an SSH session to the jump host
    /// 2. Opens a direct-tcpip channel through the jump host to the target
    /// 3. Runs the SSH protocol over that channel to authenticate with the target
    pub async fn connect_via_jump(
        host: &str,
        port: u16,
        user: &str,
        key: &PrivateKeyWithHashAlg,
        host_key_policy: HostKeyPolicy,
        jump: &ResolvedJumpHost,
    ) -> Result<Self, GlideshError> {
        // Connect and authenticate to the jump host
        let jump_config = Arc::new(client::Config::default());
        let jump_handler = SshHandler {
            host: jump.address.clone(),
            port: jump.port,
            host_key_policy,
        };

        tracing::debug!("Connecting to jump host {}:{}", jump.address, jump.port);
        let mut jump_handle = client::connect(
            jump_config,
            (jump.address.as_str(), jump.port),
            jump_handler,
        )
        .await
        .map_err(|e| GlideshError::SshConnection {
            message: format!(
                "Failed to connect to jump host {}:{}: {}",
                jump.address, jump.port, e
            ),
        })?;

        tracing::debug!("Jump host TCP connected, authenticating as '{}'", jump.user);
        let jump_auth = jump_handle
            .authenticate_publickey(&jump.user, key.clone())
            .await
            .map_err(|e| GlideshError::SshAuth {
                host: jump.address.clone(),
                user: jump.user.clone(),
                message: format!("Jump host auth failed: {}", e),
            })?;

        if !jump_auth {
            return Err(GlideshError::SshAuth {
                host: jump.address.clone(),
                user: jump.user.clone(),
                message: "Jump host authentication rejected by server".to_string(),
            });
        }

        // Open a tunnel through the jump host to the target
        tracing::debug!("Opening tunnel through jump host to {}:{}", host, port);
        let channel = jump_handle
            .channel_open_direct_tcpip(host, port as u32, "127.0.0.1", 0)
            .await
            .map_err(|e| GlideshError::SshConnection {
                message: format!(
                    "Failed to open tunnel through {} to {}:{}: {}",
                    jump.address, host, port, e
                ),
            })?;

        let stream = channel.into_stream();

        // Run SSH over the tunneled channel
        let target_config = Arc::new(client::Config::default());
        let target_handler = SshHandler {
            host: host.to_string(),
            port,
            host_key_policy,
        };

        let mut handle = client::connect_stream(target_config, stream, target_handler)
            .await
            .map_err(|e| GlideshError::SshConnection {
                message: format!(
                    "Failed SSH handshake through tunnel to {}:{}: {}",
                    host, port, e
                ),
            })?;

        tracing::debug!("Tunnel established, authenticating as '{}' on target", user);
        let auth_result = handle
            .authenticate_publickey(user, key.clone())
            .await
            .map_err(|e| GlideshError::SshAuth {
                host: host.to_string(),
                user: user.to_string(),
                message: e.to_string(),
            })?;

        if !auth_result {
            return Err(GlideshError::SshAuth {
                host: host.to_string(),
                user: user.to_string(),
                message: "Authentication rejected by target server (via jump host)".to_string(),
            });
        }

        Ok(SshSession {
            handle,
            host: host.to_string(),
            _jump_handle: Some(jump_handle),
        })
    }

    pub async fn exec(&self, command: &str) -> Result<CommandOutput, GlideshError> {
        let mut channel =
            self.handle
                .channel_open_session()
                .await
                .map_err(|e| GlideshError::SshChannel {
                    message: format!("Failed to open channel on {}: {}", self.host, e),
                })?;

        channel
            .exec(true, command)
            .await
            .map_err(|e| GlideshError::SshChannel {
                message: format!("Failed to exec command on {}: {}", self.host, e),
            })?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code: u32 = 0;

        loop {
            let Some(msg) = channel.wait().await else {
                break;
            };

            match msg {
                russh::ChannelMsg::Data { ref data } => {
                    stdout.extend_from_slice(data);
                }
                russh::ChannelMsg::ExtendedData { ref data, ext } => {
                    if ext == 1 {
                        stderr.extend_from_slice(data);
                    }
                }
                russh::ChannelMsg::ExitStatus { exit_status } => {
                    exit_code = exit_status;
                }
                _ => {}
            }
        }

        Ok(CommandOutput {
            exit_code,
            stdout: String::from_utf8_lossy(&stdout).to_string(),
            stderr: String::from_utf8_lossy(&stderr).to_string(),
        })
    }

    async fn sftp(&self) -> Result<SftpSession, GlideshError> {
        let channel =
            self.handle
                .channel_open_session()
                .await
                .map_err(|e| GlideshError::SshChannel {
                    message: format!("Failed to open SFTP channel on {}: {}", self.host, e),
                })?;

        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| GlideshError::SshChannel {
                message: format!("Failed to request SFTP subsystem on {}: {}", self.host, e),
            })?;

        let sftp = SftpSession::new(channel.into_stream()).await.map_err(|e| {
            GlideshError::SshChannel {
                message: format!("Failed to initialize SFTP session on {}: {}", self.host, e),
            }
        })?;

        Ok(sftp)
    }

    pub async fn upload_file(&self, content: &[u8], remote_path: &str) -> Result<(), GlideshError> {
        use tokio::io::AsyncWriteExt;

        let sftp = self.sftp().await?;
        let mut file: SftpFile = sftp
            .open_with_flags(
                remote_path,
                OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
            )
            .await
            .map_err(|e| GlideshError::SshChannel {
                message: format!(
                    "Failed to open file {} on {}: {}",
                    remote_path, self.host, e
                ),
            })?;
        file.write_all(content)
            .await
            .map_err(|e| GlideshError::SshChannel {
                message: format!("Failed to write to {} on {}: {}", remote_path, self.host, e),
            })?;
        file.shutdown()
            .await
            .map_err(|e| GlideshError::SshChannel {
                message: format!(
                    "Failed to flush file {} on {}: {}",
                    remote_path, self.host, e
                ),
            })?;
        sftp.close().await.map_err(|e| GlideshError::SshChannel {
            message: format!("Failed to close SFTP session on {}: {}", self.host, e),
        })?;
        Ok(())
    }

    pub async fn download_file(&self, remote_path: &str) -> Result<Vec<u8>, GlideshError> {
        let sftp = self.sftp().await?;
        let data = sftp
            .read(remote_path)
            .await
            .map_err(|e| GlideshError::SshChannel {
                message: format!(
                    "Failed to download file {} from {}: {}",
                    remote_path, self.host, e
                ),
            })?;
        sftp.close().await.map_err(|e| GlideshError::SshChannel {
            message: format!("Failed to close SFTP session on {}: {}", self.host, e),
        })?;
        Ok(data)
    }

    pub async fn checksum_remote(&self, remote_path: &str) -> Result<Option<String>, GlideshError> {
        let escaped = shell_escape(remote_path);
        let output = self
            .exec(&format!(
                "sha256sum {escaped} 2>&1 || shasum -a 256 {escaped} 2>&1",
            ))
            .await?;

        if output.exit_code != 0 {
            let combined = format!("{}{}", output.stdout, output.stderr).to_lowercase();
            if combined.contains("no such file or directory") || combined.contains("cannot open") {
                return Ok(None);
            }
            return Err(GlideshError::Module {
                module: "file".to_string(),
                message: format!(
                    "checksum of '{}' failed (exit {}): {}",
                    remote_path,
                    output.exit_code,
                    output.stdout.trim(),
                ),
            });
        }

        Ok(output
            .stdout
            .split_whitespace()
            .next()
            .map(|s| s.to_string()))
    }

    pub async fn set_file_attrs(
        &self,
        path: &str,
        owner: Option<&str>,
        group: Option<&str>,
        mode: Option<&str>,
    ) -> Result<(), GlideshError> {
        let escaped = shell_escape(path);

        if let Some(mode) = mode {
            let output = self
                .exec(&format!("chmod {} {}", shell_escape(mode), escaped))
                .await?;
            if output.exit_code != 0 {
                return Err(GlideshError::Module {
                    module: "file".to_string(),
                    message: format!("chmod failed: {}", output.stderr),
                });
            }
        }

        match (owner, group) {
            (Some(o), Some(g)) => {
                let output = self
                    .exec(&format!(
                        "chown {}:{} {}",
                        shell_escape(o),
                        shell_escape(g),
                        escaped
                    ))
                    .await?;
                if output.exit_code != 0 {
                    return Err(GlideshError::Module {
                        module: "file".to_string(),
                        message: format!("chown failed: {}", output.stderr),
                    });
                }
            }
            (Some(o), None) => {
                let output = self
                    .exec(&format!("chown {} {}", shell_escape(o), escaped))
                    .await?;
                if output.exit_code != 0 {
                    return Err(GlideshError::Module {
                        module: "file".to_string(),
                        message: format!("chown failed: {}", output.stderr),
                    });
                }
            }
            (None, Some(g)) => {
                let output = self
                    .exec(&format!("chgrp {} {}", shell_escape(g), escaped))
                    .await?;
                if output.exit_code != 0 {
                    return Err(GlideshError::Module {
                        module: "file".to_string(),
                        message: format!("chgrp failed: {}", output.stderr),
                    });
                }
            }
            (None, None) => {}
        }

        Ok(())
    }

    pub async fn close(self) -> Result<(), GlideshError> {
        self.handle
            .disconnect(russh::Disconnect::ByApplication, "session closed", "en")
            .await
            .map_err(|e| GlideshError::SshConnection {
                message: format!("Error closing connection to {}: {}", self.host, e),
            })?;

        if let Some(jump_handle) = self._jump_handle {
            jump_handle
                .disconnect(russh::Disconnect::ByApplication, "session closed", "en")
                .await
                .map_err(|e| GlideshError::SshConnection {
                    message: format!(
                        "Error closing jump host connection for {}: {}",
                        self.host, e
                    ),
                })?;
        }

        Ok(())
    }
}

/// Escape a string for safe use in shell commands by wrapping in single quotes.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
