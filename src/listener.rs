use std::{io, net::SocketAddr};

use axum::serve::Listener;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
};

pub struct ReloadableListener {
    current: TcpListener,
    replacements: mpsc::Receiver<TcpListener>,
}

impl ReloadableListener {
    pub(crate) fn new(current: TcpListener, replacements: mpsc::Receiver<TcpListener>) -> Self {
        Self {
            current,
            replacements,
        }
    }
}

impl Listener for ReloadableListener {
    type Io = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (TcpStream, SocketAddr) {
        loop {
            tokio::select! {
                biased;
                Some(listener) = self.replacements.recv() => {
                    self.current = listener;
                }
                connection = Listener::accept(&mut self.current) => return connection,
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.current.local_addr()
    }
}
