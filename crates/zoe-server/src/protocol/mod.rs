//! The protocol layer: msgpack frames, the handshake and the method table.
//!
//! Each connection has one tokio task that reads its frames, one task that
//! writes them, and one thread of the blocking pool that runs its calls in
//! order. That thread runs every core call of the connection, so one
//! thread drives each core transaction, and no core call runs on a thread
//! of the async runtime.

mod clock;
mod extension;
pub(crate) mod frame;
pub(crate) mod message;
pub(crate) mod session;
mod status;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::SystemTime;

use tokio::net::TcpStream;
use tokio::sync::{Notify, mpsc as async_mpsc};
use zoe_core::Store;

use status::Stats;

/// The protocol that the server announces and the only one it takes: ZEO 6
/// with msgpack.
pub(crate) const PROTOCOL: &[u8] = b"M5";

/// The payload that a client sends in place of a protocol to ask for the
/// status of the server.
const RUOK: &[u8] = b"ruok";

/// The name of the storage that the server serves. It is the default of
/// ZEO.
pub(crate) const STORAGE_NAME: &str = "1";

/// A store that the server serves under a name, with its counters. All the
/// connections of the store share it.
pub(crate) struct NamedStorage {
    /// The name that a client gives to `register`.
    name: String,
    /// The directory of the store.
    path: PathBuf,
    store: Arc<Store>,
    stats: Stats,
}

impl NamedStorage {
    /// Serve `store`, in the directory `path`, under the name `name`. The
    /// status starts now.
    pub(crate) fn new(name: &str, path: &Path, store: Arc<Store>) -> Self {
        Self {
            name: name.to_owned(),
            path: path.to_path_buf(),
            store,
            stats: Stats::new(SystemTime::now()),
        }
    }

    /// Return the status of the storage. It reads the store.
    fn status(&self) -> status::Status {
        self.stats.status(self.store.last_transaction())
    }

    /// Return the size of the files in the store directory, in bytes, as
    /// `zoe.Storage.getSize` gives it. The blob directory is not counted.
    fn size(&self) -> io::Result<u64> {
        let mut size = 0;
        for entry in fs::read_dir(&self.path)? {
            let metadata = entry?.metadata()?;
            if metadata.is_file() {
                size += metadata.len();
            }
        }
        Ok(size)
    }
}

/// Serve one client on `stream` until it closes, or until the server
/// closes the connection.
pub(crate) async fn serve_connection(stream: TcpStream, storage: Arc<NamedStorage>) {
    let peer = stream
        .peer_addr()
        .map_or_else(|_| "a client".to_owned(), |addr| addr.to_string());
    let (mut reader, mut writer) = stream.into_split();
    if frame::write(&mut writer, PROTOCOL).await.is_err() {
        return;
    }
    match frame::read(&mut reader).await {
        Ok(Some(protocol)) if protocol == PROTOCOL => {}
        Ok(Some(protocol)) if protocol == RUOK => {
            // The status reads the store, so it runs on the blocking pool.
            let reply = tokio::task::spawn_blocking(move || {
                status::ruok(&[(storage.name.as_str(), storage.status())])
            })
            .await;
            if let Ok(reply) = reply {
                let _ = frame::write(&mut writer, reply.as_bytes()).await;
            }
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut writer).await;
            return;
        }
        Ok(Some(_)) => {
            eprintln!("zoe serve: {peer}: the client does not speak the protocol M5");
            return;
        }
        _ => return,
    }

    let (calls, received) = mpsc::channel();
    let (send, mut outgoing) = async_mpsc::unbounded_channel::<Vec<u8>>();
    let closed = Arc::new(Notify::new());
    let worker = {
        let closed = Arc::clone(&closed);
        tokio::task::spawn_blocking(move || {
            session::run(storage, received, |bytes| send.send(bytes).is_ok());
            closed.notify_one();
        })
    };
    let writing = tokio::spawn(async move {
        while let Some(bytes) = outgoing.recv().await {
            if frame::write(&mut writer, &bytes).await.is_err() {
                break;
            }
        }
        // The session ended: the server closes the connection.
        let _ = tokio::io::AsyncWriteExt::shutdown(&mut writer).await;
    });

    loop {
        let read = tokio::select! {
            read = frame::read(&mut reader) => read,
            () = closed.notified() => break,
        };
        let bytes = match read {
            Ok(Some(bytes)) => bytes,
            Ok(None) => break,
            Err(error) => {
                eprintln!("zoe serve: {peer}: {error}");
                break;
            }
        };
        let call = match message::decode(&bytes) {
            Ok(call) => call,
            Err(error) => {
                eprintln!("zoe serve: {peer}: a frame is not a call: {error}");
                break;
            }
        };
        if call.is_heartbeat() {
            continue;
        }
        if calls.send(call).is_err() {
            break;
        }
    }
    // The session ends when its channel closes. Dropping its transaction
    // aborts it.
    drop(calls);
    let _ = worker.await;
    let _ = writing.await;
}
