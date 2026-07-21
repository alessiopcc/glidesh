//! A redacting wrapper around the executor's event channel.
//!
//! Every event carrying an interpolated string (`resource`, `stdout`, `stderr`, `error`)
//! passes through [`SecretRegistry::redact`] before it is forwarded, so decrypted secret
//! values can never reach the TUI or the on-disk run logs — even when a remote command
//! echoes one in its own output. This is the single choke point for secret redaction.

use crate::executor::result::ExecutorEvent;
use glidesh::secrets::SecretRegistry;
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct EventSink {
    inner: mpsc::UnboundedSender<ExecutorEvent>,
    registry: Arc<SecretRegistry>,
}

impl EventSink {
    pub fn new(inner: mpsc::UnboundedSender<ExecutorEvent>, registry: Arc<SecretRegistry>) -> Self {
        Self { inner, registry }
    }

    #[allow(clippy::result_large_err)]
    pub fn send(&self, event: ExecutorEvent) -> Result<(), mpsc::error::SendError<ExecutorEvent>> {
        let event = if self.registry.is_empty() {
            event
        } else {
            self.redact(event)
        };
        self.inner.send(event)
    }

    fn redact(&self, event: ExecutorEvent) -> ExecutorEvent {
        let scrub = |s: String| self.registry.redact(&s);
        match event {
            ExecutorEvent::ModuleCheck {
                host,
                module,
                resource,
            } => ExecutorEvent::ModuleCheck {
                host,
                module,
                resource: scrub(resource),
            },
            ExecutorEvent::ModuleResult {
                host,
                module,
                resource,
                changed,
                stdout,
                stderr,
                exit_code,
            } => ExecutorEvent::ModuleResult {
                host,
                module,
                resource: scrub(resource),
                changed,
                stdout: scrub(stdout),
                stderr: scrub(stderr),
                exit_code,
            },
            ExecutorEvent::ModuleFailed {
                host,
                module,
                resource,
                error,
            } => ExecutorEvent::ModuleFailed {
                host,
                module,
                resource: scrub(resource),
                error: scrub(error),
            },
            ExecutorEvent::StepFailed { host, step, error } => ExecutorEvent::StepFailed {
                host,
                step,
                error: scrub(error),
            },
            ExecutorEvent::NodeAuthFailed { host, error } => ExecutorEvent::NodeAuthFailed {
                host,
                error: scrub(error),
            },
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glidesh::secrets::Secrets;

    /// A registry seeded with a known plaintext by decrypting a real token.
    fn registry_with(plaintext: &str) -> Arc<SecretRegistry> {
        use glidesh::secrets::config::{Provider, SecretsConfig};
        use glidesh::secrets::passphrase::{PassphraseProvider, generate_dek};
        use glidesh::secrets::token;
        let dek = generate_dek();
        let wrapped = PassphraseProvider::new("pw".into()).wrap_dek(&dek).unwrap();
        let cfg = SecretsConfig {
            provider: Provider::Passphrase,
            encryptedkey: wrapped,
        };
        let secrets = Secrets::open(Some(&cfg), Some("pw")).unwrap();
        let tok = token::encrypt_value(&dek, plaintext.as_bytes()).unwrap();
        secrets.decrypt_token(&tok).unwrap();
        secrets.registry()
    }

    #[test]
    fn redacts_secret_in_module_result() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink::new(tx, registry_with("hunter2"));
        sink.send(ExecutorEvent::ModuleResult {
            host: "h".into(),
            module: "shell".into(),
            resource: "echo".into(),
            changed: true,
            stdout: "the password is hunter2 ok".into(),
            stderr: String::new(),
            exit_code: 0,
        })
        .unwrap();
        match rx.try_recv().unwrap() {
            ExecutorEvent::ModuleResult { stdout, .. } => {
                assert_eq!(stdout, "the password is *** ok");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn empty_registry_is_passthrough() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink::new(tx, Secrets::locked().registry());
        sink.send(ExecutorEvent::ModuleFailed {
            host: "h".into(),
            module: "shell".into(),
            resource: "r".into(),
            error: "boom hunter2".into(),
        })
        .unwrap();
        match rx.try_recv().unwrap() {
            ExecutorEvent::ModuleFailed { error, .. } => assert_eq!(error, "boom hunter2"),
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
