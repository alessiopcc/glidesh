use async_trait::async_trait;
use russh::client;
use ssh_key::PublicKey;
use std::path::PathBuf;

use super::HostKeyPolicy;

pub struct SshHandler {
    pub host: String,
    pub port: u16,
    pub host_key_policy: HostKeyPolicy,
}

/// Returns the path to the user's known_hosts file.
/// Unlike russh_keys which uses `~/ssh/known_hosts` on Windows,
/// we always use `~/.ssh/known_hosts` to match OpenSSH behavior.
fn known_hosts_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".ssh").join("known_hosts"))
}

#[async_trait]
impl client::Handler for SshHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        if !self.host_key_policy.verify {
            return Ok(true);
        }

        let Some(path) = known_hosts_path() else {
            tracing::warn!("Could not determine home directory for known_hosts lookup");
            return Ok(false);
        };

        match russh_keys::known_hosts::check_known_hosts_path(
            &self.host,
            self.port,
            server_public_key,
            &path,
        ) {
            Ok(true) => Ok(true),
            Ok(false) => {
                if self.host_key_policy.accept_new {
                    tracing::info!(
                        "New host key for {}:{}, adding to known_hosts",
                        self.host,
                        self.port
                    );
                    if let Err(e) = russh_keys::known_hosts::learn_known_hosts_path(
                        &self.host,
                        self.port,
                        server_public_key,
                        &path,
                    ) {
                        tracing::warn!("Could not save host key to known_hosts: {}", e);
                    }
                    Ok(true)
                } else {
                    tracing::error!(
                        "Host key for {}:{} not found in known_hosts. \
                         Use --accept-new-host-key to add it, \
                         or --no-host-key-check to skip verification entirely.",
                        self.host,
                        self.port
                    );
                    Ok(false)
                }
            }
            Err(russh_keys::Error::KeyChanged { line }) => {
                tracing::error!(
                    "HOST KEY VERIFICATION FAILED for {}:{}! \
                     Key differs from known_hosts line {}. \
                     This could indicate a man-in-the-middle attack. \
                     Use --no-host-key-check to skip verification.",
                    self.host,
                    self.port,
                    line
                );
                Ok(false)
            }
            Err(e) => {
                tracing::warn!(
                    "Could not verify host key for {}:{}: {}",
                    self.host,
                    self.port,
                    e
                );
                Ok(false)
            }
        }
    }
}
