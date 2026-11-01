//! The ZEO port: its listening socket, a TCP address or a unix domain
//! socket, the accept loop, the TLS handshake of a connection, and the
//! count of the tasks that serve, for the stop.

use std::fs;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::net::{TcpListener, TcpSocket, TcpStream, UnixListener, UnixStream};

use super::Address;
use crate::protocol::{self, Connection, Stopping, Storages};
use crate::runtime::Tokio;
use crate::tls::Tls;

/// The listening socket of the ZEO port.
pub(super) enum Listener {
    Tcp(TcpListener),
    /// A unix domain socket, with its path as the options give it.
    Unix(UnixListener, Arc<Path>),
}

/// A connection that the listener accepted.
enum Accepted {
    Tcp(TcpStream, SocketAddr),
    /// A connection on a unix domain socket, with the path of the socket.
    Unix(UnixStream, Arc<Path>),
}

impl Listener {
    /// Listen on `address`. Return the socket file of a unix domain
    /// socket too: its drop removes the file. The call must run in the
    /// context of a runtime.
    pub(super) fn bind(address: &Address) -> io::Result<(Self, Option<SocketFile>)> {
        match address {
            Address::Tcp(addr) => Ok((Self::Tcp(bind_tcp(*addr)?), None)),
            Address::Unix(path) => {
                let (listener, file) = bind_unix(path)?;
                Ok((Self::Unix(listener, Arc::from(path.as_path())), file))
            }
        }
    }

    /// The address of the listener: a TCP address with the port that it
    /// took, or the path of the socket as the options give it.
    pub(super) fn address(&self) -> io::Result<Address> {
        match self {
            Self::Tcp(listener) => listener.local_addr().map(Address::Tcp),
            Self::Unix(_, path) => Ok(Address::Unix(path.to_path_buf())),
        }
    }

    /// Accept the next connection.
    async fn accept(&self) -> io::Result<Accepted> {
        match self {
            Self::Tcp(listener) => {
                let (stream, peer) = listener.accept().await?;
                Ok(Accepted::Tcp(stream, peer))
            }
            Self::Unix(listener, path) => {
                let (stream, _) = listener.accept().await?;
                Ok(Accepted::Unix(stream, Arc::clone(path)))
            }
        }
    }
}

/// The socket file of the unix domain socket of a server. Its drop removes
/// the file, but only when the file at the path is still the one that the
/// server bound. So it never removes the socket of another server that
/// took the path meanwhile.
pub(super) struct SocketFile {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl SocketFile {
    /// Return the socket file at `path`, which the server just bound.
    fn bound(path: &Path) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        let same = fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.dev() == self.device && metadata.ino() == self.inode);
        if same && let Err(error) = fs::remove_file(&self.path) {
            tracing::warn!(
                path = %self.path.display(),
                "cannot remove the socket file: {error}"
            );
        }
    }
}

/// Listen on the unix domain socket `path`. The call binds the path as it
/// is, so a relative path is relative to the current directory. When a
/// socket file is at the path, the call connects to it first, as runzeo
/// does. When a process accepts the connection, the call fails with
/// `AddrInUse`, and the file stays. When the connection is refused, the
/// file is stale and goes. A path that is not a socket, a symbolic link
/// included, fails with `AlreadyExists` and stays.
fn bind_unix(path: &Path) -> io::Result<(UnixListener, Option<SocketFile>)> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "another process accepts connections on the socket",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    match fs::remove_file(path) {
                        Err(error) if error.kind() != io::ErrorKind::NotFound => {
                            return Err(error);
                        }
                        _ => {}
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "the path exists and is not a socket",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(path)?;
    // Without its device and inode, the stop cannot tell that the file is
    // still the socket of the server, so the file stays.
    let file = SocketFile::bound(path).ok();
    Ok((listener, file))
}

/// Listen on `addr`. The socket takes SO_REUSEADDR, so a restarted server
/// binds the same port at once.
pub(super) fn bind_tcp(addr: SocketAddr) -> io::Result<TcpListener> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    socket.listen(1024)
}

/// Accept clients until `stopping` tells that the server stops. Then drop
/// the listener, so the server accepts no more connections. Each connection
/// takes frames of at most
/// `max_frame_size` bytes, closes after `idle_timeout` without a frame and
/// without a call that runs, and
/// counts in `tasks` while it runs. With `tls`, each connection on TCP
/// starts with the TLS handshake.
pub(super) async fn accept(
    listener: Listener,
    storages: Arc<Storages>,
    max_frame_size: u32,
    idle_timeout: Option<Duration>,
    mut stopping: Stopping,
    tasks: Arc<Tasks>,
    tls: Option<Arc<Tls>>,
) {
    loop {
        let accepted = tokio::select! {
            biased;
            () = stopping.wait() => break,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok(Accepted::Unix(stream, path)) => {
                let connection = protocol::serve_unix_connection(
                    stream,
                    path,
                    Arc::clone(&storages),
                    max_frame_size,
                    idle_timeout,
                    stopping.clone(),
                );
                spawn_counted(&tasks, connection);
            }
            Ok(Accepted::Tcp(stream, peer)) => {
                let _ = stream.set_nodelay(true);
                let storages = Arc::clone(&storages);
                let stopping = stopping.clone();
                match &tls {
                    Some(tls) => spawn_counted(
                        &tasks,
                        serve_tls(
                            stream,
                            peer.to_string(),
                            Arc::clone(tls),
                            storages,
                            max_frame_size,
                            idle_timeout,
                            stopping,
                        ),
                    ),
                    None => spawn_counted(
                        &tasks,
                        protocol::serve_connection(
                            stream,
                            storages,
                            max_frame_size,
                            idle_timeout,
                            stopping,
                        ),
                    ),
                }
            }
            Err(error) => {
                tracing::warn!("accept failed: {error}");
                tokio::task::yield_now().await;
            }
        }
    }
}

/// Spawn `connection` on tokio, and count it in `tasks` while it runs. The
/// count starts before the spawn, so a stop waits for the connection.
fn spawn_counted(tasks: &Arc<Tasks>, connection: impl Future<Output = ()> + Send + 'static) {
    let running = tasks.start();
    tokio::spawn(async move {
        connection.await;
        drop(running);
    });
}

/// Run the TLS handshake of `tls` on `stream`, then serve the client
/// `peer` as `protocol::serve_connection` does. The handshake must end
/// within `idle_timeout`, as the protocol of a plain client must. A stop
/// ends it at once. A failed handshake closes only this connection.
async fn serve_tls(
    stream: TcpStream,
    peer: String,
    tls: Arc<Tls>,
    storages: Arc<Storages>,
    max_frame_size: u32,
    idle_timeout: Option<Duration>,
    mut stopping: Stopping,
) {
    let handshake = tokio::select! {
        biased;
        () = stopping.wait() => return,
        handshake = tls.accept(stream) => handshake,
        () = protocol::idle(&Tokio, idle_timeout) => {
            tracing::info!(client = peer.as_str(), "no TLS handshake within the idle timeout: closing the connection");
            return;
        }
    };
    let (stream, read_only) = match handshake {
        Ok(accepted) => accepted,
        Err(error) => {
            tracing::warn!(client = peer.as_str(), "the TLS handshake failed: {error}");
            return;
        }
    };
    let socket = stream.get_ref().0.as_raw_fd();
    let connection =
        Connection::tokio(peer, socket, max_frame_size, idle_timeout).read_only(read_only);
    let (reader, writer) = tokio::io::split(stream);
    protocol::serve_stream(reader, writer, connection, storages, stopping).await;
}

/// The count of the tasks that serve: the accept loop and one task for
/// each connection. A stop waits until no task runs.
#[derive(Default)]
pub(super) struct Tasks {
    running: Mutex<usize>,
    ended: Condvar,
}

/// The mark of a task that runs. Its drop counts the end of the task. When
/// the stop of the runtime drops the task, it drops the mark too.
pub(super) struct Running(Arc<Tasks>);

impl Tasks {
    /// Count a task that starts.
    pub(super) fn start(self: &Arc<Self>) -> Running {
        *self.lock() += 1;
        Running(Arc::clone(self))
    }

    /// Wait at most `timeout` until no task runs. Return the number of
    /// tasks that still run.
    pub(super) fn wait(&self, timeout: Duration) -> usize {
        let (running, _) = self
            .ended
            .wait_timeout_while(self.lock(), timeout, |running| *running > 0)
            .unwrap_or_else(PoisonError::into_inner);
        *running
    }

    fn lock(&self) -> MutexGuard<'_, usize> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let mut running = self.0.lock();
        *running -= 1;
        if *running == 0 {
            self.0.ended.notify_all();
        }
    }
}
