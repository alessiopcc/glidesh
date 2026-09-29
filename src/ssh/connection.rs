use crate::config::types::{ResolvedJumpHost, ResolvedRunAs};
use crate::error::GlideshError;
use crate::modules::escalation;
use crate::ssh::HostKeyPolicy;
use crate::ssh::handler::{ForwardRegistry, SshHandler, new_forward_registry};
use crate::util::shell_escape;
use crossterm::event::{self, Event, KeyModifiers};
use russh::Channel;
use russh::client;
use russh_keys::key::PrivateKeyWithHashAlg;
use russh_sftp::client::SftpSession;
use russh_sftp::client::fs::File as SftpFile;
use russh_sftp::protocol::OpenFlags;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

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
    /// `stdout` went over [`OUTPUT_LIMIT`] and lost its middle.
    pub stdout_cut: bool,
}

impl CommandOutput {
    /// Why the command failed: stderr, or stdout when stderr is empty — under a PTY
    /// (`su`) the server merges stderr into stdout.
    pub fn failure(&self) -> &str {
        match self.stderr.trim() {
            "" => self.stdout.trim(),
            err => err,
        }
    }
}

/// Exit code reported for a command that did not exit with a status of its own, as the
/// OpenSSH client does.
pub const NO_EXIT_STATUS: u32 = 255;

/// The exit code of a finished command, and why it is not the command's own when it is not.
///
/// An exit status is optional in SSH: a command killed by a signal sends `exit-signal`
/// instead, and a dropped channel sends nothing. Neither is success, so neither may read as
/// exit 0 — a `check=` guard or an `until=` gate would pass on it.
fn exit_outcome(status: Option<u32>, signal: Option<&str>) -> (u32, Option<String>) {
    match (status, signal) {
        (Some(code), _) => (code, None),
        (None, Some(signal)) => (
            NO_EXIT_STATUS,
            Some(format!("command killed by signal {signal}")),
        ),
        (None, None) => (
            NO_EXIT_STATUS,
            Some("connection closed before the command reported an exit status".into()),
        ),
    }
}

/// Most bytes of one output stream (stdout or stderr) a command keeps in memory: the first
/// half and the latest half. A command that prints without end — a `yes`, a chatty build, a
/// log followed by an `until=` gate — would otherwise grow the controller's memory until the
/// command exits, for every host at once.
pub const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;

/// One output stream, capped at a limit: it keeps the start, where a command says what it
/// is doing, and the end, where it says how it failed.
struct CappedStream {
    limit: usize,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    dropped: u64,
}

/// How much to reserve before adding `adding` bytes to a buffer holding `len` of `capacity`,
/// if it must grow: double, as `Vec` would, but never past `max` — left to itself, the
/// doubling could allocate up to twice the limit.
fn growth(len: usize, capacity: usize, adding: usize, max: usize) -> Option<usize> {
    let needed = len + adding;
    (needed > capacity).then(|| (capacity * 2).max(needed).min(max) - len)
}

impl CappedStream {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            head: Vec::new(),
            tail: VecDeque::new(),
            dropped: 0,
        }
    }

    fn push(&mut self, mut data: &[u8]) {
        let head_limit = self.limit / 2;
        let to_head = (head_limit - self.head.len()).min(data.len());
        if let Some(more) = growth(self.head.len(), self.head.capacity(), to_head, head_limit) {
            self.head.reserve_exact(more);
        }
        self.head.extend_from_slice(&data[..to_head]);
        data = &data[to_head..];

        // Evict before extending, so the deque never holds more than `tail_limit` bytes and
        // never allocates past them (draining does not give capacity back).
        let tail_limit = self.limit - head_limit;
        if data.len() >= tail_limit {
            self.dropped += (self.tail.len() + data.len() - tail_limit) as u64;
            self.tail.clear();
            data = &data[data.len() - tail_limit..];
        } else {
            let over = (self.tail.len() + data.len()).saturating_sub(tail_limit);
            self.tail.drain(..over);
            self.dropped += over as u64;
        }
        if let Some(more) = growth(
            self.tail.len(),
            self.tail.capacity(),
            data.len(),
            tail_limit,
        ) {
            self.tail.reserve_exact(more);
        }
        self.tail.extend(data);
    }

    /// The kept text, and whether anything was dropped.
    fn finish(self) -> (String, bool) {
        let mut bytes = self.head;
        let cut = self.dropped > 0;
        if cut {
            bytes.extend_from_slice(
                format!(
                    "\n[glidesh: {} bytes of output dropped here]\n",
                    self.dropped
                )
                .as_bytes(),
            );
        }
        bytes.extend(self.tail);
        (String::from_utf8_lossy(&bytes).into_owned(), cut)
    }
}

/// Optional controls for [`SshSession::exec_with`]: feed bytes on stdin (e.g. a
/// `sudo -S` password) and/or allocate a PTY (required by `su`).
#[derive(Default)]
pub struct ExecOptions {
    pub stdin: Option<Vec<u8>>,
    pub pty: bool,
}

pub struct SshSession {
    handle: tokio::sync::Mutex<client::Handle<SshHandler>>,
    host: String,
    _jump_handle: tokio::sync::Mutex<Option<client::Handle<SshHandler>>>,
    forward_registry: ForwardRegistry,
    /// The login user's uid, read once: [`trusted_paths`] trusts it.
    login_uid: tokio::sync::OnceCell<String>,
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
        let forward_registry = new_forward_registry();
        let handler = SshHandler {
            host: host.to_string(),
            port,
            host_key_policy,
            host_key_error: Arc::clone(&host_key_error),
            forward_registry: Arc::clone(&forward_registry),
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
            handle: tokio::sync::Mutex::new(handle),
            host: host.to_string(),
            _jump_handle: tokio::sync::Mutex::new(None),
            forward_registry,
            login_uid: tokio::sync::OnceCell::new(),
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
        let jump_forward_registry = new_forward_registry();
        let jump_handler = SshHandler {
            host: jump.address.clone(),
            port: jump.port,
            host_key_policy,
            host_key_error: Arc::clone(&jump_key_error),
            forward_registry: Arc::clone(&jump_forward_registry),
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
        let forward_registry = new_forward_registry();
        let target_handler = SshHandler {
            host: host.to_string(),
            port,
            host_key_policy,
            host_key_error: Arc::clone(&target_key_error),
            forward_registry: Arc::clone(&forward_registry),
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
            handle: tokio::sync::Mutex::new(handle),
            host: host.to_string(),
            _jump_handle: tokio::sync::Mutex::new(Some(jump_handle)),
            forward_registry,
            login_uid: tokio::sync::OnceCell::new(),
        })
    }

    /// Open an outgoing TCP forward channel through this SSH session (-L style source side).
    pub async fn open_direct_tcpip(
        &self,
        remote_host: &str,
        remote_port: u32,
        originator_addr: &str,
        originator_port: u32,
    ) -> Result<Channel<client::Msg>, GlideshError> {
        let guard = self.handle.lock().await;
        guard
            .channel_open_direct_tcpip(remote_host, remote_port, originator_addr, originator_port)
            .await
            .map_err(|e| GlideshError::SshChannel {
                message: format!(
                    "Failed to open direct-tcpip to {}:{} via {}: {}",
                    remote_host, remote_port, self.host, e
                ),
            })
    }

    /// Request the remote sshd to bind `bind_addr:bind_port` and forward incoming connections
    /// back to us as SSH channels. Returns a receiver that yields each incoming channel.
    pub async fn tcpip_forward(
        &self,
        bind_addr: &str,
        bind_port: u16,
    ) -> Result<mpsc::UnboundedReceiver<Channel<client::Msg>>, GlideshError> {
        let key = (bind_addr.to_string(), bind_port);
        let (tx, rx) = mpsc::unbounded_channel();
        {
            let mut reg = self
                .forward_registry
                .lock()
                .map_err(|_| GlideshError::Other("forward registry mutex poisoned".to_string()))?;
            if reg.contains_key(&key) {
                return Err(GlideshError::SshChannel {
                    message: format!(
                        "tcpip-forward for {}:{} is already registered",
                        bind_addr, bind_port
                    ),
                });
            }
            reg.insert(key.clone(), tx);
        }

        let mut guard = self.handle.lock().await;
        if let Err(e) = guard.tcpip_forward(bind_addr, bind_port as u32).await {
            drop(guard);
            if let Ok(mut reg) = self.forward_registry.lock() {
                reg.remove(&key);
            }
            return Err(GlideshError::SshChannel {
                message: format!(
                    "tcpip-forward request for {}:{} failed: {}",
                    bind_addr, bind_port, e
                ),
            });
        }
        Ok(rx)
    }

    pub async fn cancel_tcpip_forward(
        &self,
        bind_addr: &str,
        bind_port: u16,
    ) -> Result<(), GlideshError> {
        let guard = self.handle.lock().await;
        let _ = guard
            .cancel_tcpip_forward(bind_addr, bind_port as u32)
            .await;
        drop(guard);
        if let Ok(mut reg) = self.forward_registry.lock() {
            reg.remove(&(bind_addr.to_string(), bind_port));
        }
        Ok(())
    }

    pub async fn exec(&self, command: &str) -> Result<CommandOutput, GlideshError> {
        self.exec_with(command, ExecOptions::default()).await
    }

    pub async fn exec_with(
        &self,
        command: &str,
        opts: ExecOptions,
    ) -> Result<CommandOutput, GlideshError> {
        let guard = self.handle.lock().await;
        let mut channel =
            guard
                .channel_open_session()
                .await
                .map_err(|e| GlideshError::SshChannel {
                    message: format!("Failed to open channel on {}: {}", self.host, e),
                })?;
        drop(guard);

        // A PTY is needed for methods (e.g. `su`) that read their password from the
        // controlling terminal. Under a PTY the server merges stderr into stdout.
        if opts.pty {
            channel
                .request_pty(true, "xterm", 80, 24, 0, 0, &[])
                .await
                .map_err(|e| GlideshError::SshChannel {
                    message: format!("Failed to request PTY on {}: {}", self.host, e),
                })?;
        }

        channel
            .exec(true, command)
            .await
            .map_err(|e| GlideshError::SshChannel {
                message: format!("Failed to exec command on {}: {}", self.host, e),
            })?;

        if let Some(stdin) = opts.stdin.as_ref() {
            channel
                .data(&stdin[..])
                .await
                .map_err(|e| GlideshError::SshChannel {
                    message: format!("Failed to write stdin on {}: {}", self.host, e),
                })?;
            channel.eof().await.map_err(|e| GlideshError::SshChannel {
                message: format!("Failed to close stdin on {}: {}", self.host, e),
            })?;
        } else if !opts.pty {
            // No input to send. Signal EOF immediately (like `ssh -n`) so a
            // command that reads stdin gets EOF instead of blocking forever
            // waiting for input that will never arrive.
            let _ = channel.eof().await;
        }

        let mut stdout = CappedStream::new(OUTPUT_LIMIT);
        let mut stderr = CappedStream::new(OUTPUT_LIMIT);
        let mut status: Option<u32> = None;
        let mut signal: Option<String> = None;
        let mut exited = false;

        loop {
            // Once the command has reported its exit status, don't block forever
            // waiting for the channel to close. A backgrounded child (e.g. an
            // auto-started daemon) can inherit the command's stdout/stderr and
            // hold the channel open indefinitely — the command is done, but the
            // server never sends a close. After exit, drain any already-buffered
            // output briefly, then stop.
            let msg = if exited {
                match tokio::time::timeout(Duration::from_millis(200), channel.wait()).await {
                    Ok(Some(msg)) => msg,
                    Ok(None) | Err(_) => break,
                }
            } else {
                match channel.wait().await {
                    Some(msg) => msg,
                    None => break,
                }
            };

            match msg {
                russh::ChannelMsg::Data { ref data } => {
                    stdout.push(data);
                }
                russh::ChannelMsg::ExtendedData { ref data, ext: 1 } => {
                    stderr.push(data);
                }
                russh::ChannelMsg::ExitStatus { exit_status } => {
                    status = Some(exit_status);
                    exited = true;
                }
                russh::ChannelMsg::ExitSignal { signal_name, .. } => {
                    signal = Some(format!("{signal_name:?}"));
                    exited = true;
                }
                _ => {}
            }
        }

        let (exit_code, why) = exit_outcome(status, signal.as_deref());
        let (mut stderr, _) = stderr.finish();
        if let Some(why) = why {
            if !stderr.is_empty() && !stderr.ends_with('\n') {
                stderr.push('\n');
            }
            stderr.push_str(&why);
        }
        let (stdout, stdout_cut) = stdout.finish();
        Ok(CommandOutput {
            exit_code,
            stdout,
            stderr,
            stdout_cut,
        })
    }

    /// Execute `command`, escalating to another user when `run_as` is set. A denied
    /// escalation surfaces as [`GlideshError::RunAs`]; otherwise the wrapped command's
    /// output is returned verbatim for the caller to interpret.
    pub async fn exec_as(
        &self,
        command: &str,
        run_as: Option<&ResolvedRunAs>,
    ) -> Result<CommandOutput, GlideshError> {
        let Some(r) = run_as else {
            return self.exec(command).await;
        };
        escalation::precheck(r)?;
        let wrapped = escalation::wrap(r, command);
        let out = self
            .exec_with(
                &wrapped.command,
                ExecOptions {
                    stdin: wrapped.stdin,
                    pty: wrapped.pty,
                },
            )
            .await?;
        if let Some(err) = escalation::classify_failure(r, &out) {
            return Err(err);
        }
        Ok(out)
    }

    /// Create a fresh temporary file as the login user (writable for SFTP) and
    /// return its path. Used to stage privileged uploads/downloads.
    async fn mktemp_remote(&self) -> Result<String, GlideshError> {
        let out = self.exec("mktemp /tmp/glidesh.XXXXXX").await?;
        if out.exit_code != 0 {
            return Err(GlideshError::SshChannel {
                message: format!("mktemp failed on {}: {}", self.host, out.stderr.trim()),
            });
        }
        let path = out.stdout.trim().to_string();
        if path.is_empty() {
            return Err(GlideshError::SshChannel {
                message: format!("mktemp returned no path on {}", self.host),
            });
        }
        Ok(path)
    }

    async fn login_uid(&self) -> Result<&str, GlideshError> {
        self.login_uid
            .get_or_try_init(|| async {
                let out = self.exec("id -u").await?;
                let uid = out.stdout.trim();
                if out.exit_code != 0 || uid.is_empty() || !uid.bytes().all(|b| b.is_ascii_digit())
                {
                    return Err(GlideshError::SshChannel {
                        message: format!("id -u failed on {}: {}", self.host, out.failure()),
                    });
                }
                Ok(uid.to_string())
            })
            .await
            .map(String::as_str)
    }

    /// Fails unless no user other than root, the escalated user and the login user can
    /// redirect a privileged change of `path` ([`trusted_paths`]).
    async fn ensure_trusted_destination(
        &self,
        path: &str,
        r: &ResolvedRunAs,
    ) -> Result<(), GlideshError> {
        let uid = self.login_uid().await?;
        let out = self.exec_as(&guarded(&[path], uid, ""), Some(r)).await?;
        if out.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "file".to_string(),
                message: format!("unsafe destination {}: {}", path, out.failure()),
            });
        }
        Ok(())
    }

    /// `mkdir -p` each of `dirs`; escalated, only once [`trusted_paths`] holds for them,
    /// since `mkdir -p` follows symlinks on the way.
    pub async fn create_dirs_as(
        &self,
        dirs: &[&str],
        run_as: Option<&ResolvedRunAs>,
    ) -> Result<(), GlideshError> {
        if dirs.is_empty() {
            return Ok(());
        }
        let mkdir = format!(
            "mkdir -p {}",
            dirs.iter()
                .map(|d| shell_escape(d))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let command = match run_as {
            Some(_) => guarded(dirs, self.login_uid().await?, &mkdir),
            None => mkdir,
        };
        let out = self.exec_as(&command, run_as).await?;
        if out.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "file".to_string(),
                message: format!("failed to create {}: {}", dirs.join(", "), out.failure()),
            });
        }
        Ok(())
    }

    async fn sftp(&self) -> Result<SftpSession, GlideshError> {
        let guard = self.handle.lock().await;
        let channel = guard
            .channel_open_session()
            .await
            .map_err(|e| GlideshError::SshChannel {
                message: format!("Failed to open SFTP channel on {}: {}", self.host, e),
            })?;
        drop(guard);

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

    /// Upload to a destination the login user may not be able to write directly.
    /// Without escalation this is a plain SFTP write; with escalation the content is
    /// staged in `/tmp` over SFTP and written into place with the escalated shell.
    pub async fn upload_file_as(
        &self,
        content: &[u8],
        remote_path: &str,
        run_as: Option<&ResolvedRunAs>,
    ) -> Result<(), GlideshError> {
        let Some(r) = run_as else {
            return self.upload_file(content, remote_path).await;
        };
        let tmp = self.mktemp_remote().await?;
        // Any early return past this point must clean up the staged temp file,
        // so the SFTP write and the escalated write are wrapped and the temp file
        // is removed on every error path (not only on a failed escalated write).
        let result = self.stage_upload(content, &tmp, remote_path, r).await;
        if result.is_err() {
            let _ = self.exec(&format!("rm -f {}", shell_escape(&tmp))).await;
        }
        result
    }

    async fn stage_upload(
        &self,
        content: &[u8],
        tmp: &str,
        remote_path: &str,
        r: &ResolvedRunAs,
    ) -> Result<(), GlideshError> {
        self.upload_file(content, tmp).await?;
        let uid = self.login_uid().await?;
        let out = self
            .exec_as(&place_staged_upload(tmp, remote_path, uid), Some(r))
            .await?;
        if out.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "file".to_string(),
                message: format!(
                    "failed to write staged upload to {}: {}",
                    remote_path,
                    out.failure()
                ),
            });
        }
        Ok(())
    }

    /// Download a source the login user may not be able to read directly. Without
    /// escalation this is a plain SFTP read; with escalation the file is copied to a
    /// world-readable temp path with the escalated shell, then read over SFTP.
    pub async fn download_file_as(
        &self,
        remote_path: &str,
        run_as: Option<&ResolvedRunAs>,
    ) -> Result<Vec<u8>, GlideshError> {
        if run_as.is_none() {
            return self.download_file(remote_path).await;
        }
        let tmp = self.mktemp_remote().await?;
        // The staged temp file must be removed on every exit path, including the
        // error returns from `exec_as`/`download_file`, not just the happy path.
        let result = self.stage_download(remote_path, &tmp, run_as).await;
        let _ = self.exec(&format!("rm -f {}", shell_escape(&tmp))).await;
        result
    }

    async fn stage_download(
        &self,
        remote_path: &str,
        tmp: &str,
        run_as: Option<&ResolvedRunAs>,
    ) -> Result<Vec<u8>, GlideshError> {
        // tmp is owned by the login user; root can overwrite its content, and the
        // chmod keeps it readable for the SFTP read that follows.
        let stage = format!(
            "cp -f {} {} && chmod 0644 {}",
            shell_escape(remote_path),
            shell_escape(tmp),
            shell_escape(tmp)
        );
        let out = self.exec_as(&stage, run_as).await?;
        if out.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "file".to_string(),
                message: format!(
                    "failed to stage download of {}: {}",
                    remote_path,
                    out.failure()
                ),
            });
        }
        self.download_file(tmp).await
    }

    pub async fn checksum_remote(
        &self,
        remote_path: &str,
        run_as: Option<&ResolvedRunAs>,
    ) -> Result<Option<String>, GlideshError> {
        let escaped = shell_escape(remote_path);
        let output = self
            .exec_as(
                &format!("sha256sum {escaped} 2>&1 || shasum -a 256 {escaped} 2>&1"),
                run_as,
            )
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
        run_as: Option<&ResolvedRunAs>,
    ) -> Result<Option<(String, String, String)>, GlideshError> {
        let escaped = shell_escape(path);
        // BSD stat fallback for macOS targets, like the shasum fallback in
        // checksum_remote. Both forms print "owner group mode". `-L` reads a
        // symlink's target, which is what uploads, chown and chmod act on.
        let output = self
            .exec_as(
                &format!(
                    "stat -L -c '%U %G %a' {escaped} 2>/dev/null || stat -L -f '%Su %Sg %Lp' {escaped}"
                ),
                run_as,
            )
            .await?;

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
        run_as: Option<&ResolvedRunAs>,
    ) -> Result<(), GlideshError> {
        let escaped = shell_escape(path);
        let changes = owner.is_some() || group.is_some() || mode.is_some();
        if let Some(r) = run_as.filter(|_| changes) {
            self.ensure_trusted_destination(path, r).await?;
        }

        match (owner, group) {
            (Some(o), Some(g)) => {
                let output = self
                    .exec_as(
                        &format!("chown {}:{} {}", shell_escape(o), shell_escape(g), escaped),
                        run_as,
                    )
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
                    .exec_as(&format!("chown {} {}", shell_escape(o), escaped), run_as)
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
                    .exec_as(&format!("chgrp {} {}", shell_escape(g), escaped), run_as)
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

        // Last, because chown and chgrp clear setuid/setgid bits.
        if let Some(mode) = mode {
            let output = self
                .exec_as(&format!("chmod {} {}", shell_escape(mode), escaped), run_as)
                .await?;
            if output.exit_code != 0 {
                return Err(GlideshError::Module {
                    module: "file".to_string(),
                    message: format!("chmod failed: {}", output.stderr),
                });
            }
        }

        Ok(())
    }

    pub async fn set_file_attrs_recursive(
        &self,
        path: &str,
        owner: Option<&str>,
        group: Option<&str>,
        mode: Option<&str>,
        run_as: Option<&ResolvedRunAs>,
    ) -> Result<(), GlideshError> {
        let escaped = shell_escape(path);
        let changes = owner.is_some() || group.is_some() || mode.is_some();
        if let Some(r) = run_as.filter(|_| changes) {
            self.ensure_trusted_destination(path, r).await?;
        }

        match (owner, group) {
            (Some(o), Some(g)) => {
                let output = self
                    .exec_as(
                        &format!(
                            "chown -R {}:{} {}",
                            shell_escape(o),
                            shell_escape(g),
                            escaped
                        ),
                        run_as,
                    )
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
                    .exec_as(&format!("chown -R {} {}", shell_escape(o), escaped), run_as)
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
                    .exec_as(&format!("chgrp -R {} {}", shell_escape(g), escaped), run_as)
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

        // Last, because chown and chgrp clear setuid/setgid bits.
        if let Some(mode) = mode {
            let output = self
                .exec_as(
                    &format!("chmod -R {} {}", shell_escape(mode), escaped),
                    run_as,
                )
                .await?;
            if output.exit_code != 0 {
                return Err(GlideshError::Module {
                    module: "file".to_string(),
                    message: format!("chmod -R failed: {}", output.stderr),
                });
            }
        }

        Ok(())
    }

    /// Open an interactive PTY shell session.
    /// Takes over stdin/stdout until the remote shell exits.
    /// Returns the remote exit code.
    pub async fn interactive_shell(&self) -> Result<u32, GlideshError> {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));

        let guard = self.handle.lock().await;
        let mut channel =
            guard
                .channel_open_session()
                .await
                .map_err(|e| GlideshError::SshChannel {
                    message: format!("Failed to open session on {}: {}", self.host, e),
                })?;
        drop(guard);

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
                        Some(Event::Resize(cols, rows)) if (cols, rows) != last_size => {
                            last_size = (cols, rows);
                            let _ = channel
                                .window_change(cols as u32, rows as u32, 0, 0)
                                .await;
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
        let handle = self.handle.into_inner();
        handle
            .disconnect(russh::Disconnect::ByApplication, "session closed", "en")
            .await
            .map_err(|e| GlideshError::SshConnection {
                message: format!("Error closing connection to {}: {}", self.host, e),
            })?;

        if let Some(jump_handle) = self._jump_handle.into_inner() {
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

/// The escalated command that writes a staged upload from `tmp` into `dest` the way a
/// plain SFTP upload does: truncate and write, never replace. An existing file keeps its
/// inode — owner, group, mode, ACL and hard links — and a symlink is written through;
/// a new file is created by the escalated user, so the filesystem applies the umask, a
/// setgid directory's group and a default ACL. Moving the `mktemp` file (`0600`) into
/// place instead would carry its mode and ownership over, replace links, and skip
/// inheritance. Not atomic, as SFTP is not. Runs only after [`trusted_paths`] passes.
fn place_staged_upload(tmp: &str, dest: &str, login_uid: &str) -> String {
    let t = shell_escape(tmp);
    // `> dest` truncates before `cat` reads, so a staging file the escalated user cannot
    // read (a non-root run-as user other than the login user) must fail first.
    guarded(
        &[dest],
        login_uid,
        &format!(
            "test -r {t} || {{ echo 'the run-as user cannot read the staged upload' >&2; exit 1; }}\n\
             cat {t} > {} && rm -f {t}",
            shell_escape(dest)
        ),
    )
}

/// `then`, run by `sh` once [`trusted_paths`] holds for every one of `targets`. An
/// explicit `sh`, because `su` runs a command in the target's login shell, and zsh reads
/// `0777` as decimal.
fn guarded(targets: &[&str], login_uid: &str, then: &str) -> String {
    let script = format!("{}\n{then}", trusted_paths(targets, login_uid));
    format!("sh -c {}", shell_escape(&script))
}

/// A POSIX `sh` script, for an escalated shell, that exits non-zero with the reason on
/// stderr when a privileged write to, or `chown`/`chmod` of, one of `targets` could be
/// redirected by a user other than root, the escalated user, the login user (it already
/// writes the staged content) and the owners of the directories on the way. Anyone else
/// able to rename an entry there could swap in a symlink between this check and the
/// write, which would follow it: a shell cannot open a file refusing links.
///
/// For a target, and for every symlink met on its way to `/`, each directory above it and
/// each directory the link resolves to — the rule of the kernel's `protected_symlinks`,
/// applied whether that sysctl is on or not:
///
/// - a directory must not be writable by others, nor by a group other than root's, nor
///   carry an ACL while group-writable (the group bits then show the ACL mask, which may
///   grant writing to anyone);
/// - a sticky directory writable by others (`/tmp`) is accepted above an existing
///   directory or link, which others cannot rename there. Not above a file or a missing
///   entry, which anyone could create first — a link, or a hard link to a root file;
/// - a symlink must be owned by a trusted user or by the owner of its directory;
/// - an entry that does not exist yet is skipped: its directory decides who can create it.
///
/// Paths are read with `echo .` appended, since `$(…)` strips trailing newlines, which a
/// name may end in.
fn trusted_paths(targets: &[&str], login_uid: &str) -> String {
    // A raw template: the script's own braces and quotes need no escaping.
    const SCRIPT: &str = r#"u=$(id -u)
fail() { echo "$*" >&2; exit 1; }
trusted() { [ "$1" = 0 ] || [ "$1" = "$u" ] || [ "$1" = LOGIN ]; }
owner() { stat -c %u "$1" 2>/dev/null || stat -f %u "$1" 2>/dev/null || fail "cannot inspect $1"; }
up() { up=$(dirname "$1" && echo .) || fail "cannot resolve $1"; up=${up%??}; }
has_acl() { case $(ls -ld "$1" 2>/dev/null) in ??????????+*) return 0 ;; esac; return 1; }
entry() {
  if [ -L "$1" ]; then
    kind=link
    o=$(owner "$1") || exit 1
    trusted "$o" && return 0
    up "$1"; od=$(owner "$up") || exit 1
    [ "$o" = "$od" ] ||
      fail "refusing: the symlink $1 is owned by uid $o, neither a trusted user nor the owner of its directory"
    return 0
  fi
  if [ ! -d "$1" ]; then
    kind=missing; [ -e "$1" ] && kind=file
    return 0
  fi
  kind=dir
  a=$(stat -c '%g %a' "$1" 2>/dev/null || stat -f '%g %Mp%Lp' "$1" 2>/dev/null) || fail "cannot inspect $1"
  g=${a%% *}; m=$((0${a#* }))
  if [ $((m & 02)) -ne 0 ] || { [ $((m & 020)) -ne 0 ] && [ "$g" != 0 ]; }; then
    { [ $((m & 01000)) -ne 0 ] && [ "$2" = ancestor ]; } || fail "refusing: other users can write to $1"; fi
  if [ $((m & 020)) -ne 0 ] && has_acl "$1"; then
    fail "refusing: $1 has an ACL, which may let other users write to it"; fi
}
check() {
  p=$1; d=$2; role=$3
  [ "$d" -le 40 ] || fail "refusing: too many symlinks under $1"
  while :; do
    entry "$p" "$role"
    if [ "$kind" = link ]; then
      l=$(readlink "$p" && echo .) || fail "cannot read the symlink $p"; l=${l%??}
      case $l in /*) ;; *) up "$p"; l=$up/$l ;; esac
      ( check "$l" $((d + 1)) "$role" ) || exit 1
    fi
    up "$p"
    [ "$up" = "$p" ] && return 0
    p=$up
    case $kind in dir|link) role=ancestor ;; *) role=parent ;; esac
  done
}
for t in TARGETS; do
  case $t in /*) ;; *) t=$(pwd -P)/$t ;; esac
  ( check "$t" 0 target ) || exit 1
done"#;
    // A trailing slash would make `test -L` look through a symlink.
    let targets: Vec<String> = targets
        .iter()
        .map(|t| match t.trim_end_matches('/') {
            "" if t.starts_with('/') => shell_escape("/"),
            trimmed => shell_escape(trimmed),
        })
        .collect();
    // LOGIN first: a target path may itself contain the text "LOGIN".
    SCRIPT
        .replace("LOGIN", &shell_escape(login_uid))
        .replace("TARGETS", &targets.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn place(tmp: &std::path::Path, dest: &std::path::Path) -> std::process::Output {
        let cmd = place_staged_upload(tmp.to_str().unwrap(), dest.to_str().unwrap(), "0");
        std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .output()
            .unwrap()
    }

    #[cfg(unix)]
    fn staged(dir: &std::path::Path, content: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let tmp = dir.join("glidesh.staged");
        std::fs::write(&tmp, content).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).unwrap();
        tmp
    }

    #[cfg(unix)]
    fn mode_of(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[cfg(unix)]
    #[test]
    fn a_replaced_file_keeps_its_mode_not_the_staging_one() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("run.sh");
        std::fs::write(&dest, "old").unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tmp = staged(dir.path(), "new");

        let out = place(&tmp, &dest);
        assert!(out.status.success(), "{:?}", out);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
        assert_eq!(mode_of(&dest), 0o755);
        assert!(!tmp.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_new_file_gets_the_umask_mode_not_the_staging_one() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("new.conf");
        let tmp = staged(dir.path(), "content");

        let cmd = place_staged_upload(tmp.to_str().unwrap(), dest.to_str().unwrap(), "0");
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("umask 027 && {cmd}"))
            .output()
            .unwrap();
        assert!(out.status.success(), "{:?}", out);
        assert_eq!(mode_of(&dest), 0o640);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_destination_is_written_through_not_replaced() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.conf");
        std::fs::write(&target, "old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        let link = dir.path().join("link.conf");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let tmp = staged(dir.path(), "new");

        let out = place(&tmp, &link);
        assert!(out.status.success(), "{:?}", out);
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert_eq!(mode_of(&target), 0o640);
        assert!(!tmp.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_destination_with_quotes_and_spaces_is_placed() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("it's a $file");
        std::fs::write(&dest, "old").unwrap();
        let before = mode_of(&dest);
        let tmp = staged(dir.path(), "new");

        let out = place(&tmp, &dest);
        assert!(out.status.success(), "{:?}", out);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
        assert_eq!(mode_of(&dest), before);
    }

    #[test]
    fn a_failure_under_a_pty_is_read_from_stdout() {
        let out = |stdout: &str, stderr: &str| CommandOutput {
            exit_code: 1,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            stdout_cut: false,
        };
        assert_eq!(out("", " denied\n").failure(), "denied");
        assert_eq!(out("merged by su\n", "").failure(), "merged by su");
        assert_eq!(out("noise", "denied").failure(), "denied");
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_staging_file_leaves_the_destination_untouched() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::process::Command::new("id").arg("-u").output().unwrap();
        if String::from_utf8_lossy(&root.stdout).trim() == "0" {
            return; // root reads any file, so the staging file cannot be made unreadable
        }
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("app.conf");
        std::fs::write(&dest, "keep me").unwrap();
        let tmp = staged(dir.path(), "new");
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o000)).unwrap();

        let out = place(&tmp, &dest);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("cannot read the staged upload"));
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "keep me");
    }

    #[cfg(unix)]
    fn trusted(dest: &std::path::Path) -> std::process::Output {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(guarded(&[dest.to_str().unwrap()], "0", ""))
            .output()
            .unwrap()
    }

    #[cfg(unix)]
    fn refused(out: &std::process::Output, why: &str) {
        assert!(!out.status.success(), "{:?}", out);
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(why),
            "{:?}",
            out
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_world_writable_directory_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();

        refused(&trusted(&shared.join("app.conf")), "other users can write");
        // Deeper down too, and for a directory about to be created.
        refused(
            &trusted(&shared.join("new/app.conf")),
            "other users can write",
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_sticky_directory_is_accepted_above_the_destination_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let sticky = dir.path().join("sticky");
        std::fs::create_dir_all(sticky.join("app")).unwrap();
        std::fs::set_permissions(&sticky, std::fs::Permissions::from_mode(0o1777)).unwrap();

        // Others could create the file first in the destination's own directory.
        refused(&trusted(&sticky.join("app.conf")), "other users can write");
        let out = trusted(&sticky.join("app/app.conf"));
        assert!(out.status.success(), "{:?}", out);
        // An existing directory there, as `mkdir -p` and recursive uploads check it,
        // cannot be renamed by others.
        let out = trusted(&sticky.join("app"));
        assert!(out.status.success(), "{:?}", out);
        // A missing one could be created first, as a link, before `mkdir -p` runs.
        refused(&trusted(&sticky.join("new/sub")), "other users can write");
    }

    #[cfg(unix)]
    #[test]
    fn a_trailing_slash_does_not_hide_a_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(shared.join("app")).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        let link = dir.path().join("app");
        std::os::unix::fs::symlink(shared.join("app"), &link).unwrap();

        let target = format!("{}/", link.display());
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(guarded(&[target.as_str()], "0", ""))
            .output()
            .unwrap();
        refused(&out, "other users can write");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_followed_into_the_directory_it_points_to() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        let safe = dir.path().join("safe");
        std::fs::create_dir(&safe).unwrap();

        // The link itself sits in a trusted directory, but what it resolves to does not.
        let link = safe.join("app.conf");
        std::os::unix::fs::symlink(shared.join("app.conf"), &link).unwrap();
        refused(&trusted(&link), "other users can write");

        // And through a symlinked directory on the way.
        let via = safe.join("via");
        std::os::unix::fs::symlink(&shared, &via).unwrap();
        refused(&trusted(&via.join("app.conf")), "other users can write");
    }

    #[cfg(unix)]
    #[test]
    fn a_path_with_a_double_slash_ends() {
        let dir = tempfile::tempdir().unwrap();
        let dest = format!("/{}/app.conf", dir.path().display());
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(guarded(&[dest.as_str()], "0", ""))
            .output()
            .unwrap();
        assert!(out.status.success(), "{:?}", out);
    }

    #[cfg(unix)]
    #[test]
    fn a_world_writable_directory_fails_the_upload_before_writing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        let dest = shared.join("app.conf");
        std::fs::write(&dest, "keep me").unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        let tmp = staged(dir.path(), "new");

        let out = place(&tmp, &dest);
        assert!(!out.status.success());
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "keep me");
    }

    #[test]
    fn a_login_uid_placeholder_in_the_path_is_not_replaced() {
        let script = trusted_paths(&["/srv/LOGIN/TARGETS.conf"], "1000");
        assert!(script.contains("'/srv/LOGIN/TARGETS.conf'"), "{script}");
        assert!(script.contains("= '1000' ]"), "{script}");
    }

    #[cfg(unix)]
    #[test]
    fn a_hard_linked_destination_is_rewritten_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("app.conf");
        std::fs::write(&dest, "old").unwrap();
        let peer = dir.path().join("peer.conf");
        std::fs::hard_link(&dest, &peer).unwrap();
        let tmp = staged(dir.path(), "new");

        let out = place(&tmp, &dest);
        assert!(out.status.success(), "{:?}", out);
        assert_eq!(std::fs::read_to_string(&peer).unwrap(), "new");
    }

    #[test]
    fn an_exit_status_is_the_commands_own() {
        assert_eq!(exit_outcome(Some(0), None), (0, None));
        assert_eq!(exit_outcome(Some(3), None), (3, None));
    }

    #[test]
    fn a_command_killed_by_a_signal_did_not_succeed() {
        let (code, why) = exit_outcome(None, Some("KILL"));
        assert_eq!(code, NO_EXIT_STATUS);
        assert_eq!(why.as_deref(), Some("command killed by signal KILL"));
    }

    #[test]
    fn a_channel_closed_without_a_status_did_not_succeed() {
        let (code, why) = exit_outcome(None, None);
        assert_eq!(code, NO_EXIT_STATUS);
        assert!(
            why.unwrap()
                .contains("before the command reported an exit status")
        );
    }

    fn capped(limit: usize, chunks: &[&[u8]]) -> (String, bool) {
        let mut stream = CappedStream::new(limit);
        for chunk in chunks {
            stream.push(chunk);
        }
        stream.finish()
    }

    #[test]
    fn output_within_the_limit_is_kept_whole() {
        assert_eq!(
            capped(8, &[b"abc", b"defgh"]),
            ("abcdefgh".to_string(), false)
        );
    }

    #[test]
    fn output_over_the_limit_keeps_its_start_and_its_end() {
        let (text, cut) = capped(8, &[b"abc", b"defghij", b"klmn"]);
        assert_eq!(
            text,
            "abcd\n[glidesh: 6 bytes of output dropped here]\nklmn"
        );
        assert!(cut);
    }

    /// Only a cut is a cut: output that prints the marker line itself is kept as it is.
    #[test]
    fn output_that_prints_the_marker_is_not_cut() {
        let marker = b"a\n[glidesh: 42 bytes of output dropped here]\nb";
        let (text, cut) = capped(1024, &[marker]);
        assert_eq!(text.as_bytes(), marker);
        assert!(!cut);
    }

    #[test]
    fn a_chunk_larger_than_the_limit_keeps_only_its_end() {
        let (text, _) = capped(4, &[b"a", b"bcdefghijklm", b"no"]);
        assert_eq!(text, "ab\n[glidesh: 11 bytes of output dropped here]\nno");
    }

    #[test]
    fn many_small_chunks_count_every_dropped_byte() {
        let chunks: Vec<&[u8]> = std::iter::repeat_n(&b"x"[..], 1000).collect();
        let (text, _) = capped(10, &chunks);
        assert!(text.contains("[glidesh: 990 bytes of output dropped here]"));
    }

    /// The limit bounds memory, not just what is returned: neither buffer may allocate past
    /// its half, whatever the chunk sizes.
    #[test]
    fn the_buffers_never_allocate_past_the_limit() {
        let limit = 1000;
        let mut stream = CappedStream::new(limit);
        for size in (1..400).cycle().step_by(37).take(500) {
            stream.push(&vec![b'x'; size]);
            assert!(stream.head.capacity() <= limit / 2, "head");
            assert!(stream.tail.capacity() <= limit - limit / 2, "tail");
            assert!(stream.tail.len() <= limit - limit / 2);
        }
    }
}
