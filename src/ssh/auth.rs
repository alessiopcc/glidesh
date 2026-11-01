//! How a connection logs in: with a key glidesh loaded, or through the user's SSH agent.

use super::handler::SshHandler;
use crate::error::GlideshError;
use russh::client;
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::keys::ssh_key::{Algorithm, HashAlg, PublicKey};
use std::path::{Path, PathBuf};

/// How every connection of a command logs in.
#[derive(Clone)]
pub enum Auth {
    Key(PrivateKeyWithHashAlg),
    /// Each connection asks the agent afresh, so concurrent hosts never share one stream.
    Agent,
}

impl From<PrivateKeyWithHashAlg> for Auth {
    fn from(key: PrivateKeyWithHashAlg) -> Self {
        Auth::Key(key)
    }
}

/// Where the login comes from, before anything is loaded.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Choice {
    Key(PathBuf),
    Agent,
}

/// An explicit key (`--key`, the inventory's `ssh-key`) is used alone; otherwise the agent
/// when it holds a key, else the default key file.
pub(crate) fn choose(explicit: Option<&Path>, default: &Path, agent_has_keys: bool) -> Choice {
    match explicit {
        Some(path) => Choice::Key(path.to_path_buf()),
        None if agent_has_keys => Choice::Agent,
        None => Choice::Key(default.to_path_buf()),
    }
}

impl Auth {
    /// Settles how this command logs in, unlocking a key file if that is the choice.
    pub async fn resolve(
        explicit: Option<&Path>,
        default: &Path,
        use_agent: bool,
    ) -> Result<Auth, GlideshError> {
        let agent_has_keys = use_agent && explicit.is_none() && !agent_keys().await.is_empty();
        match choose(explicit, default, agent_has_keys) {
            Choice::Agent => {
                tracing::debug!("Using the SSH agent");
                Ok(Auth::Agent)
            }
            Choice::Key(path) if explicit.is_none() && !path.exists() => {
                let agent = if use_agent {
                    "no SSH agent holds a key"
                } else {
                    "--no-agent is set"
                };
                Err(GlideshError::KeyLoad {
                    message: format!(
                        "no SSH key to log in with: {} does not exist and {agent}; pass --key, \
                         or add a key to ssh-agent",
                        path.display()
                    ),
                })
            }
            Choice::Key(path) => Ok(Auth::Key(super::key::load(&path)?)),
        }
    }

    /// Logs `handle` in as `user`, or says why not.
    pub(crate) async fn login(
        &self,
        handle: &mut client::Handle<SshHandler>,
        user: &str,
    ) -> Result<(), String> {
        match self {
            Auth::Key(key) => {
                let result = handle
                    .authenticate_publickey(user, key.clone())
                    .await
                    .map_err(|e| e.to_string())?;
                tracing::debug!("Auth result: {:?}", result);
                if result.success() {
                    Ok(())
                } else {
                    Err("Authentication rejected by server".to_string())
                }
            }
            Auth::Agent => {
                let mut agent = connect_agent()
                    .await
                    .ok_or("the SSH agent is no longer reachable")?;
                // OpenSSH agents sign with a key restricted by `ssh-add -h` only for a bound
                // session; an agent without the extension refuses it and signs as before.
                if let Some(binding) = handle.agent_session_binding()
                    && let Err(e) = agent.bind_session(binding, false).await
                {
                    tracing::debug!("The SSH agent did not bind the session: {e}");
                }
                let keys = identities(&mut agent).await;
                let mut refused = None;
                for key in &keys {
                    let hash =
                        matches!(key.algorithm(), Algorithm::Rsa { .. }).then_some(HashAlg::Sha256);
                    match handle
                        .authenticate_publickey_with(user, key.clone(), hash, &mut agent)
                        .await
                    {
                        Ok(result) if result.success() => return Ok(()),
                        Ok(_) => {}
                        Err(e) => refused = Some(e.to_string()),
                    }
                }
                // A key the agent would not sign with may be the right one, so its refusal is
                // a better clue than the server's.
                Err(match refused {
                    Some(e) => format!("the SSH agent did not sign: {e}"),
                    None => format!(
                        "the server accepted none of the SSH agent's {} key(s); add the right \
                         one with ssh-add, or pass --key",
                        keys.len()
                    ),
                })
            }
        }
    }
}

type Agent = AgentClient<Box<dyn AgentStream + Send + Unpin>>;

/// How long an agent may take to list its keys before glidesh goes on without it.
const AGENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The keys the user's agent holds, none when there is no agent. A socket that accepts but
/// never answers, like a stale forwarded agent, counts as none rather than hanging the run.
async fn agent_keys() -> Vec<PublicKey> {
    keys_within(AGENT_TIMEOUT, connect_agent()).await
}

async fn keys_within(
    limit: std::time::Duration,
    agent: impl std::future::Future<Output = Option<Agent>>,
) -> Vec<PublicKey> {
    let list = async {
        match agent.await {
            Some(mut agent) => identities(&mut agent).await,
            None => Vec::new(),
        }
    };
    tokio::time::timeout(limit, list).await.unwrap_or_else(|_| {
        tracing::debug!("The SSH agent did not answer within {limit:?}");
        Vec::new()
    })
}

/// Certificates are left out: glidesh authenticates with plain keys.
async fn identities(agent: &mut Agent) -> Vec<PublicKey> {
    match agent.request_identities().await {
        Ok(identities) => identities
            .into_iter()
            .filter_map(|identity| match identity {
                AgentIdentity::PublicKey { key, .. } => Some(key),
                AgentIdentity::Certificate { .. } => None,
            })
            .collect(),
        Err(e) => {
            tracing::debug!("The SSH agent did not list its keys: {e}");
            Vec::new()
        }
    }
}

#[cfg(unix)]
async fn connect_agent() -> Option<Agent> {
    AgentClient::connect_env()
        .await
        .ok()
        .map(AgentClient::dynamic)
}

/// `SSH_AUTH_SOCK` names a pipe on Windows too; Windows OpenSSH's agent listens on a fixed
/// one, and Pageant answers otherwise. A stale `SSH_AUTH_SOCK` must not hide either.
#[cfg(windows)]
async fn connect_agent() -> Option<Agent> {
    const OPENSSH_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";
    let pipes = std::env::var("SSH_AUTH_SOCK").into_iter();
    for pipe in pipes.chain([OPENSSH_PIPE.to_string()]) {
        if let Ok(agent) = AgentClient::connect_named_pipe(&pipe).await {
            return Some(agent.dynamic());
        }
    }
    AgentClient::connect_pageant()
        .await
        .ok()
        .map(AgentClient::dynamic)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_key_is_used_alone() {
        let chosen = choose(Some(Path::new("deploy")), Path::new("default"), true);
        assert_eq!(chosen, Choice::Key(PathBuf::from("deploy")));
    }

    #[test]
    fn the_agent_comes_before_the_default_key() {
        assert_eq!(choose(None, Path::new("default"), true), Choice::Agent);
        assert_eq!(
            choose(None, Path::new("default"), false),
            Choice::Key(PathBuf::from("default"))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_agent_that_never_answers_counts_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("agent.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let _silent = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let agent = async {
            AgentClient::connect_uds(&socket)
                .await
                .ok()
                .map(AgentClient::dynamic)
        };
        let started = std::time::Instant::now();
        let keys = keys_within(std::time::Duration::from_millis(200), agent).await;
        assert!(keys.is_empty());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[tokio::test]
    async fn no_key_and_no_agent_says_how_to_log_in() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("id_ed25519");
        let error = Auth::resolve(None, &missing, false).await.err().unwrap();
        let error = error.to_string();
        assert!(
            error.contains("--no-agent is set") && error.contains("pass --key"),
            "{error}"
        );
    }
}
