use crate::config::types::{ResolvedJumpHost, ResolvedRunAs, RunAsMethod};
use crate::error::GlideshError;
use crate::modules::escalation;
use crate::modules::file_tree::{PathKind, PathStat, RemoteKind, Strays, join as tree_join};
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

/// Optional controls for [`SshSession::exec_with`]: feed bytes on stdin (e.g. `su`'s
/// password), allocate a PTY (required by `su`), or hold stdin for a [`Handshake`] —
/// which then writes stdin alone, and `stdin` is ignored.
#[derive(Default)]
pub struct ExecOptions {
    pub stdin: Option<Vec<u8>>,
    pub pty: bool,
    pub handshake: Option<Handshake>,
}

/// Stdin sent only when the remote side asks for it on stderr: a `sudo -S` password when
/// sudo prompts for it, the input once the command prints `ready`. A password sudo does
/// not ask for — `NOPASSWD`, a policy changed mid-run — would reach the command's stdin,
/// which a sudoers `log_input` I/O log records; input sent while sudo still authenticates
/// would be read as more password attempts.
pub struct Handshake {
    /// sudo's `-p` prompt, and the password line that answers the first prompt.
    pub prompt: Option<(String, Vec<u8>)>,
    /// The line the command prints on stderr first: sudo is done with the password, and
    /// what follows is the command's own output, never a prompt.
    pub started: String,
    /// The line the command prints on stderr before it reads stdin; may be `started`.
    pub ready: String,
    /// Sent once `ready` appears, then stdin is closed.
    pub input: Vec<u8>,
}

/// What a [`Handshake`] writes on stdin next.
#[derive(Debug, PartialEq, Eq)]
enum Reply {
    Bytes(Vec<u8>),
    Close,
}

/// How long a line sudo left unfinished stays quiet before it counts as a prompt: PAM
/// shows its own prompt instead of `-p`'s when it is not the standard one (Kerberos,
/// one-time codes).
const PROMPT_QUIET: Duration = Duration::from_secs(2);

/// A [`Handshake`] in progress. The first prompt gets the password; a second means it
/// was wrong, and stdin is closed with nothing more sent.
struct Handshaking {
    handshake: Handshake,
    pending: Vec<u8>,
    marked_prompts: usize,
    prompts: usize,
    answered: usize,
    started: bool,
    done: bool,
}

impl Handshaking {
    fn new(handshake: Handshake) -> Self {
        Handshaking {
            handshake,
            pending: Vec::new(),
            marked_prompts: 0,
            prompts: 0,
            answered: 0,
            started: false,
            done: false,
        }
    }

    /// Whether a quiet unfinished line could still be a prompt.
    fn awaits_prompt(&self) -> bool {
        !self.done && !self.started && self.handshake.prompt.is_some()
    }

    /// Takes a stderr chunk; returns what to write and the stderr to keep, without the
    /// markers. Stderr is held back until the exchange ends, as a marker may be split
    /// across chunks.
    fn stderr(&mut self, chunk: &[u8]) -> (Vec<Reply>, Vec<u8>) {
        if self.done {
            return (Vec::new(), chunk.to_vec());
        }
        self.pending.extend_from_slice(chunk);
        let mut replies = Vec::new();
        if !self.started {
            let started = line(&self.handshake.started);
            let at = find(&self.pending, &started);
            let before = at.unwrap_or(self.pending.len());
            if let Some((prompt, _)) = &self.handshake.prompt {
                let seen = occurrences(&self.pending[..before], prompt.as_bytes());
                while self.marked_prompts < seen && !self.done {
                    self.marked_prompts += 1;
                    self.answered = self.pending.len();
                    replies.extend(self.prompted());
                }
            }
            self.started = at.is_some();
        }
        if self.started && !self.done && find(&self.pending, &line(&self.handshake.ready)).is_some()
        {
            let input = std::mem::take(&mut self.handshake.input);
            if !input.is_empty() {
                replies.push(Reply::Bytes(input));
            }
            replies.push(Reply::Close);
            self.done = true;
        }
        let kept = if self.done { self.finish() } else { Vec::new() };
        (replies, kept)
    }

    /// Stderr went quiet for [`PROMPT_QUIET`]: a line sudo left unfinished since the last
    /// prompt is one — unless it is the start of `-p`'s, still arriving.
    fn quiet(&mut self) -> (Vec<Reply>, Vec<u8>) {
        if !self.awaits_prompt()
            || self.pending.len() <= self.answered
            || self.pending.ends_with(b"\n")
        {
            return (Vec::new(), Vec::new());
        }
        let tail_from = self
            .pending
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |at| at + 1);
        let tail = &self.pending[tail_from..];
        if self
            .handshake
            .prompt
            .as_ref()
            .is_some_and(|(prompt, _)| prompt.as_bytes().starts_with(tail))
        {
            return (Vec::new(), Vec::new());
        }
        self.answered = self.pending.len();
        let replies = self.prompted();
        let kept = if self.done { self.finish() } else { Vec::new() };
        (replies, kept)
    }

    fn prompted(&mut self) -> Vec<Reply> {
        self.prompts += 1;
        match &self.handshake.prompt {
            Some((_, password)) if self.prompts == 1 => vec![Reply::Bytes(password.clone())],
            _ => {
                self.done = true;
                vec![Reply::Close]
            }
        }
    }

    /// The stderr still held back, without the markers.
    fn finish(&mut self) -> Vec<u8> {
        let mut kept = std::mem::take(&mut self.pending);
        if let Some((prompt, _)) = &self.handshake.prompt {
            kept = without(&kept, prompt.as_bytes());
        }
        kept = without(&kept, &line(&self.handshake.started));
        without(&kept, &line(&self.handshake.ready))
    }
}

fn line(marker: &str) -> Vec<u8> {
    format!("{marker}\n").into_bytes()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    let (mut count, mut at) = (0, 0);
    while at + needle.len() <= haystack.len() {
        if &haystack[at..at + needle.len()] == needle {
            count += 1;
            at += needle.len();
        } else {
            at += 1;
        }
    }
    count
}

fn without(haystack: &[u8], needle: &[u8]) -> Vec<u8> {
    let mut kept = Vec::with_capacity(haystack.len());
    let mut at = 0;
    while at < haystack.len() {
        if !needle.is_empty() && haystack[at..].starts_with(needle) {
            at += needle.len();
        } else {
            kept.push(haystack[at]);
            at += 1;
        }
    }
    kept
}

/// A line no command prints by chance.
fn marker(kind: &str) -> String {
    format!("glidesh-{kind}-{}", uuid::Uuid::new_v4().simple())
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

        let mut handshaking = opts.handshake.map(Handshaking::new);
        // A task writes the handshake's replies while this loop keeps reading: a large
        // input to a command that exits early would otherwise wait forever for a window
        // the closed channel never grants.
        let (replies, writer) = match &handshaking {
            Some(_) => {
                let (replies, mut queued) = tokio::sync::mpsc::unbounded_channel::<Reply>();
                let mut stdin = channel.make_writer();
                let writer = tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    while let Some(reply) = queued.recv().await {
                        let written = match reply {
                            Reply::Bytes(bytes) => stdin.write_all(&bytes).await,
                            Reply::Close => {
                                let _ = stdin.shutdown().await;
                                break;
                            }
                        };
                        if written.is_err() {
                            break;
                        }
                    }
                });
                (Some(replies), Some(writer))
            }
            None => (None, None),
        };
        let send = |sent: Vec<Reply>| {
            if let Some(replies) = &replies {
                for reply in sent {
                    let _ = replies.send(reply);
                }
            }
        };
        if handshaking.is_none() {
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
            } else if let Some(h) = handshaking.as_mut().filter(|h| h.awaits_prompt()) {
                match tokio::time::timeout(PROMPT_QUIET, channel.wait()).await {
                    Ok(Some(msg)) => msg,
                    Ok(None) => break,
                    Err(_) => {
                        let (sent, kept) = h.quiet();
                        stderr.push(&kept);
                        send(sent);
                        continue;
                    }
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
                russh::ChannelMsg::ExtendedData { ref data, ext: 1 } => match &mut handshaking {
                    Some(h) => {
                        let (sent, kept) = h.stderr(data);
                        stderr.push(&kept);
                        send(sent);
                    }
                    None => stderr.push(data),
                },
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
        if let Some(h) = &mut handshaking {
            stderr.push(&h.finish());
        }
        if let Some(writer) = writer {
            writer.abort();
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
        self.exec_as_within(command, r, str::to_string, None).await
    }

    /// [`Self::exec_as`], the escalated command placed in a login-user script by `outer`,
    /// to keep its output in a file only the login user can read. `input` is the stdin
    /// for a command that prints its `ready` line on stderr before reading it. With a
    /// [`Handshake`], the command is prefixed with printing its `started` line.
    async fn exec_as_within(
        &self,
        command: &str,
        r: &ResolvedRunAs,
        outer: impl FnOnce(&str) -> String,
        input: Option<(String, Vec<u8>)>,
    ) -> Result<CommandOutput, GlideshError> {
        escalation::precheck(r)?;
        let asks_password = r.method == RunAsMethod::Sudo && r.password.is_some();
        let started = marker("started");
        // `started` says sudo is done: stdin can close without a password, or carry the
        // input once `ready` follows.
        let (ready, input) = match input {
            Some((ready, input)) => (Some(ready), input),
            None if asks_password => (Some(started.clone()), Vec::new()),
            None => (None, Vec::new()),
        };
        let command = match ready {
            Some(_) => format!("printf '%s\\n' {} >&2; {command}", shell_escape(&started)),
            None => command.to_string(),
        };
        let wrapped = escalation::wrap(r, &command);
        let handshake = ready.map(|ready| Handshake {
            prompt: wrapped.prompt.clone(),
            started,
            ready,
            input,
        });
        let out = self
            .exec_with(
                &outer(&wrapped.command),
                ExecOptions {
                    stdin: wrapped.stdin,
                    pty: wrapped.pty,
                    handshake,
                },
            )
            .await?;
        if let Some(err) = escalation::classify_failure(r, &out) {
            return Err(err);
        }
        Ok(out)
    }

    /// Create a fresh temporary file as the login user (writable for SFTP) and
    /// return its path. Used to stage privileged uploads.
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

    /// Whether `path` is the host's `/` once resolved there — literally, as `/tmp/..` or
    /// `/.`, or through a symlink. A path that does not exist is not.
    pub async fn is_root_dir_as(
        &self,
        path: &str,
        run_as: Option<&ResolvedRunAs>,
    ) -> Result<bool, GlideshError> {
        let out = self.exec_as(&root_dir_check(path), run_as).await?;
        root_dir_answer(&out.stdout).ok_or_else(|| GlideshError::Module {
            module: "file".to_string(),
            message: format!("could not tell whether {path} is /: {}", out.failure()),
        })
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
        let command = match run_as {
            Some(_) => guarded_mkdir(dirs, self.login_uid().await?),
            None => format!(
                "mkdir -p {}",
                dirs.iter()
                    .map(|d| shell_escape(d))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
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
    /// Without escalation this is a plain SFTP write. With escalation to root, or through
    /// `su`, the content is staged in the login user's private file in `/tmp` over SFTP
    /// and read escalated; any other run-as user cannot open that file, so the content is
    /// sent on the escalated command's stdin instead.
    pub async fn upload_file_as(
        &self,
        content: &[u8],
        remote_path: &str,
        run_as: Option<&ResolvedRunAs>,
    ) -> Result<(), GlideshError> {
        let Some(r) = run_as else {
            return self.upload_file(content, remote_path).await;
        };
        if !stages_escalated(r) {
            return self.stream_upload(content, remote_path, r).await;
        }
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

    /// Sends `content` on the stdin of an escalated [`write_streamed_upload`], once the
    /// destination passed its check and sudo is done with its password.
    async fn stream_upload(
        &self,
        content: &[u8],
        remote_path: &str,
        r: &ResolvedRunAs,
    ) -> Result<(), GlideshError> {
        let uid = self.login_uid().await?;
        let ready = marker("ready");
        let out = self
            .exec_as_within(
                &write_streamed_upload(remote_path, uid, &ready),
                r,
                str::to_string,
                Some((ready, content.to_vec())),
            )
            .await?;
        if out.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "file".to_string(),
                message: format!(
                    "failed to write upload to {}: {}",
                    remote_path,
                    out.failure()
                ),
            });
        }
        Ok(())
    }

    /// Download a source the login user may not be able to read directly. Without
    /// escalation this is a plain SFTP read; with escalation the escalated shell reads
    /// the file into the login user's private staging file, then it is read over SFTP.
    pub async fn download_file_as(
        &self,
        remote_path: &str,
        run_as: Option<&ResolvedRunAs>,
    ) -> Result<Vec<u8>, GlideshError> {
        self.download_staged(remote_path, run_as, false).await
    }

    /// [`Self::download_file_as`], refusing with escalation a path another user could
    /// redirect ([`trusted_paths`]) — for reads whose content must not leave the host
    /// unless it is the file that was checked, as `--diff` checks it is world-readable.
    pub async fn download_trusted_as(
        &self,
        remote_path: &str,
        run_as: Option<&ResolvedRunAs>,
    ) -> Result<Vec<u8>, GlideshError> {
        self.download_staged(remote_path, run_as, true).await
    }

    async fn download_staged(
        &self,
        remote_path: &str,
        run_as: Option<&ResolvedRunAs>,
        trusted: bool,
    ) -> Result<Vec<u8>, GlideshError> {
        let Some(r) = run_as else {
            return self.download_file(remote_path).await;
        };
        let dir = self.mktemp_dir_remote().await?;
        let tmp = format!("{dir}/content");
        // The staging directory must be removed on every exit path, including the
        // error returns from `exec_as`/`download_file`, not just the happy path.
        let result = self.stage_download(remote_path, &tmp, r, trusted).await;
        let _ = self.exec(&format!("rm -rf {}", shell_escape(&dir))).await;
        result
    }

    /// A private directory holding an empty `0600` `content` file, both the login user's,
    /// to stage a download in. Not a file straight in `/tmp`: there `fs.protected_regular`
    /// (enabled by default under systemd) refuses even root a `>` into another user's file.
    async fn mktemp_dir_remote(&self) -> Result<String, GlideshError> {
        // An explicit `sh`: the login shell may not be POSIX. A failure after `mktemp -d`
        // removes the directory here, since the caller never learns its path.
        const SCRIPT: &str = r#"d=$(mktemp -d /tmp/glidesh.XXXXXX) || exit 1
{ : > "$d/content" && chmod 0600 "$d/content"; } || { rm -rf "$d"; exit 1; }
printf '%s' "$d""#;
        let out = self
            .exec(&format!("sh -c {}", shell_escape(SCRIPT)))
            .await?;
        let dir = out.stdout.trim();
        if out.exit_code != 0 || dir.is_empty() {
            return Err(GlideshError::SshChannel {
                message: format!("mktemp -d failed on {}: {}", self.host, out.failure()),
            });
        }
        Ok(dir.to_string())
    }

    async fn stage_download(
        &self,
        remote_path: &str,
        tmp: &str,
        r: &ResolvedRunAs,
        trusted: bool,
    ) -> Result<Vec<u8>, GlideshError> {
        // The login user's `0600` staging file keeps its owner and mode: readable over SFTP
        // by the login user, by nobody else.
        let src = shell_escape(remote_path);
        let t = shell_escape(tmp);
        let escalated_writes = stages_escalated(r);
        let mark = format!("glidesh-content-{}", uuid::Uuid::new_v4().simple());
        let read = if escalated_writes {
            format!(
                "test -w {t} || {{ echo {} >&2; exit 1; }}\ncat {src} > {t}",
                shell_escape(SU_STAGING_LIMIT)
            )
        } else {
            format!("printf '%s\\n' {}\ncat {src}", shell_escape(&mark))
        };
        let command = if trusted {
            guarded(&[remote_path], self.login_uid().await?, &read)
        } else {
            read
        };
        let out = if escalated_writes {
            self.exec_as(&command, Some(r)).await?
        } else {
            self.exec_as_within(
                &command,
                r,
                |escalated| format!("sh -c {}", shell_escape(&format!("{escalated} > {t}"))),
                None,
            )
            .await?
        };
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
        let staged = self.download_file(tmp).await?;
        if escalated_writes {
            return Ok(staged);
        }
        after_mark(&staged, &mark).ok_or_else(|| GlideshError::Module {
            module: "file".to_string(),
            message: format!("the staged download of {remote_path} lost its start"),
        })
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

    /// Every entry under `dest`, relative to it, for `prune`; none when `dest` does not
    /// exist. `prune` deletes, so it refuses a `dest` that is `/` or a symlink, and a listing
    /// it could not finish or read: a name that is not UTF-8 could not be matched.
    pub async fn list_tree_as(
        &self,
        dest: &str,
        run_as: Option<&ResolvedRunAs>,
    ) -> Result<Vec<(RemoteKind, String)>, GlideshError> {
        let refuse = |why: String| GlideshError::Module {
            module: "file".to_string(),
            message: format!("prune: {why}"),
        };
        if self.is_root_dir_as(dest, run_as).await? {
            return Err(refuse(format!(
                "refusing to prune / (destination {dest:?})"
            )));
        }
        let out = self.exec_as(&tree_listing(dest), run_as).await?;
        if out.exit_code != 0 {
            return Err(refuse(format!("cannot list {dest}: {}", out.failure())));
        }
        if out.stdout_cut {
            return Err(refuse(format!("{dest} holds too many entries to list")));
        }
        parse_tree_listing(dest, &out.stdout).map_err(refuse)
    }

    /// Remove `strays` from under `dest`: files and links, then directories deepest first,
    /// each empty by then. Escalated, each batch only once [`trusted_paths`] holds for the
    /// directories it removes from: someone able to write there could swap in a symlink.
    pub async fn remove_strays_as(
        &self,
        dest: &str,
        strays: &Strays,
        run_as: Option<&ResolvedRunAs>,
    ) -> Result<(), GlideshError> {
        let files: Vec<String> = strays.files.iter().map(|f| tree_join(dest, f)).collect();
        let dirs: Vec<String> = strays.dirs.iter().map(|d| tree_join(dest, d)).collect();
        self.each_chunk_as("rm -f --", &files, run_as, true).await?;
        self.each_chunk_as("rmdir --", &dirs, run_as, true).await
    }

    /// What each of `paths` is on the host — kind, owner, group, mode; a link's own — or
    /// `None` for one that does not exist.
    pub async fn stat_many_as(
        &self,
        paths: &[String],
        run_as: Option<&ResolvedRunAs>,
    ) -> Result<Vec<Option<PathStat>>, GlideshError> {
        let mut found = Vec::with_capacity(paths.len());
        for chunk in path_chunks(paths) {
            let out = self.exec_as(&stat_listing(chunk), run_as).await?;
            let stats = (out.exit_code == 0)
                .then(|| parse_stats(&out.stdout, chunk.len()))
                .flatten()
                .ok_or_else(|| GlideshError::Module {
                    module: "file".to_string(),
                    message: format!("stat of {} paths failed: {}", chunk.len(), out.failure()),
                })?;
            found.extend(stats);
        }
        Ok(found)
    }

    /// Set the owner and group of `files` and `dirs`, then the mode of each kind: exactly
    /// the paths a recursive upload manages, under `root`. A symlink among them is never
    /// followed — `chown -h` changes the link, `chmod` skips it — so, as `chown -R` did, the
    /// escalated change needs [`trusted_paths`] to hold for `root` only.
    #[allow(clippy::too_many_arguments)]
    pub async fn set_tree_attrs_as(
        &self,
        root: &str,
        files: &[String],
        dirs: &[String],
        owner: Option<&str>,
        group: Option<&str>,
        file_mode: Option<&str>,
        dir_mode: Option<&str>,
        run_as: Option<&ResolvedRunAs>,
    ) -> Result<(), GlideshError> {
        let changes =
            owner.is_some() || group.is_some() || file_mode.is_some() || dir_mode.is_some();
        if !changes {
            return Ok(());
        }
        if self.is_root_dir_as(root, run_as).await? {
            return Err(root_refusal(root));
        }
        if let Some(r) = run_as {
            self.ensure_trusted_destination(root, r).await?;
        }
        let all: Vec<String> = dirs.iter().chain(files).cloned().collect();
        match (owner, group) {
            (Some(o), Some(g)) => {
                let command = format!("chown -h {}:{} --", shell_escape(o), shell_escape(g));
                self.each_chunk_as(&command, &all, run_as, false).await?;
            }
            (Some(o), None) => {
                let command = format!("chown -h {} --", shell_escape(o));
                self.each_chunk_as(&command, &all, run_as, false).await?;
            }
            (None, Some(g)) => {
                let command = format!("chgrp -h {} --", shell_escape(g));
                self.each_chunk_as(&command, &all, run_as, false).await?;
            }
            (None, None) => {}
        }
        // Last, because chown and chgrp clear setuid/setgid bits. `find -type` leaves out a
        // path that is a symlink, which `chmod` would follow. A symlinked directory on the way
        // to a path is followed, as uploads follow it: escalated, `create_dirs_as` has
        // already held every directory of the tree to [`trusted_paths`].
        for (mode, paths, kind) in [(dir_mode, dirs, "d"), (file_mode, files, "f")] {
            if let Some(mode) = mode {
                // No `-P`: busybox `find` does not take it, and not following is the default.
                let tail = format!(
                    "-prune -type {kind} -exec chmod {} {{}} +",
                    shell_escape(mode)
                );
                self.each_chunk_around_as("find", &tail, paths, run_as)
                    .await?;
            }
        }
        Ok(())
    }

    /// Run `before`, then `paths`, then `after`, in chunks short enough for one argument
    /// list — for `find`, whose expression follows the paths.
    async fn each_chunk_around_as(
        &self,
        before: &str,
        after: &str,
        paths: &[String],
        run_as: Option<&ResolvedRunAs>,
    ) -> Result<(), GlideshError> {
        for chunk in path_chunks(paths) {
            let list: Vec<String> = chunk.iter().map(|p| shell_escape(p)).collect();
            let out = self
                .exec_as(&format!("{before} {} {after}", list.join(" ")), run_as)
                .await?;
            if out.exit_code != 0 {
                return Err(GlideshError::Module {
                    module: "file".to_string(),
                    message: format!("{before} {after} failed: {}", out.failure()),
                });
            }
        }
        Ok(())
    }

    /// Run `command` with `paths` appended, in chunks short enough for one argument list;
    /// escalated with `guard_parents`, each chunk only once [`trusted_paths`] holds for the
    /// directories its paths are in.
    async fn each_chunk_as(
        &self,
        command: &str,
        paths: &[String],
        run_as: Option<&ResolvedRunAs>,
        guard_parents: bool,
    ) -> Result<(), GlideshError> {
        for chunk in path_chunks(paths) {
            let list: Vec<String> = chunk.iter().map(|p| shell_escape(p)).collect();
            let run = format!("{command} {}", list.join(" "));
            let line = match run_as.filter(|_| guard_parents) {
                Some(_) => {
                    let mut parents: Vec<&str> = chunk
                        .iter()
                        .map(|p| match p.rsplit_once('/') {
                            Some(("", _)) | None => "/",
                            Some((parent, _)) => parent,
                        })
                        .collect();
                    parents.sort_unstable();
                    parents.dedup();
                    guarded(&parents, self.login_uid().await?, &run)
                }
                None => run,
            };
            let out = self.exec_as(&line, run_as).await?;
            if out.exit_code != 0 {
                let verb = command.split_whitespace().next().unwrap_or(command);
                return Err(GlideshError::Module {
                    module: "file".to_string(),
                    message: format!("{verb} failed: {}", out.failure()),
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
            "test -r {t} || {{ echo {} >&2; exit 1; }}\n\
             cat {t} > {} && rm -f {t}",
            shell_escape(SU_STAGING_LIMIT),
            shell_escape(dest)
        ),
    )
}

/// Whether the escalated side opens the login user's private staging file itself, which
/// only root or the login user can. Root does, so content never passes through sudo's
/// stdin or stdout, which an I/O log (`log_input`, `log_output`) records; `su` must, as
/// its PTY mangles binary data.
fn stages_escalated(r: &ResolvedRunAs) -> bool {
    r.method == RunAsMethod::Su || r.user == "root"
}

/// What follows the first `mark` line in a staged download: the escalated side prints
/// the mark before the content, and anything sudo printed ahead of it (a PAM notice) is
/// dropped.
fn after_mark(staged: &[u8], mark: &str) -> Option<Vec<u8>> {
    let line = format!("{mark}\n");
    staged
        .windows(line.len())
        .position(|w| w == line.as_bytes())
        .map(|at| staged[at + line.len()..].to_vec())
}

/// Why `su` cannot stage a file for most users: its PTY mangles binary data, so the
/// escalated side reads or writes the login user's private staging file itself.
const SU_STAGING_LIMIT: &str = "with run-as-method su, files are staged only for root or the \
     login user: the run-as user cannot open the login user's private staging file; use \
     run-as-method sudo or doas";

/// The escalated command of a streamed upload: once [`trusted_paths`] passes it prints
/// `ready` on stderr and writes its stdin into `dest` as [`place_staged_upload`] does.
fn write_streamed_upload(dest: &str, login_uid: &str, ready: &str) -> String {
    guarded(
        &[dest],
        login_uid,
        &format!(
            "printf '%s\\n' {} >&2\ncat > {}",
            shell_escape(ready),
            shell_escape(dest)
        ),
    )
}

/// The command behind [`SshSession::is_root_dir_as`]. `/` is compared on the host by
/// device and inode and the answer is a word, since `pwd -P` may print `//`, and under
/// `su`'s PTY every line ends in `\r\n`, after any prompt.
fn root_dir_check(path: &str) -> String {
    let dir = shell_escape(&format!("{}/", path.trim_end_matches('/')));
    let script = format!("if [ {dir} -ef / ]; then echo is-root; else echo not-root; fi");
    format!("sh -c {}", shell_escape(&script))
}

/// [`root_dir_check`]'s answer; `None` when it gave none.
fn root_dir_answer(stdout: &str) -> Option<bool> {
    match (stdout.contains("is-root"), stdout.contains("not-root")) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
    }
}

/// Printed before the output of a tree listing or a batch `stat`: under `su` the PTY puts
/// the password prompt, and anything else on stderr, in front of stdout.
const TREE_MARK: &str = "@@glidesh-tree-5f0c2e@@";

/// What follows the first [`TREE_MARK`] line in `stdout`.
fn after_tree_mark(stdout: &str) -> Option<&str> {
    let (_, rest) = stdout.split_once(TREE_MARK)?;
    Some(
        rest.strip_prefix("\r\n")
            .or_else(|| rest.strip_prefix('\n'))
            .unwrap_or(rest),
    )
}

/// The command listing every entry under `dest` for [`SshSession::list_tree_as`]: after
/// [`TREE_MARK`], NUL-separated entries — `dest`'s real path after `r`, then each name after
/// `d` (a directory) or `f` — then `@@` and find's status; a name may hold any other byte.
/// `@@link` and `@@none` stand for a `dest` that is a symlink or missing.
fn tree_listing(dest: &str) -> String {
    let script = format!(
        "printf '%s\\n' {TREE_MARK}\nd={}\n\
         if [ -L \"$d\" ]; then printf '@@link'; exit 0; fi\n\
         [ -d \"$d\" ] || {{ printf '@@none'; exit 0; }}\n\
         cd \"$d\" || exit 1\n\
         printf 'r%s\\0' \"$(pwd -P)\"\n\
         find . -mindepth 1 \\( -type d -exec printf 'd%s\\0' {{}} + \\) -o \
         -exec printf 'f%s\\0' {{}} + 2>/dev/null\n\
         printf '@@%s' \"$?\"",
        shell_escape(dest.trim_end_matches('/'))
    );
    format!("sh -c {}", shell_escape(&script))
}

/// The entries [`tree_listing`] printed, relative to `dest`, or why prune must stop: any
/// entry not in the form it prints — a name the PTY of `su` rewrote (a line break) or one
/// that is not UTF-8 — since a name that cannot be matched could be removed by mistake.
fn parse_tree_listing(dest: &str, stdout: &str) -> Result<Vec<(RemoteKind, String)>, String> {
    let listing = after_tree_mark(stdout)
        .ok_or_else(|| format!("could not list {dest}: {}", stdout.trim()))?;
    let (entries, status) = listing.rsplit_once('\0').unwrap_or(("", listing));
    match status.trim() {
        "@@none" => return Ok(Vec::new()),
        "@@link" => return Err(format!("{dest} is a symlink")),
        "@@0" => {}
        other => {
            return Err(format!(
                "could not list every entry under {dest} ({})",
                other.trim_start_matches('@')
            ));
        }
    }
    let mut entries = entries.split('\0').filter(|_| !entries.is_empty());
    // A symlinked directory on the way may put the destination anywhere: prune holds its
    // real path to the same depth as the one written.
    let real = entries
        .next()
        .and_then(|first| first.strip_prefix('r'))
        .ok_or_else(|| format!("could not list {dest}: no real path"))?;
    if real.split('/').filter(|n| !n.is_empty()).count() < 2 {
        return Err(format!(
            "{dest} is {real} on the host, less than two directories deep; refusing to prune it"
        ));
    }
    let mut listed = Vec::new();
    for entry in entries {
        let unreadable = || format!("{dest} holds a name that cannot be read exactly: {entry:?}");
        if entry.contains(['\u{FFFD}', '\r']) {
            return Err(unreadable());
        }
        let (kind, path) = match (entry.strip_prefix("d./"), entry.strip_prefix("f./")) {
            (Some(path), _) => (RemoteKind::Dir, path),
            (None, Some(path)) => (RemoteKind::Other, path),
            (None, None) => return Err(unreadable()),
        };
        if path
            .split('/')
            .any(|name| name.is_empty() || name == "." || name == "..")
        {
            return Err(unreadable());
        }
        listed.push((kind, path.to_string()));
    }
    Ok(listed)
}

/// The command printing, after [`TREE_MARK`], one line per path: `-` when it does not exist,
/// else its kind (`d`, `f`, or a link — tested first — as `ld`, `lf` or `l-` by what it
/// points to) then owner, group and mode, a link's own. `dest/` names the directory a
/// symlinked destination points to.
fn stat_listing(paths: &[String]) -> String {
    let list: Vec<String> = paths.iter().map(|p| shell_escape(p)).collect();
    // BSD stat for macOS targets, as in `get_file_attrs`.
    let script = format!(
        "printf '%s\\n' {TREE_MARK}\n\
         for p in {}; do\n\
         if [ -L \"$p\" ]; then \
         if [ -d \"$p\" ]; then k=ld; elif [ -e \"$p\" ]; then k=lf; else k=l-; fi; \
         elif [ -d \"$p\" ]; then k=d; elif [ -e \"$p\" ]; then k=f; else echo -; continue; fi\n\
         a=$(stat -c '%U %G %a' -- \"$p\" 2>/dev/null || \
         stat -f '%Su %Sg %Lp' -- \"$p\" 2>/dev/null) || a='? ? ?'\n\
         printf '%s %s\\n' \"$k\" \"$a\"\n\
         done",
        list.join(" ")
    );
    format!("sh -c {}", shell_escape(&script))
}

/// The `count` lines [`stat_listing`] printed, or `None` when they are not all there.
fn parse_stats(stdout: &str, count: usize) -> Option<Vec<Option<PathStat>>> {
    let lines: Vec<&str> = after_tree_mark(stdout)?.lines().map(str::trim).collect();
    if lines.len() != count {
        return None;
    }
    lines
        .into_iter()
        .map(|line| {
            if line == "-" {
                return Some(None);
            }
            let parts: Vec<&str> = line.split(' ').collect();
            let [kind, owner, group, mode] = parts.as_slice() else {
                return None;
            };
            let kind = match *kind {
                "d" => PathKind::Dir,
                "f" => PathKind::File,
                "ld" => PathKind::Link { to_dir: Some(true) },
                "lf" => PathKind::Link {
                    to_dir: Some(false),
                },
                "l-" => PathKind::Link { to_dir: None },
                _ => return None,
            };
            Some(Some(PathStat {
                kind,
                owner: owner.to_string(),
                group: group.to_string(),
                mode: mode.to_string(),
            }))
        })
        .collect()
}

/// `paths` in runs short enough for the 128 KiB a single argument may have: an escalated
/// command is one argument, and a path in it is quoted up to three times — in its list, in
/// the `sh -c` script, and by the escalation's wrapper — each `'` growing fourfold. The limit
/// leaves room for the script and a guard's copy of the parent directories.
fn path_chunks(paths: &[String]) -> Vec<&[String]> {
    const LIMIT: usize = 32 * 1024;
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut length = 0;
    for (i, path) in paths.iter().enumerate() {
        let quoted = sent_length(path);
        if i > start && length + quoted > LIMIT {
            chunks.push(&paths[start..i]);
            start = i;
            length = 0;
        }
        length += quoted;
    }
    if start < paths.len() {
        chunks.push(&paths[start..]);
    }
    chunks
}

/// How long `path` is once quoted three times over, as [`path_chunks`] counts it.
fn sent_length(path: &str) -> usize {
    shell_escape(&shell_escape(&shell_escape(path))).len() + 1
}

/// The error for a recursive owner, group or mode change whose destination is `/`: it
/// would change the whole filesystem.
pub fn root_refusal(path: &str) -> GlideshError {
    GlideshError::Module {
        module: "file".to_string(),
        message: format!(
            "refusing to change owner, group or mode recursively on / (destination {path:?})"
        ),
    }
}

/// The escalated `mkdir -p` of `dirs`, one directory at a time, each checked by
/// [`trusted_paths`] before the next goes in it: a new one may come out writable by a
/// group (umask, setgid parent, default ACL), whose members could swap in a symlink while
/// a single `mkdir -p` descends.
fn guarded_mkdir(dirs: &[&str], login_uid: &str) -> String {
    let escaped: Vec<String> = dirs.iter().map(|d| shell_escape(d)).collect();
    guarded(
        dirs,
        login_uid,
        &format!(
            "for t in {}; do\n  case $t in /*) ;; *) t=$(pwd -P)/$t ;; esac\n  \
             ( mkd \"$t\" ) || exit 1\ndone",
            escaped.join(" ")
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
///   carry an ACL that may grant writing. A group may be the private group of a trusted
///   user or of the directory's owner (the user-private-group scheme, whose umask `002`
///   makes every new directory group-writable): named after that user, its primary group,
///   and listing no other member — another account given the same primary group is not
///   seen. Of ACLs, on Linux one on a group-writable directory
///   (the group bits show its mask); on macOS/BSD, where the mode bits do not show it, an
///   entry allowing `add_file`, `add_subdirectory`, `delete_child`, `writesecurity` or
///   `chown`, or one not listed in macOS's `ls -le` form (FreeBSD's, say) — not any ACL,
///   as macOS homes carry a deny entry;
/// - a sticky directory writable by others (`/tmp`) is accepted above an existing
///   directory or link, which others cannot rename there. Not above a file or a missing
///   entry, which anyone could create first — a link, or a hard link to a root file;
/// - a symlink must be owned by a trusted user or by the owner of its directory — the
///   directory it really sits in, when the path reaches it through another symlink;
/// - an entry that does not exist yet is skipped: its directory decides who can create it.
///
/// Paths are read with `echo .` appended, since `$(…)` strips trailing newlines, which a
/// name may end in. The script also defines `mkd`, for [`guarded_mkdir`].
fn trusted_paths(targets: &[&str], login_uid: &str) -> String {
    const SCRIPT: &str = r#"u=$(id -u)
fail() { echo "$*" >&2; exit 1; }
trusted() { [ "$1" = 0 ] || [ "$1" = "$u" ] || [ "$1" = LOGIN ]; }
owner() { stat -c %u "$1" 2>/dev/null || stat -f %u "$1" 2>/dev/null || fail "cannot inspect $1"; }
dir_owner() { stat -L -c %u "$1" 2>/dev/null || stat -L -f %u "$1" 2>/dev/null || fail "cannot inspect $1"; }
up() { up=$(dirname "$1" && echo .) || fail "cannot resolve $1"; up=${up%??}; }
private_group() {
  gr=$(getent group "$1" 2>/dev/null) || return 1
  gn=${gr%%:*}; gm=${gr##*:}
  for pu in "$u" LOGIN "$2"; do
    pw=$(getent passwd "$pu" 2>/dev/null) || continue
    pn=${pw%%:*}; pg=${pw#*:*:*:}; pg=${pg%%:*}
    [ "$gn" = "$pn" ] && [ "$pg" = "$1" ] && { [ -z "$gm" ] || [ "$gm" = "$pn" ]; } && return 0
  done
  return 1
}
bsd=
acl_writable() {
  case $(ls -ld "$1" 2>/dev/null) in ??????????+*) ;; *) return 1 ;; esac
  if [ -z "$bsd" ]; then [ $((m & 020)) -ne 0 ]; return; fi
  e=$(ls -led "$1" 2>/dev/null) || return 0
  printf '%s\n' "$e" | grep -Eq '^ *[0-9]+: ' || return 0
  printf '%s\n' "$e" |
    grep -Eq '^ *[0-9]+: .* allow .*(add_file|add_subdirectory|delete_child|writesecurity|chown)'
}
entry() {
  if [ -L "$1" ]; then
    kind=link
    o=$(owner "$1") || exit 1
    trusted "$o" && return 0
    up "$1"; od=$(dir_owner "$up") || exit 1
    [ "$o" = "$od" ] ||
      fail "refusing: the symlink $1 is owned by uid $o, neither a trusted user nor the owner of its directory"
    return 0
  fi
  if [ ! -d "$1" ]; then
    kind=missing; [ -e "$1" ] && kind=file
    return 0
  fi
  kind=dir
  a=$(stat -c '%u %g %a' "$1" 2>/dev/null) || { a=$(stat -f '%u %g %Mp%Lp' "$1" 2>/dev/null) && bsd=1; } ||
    fail "cannot inspect $1"
  du=${a%% *}; a=${a#* }; g=${a%% *}; m=$((0${a#* }))
  if [ $((m & 02)) -ne 0 ] ||
    { [ $((m & 020)) -ne 0 ] && [ "$g" != 0 ] && ! private_group "$g" "$du"; }; then
    { [ $((m & 01000)) -ne 0 ] && [ "$2" = ancestor ]; } || fail "refusing: other users can write to $1"; fi
  if acl_writable "$1"; then
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
mkd() {
  [ -d "$1" ] && return 0
  up "$1"; ( mkd "$up" ) || exit 1
  mkdir "$1" || fail "cannot create $1"
  ( check "$1" 0 target ) || exit 1
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

    /// The listing prune decides from, run by a real `sh`: every name, whatever it holds,
    /// and a symlink never followed.
    #[cfg(unix)]
    #[test]
    fn the_tree_listing_names_every_entry_and_follows_no_link() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("dest");
        for dir in ["lib", "old/deep", "sp ace"] {
            std::fs::create_dir_all(dest.join(dir)).unwrap();
        }
        for file in ["a.conf", "lib/x", "old/deep/f", "sp ace/new\nline"] {
            std::fs::write(dest.join(file), "x").unwrap();
        }
        std::os::unix::fs::symlink("/etc", dest.join("link")).unwrap();
        std::os::unix::fs::symlink(&dest, tmp.path().join("aliased")).unwrap();
        let list = |path: &std::path::Path| {
            let path = path.to_str().unwrap();
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(tree_listing(path))
                .output()
                .unwrap();
            parse_tree_listing(path, &String::from_utf8_lossy(&out.stdout))
        };

        let mut listed = list(&dest).unwrap();
        listed.sort_by(|a, b| a.1.cmp(&b.1));
        let dirs: Vec<&str> = listed
            .iter()
            .filter(|(kind, _)| *kind == RemoteKind::Dir)
            .map(|(_, p)| p.as_str())
            .collect();
        let others: Vec<&str> = listed
            .iter()
            .filter(|(kind, _)| *kind == RemoteKind::Other)
            .map(|(_, p)| p.as_str())
            .collect();
        assert_eq!(dirs, ["lib", "old", "old/deep", "sp ace"]);
        assert_eq!(
            others,
            ["a.conf", "lib/x", "link", "old/deep/f", "sp ace/new\nline"]
        );
        assert!(
            list(&tmp.path().join("aliased"))
                .unwrap_err()
                .contains("is a symlink")
        );
        assert!(list(&tmp.path().join("missing")).unwrap().is_empty());
    }

    #[test]
    fn a_listing_that_did_not_finish_or_holds_a_foreign_name_stops_prune() {
        let listing = |body: &str| format!("{TREE_MARK}\nr/srv/a\0{body}");
        let err = |body: &str| parse_tree_listing("/srv/a", &listing(body)).unwrap_err();
        assert!(err("f./x\0@@1").contains("could not list every entry under /srv/a (1)"));
        for foreign in [
            "f./\u{FFFD}",
            "f./a\r\nb",
            "x./y",
            "f./a//b",
            "f./../etc",
            "garbage",
        ] {
            let body = format!("{foreign}\0@@0");
            assert!(err(&body).contains("cannot be read exactly"), "{foreign:?}");
        }
        assert!(
            parse_tree_listing("/srv/a", "no mark")
                .unwrap_err()
                .contains("could not list /srv/a")
        );
        assert_eq!(
            parse_tree_listing("/srv/a", &listing("d./x\0f./x/y\0@@0")).unwrap(),
            [
                (RemoteKind::Dir, "x".to_string()),
                (RemoteKind::Other, "x/y".to_string())
            ]
        );
        assert!(
            parse_tree_listing("/srv/a", &listing("@@0"))
                .unwrap()
                .is_empty()
        );
    }

    /// A symlinked directory on the way can make a deep destination `/` or `/etc`: prune
    /// holds the real path to the same depth.
    #[test]
    fn a_destination_shallow_on_the_host_stops_prune() {
        for real in ["/", "/etc"] {
            let stdout = format!("{TREE_MARK}\nr{real}\0f./passwd\0@@0");
            let err = parse_tree_listing("/srv/link/etc", &stdout).unwrap_err();
            assert!(
                err.contains("less than two directories deep"),
                "{real}: {err}"
            );
        }
        assert!(
            parse_tree_listing("/srv/a", &format!("{TREE_MARK}\nf./x\0@@0"))
                .unwrap_err()
                .contains("no real path")
        );
    }

    /// Under `su` a PTY puts the password prompt ahead of stdout and turns `\n` into `\r\n`.
    #[test]
    fn a_su_prompt_ahead_of_the_mark_is_skipped() {
        let stdout = format!("Password: \r\n{TREE_MARK}\r\nr/srv/a\0d./x\0f./x/y\0@@0");
        assert_eq!(
            parse_tree_listing("/srv/a", &stdout).unwrap(),
            [
                (RemoteKind::Dir, "x".to_string()),
                (RemoteKind::Other, "x/y".to_string())
            ]
        );
        assert_eq!(
            after_tree_mark(&format!("Password: \r\n{TREE_MARK}\r\nroot root 644\r\n")),
            Some("root root 644\r\n")
        );
    }

    #[test]
    fn long_path_lists_are_split_under_the_argument_limit() {
        let paths: Vec<String> = (0..2000).map(|i| format!("/srv/app/{i:0>40}")).collect();
        let chunks = path_chunks(&paths);
        assert!(chunks.len() > 1);
        assert_eq!(chunks.iter().map(|c| c.len()).sum::<usize>(), paths.len());
        for chunk in chunks {
            let quoted: usize = chunk.iter().map(|p| sent_length(p)).sum();
            assert!(quoted <= 32 * 1024);
        }
        assert!(path_chunks(&[]).is_empty());

        // A `'` grows fourfold at each of three quotings: the limit counts what is sent.
        let quotes: Vec<String> = (0..40)
            .map(|_| format!("/srv/{}", "'".repeat(100)))
            .collect();
        let chunks = path_chunks(&quotes);
        assert!(chunks.len() > 1, "6 KiB each once quoted three times");
        for chunk in chunks {
            let sent: usize = chunk
                .iter()
                .map(|p| shell_escape(&shell_escape(&shell_escape(p))).len() + 1)
                .sum();
            assert!(sent <= 32 * 1024, "{sent}");
        }
    }

    #[test]
    fn stats_read_the_kind_and_a_link_s_own_attributes() {
        let stdout = format!(
            "Password: \r\n{TREE_MARK}\r\nd root root 755\r\n-\r\nld app app 777\r\nf root root 644\r\n"
        );
        let stats = parse_stats(&stdout, 4).unwrap();
        assert_eq!(stats[0].as_ref().unwrap().kind, PathKind::Dir);
        assert!(stats[1].is_none());
        assert_eq!(
            stats[2].as_ref().unwrap().kind,
            PathKind::Link { to_dir: Some(true) }
        );
        assert_eq!(stats[3].as_ref().unwrap().mode, "644");
        assert!(parse_stats(&stdout, 3).is_none(), "a line short");
        assert!(parse_stats("d root root 755\n", 1).is_none(), "no mark");
    }

    /// The batch stat, run by a real `sh`: kinds, a link's own owner, a missing path.
    #[cfg(unix)]
    #[test]
    fn the_stat_listing_tells_each_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("d");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("f"), "x").unwrap();
        std::os::unix::fs::symlink(&dir, tmp.path().join("l")).unwrap();
        let paths: Vec<String> = [
            format!("{}/", tmp.path().join("l").display()),
            dir.join("f").display().to_string(),
            tmp.path().join("l").display().to_string(),
            tmp.path().join("missing").display().to_string(),
        ]
        .to_vec();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(stat_listing(&paths))
            .output()
            .unwrap();
        let stats = parse_stats(&String::from_utf8_lossy(&out.stdout), paths.len()).unwrap();
        let kinds: Vec<Option<PathKind>> =
            stats.iter().map(|s| s.as_ref().map(|s| s.kind)).collect();
        assert_eq!(
            kinds,
            [
                Some(PathKind::Dir),
                Some(PathKind::File),
                Some(PathKind::Link { to_dir: Some(true) }),
                None
            ],
            "a symlinked destination named `dest/` is its directory"
        );
    }

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
        assert!(String::from_utf8_lossy(&out.stderr).contains("run-as-method sudo or doas"));
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

        let link = safe.join("app.conf");
        std::os::unix::fs::symlink(shared.join("app.conf"), &link).unwrap();
        refused(&trusted(&link), "other users can write");

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
    fn a_staged_download_drops_what_sudo_printed_before_the_mark() {
        let staged = b"Warning: your password will expire in 3 days\nm-1\n\x00a\nm-1\nb";
        assert_eq!(after_mark(staged, "m-1").unwrap(), b"\x00a\nm-1\nb");
        assert_eq!(after_mark(b"m-1\n", "m-1").unwrap(), b"");
    }

    #[test]
    fn a_staged_download_without_its_mark_is_refused() {
        assert_eq!(after_mark(b"content", "m-1"), None);
        assert_eq!(after_mark(b"m-1", "m-1"), None);
    }

    #[test]
    fn only_root_and_su_read_the_staging_file_escalated() {
        let run_as = |user: &str, method| ResolvedRunAs {
            user: user.to_string(),
            method,
            password: None,
        };
        assert!(stages_escalated(&run_as("root", RunAsMethod::Sudo)));
        assert!(stages_escalated(&run_as("postgres", RunAsMethod::Su)));
        assert!(!stages_escalated(&run_as("postgres", RunAsMethod::Sudo)));
        assert!(!stages_escalated(&run_as("postgres", RunAsMethod::Doas)));
    }

    fn handshake(prompt: bool, input: &[u8]) -> Handshaking {
        Handshaking::new(Handshake {
            prompt: prompt.then(|| ("PROMPT".to_string(), b"pw\n".to_vec())),
            started: "STARTED".to_string(),
            ready: "READY".to_string(),
            input: input.to_vec(),
        })
    }

    fn password() -> Reply {
        Reply::Bytes(b"pw\n".to_vec())
    }

    #[test]
    fn a_password_is_sent_when_sudo_prompts_and_input_when_the_command_is_ready() {
        let mut h = handshake(true, b"content");
        assert_eq!(h.stderr(b"PROMPT"), (vec![password()], vec![]));
        assert_eq!(h.stderr(b"STARTED\n"), (vec![], vec![]));
        assert_eq!(
            h.stderr(b"READY\n"),
            (
                vec![Reply::Bytes(b"content".to_vec()), Reply::Close],
                vec![]
            )
        );
        assert_eq!(h.stderr(b"later"), (vec![], b"later".to_vec()));
    }

    #[test]
    fn no_password_is_sent_when_sudo_does_not_prompt() {
        let mut h = Handshaking::new(Handshake {
            prompt: Some(("PROMPT".to_string(), b"pw\n".to_vec())),
            started: "STARTED".to_string(),
            ready: "STARTED".to_string(),
            input: Vec::new(),
        });
        assert_eq!(h.stderr(b"STARTED\n"), (vec![Reply::Close], vec![]));
    }

    #[test]
    fn a_second_prompt_closes_stdin_and_sends_nothing_more() {
        let mut h = handshake(true, b"content");
        h.stderr(b"PROMPT");
        let (sent, kept) = h.stderr(b"Sorry, try again.\nPROMPT");
        assert_eq!(sent, vec![Reply::Close]);
        assert_eq!(kept, b"Sorry, try again.\n".to_vec());
        assert_eq!(
            h.stderr(b"STARTED\nREADY\n"),
            (vec![], b"STARTED\nREADY\n".to_vec())
        );
    }

    #[test]
    fn a_prompt_after_the_command_started_is_its_output_not_sudos() {
        let mut h = handshake(true, b"x");
        assert_eq!(h.stderr(b"STARTED\nrefusing: PROMPT\n"), (vec![], vec![]));
        assert!(!h.awaits_prompt());
        assert_eq!(h.finish(), b"refusing: \n".to_vec());
    }

    #[test]
    fn markers_split_across_chunks_are_found_and_removed() {
        let mut h = handshake(true, b"x");
        assert_eq!(h.stderr(b"notice\nPRO"), (vec![], vec![]));
        assert_eq!(h.stderr(b"MPT"), (vec![password()], vec![]));
        assert_eq!(h.stderr(b"STAR"), (vec![], vec![]));
        assert_eq!(h.stderr(b"TED\nREA"), (vec![], vec![]));
        let (sent, kept) = h.stderr(b"DY\nwarn");
        assert_eq!(sent, vec![Reply::Bytes(b"x".to_vec()), Reply::Close]);
        assert_eq!(kept, b"notice\nwarn".to_vec());
    }

    #[test]
    fn a_quiet_unfinished_line_is_a_prompt_of_pams_own() {
        let mut h = handshake(true, b"x");
        assert_eq!(
            h.stderr(b"Password for ops@EXAMPLE.COM: "),
            (vec![], vec![])
        );
        assert_eq!(h.quiet(), (vec![password()], vec![]));
        assert_eq!(h.quiet(), (vec![], vec![]), "answered once");
        h.stderr(b"\nVerification code: ");
        let (sent, kept) = h.quiet();
        assert_eq!(sent, vec![Reply::Close]);
        assert_eq!(
            kept,
            b"Password for ops@EXAMPLE.COM: \nVerification code: ".to_vec()
        );
    }

    #[test]
    fn a_quiet_start_of_the_own_prompt_is_not_a_prompt() {
        let mut h = handshake(true, b"x");
        h.stderr(b"PROM");
        assert_eq!(h.quiet(), (vec![], vec![]));
        assert_eq!(h.stderr(b"PT"), (vec![password()], vec![]));
    }

    #[test]
    fn a_finished_line_or_no_password_is_never_a_prompt() {
        let mut h = handshake(true, b"x");
        h.stderr(b"a notice\n");
        assert_eq!(h.quiet(), (vec![], vec![]));
        let mut h = handshake(false, b"x");
        h.stderr(b"half a line");
        assert!(!h.awaits_prompt());
        assert_eq!(h.quiet(), (vec![], vec![]));
    }

    #[test]
    fn stderr_held_back_is_kept_when_the_command_never_gets_ready() {
        let mut h = handshake(false, b"x");
        h.stderr(b"STARTED\n");
        assert_eq!(
            h.stderr(b"refusing: other users can write to /srv\n"),
            (vec![], vec![])
        );
        assert_eq!(
            h.finish(),
            b"refusing: other users can write to /srv\n".to_vec()
        );
    }

    /// Runs a streamed upload of `content` with `sh` standing in for the escalation.
    #[cfg(unix)]
    fn stream(dest: &std::path::Path, content: &[u8]) -> std::process::Output {
        use std::io::Write;
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(write_streamed_upload(dest.to_str().unwrap(), "0", "READY"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(content).unwrap();
        child.wait_with_output().unwrap()
    }

    #[cfg(unix)]
    const BINARY: &[u8] = b"\x00\xff\nREADY\n\r\nno newline at the end";

    #[cfg(unix)]
    #[test]
    fn a_streamed_upload_writes_its_stdin_after_saying_it_is_ready() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("app.bin");

        let out = stream(&dest, BINARY);
        assert!(out.status.success(), "{:?}", out);
        assert_eq!(out.stderr, b"READY\n");
        assert_eq!(std::fs::read(&dest).unwrap(), BINARY);
    }

    #[cfg(unix)]
    #[test]
    fn a_refused_streamed_upload_never_says_it_is_ready() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        let dest = shared.join("app.conf");
        std::fs::write(&dest, "keep me").unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();

        let out = stream(&dest, b"");
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("other users can write"), "{stderr}");
        assert!(!stderr.contains("READY"), "{stderr}");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "keep me");
    }

    #[cfg(unix)]
    fn mkdir_with_umask(dir: &std::path::Path, umask: &str) -> std::process::Output {
        let cmd = guarded_mkdir(&[dir.to_str().unwrap()], "0");
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("umask {umask} && {cmd}"))
            .output()
            .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn missing_directories_are_created_one_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let out = mkdir_with_umask(&dir.path().join("a/b/c"), "022");
        assert!(out.status.success(), "{:?}", out);
        assert!(dir.path().join("a/b/c").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn nothing_is_created_inside_a_new_directory_a_shared_group_could_write() {
        use std::os::unix::fs::PermissionsExt;
        let Some(gid) = own_groups()
            .into_iter()
            .find(|gid| gid != "0" && !is_private_group(gid))
        else {
            return; // the account belongs to no shared group to try
        };
        // A setgid parent hands its group to what is created in it.
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::chown(dir.path(), None, Some(gid.parse().unwrap())).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o2700)).unwrap();

        let out = mkdir_with_umask(&dir.path().join("a/b"), "002");
        refused(&out, "other users can write");
        assert!(dir.path().join("a").is_dir());
        assert!(!dir.path().join("a/b").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_new_directory_of_the_users_private_group_is_trusted() {
        let primary = std::process::Command::new("id").arg("-g").output().unwrap();
        let primary = String::from_utf8_lossy(&primary.stdout).trim().to_string();
        if !is_private_group(&primary) {
            return; // the account has no private group
        }
        let dir = tempfile::tempdir().unwrap();
        let out = mkdir_with_umask(&dir.path().join("a/b"), "002");
        assert!(out.status.success(), "{:?}", out);
        assert!(dir.path().join("a/b").is_dir());
    }

    /// This account's group ids.
    #[cfg(unix)]
    fn own_groups() -> Vec<String> {
        let out = std::process::Command::new("id").arg("-G").output().unwrap();
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect()
    }

    /// Whether `gid` is this account's private group, as the guard's `private_group` reads it.
    #[cfg(unix)]
    fn is_private_group(gid: &str) -> bool {
        let user = std::process::Command::new("id")
            .arg("-un")
            .output()
            .unwrap();
        let user = String::from_utf8_lossy(&user.stdout).trim().to_string();
        let primary = std::process::Command::new("id").arg("-g").output().unwrap();
        let Ok(group) = std::process::Command::new("getent")
            .args(["group", gid])
            .output()
        else {
            return false;
        };
        let group = String::from_utf8_lossy(&group.stdout).trim().to_string();
        let fields: Vec<&str> = group.split(':').collect();
        fields.len() == 4
            && fields[0] == user
            && String::from_utf8_lossy(&primary.stdout).trim() == gid
            && (fields[3].is_empty() || fields[3] == user)
    }

    #[test]
    fn the_root_answer_survives_a_pty_and_a_prompt() {
        assert_eq!(root_dir_answer("Password: \r\nis-root\r\n"), Some(true));
        assert_eq!(root_dir_answer("not-root\n"), Some(false));
        assert_eq!(root_dir_answer("sudo: a password is required\n"), None);
    }

    #[cfg(unix)]
    #[test]
    fn every_name_of_root_is_root() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("root");
        std::os::unix::fs::symlink("/", &link).unwrap();
        let answer = |path: &str| {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(root_dir_check(path))
                .output()
                .unwrap();
            root_dir_answer(&String::from_utf8_lossy(&out.stdout))
        };
        for path in ["/", "//", "", "/tmp/..", "/.", link.to_str().unwrap()] {
            assert_eq!(answer(path), Some(true), "{path:?}");
        }
        assert_eq!(answer(dir.path().to_str().unwrap()), Some(false));
        assert_eq!(answer("/no/such/dir"), Some(false));
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
