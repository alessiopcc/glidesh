use crate::config::types::ResolvedHost;
use crate::error::GlideshError;
use crate::ssh::{HostKeyPolicy, SshSession};
use russh_keys::key::PrivateKeyWithHashAlg;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// A per-address cache of live SSH sessions. Used by the console TUI so
/// long-running tunnels survive across shell entries and can share one
/// underlying SSH connection with other features.
pub struct SessionPool {
    inner: Mutex<HashMap<String, Arc<SshSession>>>,
    key: PrivateKeyWithHashAlg,
    policy: HostKeyPolicy,
}

impl SessionPool {
    pub fn new(key: PrivateKeyWithHashAlg, policy: HostKeyPolicy) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            key,
            policy,
        }
    }

    pub async fn get_or_connect(
        &self,
        host: &ResolvedHost,
    ) -> Result<Arc<SshSession>, GlideshError> {
        let addr_key = format!("{}@{}:{}", host.user, host.address, host.port);

        {
            let guard = self.inner.lock().await;
            if let Some(s) = guard.get(&addr_key) {
                return Ok(Arc::clone(s));
            }
        }

        let session = match &host.jump {
            Some(jump) => {
                SshSession::connect_via_jump(
                    &host.address,
                    host.port,
                    &host.user,
                    &self.key,
                    self.policy,
                    jump,
                )
                .await?
            }
            None => {
                SshSession::connect(&host.address, host.port, &host.user, &self.key, self.policy)
                    .await?
            }
        };
        let arc = Arc::new(session);

        let mut guard = self.inner.lock().await;
        guard.entry(addr_key).or_insert_with(|| Arc::clone(&arc));
        Ok(arc)
    }

    pub async fn remove(&self, host: &ResolvedHost) {
        let addr_key = format!("{}@{}:{}", host.user, host.address, host.port);
        let mut guard = self.inner.lock().await;
        guard.remove(&addr_key);
    }
}
