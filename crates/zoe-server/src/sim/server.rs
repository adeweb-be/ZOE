//! The server side of a simulated connection.
//!
//! The connection task does what `protocol::serve_connection` does: the
//! handshake, a session thread that runs the calls in order, a writer
//! task for the replies, and a read loop. The session thread runs the
//! session of the server, `protocol::session::run`, unchanged.

use std::future::poll_fn;
use std::sync::Arc;
use std::task::Poll;

use tokio::io::AsyncWriteExt;

use super::net::SimStream;
use super::world::{self, Owner, Resume, Shared, Why};
use crate::protocol::message::{self, Call};
use crate::protocol::{NamedStorage, PROTOCOL, frame, session};

/// Serve one client on `stream` until the connection ends.
pub(crate) async fn serve_conn(stream: SimStream, storage: Arc<NamedStorage>) {
    let shared = Arc::clone(&stream.shared);
    let conn = stream.conn;
    let mut reader = stream.clone();
    let mut writer = stream;
    if frame::write(&mut writer, PROTOCOL).await.is_err() {
        return;
    }
    match frame::read(&mut reader).await {
        Ok(Some(protocol)) if protocol == PROTOCOL => {}
        _ => return,
    }
    let (thread, client) = {
        let mut state = shared.lock();
        let Some(client) = state.conns.get(&conn).map(|conn| conn.client) else {
            return;
        };
        let body_shared = Arc::clone(&shared);
        let thread = world::spawn_session(&mut state, client, move |id| {
            let calls = Calls {
                shared: Arc::clone(&body_shared),
                thread: id,
            };
            let replies = Arc::clone(&body_shared);
            session::run(storage, calls, move |bytes| {
                let mut state = replies.lock();
                match state.conns.get_mut(&conn) {
                    Some(conn) if !conn.reset => {
                        conn.replies.push_back(bytes);
                        if let Some(waker) = conn.replies_waker.take() {
                            waker.wake();
                        }
                        true
                    }
                    _ => false,
                }
            });
            let mut state = body_shared.lock();
            if let Some(conn) = state.conns.get_mut(&conn) {
                conn.replies_closed = true;
                if let Some(waker) = conn.replies_waker.take() {
                    waker.wake();
                }
            }
        });
        if let Some(conn) = state.conns.get_mut(&conn) {
            conn.thread = Some(thread);
        }
        let generation = state.server.generation;
        let writing = Box::pin(write_replies(Arc::clone(&shared), conn, writer));
        state.spawn(&shared.ready, Owner::Server(generation), writing);
        (thread, client)
    };
    loop {
        let bytes = match frame::read(&mut reader).await {
            Ok(Some(bytes)) => bytes,
            _ => break,
        };
        let call = match message::decode(&bytes) {
            Ok(call) => call,
            Err(_) => break,
        };
        if call.is_heartbeat() {
            continue;
        }
        let mut state = shared.lock();
        state.trace(format!("s  <c{client} {}", call.method));
        match state.threads.get_mut(&thread) {
            Some(session) if !session.closed => session.inbox.push_back(call),
            _ => break,
        }
    }
    // The session ends when its inbox closes. Dropping its transaction
    // aborts it.
    if let Some(session) = shared.lock().threads.get_mut(&thread) {
        session.closed = true;
    }
}

/// Write the replies of the session thread of `conn` until it ends, then
/// close the connection.
async fn write_replies(shared: Arc<Shared>, conn: u64, mut writer: SimStream) {
    loop {
        let next = poll_fn(|cx| {
            let mut state = shared.lock();
            let Some(conn) = state.conns.get_mut(&conn) else {
                return Poll::Ready(None);
            };
            if let Some(bytes) = conn.replies.pop_front() {
                return Poll::Ready(Some(bytes));
            }
            if conn.replies_closed || conn.reset {
                return Poll::Ready(None);
            }
            conn.replies_waker = Some(cx.waker().clone());
            Poll::Pending
        })
        .await;
        let Some(bytes) = next else { break };
        if frame::write(&mut writer, &bytes).await.is_err() {
            return;
        }
    }
    let _ = writer.shutdown().await;
}

/// The calls of a session thread. When its inbox is empty, the thread
/// gives the baton back and waits.
struct Calls {
    shared: Arc<Shared>,
    thread: u64,
}

impl Iterator for Calls {
    type Item = Call;

    fn next(&mut self) -> Option<Call> {
        loop {
            {
                let mut state = self.shared.lock();
                let session = state.threads.get_mut(&self.thread)?;
                if let Some(call) = session.inbox.pop_front() {
                    return Some(call);
                }
                if session.closed {
                    return None;
                }
            }
            if world::give_back(Why::Idle) == Resume::Killed {
                return None;
            }
        }
    }
}
