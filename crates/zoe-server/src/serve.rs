//! `zoe serve` as a library: [`Server`] serves one store with the ZEO 6
//! msgpack protocol, under the storage name `1`.
//!
//! The binary runs it until SIGTERM. A test runs it in its own process
//! with [`Server::start`] and [`Server::stop`].

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::net::{TcpListener, TcpSocket};
use tokio::runtime::Runtime;
use zoe_core::{Store, ZoeError};

use crate::protocol;

/// The settings of a server.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    path: PathBuf,
    listen: SocketAddr,
}

impl ServeOptions {
    /// Serve the store in `path` on a free port of 127.0.0.1.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        }
    }

    /// Listen on `addr`. A port of 0 takes a free port.
    #[must_use]
    pub fn listen(mut self, addr: SocketAddr) -> Self {
        self.listen = addr;
        self
    }
}

/// Why a server did not start.
#[derive(Debug)]
pub enum Error {
    /// The server cannot listen on the address.
    Listen(SocketAddr, io::Error),
    /// The store does not open.
    Store(PathBuf, ZoeError),
    /// The async runtime does not start.
    Runtime(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Listen(addr, error) => write!(f, "cannot listen on {addr}: {error}"),
            Self::Store(path, error) => write!(f, "{}: {error}", path.display()),
            Self::Runtime(error) => write!(f, "cannot start the runtime: {error}"),
        }
    }
}

impl std::error::Error for Error {}

/// A running server. Dropping it stops it.
pub struct Server {
    runtime: Option<Runtime>,
    store: Option<Arc<Store>>,
    addr: SocketAddr,
}

impl Server {
    /// Listen, open the store and accept clients. Return once the server
    /// accepts connections.
    pub fn start(options: ServeOptions) -> Result<Self, Error> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("zoe-serve")
            .build()
            .map_err(Error::Runtime)?;
        // Listen first: a server that cannot listen changes nothing on
        // disk.
        let listener = {
            let _enter = runtime.enter();
            bind(options.listen).map_err(|error| Error::Listen(options.listen, error))?
        };
        let addr = listener
            .local_addr()
            .map_err(|error| Error::Listen(options.listen, error))?;
        // This thread is not a thread of the runtime.
        let store = Store::open(&options.path)
            .map(Arc::new)
            .map_err(|error| Error::Store(options.path.clone(), error))?;
        runtime.spawn(accept(listener, Arc::clone(&store)));
        Ok(Self {
            runtime: Some(runtime),
            store: Some(store),
            addr,
        })
    }

    /// The address that the server listens on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stop the server: close every connection, abort the open
    /// transactions and close the store. A commit that is in `finish`
    /// completes first.
    pub fn stop(mut self) {
        self.shutdown();
    }

    /// Call `ready` with the address, then serve until SIGTERM or SIGINT.
    /// Then stop.
    pub(crate) fn serve_until_signal(
        mut self,
        ready: impl FnOnce(SocketAddr) -> io::Result<()>,
    ) -> io::Result<()> {
        use tokio::signal::unix::{SignalKind, signal};

        let runtime = self.runtime.as_ref().expect("the server runs");
        let (mut term, mut interrupt) = {
            let _enter = runtime.enter();
            (
                signal(SignalKind::terminate())?,
                signal(SignalKind::interrupt())?,
            )
        };
        ready(self.addr)?;
        runtime.block_on(async {
            tokio::select! {
                _ = term.recv() => {}
                _ = interrupt.recv() => {}
            }
        });
        self.shutdown();
        Ok(())
    }

    fn shutdown(&mut self) {
        // Dropping the runtime drops every connection task. Each session
        // then ends after its current call, and the drop waits for it.
        // Tokio refuses that drop on a thread of an async runtime, as in an
        // async test, so another thread drops it.
        if let Some(runtime) = self.runtime.take() {
            std::thread::scope(|scope| {
                scope.spawn(move || drop(runtime));
            });
        }
        // The sessions dropped their references, so this closes the store.
        drop(self.store.take());
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Listen on `addr`. The socket takes SO_REUSEADDR, so a restarted server
/// binds the same port at once.
fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    socket.listen(1024)
}

/// Accept clients until the runtime stops.
async fn accept(listener: TcpListener, store: Arc<Store>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let _ = stream.set_nodelay(true);
                tokio::spawn(protocol::serve_connection(
                    stream,
                    Arc::clone(&store),
                    protocol::STORAGE_NAME.to_owned(),
                ));
            }
            Err(error) => {
                eprintln!("zoe serve: accept failed: {error}");
                tokio::task::yield_now().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zoe-serve-lib-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn stop_closes_the_store_also_in_an_async_context() {
        let dir = fresh_dir("async-stop");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let server = Server::start(ServeOptions::new(&dir)).unwrap();
            assert!(server.addr().ip().is_loopback());
            assert_ne!(server.addr().port(), 0);
            server.stop();
        });
        // The lock of the store is free again.
        Store::open(&dir).unwrap();
    }

    #[test]
    fn a_server_that_cannot_listen_creates_no_store() {
        let dir = fresh_dir("no-listen");
        let first = Server::start(ServeOptions::new(fresh_dir("listening"))).unwrap();
        let taken = first.addr();
        // The port is taken by a listening socket, so the bind fails.
        let error = Server::start(ServeOptions::new(&dir).listen(taken))
            .err()
            .expect("the port is taken");
        assert!(matches!(error, Error::Listen(..)), "{error}");
        assert!(!dir.exists());
    }
}
