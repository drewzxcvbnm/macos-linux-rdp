//! Listener: accepts connections, wraps them in TLS and runs one session per connection.
//! Only one authenticated session is active at a time; a new one replaces the old.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::identity::Identity;
use crate::Config;

/// Kill switch of the currently active (authenticated) session.
#[derive(Default)]
pub struct Active(Mutex<Option<oneshot::Sender<()>>>);

impl Active {
    /// Makes the caller the active session, ending the previous one. Returns the caller's kill signal.
    pub fn take_over(&self) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        if let Some(prev) = self.0.lock().unwrap().replace(tx) {
            let _ = prev.send(());
        }
        rx
    }
}

pub async fn serve(bind: &str, port: u16, config: Arc<Config>, tls: Option<Identity>) -> Result<()> {
    let listener = TcpListener::bind((bind, port)).await.with_context(|| format!("listening on {bind}:{port}"))?;
    log::info!("listening on {bind}:{port}{}", if tls.is_none() { " (INSECURE, no TLS)" } else { "" });
    let acceptor = tls.map(|t| t.acceptor);
    let active = Arc::new(Active::default());
    let clipboard = crate::clipboard::spawn();
    loop {
        let (tcp, peer) = listener.accept().await?;
        tcp.set_nodelay(true).ok();
        let (config, acceptor, active, clipboard) = (config.clone(), acceptor.clone(), active.clone(), clipboard.clone());
        tokio::spawn(async move {
            log::info!("[{peer}] connected");
            let r = match acceptor {
                Some(a) => match a.accept(tcp).await {
                    Ok(tls) => crate::session::run(tls, peer, config, active, clipboard).await,
                    Err(e) => Err(anyhow::anyhow!("TLS handshake failed: {e}")),
                },
                None => crate::session::run(tcp, peer, config, active, clipboard).await,
            };
            match r {
                Ok(reason) => log::info!("[{peer}] disconnected: {reason}"),
                Err(e) => log::info!("[{peer}] disconnected: {e:#}"),
            }
        });
    }
}
