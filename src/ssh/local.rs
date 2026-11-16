//! The controller as a host: what [`Session`](super::Session) does over SSH, done on
//! this machine for a host declared `local=#true`. Commands run in `sh -c` as the user running
//! glidesh, and files are read and written directly; escalation, staging and every module's
//! logic above that are shared with SSH hosts.

use super::connection::{
    CappedStream, CommandOutput, ExecOptions, Handshaking, OUTPUT_LIMIT, PROMPT_QUIET, Reply,
    collected,
};
use crate::error::GlideshError;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// Fails unless the controller can be a host: every module runs POSIX shell commands.
pub fn supported(name: &str) -> Result<(), GlideshError> {
    if cfg!(unix) {
        return Ok(());
    }
    Err(GlideshError::Local {
        host: name.to_string(),
        message: "a local host needs a Linux or macOS controller: modules run POSIX shell \
                  commands on their host"
            .to_string(),
    })
}

/// A local command: in the user's home directory, as an SSH command starts, and without the
/// variables that hold glidesh's own passphrases or reach its SSH agent, which a command run
/// over SSH never sees either.
fn command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    for (name, _) in std::env::vars_os() {
        let text = name.to_string_lossy();
        if text.starts_with("GLIDESH_") || text == "SSH_AUTH_SOCK" {
            cmd.env_remove(&name);
        }
    }
    if let Some(home) = dirs::home_dir().filter(|h| h.is_dir()) {
        cmd.current_dir(home);
    }
    cmd
}

/// The exit status as SSH reports it: a code, or the signal that killed the command, named
/// as SSH names it (`KILL`).
fn exit_parts(status: ExitStatus) -> (Option<u32>, Option<String>) {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return (None, Some(signal_name(signal)));
        }
    }
    (status.code().map(|c| c as u32), None)
}

#[cfg(unix)]
fn signal_name(signal: i32) -> String {
    let name = match signal {
        libc::SIGHUP => "HUP",
        libc::SIGINT => "INT",
        libc::SIGQUIT => "QUIT",
        libc::SIGILL => "ILL",
        libc::SIGABRT => "ABRT",
        libc::SIGFPE => "FPE",
        libc::SIGKILL => "KILL",
        libc::SIGSEGV => "SEGV",
        libc::SIGPIPE => "PIPE",
        libc::SIGALRM => "ALRM",
        libc::SIGTERM => "TERM",
        libc::SIGUSR1 => "USR1",
        libc::SIGUSR2 => "USR2",
        other => return other.to_string(),
    };
    name.to_string()
}

enum Event {
    Stdout(std::io::Result<usize>),
    Stderr(std::io::Result<usize>),
    Exited(std::io::Result<ExitStatus>),
    Quiet,
}

/// [`Session::exec_with`](super::Session::exec_with) on the controller.
pub(super) async fn exec(
    host: &str,
    command_line: &str,
    opts: ExecOptions,
) -> Result<CommandOutput, GlideshError> {
    let failed = |what: &str, e: std::io::Error| GlideshError::Local {
        host: host.to_string(),
        message: format!("failed to {what}: {e}"),
    };
    if opts.pty {
        return Err(GlideshError::Local {
            host: host.to_string(),
            message: "it has no terminal for `su` to ask its password on: use \
                      run-as-method=\"sudo\" or \"doas\""
                .to_string(),
        });
    }
    let feeds_stdin = opts.stdin.is_some() || opts.handshake.is_some();
    let mut cmd = command("sh");
    // A session of its own, without glidesh's terminal: a command that would ask on /dev/tty
    // (a sudo with no run-as, an ssh) fails at once, as it does over SSH without a PTY,
    // instead of waiting behind the TUI for an answer nobody sees it ask for.
    #[cfg(unix)]
    unsafe {
        // SAFETY: `setsid` is async-signal-safe and touches no memory of this process.
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd
        .arg("-c")
        .arg(command_line)
        .stdin(if feeds_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| failed("start sh", e))?;

    let mut handshaking = opts.handshake.map(Handshaking::new);
    let stdin = child.stdin.take();
    // A task writes stdin while this loop keeps reading, as over SSH: a command that exits
    // without reading it all would otherwise leave the write waiting.
    let (replies, writer) = match (stdin, &handshaking, opts.stdin) {
        (Some(mut stdin), Some(_), _) => {
            let (replies, mut queued) = tokio::sync::mpsc::unbounded_channel::<Reply>();
            let writer = tokio::spawn(async move {
                while let Some(reply) = queued.recv().await {
                    match reply {
                        Reply::Bytes(bytes) => {
                            if stdin.write_all(&bytes).await.is_err() {
                                break;
                            }
                        }
                        Reply::Close => break,
                    }
                }
            });
            (Some(replies), Some(writer))
        }
        (Some(mut stdin), None, Some(input)) => {
            let writer = tokio::spawn(async move {
                let _ = stdin.write_all(&input).await;
            });
            (None, Some(writer))
        }
        _ => (None, None),
    };
    let send = |sent: Vec<Reply>| {
        if let Some(replies) = &replies {
            for reply in sent {
                let _ = replies.send(reply);
            }
        }
    };

    let mut out_pipe = child.stdout.take().expect("stdout is piped");
    let mut err_pipe = child.stderr.take().expect("stderr is piped");
    let (mut out_buf, mut err_buf) = (vec![0u8; 32 * 1024], vec![0u8; 32 * 1024]);
    let (mut out_open, mut err_open) = (true, true);
    let mut stdout = CappedStream::new(OUTPUT_LIMIT);
    let mut stderr = CappedStream::new(OUTPUT_LIMIT);
    let mut status: Option<(Option<u32>, Option<String>)> = None;

    while status.is_none() || out_open || err_open {
        // Once the command has exited, a child it left in the background may hold its output
        // open for good: take what is already there, then stop, as over SSH.
        let wait = if status.is_some() {
            Some(Duration::from_millis(200))
        } else if handshaking.as_ref().is_some_and(|h| h.awaits_prompt()) {
            Some(PROMPT_QUIET)
        } else {
            None
        };
        let next = async {
            tokio::select! {
                n = out_pipe.read(&mut out_buf), if out_open => Event::Stdout(n),
                n = err_pipe.read(&mut err_buf), if err_open => Event::Stderr(n),
                s = child.wait(), if status.is_none() => Event::Exited(s),
            }
        };
        let event = match wait {
            Some(wait) => tokio::time::timeout(wait, next)
                .await
                .unwrap_or(Event::Quiet),
            None => next.await,
        };
        match event {
            Event::Stdout(Ok(0) | Err(_)) => out_open = false,
            Event::Stdout(Ok(n)) => stdout.push(&out_buf[..n]),
            Event::Stderr(Ok(0) | Err(_)) => err_open = false,
            Event::Stderr(Ok(n)) => match &mut handshaking {
                Some(h) => {
                    let (sent, kept) = h.stderr(&err_buf[..n]);
                    stderr.push(&kept);
                    send(sent);
                }
                None => stderr.push(&err_buf[..n]),
            },
            Event::Exited(exited) => {
                status = Some(exit_parts(exited.map_err(|e| failed("wait for sh", e))?));
            }
            Event::Quiet if status.is_some() => break,
            Event::Quiet => {
                if let Some(h) = &mut handshaking {
                    let (sent, kept) = h.quiet();
                    stderr.push(&kept);
                    send(sent);
                }
            }
        }
    }
    if let Some(h) = &mut handshaking {
        stderr.push(&h.finish());
    }
    if let Some(writer) = writer {
        writer.abort();
    }
    let (code, signal) = status.unwrap_or((None, None));
    Ok(collected(stdout, stderr, code, signal.as_deref()))
}

/// [`Session::upload_file`](super::Session::upload_file) on the controller: created
/// or truncated in place, as the SFTP write is.
pub(super) async fn write_file(host: &str, content: &[u8], path: &str) -> Result<(), GlideshError> {
    let failed = |what: &str, e: std::io::Error| GlideshError::Local {
        host: host.to_string(),
        message: format!("failed to {what} {path}: {e}"),
    };
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .await
        .map_err(|e| failed("open file", e))?;
    file.write_all(content)
        .await
        .map_err(|e| failed("write to", e))?;
    file.flush().await.map_err(|e| failed("flush file", e))
}

/// [`Session::download_file`](super::Session::download_file) on the controller.
pub(super) async fn read_file(host: &str, path: &str) -> Result<Vec<u8>, GlideshError> {
    tokio::fs::read(path)
        .await
        .map_err(|e| GlideshError::Local {
            host: host.to_string(),
            message: format!("failed to read {path}: {e}"),
        })
}

/// [`Session::interactive_shell`](super::Session::interactive_shell) on the controller:
/// the user's `$SHELL` on this terminal.
pub(super) async fn interactive_shell(host: &str) -> Result<u32, GlideshError> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string());
    let status = command(&shell)
        .status()
        .await
        .map_err(|e| GlideshError::Local {
            host: host.to_string(),
            message: format!("failed to start {shell}: {e}"),
        })?;
    let (code, signal) = exit_parts(status);
    Ok(code.unwrap_or(if signal.is_some() {
        super::connection::NO_EXIT_STATUS
    } else {
        0
    }))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::ssh::connection::Handshake;

    async fn run(command_line: &str, opts: ExecOptions) -> CommandOutput {
        exec("self", command_line, opts).await.unwrap()
    }

    #[tokio::test]
    async fn a_command_reports_its_output_and_exit_code() {
        let out = run("echo out; echo err >&2; exit 3", ExecOptions::default()).await;
        assert_eq!(out.stdout, "out\n");
        assert_eq!(out.stderr, "err\n");
        assert_eq!(out.exit_code, 3);
        assert!(!out.stdout_cut);
    }

    #[tokio::test]
    async fn a_command_that_reads_stdin_gets_the_input_then_eof() {
        let out = run(
            "cat",
            ExecOptions {
                stdin: Some(b"given\n".to_vec()),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out.stdout, "given\n");
        assert_eq!(run("cat", ExecOptions::default()).await.stdout, "");
    }

    #[tokio::test]
    async fn a_killed_command_is_not_reported_as_success() {
        let out = run("kill -9 $$", ExecOptions::default()).await;
        assert_eq!(out.exit_code, crate::ssh::connection::NO_EXIT_STATUS);
        assert!(
            out.stderr.contains("command killed by signal KILL"),
            "{}",
            out.stderr
        );
    }

    #[tokio::test]
    async fn a_command_has_no_terminal_to_ask_on() {
        let out = run(
            "if (exec 3</dev/tty) 2>/dev/null; then echo terminal; else echo none; fi",
            ExecOptions::default(),
        )
        .await;
        assert_eq!(out.stdout, "none\n");
    }

    #[tokio::test]
    async fn a_child_left_running_does_not_hold_the_command_open() {
        let started = std::time::Instant::now();
        let out = run("sleep 30 & echo done", ExecOptions::default()).await;
        assert_eq!(out.stdout, "done\n");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn a_handshake_sends_its_input_once_the_command_is_ready() {
        let out = run(
            "printf 'started\\n' >&2; printf 'ready\\n' >&2; cat",
            ExecOptions {
                handshake: Some(Handshake {
                    prompt: None,
                    started: "started".into(),
                    ready: "ready".into(),
                    input: b"content".to_vec(),
                }),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out.stdout, "content");
        assert_eq!(out.stderr, "");
        assert_eq!(out.exit_code, 0);
    }

    #[tokio::test]
    async fn su_is_refused_for_want_of_a_terminal() {
        let err = exec(
            "self",
            "true",
            ExecOptions {
                pty: true,
                ..Default::default()
            },
        )
        .await
        .err()
        .unwrap();
        assert!(err.to_string().contains("run-as-method=\"sudo\""), "{err}");
    }

    #[tokio::test]
    async fn glidesh_passphrases_do_not_reach_the_command() {
        // SAFETY: set before the command starts and read only by it; no other test reads it.
        unsafe { std::env::set_var("GLIDESH_LOCAL_TEST_SECRET", "hidden") };
        let out = run(
            "printf '%s' \"${GLIDESH_LOCAL_TEST_SECRET-unset}\"",
            ExecOptions::default(),
        )
        .await;
        assert_eq!(out.stdout, "unset");
    }

    #[tokio::test]
    async fn files_are_written_and_read_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let path = path.to_str().unwrap();
        write_file("self", b"long content", path).await.unwrap();
        write_file("self", b"short", path).await.unwrap();
        assert_eq!(read_file("self", path).await.unwrap(), b"short");
        let missing = read_file("self", &format!("{path}.missing")).await;
        assert!(missing.is_err());
    }
}
