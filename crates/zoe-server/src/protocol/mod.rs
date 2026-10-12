//! The protocol layer: msgpack frames, the handshake and the method table.
//!
//! Each connection has one tokio task that reads its frames, one task that
//! writes them, and one thread of the blocking pool that runs its calls in
//! order. That thread runs every core call of the connection, so one
//! thread drives each core transaction, and no core call runs on a thread
//! of the async runtime.
//!
//! The frames for a client wait in the outbox of its connection, which
//! holds at most `client-queue-size` frames. The session waits for room for
//! its replies. The invalidations of the storage never wait: a full outbox
//! closes the connection.

mod clock;
mod extension;
pub(crate) mod frame;
mod invalidation;
mod message;
mod session;
mod status;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::SystemTime;

use tokio::net::TcpStream;
use tokio::sync::Notify;
use zoe_core::Store;

use invalidation::{Invalidations, Outbox};
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

/// The bounds of the queues of a storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Queues {
    /// The most transactions that `getInvalidations` covers. Past it, the
    /// reply is nil.
    pub(crate) invalidations: usize,
    /// The most messages that wait for one client. Past it, the server
    /// closes the connection.
    pub(crate) client: usize,
}

impl Queues {
    /// The bounds of a server that gets no option: 100,000 transactions
    /// and 1,000 messages.
    pub(crate) const DEFAULT: Self = Self {
        invalidations: 100_000,
        client: 1_000,
    };
}

/// A store that the server serves under a name, with its counters and its
/// invalidations. All the connections of the store share it.
pub(crate) struct NamedStorage {
    /// The name that a client gives to `register`.
    name: String,
    /// The directory of the store.
    path: PathBuf,
    store: Arc<Store>,
    stats: Stats,
    /// The publication of the commits to the connections.
    invalidations: Invalidations,
    queues: Queues,
}

impl NamedStorage {
    /// Serve `store`, in the directory `path`, under the name `name`, with
    /// the bounds `queues`. The status starts now.
    pub(crate) fn new(name: &str, path: &Path, store: Arc<Store>, queues: Queues) -> Self {
        Self {
            name: name.to_owned(),
            path: path.to_path_buf(),
            invalidations: Invalidations::new(store.last_transaction()),
            store,
            stats: Stats::new(SystemTime::now()),
            queues,
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
    let (outbox, mut frames, mut closing) = Outbox::new(storage.queues.client);
    let closed = Arc::new(Notify::new());
    let worker = {
        let closed = Arc::clone(&closed);
        tokio::task::spawn_blocking(move || {
            session::run(storage, received, outbox);
            closed.notify_one();
        })
    };
    let writing = {
        let mut closing = closing.clone();
        tokio::spawn(async move {
            // The writer stops at once when the server closes the
            // connection. Otherwise it sends every frame, until the session
            // ends.
            loop {
                let frame = tokio::select! {
                    biased;
                    () = closing.wait() => break,
                    frame = frames.recv() => match frame {
                        Some(frame) => frame,
                        None => break,
                    },
                };
                let written = tokio::select! {
                    biased;
                    () = closing.wait() => break,
                    written = frame::write(&mut writer, frame.as_slice()) => written,
                };
                if written.is_err() {
                    break;
                }
            }
            // A session that waits for room in the outbox stops.
            drop(frames);
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut writer).await;
        })
    };

    loop {
        let read = tokio::select! {
            read = frame::read(&mut reader) => read,
            () = closed.notified() => break,
            () = closing.wait() => break,
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

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpStream as Client;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use rmpv::Value;
    use zoe_core::Tid;

    use super::invalidation::{Commit, Others};
    use super::*;

    fn read_frame(client: &mut Client) -> Vec<u8> {
        let mut len = [0u8; 4];
        client.read_exact(&mut len).unwrap();
        let mut payload = vec![0u8; u32::from_be_bytes(len) as usize];
        client.read_exact(&mut payload).unwrap();
        payload
    }

    fn write_frame(client: &mut Client, payload: &[u8]) {
        client.write_all(&frame::encode(payload).unwrap()).unwrap();
    }

    #[test]
    fn a_connection_that_the_hub_closes_ends_with_its_session() {
        let dir = std::env::temp_dir().join(format!("zoe-protocol-close-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(Store::open(&dir).unwrap());
        let storage = Arc::new(NamedStorage::new("1", &dir, store, Queues::DEFAULT));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let served = Arc::clone(&storage);
        runtime.spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_connection(stream, served).await;
        });

        let mut client = Client::connect(addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        assert_eq!(read_frame(&mut client), PROTOCOL);
        write_frame(&mut client, PROTOCOL);
        let register = Value::Array(vec![
            Value::from(1),
            Value::Boolean(false),
            Value::from("register"),
            Value::Array(vec![Value::from("1"), Value::Boolean(false)]),
        ]);
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &register).unwrap();
        write_frame(&mut client, &bytes);
        assert_eq!(
            read_frame(&mut client),
            message::reply(Value::from(1), Value::Binary(vec![0; 8]))
        );
        assert_eq!(storage.stats.connections.load(Ordering::Relaxed), 1);

        // A commit whose oids are unknown closes every other connection.
        let (outbox, _frames, _closing) = Outbox::new(1);
        storage.invalidations.publish(Commit {
            previous: Tid(0),
            tid: Tid(1),
            origin: None,
            outbox,
            own: Vec::new(),
            others: Others::Close,
        });
        // The client reads the end of the connection, and the session ends.
        let mut rest = Vec::new();
        let read = client.read_to_end(&mut rest);
        assert!(
            read.is_ok()
                || read
                    .as_ref()
                    .is_err_and(|e| e.kind() == io::ErrorKind::ConnectionReset),
            "{read:?}"
        );
        assert!(rest.is_empty());
        let deadline = Instant::now() + Duration::from_secs(30);
        while storage.stats.connections.load(Ordering::Relaxed) > 0 {
            assert!(Instant::now() < deadline, "the session did not end");
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(runtime);
    }
}
