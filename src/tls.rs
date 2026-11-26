use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::serve::Listener;
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinSet,
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};

use crate::{
    StartupError,
    certificates::{Acme, Certificates},
    config,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug)]
pub struct TlsPeer(pub SocketAddr);

type Handshake = Option<(TlsStream<TcpStream>, TlsPeer)>;

pub struct TlsListener {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
    tasks: JoinSet<Handshake>,
    stop: CancellationToken,
}

impl TlsListener {
    pub async fn bind(
        doc: &serde_json::Value,
        acme: Option<Acme>,
        certificates: Arc<Certificates>,
    ) -> Result<Option<Self>, StartupError> {
        if doc["enable"].as_bool() != Some(true) {
            return Ok(None);
        }
        let address = config::address(doc["address"].as_str().ok_or("invalid TLS address")?)?;
        let stop = CancellationToken::new();
        let tcp = TcpListener::bind(address).await?;
        let child = stop.clone().drop_guard();
        if let Some(acme) = acme {
            certificates.automatic(acme, stop.clone()).await?;
        } else {
            let cert = doc["cert_file"]
                .as_str()
                .filter(|path| !path.is_empty())
                .ok_or("missing TLS certificate")?;
            let key = doc["key_file"]
                .as_str()
                .filter(|path| !path.is_empty())
                .ok_or("missing TLS key")?;
            certificates
                .manual(cert.into(), key.into(), stop.clone())
                .await?;
        }
        let mut configuration =
            rustls::ServerConfig::builder_with_provider(certificates.provider())
                .with_safe_default_protocol_versions()?
                .with_no_client_auth()
                .with_cert_resolver(certificates);
        configuration.alpn_protocols = vec![b"http/1.1".to_vec()];
        child.disarm();
        Ok(Some(Self {
            tcp,
            acceptor: TlsAcceptor::from(Arc::new(configuration)),
            tasks: JoinSet::new(),
            stop,
        }))
    }
}

impl Drop for TlsListener {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = TlsPeer;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            tokio::select! {
                Some(result)=self.tasks.join_next(), if !self.tasks.is_empty() => {
                    if let Ok(Some(connection))=result {return connection;}
                }
                (socket,addr)=Listener::accept(&mut self.tcp), if self.tasks.len()<64 => {
                    let acceptor=self.acceptor.clone();
                    self.tasks.spawn(async move {
                        socket.set_nodelay(true).ok();
                        let stream=tokio::time::timeout(Duration::from_secs(4),acceptor.accept(socket)).await.ok()?.ok()?;
                        Some((stream,TlsPeer(addr)))
                    });
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.tcp.local_addr().map(TlsPeer)
    }
}
