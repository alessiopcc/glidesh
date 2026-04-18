use crate::config::types::ResolvedHost;
use crate::error::GlideshError;
use crate::ssh::{HostKeyPolicy, SshSession};
use russh_keys::key::PrivateKeyWithHashAlg;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

type Slot = Arc<Mutex<Option<Arc<SshSession>>>>;

/// A per-host cache of live SSH sessions. Used by the console TUI so
/// long-running tunnels survive across shell entries and can share one
/// underlying SSH connection with other features.
pub struct SessionPool {
    inner: Mutex<HashMap<String, Slot>>,
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
        let key = pool_key(host);

        let slot: Slot = {
            let mut guard = self.inner.lock().await;
            Arc::clone(
                guard
                    .entry(key.clone())
                    .or_insert_with(|| Arc::new(Mutex::new(None))),
            )
        };

        let mut slot_guard = slot.lock().await;
        if let Some(s) = slot_guard.as_ref() {
            return Ok(Arc::clone(s));
        }

        let session = self.connect_one(host).await?;
        let arc = Arc::new(session);
        *slot_guard = Some(Arc::clone(&arc));
        Ok(arc)
    }

    async fn connect_one(&self, host: &ResolvedHost) -> Result<SshSession, GlideshError> {
        match &host.jump {
            Some(jump) => {
                SshSession::connect_via_jump(
                    &host.address,
                    host.port,
                    &host.user,
                    &self.key,
                    self.policy,
                    jump,
                )
                .await
            }
            None => {
                SshSession::connect(&host.address, host.port, &host.user, &self.key, self.policy)
                    .await
            }
        }
    }

    pub async fn remove(&self, host: &ResolvedHost) {
        let key = pool_key(host);
        let slot = {
            let mut guard = self.inner.lock().await;
            guard.remove(&key)
        };
        if let Some(slot) = slot {
            let mut g = slot.lock().await;
            *g = None;
        }
    }
}

fn pool_key(host: &ResolvedHost) -> String {
    let base = format!("{}@{}:{}", host.user, host.address, host.port);
    match &host.jump {
        Some(j) => format!("{}|via={}@{}:{}", base, j.user, j.address, j.port),
        None => base,
    }
}
