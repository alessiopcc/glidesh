//! Privilege escalation (`run-as`) command wrapping.
//!
//! A module builds an ordinary remote command (which may contain pipes, `&&`,
//! redirects); [`wrap`] turns it into an equivalent command that runs through a
//! shell under `sudo`/`doas`/`su` as the target user. The inner command is passed
//! to `sh -c` as a single single-quoted argument so its own shell metacharacters
//! survive intact.

use crate::config::types::{ResolvedRunAs, RunAsMethod};
use crate::error::GlideshError;
use crate::ssh::connection::CommandOutput;
use crate::util::shell_escape;
use std::sync::OnceLock;

/// The escalation password, sourced once at startup (`--ask-pass` prompt or
/// `GLIDESH_RUNAS_PASS`). Global because it applies to every host in a run; held in
/// process memory only, never logged or persisted.
static RUN_AS_PASSWORD: OnceLock<Option<String>> = OnceLock::new();

/// Set the global escalation password. Idempotent; the first value wins.
pub fn set_password(password: Option<String>) {
    let _ = RUN_AS_PASSWORD.set(password);
}

/// The configured escalation password, if any.
pub fn password() -> Option<&'static str> {
    RUN_AS_PASSWORD.get().and_then(|p| p.as_deref())
}

/// A command rewritten to run under privilege escalation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wrapped {
    /// The command to execute on the remote host.
    pub command: String,
    /// Bytes to feed on stdin before EOF (the escalation password + newline).
    pub stdin: Option<Vec<u8>>,
    /// Whether the exec channel must allocate a PTY (required by `su`, which reads
    /// its password from the controlling terminal rather than stdin).
    pub pty: bool,
}

/// Human-readable method name for error messages.
pub fn method_name(method: RunAsMethod) -> &'static str {
    match method {
        RunAsMethod::Sudo => "sudo",
        RunAsMethod::Doas => "doas",
        RunAsMethod::Su => "su",
    }
}

/// Wrap `inner` so it executes as `run_as.user` via the configured method.
pub fn wrap(run_as: &ResolvedRunAs, inner: &str) -> Wrapped {
    let cmd = shell_escape(inner);
    let user = shell_escape(&run_as.user);

    match run_as.method {
        // `-n` fails fast instead of blocking on a password prompt; with a password
        // we use `-S` (read from stdin) and an empty prompt so nothing pollutes stderr.
        RunAsMethod::Sudo => match &run_as.password {
            None => Wrapped {
                command: format!("sudo -n -u {user} -- sh -c {cmd}"),
                stdin: None,
                pty: false,
            },
            Some(pw) => Wrapped {
                command: format!("sudo -S -p '' -u {user} -- sh -c {cmd}"),
                stdin: Some(format!("{pw}\n").into_bytes()),
                pty: false,
            },
        },
        // doas cannot read a password from stdin (it requires a tty), so it is
        // passwordless-only here; `-n` makes a missing-credential case fail cleanly.
        RunAsMethod::Doas => Wrapped {
            command: format!("doas -n -u {user} sh -c {cmd}"),
            stdin: None,
            pty: false,
        },
        // su reads its password from the controlling terminal, so a PTY is required;
        // stderr is merged into stdout under a PTY (documented best-effort path).
        RunAsMethod::Su => Wrapped {
            command: format!("su {user} -c {cmd}"),
            stdin: run_as
                .password
                .as_ref()
                .map(|pw| format!("{pw}\n").into_bytes()),
            pty: true,
        },
    }
}

/// Substrings that indicate the escalation itself was denied (wrong password, not a
/// sudoer, missing tty) rather than the wrapped command failing on its own merits.
const DENIAL_MARKERS: &[&str] = &[
    "incorrect password",
    "sorry, try again",
    "is not in the sudoers file",
    "is not permitted",
    "a password is required",
    "a terminal is required",
    "no tty present",
    "authentication failure",
    "authentication failed",
];

/// Inspect a wrapped command's output. Returns a [`GlideshError::RunAs`] when the
/// failure looks like a denied escalation, otherwise `None` (the caller treats the
/// output as a normal command result).
pub fn classify_failure(run_as: &ResolvedRunAs, out: &CommandOutput) -> Option<GlideshError> {
    if out.exit_code == 0 {
        return None;
    }
    let combined = format!("{}\n{}", out.stdout, out.stderr).to_lowercase();
    if DENIAL_MARKERS.iter().any(|m| combined.contains(m)) {
        // Prefer stderr, but fall back to stdout: under a PTY (e.g. `su`) the
        // server merges stderr into stdout, so the denial text lives there.
        let stderr = out.stderr.trim();
        let message = if stderr.is_empty() {
            out.stdout.trim().to_string()
        } else {
            stderr.to_string()
        };
        return Some(GlideshError::RunAs {
            user: run_as.user.clone(),
            method: method_name(run_as.method).to_string(),
            message,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(method: RunAsMethod, password: Option<&str>) -> ResolvedRunAs {
        ResolvedRunAs {
            user: "root".to_string(),
            method,
            password: password.map(|p| p.to_string()),
        }
    }

    #[test]
    fn sudo_passwordless() {
        let w = wrap(&resolved(RunAsMethod::Sudo, None), "id -u");
        assert_eq!(w.command, "sudo -n -u 'root' -- sh -c 'id -u'");
        assert_eq!(w.stdin, None);
        assert!(!w.pty);
    }

    #[test]
    fn sudo_with_password() {
        let w = wrap(&resolved(RunAsMethod::Sudo, Some("s3cr3t")), "id -u");
        assert_eq!(w.command, "sudo -S -p '' -u 'root' -- sh -c 'id -u'");
        assert_eq!(w.stdin, Some(b"s3cr3t\n".to_vec()));
        assert!(!w.pty);
    }

    #[test]
    fn doas_is_passwordless_no_pty() {
        let w = wrap(&resolved(RunAsMethod::Doas, Some("ignored")), "id -u");
        assert_eq!(w.command, "doas -n -u 'root' sh -c 'id -u'");
        assert_eq!(w.stdin, None);
        assert!(!w.pty);
    }

    #[test]
    fn su_requires_pty_and_feeds_password() {
        let w = wrap(&resolved(RunAsMethod::Su, Some("pw")), "id -u");
        assert_eq!(w.command, "su 'root' -c 'id -u'");
        assert_eq!(w.stdin, Some(b"pw\n".to_vec()));
        assert!(w.pty);
    }

    #[test]
    fn inner_single_quotes_are_escaped() {
        let w = wrap(&resolved(RunAsMethod::Sudo, None), "echo 'hi there'");
        assert_eq!(
            w.command,
            r#"sudo -n -u 'root' -- sh -c 'echo '\''hi there'\'''"#
        );
    }

    #[test]
    fn classify_detects_denial() {
        let r = resolved(RunAsMethod::Sudo, None);
        let out = CommandOutput {
            exit_code: 1,
            stdout: String::new(),
            stderr: "sudo: a password is required".to_string(),
        };
        assert!(classify_failure(&r, &out).is_some());
    }

    #[test]
    fn classify_uses_stdout_when_stderr_is_empty() {
        // Under a PTY (su) the denial text is merged into stdout; the surfaced
        // message must still be populated rather than empty.
        let r = resolved(RunAsMethod::Su, Some("wrong"));
        let out = CommandOutput {
            exit_code: 1,
            stdout: "su: Authentication failure".to_string(),
            stderr: String::new(),
        };
        match classify_failure(&r, &out) {
            Some(GlideshError::RunAs { message, .. }) => {
                assert_eq!(message, "su: Authentication failure");
            }
            other => panic!("expected RunAs error, got {:?}", other),
        }
    }

    #[test]
    fn classify_ignores_normal_command_failure() {
        let r = resolved(RunAsMethod::Sudo, None);
        let out = CommandOutput {
            exit_code: 2,
            stdout: String::new(),
            stderr: "mkfs: device not found".to_string(),
        };
        assert!(classify_failure(&r, &out).is_none());
    }

    #[test]
    fn classify_ignores_success() {
        let r = resolved(RunAsMethod::Sudo, None);
        let out = CommandOutput {
            exit_code: 0,
            stdout: "0".to_string(),
            stderr: String::new(),
        };
        assert!(classify_failure(&r, &out).is_none());
    }
}
