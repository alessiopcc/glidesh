use crate::config::types::ResolvedJumpHost;
use crate::error::GlideshError;
use crate::ssh::HostKeyPolicy;
use crate::ssh::handler::SshHandler;
use crossterm::event::{self, Event, KeyModifiers};
use russh::client;
use russh_keys::key::PrivateKeyWithHashAlg;
use russh_sftp::client::SftpSession;
use russh_sftp::client::fs::File as SftpFile;
use russh_sftp::protocol::OpenFlags;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn ssh_config() -> Arc<client::Config> {
    Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(15)),
        keepalive_max: 3,
        ..client::Config::default()
    })
}

pub struct CommandOutput {
    pub exit_code: u32,
    pub stdout: String,
    pub stderr: String,
}

pub struct SshSession {
    handle: client::Handle<SshHandler>,
    host: String,
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
        let config = ssh_config();
        let host_key_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let handler = SshHandler {
            host: host.to_string(),
            port,
            host_key_policy,
            host_key_error: Arc::clone(&host_key_error),
        };

        tracing::debug!("Connecting to {}:{}", host, port);
        let mut handle = client::connect(config, (host, port), handler)
            .await
            .map_err(|e| {
                if let Some(reason) = host_key_error.lock().ok().and_then(|g| g.clone()) {
                    return GlideshError::SshConnection { message: reason };
                }
                GlideshError::SshConnection {
                    message: format!("Failed to connect to {}:{}: {}", host, port, e),
                }
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
        let jump_config = ssh_config();
        let jump_key_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let jump_handler = SshHandler {
            host: jump.address.clone(),
            port: jump.port,
            host_key_policy,
            host_key_error: Arc::clone(&jump_key_error),
        };

        tracing::debug!("Connecting to jump host {}:{}", jump.address, jump.port);
        let mut jump_handle = client::connect(
            jump_config,
            (jump.address.as_str(), jump.port),
            jump_handler,
        )
        .await
        .map_err(|e| {
            if let Some(reason) = jump_key_error.lock().ok().and_then(|g| g.clone()) {
                return GlideshError::SshConnection { message: reason };
            }
            GlideshError::SshConnection {
                message: format!(
                    "Failed to connect to jump host {}:{}: {}",
                    jump.address, jump.port, e
                ),
            }
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

        let target_config = ssh_config();
        let target_key_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let target_handler = SshHandler {
            host: host.to_string(),
            port,
            host_key_policy,
            host_key_error: Arc::clone(&target_key_error),
        };

        let mut handle = client::connect_stream(target_config, stream, target_handler)
            .await
            .map_err(|e| {
                if let Some(reason) = target_key_error.lock().ok().and_then(|g| g.clone()) {
                    return GlideshError::SshConnection { message: reason };
                }
                GlideshError::SshConnection {
                    message: format!(
                        "Failed SSH handshake through tunnel to {}:{}: {}",
                        host, port, e
                    ),
                }
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

    /// Returns (owner, group, octal_mode) for a remote file, or None if the file doesn't exist.
    pub async fn get_file_attrs(
        &self,
        path: &str,
    ) -> Result<Option<(String, String, String)>, GlideshError> {
        let escaped = shell_escape(path);
        let output = self.exec(&format!("stat -c '%U %G %a' {escaped}")).await?;

        if output.exit_code != 0 {
            let combined = format!("{}{}", output.stdout, output.stderr).to_lowercase();
            if combined.contains("no such file or directory") || combined.contains("cannot stat") {
                return Ok(None);
            }
            return Err(GlideshError::Module {
                module: "file".to_string(),
                message: format!(
                    "stat of '{}' failed (exit {}): {}{}",
                    path,
                    output.exit_code,
                    output.stdout.trim(),
                    output.stderr.trim(),
                ),
            });
        }

        let parts: Vec<&str> = output.stdout.trim().splitn(3, ' ').collect();
        if parts.len() == 3 {
            Ok(Some((
                parts[0].to_string(),
                parts[1].to_string(),
                parts[2].to_string(),
            )))
        } else {
            Ok(None)
        }
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

    pub async fn set_file_attrs_recursive(
        &self,
        path: &str,
        owner: Option<&str>,
        group: Option<&str>,
        mode: Option<&str>,
    ) -> Result<(), GlideshError> {
        let escaped = shell_escape(path);

        if let Some(mode) = mode {
            let output = self
                .exec(&format!("chmod -R {} {}", shell_escape(mode), escaped))
                .await?;
            if output.exit_code != 0 {
                return Err(GlideshError::Module {
                    module: "file".to_string(),
                    message: format!("chmod -R failed: {}", output.stderr),
                });
            }
        }

        match (owner, group) {
            (Some(o), Some(g)) => {
                let output = self
                    .exec(&format!(
                        "chown -R {}:{} {}",
                        shell_escape(o),
                        shell_escape(g),
                        escaped
                    ))
                    .await?;
                if output.exit_code != 0 {
                    return Err(GlideshError::Module {
                        module: "file".to_string(),
                        message: format!("chown -R failed: {}", output.stderr),
                    });
                }
            }
            (Some(o), None) => {
                let output = self
                    .exec(&format!("chown -R {} {}", shell_escape(o), escaped))
                    .await?;
                if output.exit_code != 0 {
                    return Err(GlideshError::Module {
                        module: "file".to_string(),
                        message: format!("chown -R failed: {}", output.stderr),
                    });
                }
            }
            (None, Some(g)) => {
                let output = self
                    .exec(&format!("chgrp -R {} {}", shell_escape(g), escaped))
                    .await?;
                if output.exit_code != 0 {
                    return Err(GlideshError::Module {
                        module: "file".to_string(),
                        message: format!("chgrp -R failed: {}", output.stderr),
                    });
                }
            }
            (None, None) => {}
        }

        Ok(())
    }

    /// Open an interactive PTY shell session.
    /// Takes over stdin/stdout until the remote shell exits.
    /// Returns the remote exit code.
    pub async fn interactive_shell(&self) -> Result<u32, GlideshError> {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));

        let mut channel =
            self.handle
                .channel_open_session()
                .await
                .map_err(|e| GlideshError::SshChannel {
                    message: format!("Failed to open session on {}: {}", self.host, e),
                })?;

        channel
            .request_pty(true, "xterm-256color", cols as u32, rows as u32, 0, 0, &[])
            .await
            .map_err(|e| GlideshError::SshChannel {
                message: format!("Failed to request PTY on {}: {}", self.host, e),
            })?;

        channel
            .request_shell(true)
            .await
            .map_err(|e| GlideshError::SshChannel {
                message: format!("Failed to request shell on {}: {}", self.host, e),
            })?;

        crossterm::terminal::enable_raw_mode().map_err(|e| GlideshError::Other(e.to_string()))?;
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste);

        let exit_code = self.pty_proxy_loop(&mut channel).await;

        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
        let _ = crossterm::terminal::disable_raw_mode();

        match exit_code {
            Ok(code) => Ok(code),
            Err(e) => Err(e),
        }
    }

    async fn pty_proxy_loop(
        &self,
        channel: &mut russh::Channel<russh::client::Msg>,
    ) -> Result<u32, GlideshError> {
        let mut exit_code: u32 = 0;
        let mut last_size = crossterm::terminal::size().unwrap_or((80, 24));

        let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let stdin_reader = tokio::task::spawn_blocking(move || {
            loop {
                if input_tx.is_closed() {
                    break;
                }
                match event::poll(Duration::from_millis(50)) {
                    Ok(true) => {
                        if let Ok(ev) = event::read() {
                            // Filter out Release/Repeat events (Windows sends both)
                            if let Event::Key(ref k) = ev {
                                if k.kind != crossterm::event::KeyEventKind::Press {
                                    continue;
                                }
                            }
                            if input_tx.send(ev).is_err() {
                                break;
                            }
                        }
                    }
                    Ok(false) => {}
                    Err(_) => break,
                }
            }
        });

        loop {
            tokio::select! {
                msg = channel.wait() => {
                    match msg {
                        Some(russh::ChannelMsg::Data { ref data }) => {
                            let mut stdout = std::io::stdout().lock();
                            let _ = stdout.write_all(data);
                            let _ = stdout.flush();
                        }
                        Some(russh::ChannelMsg::ExitStatus { exit_status }) => {
                            exit_code = exit_status;
                        }
                        Some(russh::ChannelMsg::Eof) | None => {
                            break;
                        }
                        _ => {}
                    }
                }
                ev = input_rx.recv() => {
                    match ev {
                        Some(Event::Key(key)) => {
                            let data = key_event_to_bytes(&key);
                            if !data.is_empty() {
                                let _ = channel.data(&data[..]).await;
                            }
                        }
                        Some(Event::Paste(text)) => {
                            let _ = channel.data(text.as_bytes()).await;
                        }
                        Some(Event::Resize(cols, rows)) => {
                            if (cols, rows) != last_size {
                                last_size = (cols, rows);
                                let _ = channel
                                    .window_change(cols as u32, rows as u32, 0, 0)
                                    .await;
                            }
                        }
                        None => break,
                        _ => {}
                    }
                }
            }
        }

        // Drop the receiver so input_tx.send() fails, causing the reader to exit
        drop(input_rx);
        let _ = stdin_reader.await;
        Ok(exit_code)
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

/// Convert a crossterm key event into the byte sequence expected by a remote terminal.
fn key_event_to_bytes(key: &crossterm::event::KeyEvent) -> Vec<u8> {
    use crossterm::event::KeyCode;

    // AltGr on Windows is reported as Ctrl+Alt. Only treat as a real Ctrl
    // shortcut when Alt is NOT pressed, so AltGr-produced characters (brackets,
    // braces, etc. on non-US keyboard layouts) pass through normally.
    let ctrl =
        key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        KeyCode::Char(c) if ctrl => {
            // Ctrl+A..Z maps to 0x01..0x1A
            let b = c.to_ascii_lowercase() as u8;
            if b.is_ascii_lowercase() {
                vec![b - b'a' + 1]
            } else {
                vec![]
            }
        }
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            s.as_bytes().to_vec()
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::F(n) => f_key_escape(n),
        _ => vec![],
    }
}

fn f_key_escape(n: u8) -> Vec<u8> {
    match n {
        1 => b"\x1bOP".to_vec(),
        2 => b"\x1bOQ".to_vec(),
        3 => b"\x1bOR".to_vec(),
        4 => b"\x1bOS".to_vec(),
        5 => b"\x1b[15~".to_vec(),
        6 => b"\x1b[17~".to_vec(),
        7 => b"\x1b[18~".to_vec(),
        8 => b"\x1b[19~".to_vec(),
        9 => b"\x1b[20~".to_vec(),
        10 => b"\x1b[21~".to_vec(),
        11 => b"\x1b[23~".to_vec(),
        12 => b"\x1b[24~".to_vec(),
        _ => vec![],
    }
}
