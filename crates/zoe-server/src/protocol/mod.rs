//! The protocol layer: msgpack frames, the handshake and the method table.
//!
//! Each connection has one tokio task that reads its frames, one task that
//! writes them, and one thread of the blocking pool that runs its calls in
//! order. That thread runs every core call of the connection, so one
//! thread drives each core transaction, and no core call runs on a thread
//! of the async runtime.

mod extension;
pub(crate) mod frame;
mod message;
mod session;

use std::sync::{Arc, mpsc};

use tokio::net::TcpStream;
use tokio::sync::{Notify, mpsc as async_mpsc};
use zoe_core::Store;

/// The protocol that the server announces and the only one it takes: ZEO 6
/// with msgpack.
pub(crate) const PROTOCOL: &[u8] = b"M5";

/// The name of the storage that the server serves. It is the default of
/// ZEO.
pub(crate) const STORAGE_NAME: &str = "1";

/// Serve one client on `stream` until it closes, or until the server
/// closes the connection.
pub(crate) async fn serve_connection(stream: TcpStream, store: Arc<Store>, name: String) {
    let peer = stream
        .peer_addr()
        .map_or_else(|_| "a client".to_owned(), |addr| addr.to_string());
    let (mut reader, mut writer) = stream.into_split();
    if frame::write(&mut writer, PROTOCOL).await.is_err() {
        return;
    }
    match frame::read(&mut reader).await {
        Ok(Some(protocol)) if protocol == PROTOCOL => {}
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
            session::run(store, name, received, send);
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
