use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::serve::Listener;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinSet,
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};

use crate::{StartupError, config};

#[derive(Clone, Copy, Debug)]
pub struct TlsPeer(pub SocketAddr);

type Handshake = Option<(TlsStream<TcpStream>, TlsPeer)>;

pub struct TlsListener {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
    tasks: JoinSet<Handshake>,
}

impl TlsListener {
    pub async fn bind(doc: &serde_json::Value) -> Result<Option<Self>, StartupError> {
        if doc["enable"].as_bool() != Some(true) {
            return Ok(None);
        }
        if doc["auto"].as_bool() == Some(true) {
            return Err("configure local TLS certificate and key files".into());
        }
        let address = config::address(doc["address"].as_str().ok_or("invalid TLS address")?)?;
        let cert =
            tokio::fs::read(doc["cert_file"].as_str().ok_or("missing TLS certificate")?).await?;
        let key = zeroize::Zeroizing::new(
            tokio::fs::read(doc["key_file"].as_str().ok_or("missing TLS key")?).await?,
        );
        let certs = CertificateDer::pem_slice_iter(&cert).collect::<Result<Vec<_>, _>>()?;
        let private = PrivateKeyDer::from_pem_slice(&key)?;
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut configuration = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(certs, private)?;
        configuration.alpn_protocols = vec![b"http/1.1".to_vec()];
        let tcp = TcpListener::bind(address).await?;
        Ok(Some(Self {
            tcp,
            acceptor: TlsAcceptor::from(Arc::new(configuration)),
            tasks: JoinSet::new(),
        }))
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
