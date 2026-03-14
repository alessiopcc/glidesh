pub mod connection;
pub mod handler;
pub use connection::SshSession;

/// Controls SSH host key verification behavior.
#[derive(Debug, Clone, Copy)]
pub struct HostKeyPolicy {
    /// Verify the server's host key against known_hosts.
    pub verify: bool,
    /// Accept and save unknown host keys to known_hosts.
    pub accept_new: bool,
}
