use crate::error::GlideshError;
use crate::ssh::SshSession;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::io;
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelDirection {
    Local,
    Reverse,
}

static NEXT_TUNNEL_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_TUNNEL_ID.fetch_add(1, Ordering::Relaxed)
}

pub struct LocalForward {
    pub id: u64,
    pub via_host: String,
    pub local_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
    pub accepts: Arc<AtomicUsize>,
    cancel_token: CancellationToken,
}

impl LocalForward {
    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }
}

pub async fn start_local_forward(
    session: Arc<SshSession>,
    via_host: String,
    local_port: u16,
    remote_host: String,
    remote_port: u16,
) -> Result<LocalForward, GlideshError> {
    let listener = TcpListener::bind(("127.0.0.1", local_port))
        .await
        .map_err(|e| {
            GlideshError::Other(format!("Failed to bind 127.0.0.1:{}: {}", local_port, e))
        })?;

    let cancel_token = CancellationToken::new();
    let accepts = Arc::new(AtomicUsize::new(0));
    let id = next_id();

    let ln_cancel = cancel_token.clone();
    let ln_accepts = Arc::clone(&accepts);
    let ln_remote_host = remote_host.clone();
    let ln_session = Arc::clone(&session);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = ln_cancel.cancelled() => break,
                accept_result = listener.accept() => {
                    let (tcp, peer) = match accept_result {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!("local forward accept error: {}", e);
                            continue;
                        }
                    };
                    let session = Arc::clone(&ln_session);
                    let remote_host = ln_remote_host.clone();
                    let accepts = Arc::clone(&ln_accepts);
                    let conn_cancel = ln_cancel.clone();
                    tokio::spawn(async move {
                        let channel = match session
                            .open_direct_tcpip(
                                &remote_host,
                                remote_port as u32,
                                &peer.ip().to_string(),
                                peer.port() as u32,
                            )
                            .await
                        {
                            Ok(c) => c,
                            Err(e) => {
                                tracing::warn!("open_direct_tcpip failed: {}", e);
                                return;
                            }
                        };
                        accepts.fetch_add(1, Ordering::Relaxed);
                        let mut tcp = tcp;
                        let mut remote = channel.into_stream();
                        tokio::select! {
                            _ = conn_cancel.cancelled() => {}
                            _ = io::copy_bidirectional(&mut tcp, &mut remote) => {}
                        }
                    });
                }
            }
        }
    });

    Ok(LocalForward {
        id,
        via_host,
        local_port,
        remote_host,
        remote_port,
        accepts,
        cancel_token,
    })
}

pub struct ReverseForward {
    pub id: u64,
    pub via_host: String,
    pub remote_bind_addr: String,
    pub remote_bind_port: u16,
    pub local_host: String,
    pub local_port: u16,
    pub accepts: Arc<AtomicUsize>,
    cancel_token: CancellationToken,
    session: Arc<SshSession>,
}

impl ReverseForward {
    pub async fn cancel(&self) {
        self.cancel_token.cancel();
        let _ = self
            .session
            .cancel_tcpip_forward(&self.remote_bind_addr, self.remote_bind_port)
            .await;
    }
}

pub async fn start_reverse_forward(
    session: Arc<SshSession>,
    via_host: String,
    remote_bind_addr: String,
    remote_bind_port: u16,
    local_host: String,
    local_port: u16,
) -> Result<ReverseForward, GlideshError> {
    let mut rx = session
        .tcpip_forward(&remote_bind_addr, remote_bind_port)
        .await?;

    let cancel_token = CancellationToken::new();
    let accepts = Arc::new(AtomicUsize::new(0));
    let id = next_id();

    let task_cancel = cancel_token.clone();
    let task_accepts = Arc::clone(&accepts);
    let task_local_host = local_host.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = task_cancel.cancelled() => break,
                ch = rx.recv() => {
                    let Some(channel) = ch else { break };
                    let local_host = task_local_host.clone();
                    let accepts = Arc::clone(&task_accepts);
                    let conn_cancel = task_cancel.clone();
                    tokio::spawn(async move {
                        let tcp = match TcpStream::connect((local_host.as_str(), local_port)).await {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::warn!("reverse forward local connect failed: {}", e);
                                return;
                            }
                        };
                        accepts.fetch_add(1, Ordering::Relaxed);
                        let mut tcp = tcp;
                        let mut remote = channel.into_stream();
                        tokio::select! {
                            _ = conn_cancel.cancelled() => {}
                            _ = io::copy_bidirectional(&mut tcp, &mut remote) => {}
                        }
                    });
                }
            }
        }
    });

    Ok(ReverseForward {
        id,
        via_host,
        remote_bind_addr,
        remote_bind_port,
        local_host,
        local_port,
        accepts,
        cancel_token,
        session,
    })
}
