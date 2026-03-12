use crate::error::GlideshError;
use crate::ssh::HostKeyPolicy;
use crate::ssh::connection::SshSession;
use russh_keys::key::PrivateKeyWithHashAlg;
use std::collections::HashMap;
use tokio::sync::Mutex;

/// Connection pool: caches one SSH session per host.
#[allow(dead_code)]
pub struct SshPool {
    connections: Mutex<HashMap<String, SshSession>>,
}

#[allow(dead_code)]
impl Default for SshPool {
    fn default() -> Self {
        Self::new()
    }
}

impl SshPool {
    pub fn new() -> Self {
        SshPool {
            connections: Mutex::new(HashMap::new()),
        }
    }

    pub async fn get_or_connect(
        &self,
        host: &str,
        port: u16,
        user: &str,
        key: &PrivateKeyWithHashAlg,
        host_key_policy: HostKeyPolicy,
    ) -> Result<(), GlideshError> {
        let mut conns = self.connections.lock().await;
        if !conns.contains_key(host) {
            let session = SshSession::connect(host, port, user, key, host_key_policy).await?;
            conns.insert(host.to_string(), session);
        }
        Ok(())
    }
}
