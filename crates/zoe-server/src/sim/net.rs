//! The simulated network: TCP connections between a client and the
//! server, with a delay on each frame and seeded losses.
//!
//! A connection keeps the order of its bytes, as TCP does. A lost frame
//! breaks the connection: both ends see a reset, as when TCP gives up.

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::server;
use super::world::{Owner, Shared, State, Timer, sleep};

/// One direction of a connection.
#[derive(Default)]
pub(crate) struct Pipe {
    buf: VecDeque<u8>,
    eof: bool,
    reader: Option<Waker>,
    /// The time of the last delivery, so that the bytes keep their order.
    last: Duration,
}

/// A connection between a client and one generation of the server.
pub(crate) struct Conn {
    pub(crate) client: usize,
    pub(crate) generation: u32,
    to_client: Pipe,
    to_server: Pipe,
    pub(crate) reset: bool,
    /// The replies that the session thread gives to the writer task.
    pub(crate) replies: VecDeque<Vec<u8>>,
    pub(crate) replies_closed: bool,
    pub(crate) replies_waker: Option<Waker>,
    /// The session thread of the connection, once the handshake is done.
    pub(crate) thread: Option<u64>,
}

impl Conn {
    fn new(client: usize, generation: u32) -> Self {
        Self {
            client,
            generation,
            to_client: Pipe::default(),
            to_server: Pipe::default(),
            reset: false,
            replies: VecDeque::new(),
            replies_closed: false,
            replies_waker: None,
            thread: None,
        }
    }

    fn pipe(&mut self, to_client: bool) -> &mut Pipe {
        if to_client {
            &mut self.to_client
        } else {
            &mut self.to_server
        }
    }

    /// Return the wakers of everything that waits on the connection.
    pub(crate) fn wakers(&mut self) -> Vec<Waker> {
        [
            self.to_client.reader.take(),
            self.to_server.reader.take(),
            self.replies_waker.take(),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

/// Deliver `bytes` to the reader of one direction of `conn`.
pub(crate) fn deliver(
    state: &mut State,
    conn: u64,
    to_client: bool,
    bytes: Vec<u8>,
) -> Option<Waker> {
    let conn = state.conns.get_mut(&conn)?;
    if conn.reset {
        return None;
    }
    let pipe = conn.pipe(to_client);
    pipe.buf.extend(bytes);
    pipe.reader.take()
}

/// End one direction of `conn`.
pub(crate) fn eof(state: &mut State, conn: u64, to_client: bool) -> Option<Waker> {
    let conn = state.conns.get_mut(&conn)?;
    let pipe = conn.pipe(to_client);
    pipe.eof = true;
    pipe.reader.take()
}

/// Break `conn`: both ends see a reset, and its session thread gets no
/// more calls. Return the wakers to call.
pub(crate) fn reset(state: &mut State, id: u64) -> Vec<Waker> {
    let Some(conn) = state.conns.get_mut(&id) else {
        return Vec::new();
    };
    if conn.reset {
        return Vec::new();
    }
    conn.reset = true;
    let wakers = conn.wakers();
    if let Some(thread) = conn.thread
        && let Some(thread) = state.threads.get_mut(&thread)
    {
        thread.closed = true;
    }
    wakers
}

/// One end of a connection.
#[derive(Clone)]
pub(crate) struct SimStream {
    pub(crate) shared: Arc<Shared>,
    pub(crate) conn: u64,
    pub(crate) client_side: bool,
}

impl SimStream {
    /// Send `bytes` to the other end, after a delay. A lost frame breaks
    /// the connection at the time that it would arrive.
    fn send(&self, state: &mut State, bytes: Vec<u8>) {
        let to_client = !self.client_side;
        let latency = state.latency();
        let lost = {
            let loss = state.faults.loss;
            state.rng.chance(loss)
        };
        let now = state.now;
        let Some(conn) = state.conns.get_mut(&self.conn) else {
            return;
        };
        let pipe = conn.pipe(to_client);
        let at = (now + latency).max(pipe.last);
        pipe.last = at;
        let timer = if lost {
            Timer::Reset { conn: self.conn }
        } else {
            Timer::Deliver {
                conn: self.conn,
                to_client,
                bytes,
            }
        };
        state.at(at, timer);
    }
}

impl AsyncRead for SimStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut state = self.shared.lock();
        let Some(conn) = state.conns.get_mut(&self.conn) else {
            return Poll::Ready(Err(io::ErrorKind::ConnectionReset.into()));
        };
        if conn.reset {
            return Poll::Ready(Err(io::ErrorKind::ConnectionReset.into()));
        }
        let pipe = conn.pipe(self.client_side);
        if !pipe.buf.is_empty() {
            let n = pipe.buf.len().min(buf.remaining());
            let bytes: Vec<u8> = pipe.buf.drain(..n).collect();
            buf.put_slice(&bytes);
            return Poll::Ready(Ok(()));
        }
        if pipe.eof {
            return Poll::Ready(Ok(()));
        }
        pipe.reader = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl AsyncWrite for SimStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut state = self.shared.lock();
        match state.conns.get(&self.conn) {
            Some(conn) if !conn.reset => {}
            _ => return Poll::Ready(Err(io::ErrorKind::ConnectionReset.into())),
        }
        self.send(&mut state, buf.to_vec());
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut state = self.shared.lock();
        let to_client = !self.client_side;
        let latency = state.latency();
        let now = state.now;
        if let Some(conn) = state.conns.get_mut(&self.conn) {
            let pipe = conn.pipe(to_client);
            let at = (now + latency).max(pipe.last);
            pipe.last = at;
            state.at(
                at,
                Timer::Eof {
                    conn: self.conn,
                    to_client,
                },
            );
        }
        Poll::Ready(Ok(()))
    }
}

/// Connect `client` to the server. The server end gets a connection task
/// of the current server generation, as an accept would give it.
pub(crate) async fn connect(shared: &Arc<Shared>, client: usize) -> io::Result<SimStream> {
    let delay = shared.lock().latency();
    sleep(shared, delay).await;
    let mut state = shared.lock();
    let Some(storage) = state.server.storage.clone().filter(|_| state.server.up) else {
        state.trace(format!("c{client} connect refused"));
        return Err(io::ErrorKind::ConnectionRefused.into());
    };
    let generation = state.server.generation;
    let conn = state.new_conn(Conn::new(client, generation));
    state.trace(format!("c{client} connected (connection {conn})"));
    let server_end = SimStream {
        shared: Arc::clone(shared),
        conn,
        client_side: false,
    };
    let task = Box::pin(server::serve_conn(server_end, storage));
    state.spawn(&shared.ready, Owner::Server(generation), task);
    Ok(SimStream {
        shared: Arc::clone(shared),
        conn,
        client_side: true,
    })
}
