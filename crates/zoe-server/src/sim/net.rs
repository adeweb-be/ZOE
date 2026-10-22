//! The simulated network: TCP connections between a client and the
//! server, with a delay on each frame, seeded losses, partitions and
//! closes.
//!
//! A connection keeps the order of its bytes, as TCP does. A lost frame
//! breaks the connection: both ends see a reset, as when TCP gives up. A
//! partition cuts one client from the server for a time: its frames
//! arrive after the partition ends, as TCP sends them again, and its
//! connects time out. A close ends both directions of a connection after
//! the bytes in flight, as a proxy that closes it does. The network
//! traces each frame with its method.

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use rmpv::Value;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::server;
use super::world::{NetFault, Owner, Shared, State, Timer, sleep};

/// The longest time that a connect waits for a server that a partition
/// cuts off.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// The shortest and the longest partition.
const PARTITION: (Duration, Duration) = (Duration::from_millis(100), Duration::from_secs(3));

/// One direction of a connection.
#[derive(Default)]
pub(crate) struct Pipe {
    buf: VecDeque<u8>,
    eof: bool,
    reader: Option<Waker>,
    /// The arrival time of the last bytes, so that the bytes keep their
    /// order.
    last: Duration,
    /// The bytes of the frame that the sender writes, for the trace.
    framing: Vec<u8>,
}

impl Pipe {
    /// Add `bytes` to the frame that the sender writes. Return the name of
    /// each frame that they complete.
    fn frames(&mut self, bytes: &[u8]) -> Vec<String> {
        self.framing.extend_from_slice(bytes);
        let mut names = Vec::new();
        while let Some(head) = self.framing.first_chunk::<4>() {
            let len = zoe_wire::frame::length(*head) as usize;
            let Some(payload) = self.framing.get(4..4 + len) else {
                break;
            };
            names.push(name(payload));
            self.framing.drain(..4 + len);
        }
        names
    }
}

/// Return the name of a frame in the trace: the method of a message, or
/// the text of a protocol frame.
fn name(payload: &[u8]) -> String {
    if let Ok(Value::Array(parts)) = rmpv::decode::read_value(&mut &payload[..])
        && let [id, flags, method, _] = parts.as_slice()
        && let Some(method) = method.as_str()
    {
        return match (id.as_i64(), flags.as_i64()) {
            (Some(-1), _) => "heartbeat".to_owned(),
            (_, Some(2)) => format!("{method} error"),
            _ => method.to_owned(),
        };
    }
    if payload.len() <= 8 && payload.iter().all(u8::is_ascii_alphanumeric) {
        return String::from_utf8_lossy(payload).into_owned();
    }
    "?".to_owned()
}

/// A connection between a client and one generation of the server.
pub(crate) struct Conn {
    pub(crate) client: usize,
    pub(crate) generation: u32,
    to_client: Pipe,
    to_server: Pipe,
    /// A reset broke the connection.
    pub(crate) reset: bool,
    /// The network closed the connection: a write fails, and a read ends
    /// after the bytes in flight.
    closed: bool,
}

impl Conn {
    fn new(client: usize, generation: u32) -> Self {
        Self {
            client,
            generation,
            to_client: Pipe::default(),
            to_server: Pipe::default(),
            reset: false,
            closed: false,
        }
    }

    fn pipe(&mut self, to_client: bool) -> &mut Pipe {
        if to_client {
            &mut self.to_client
        } else {
            &mut self.to_server
        }
    }

    /// Return the wakers of the readers of the connection.
    fn wakers(&mut self) -> Vec<Waker> {
        [self.to_client.reader.take(), self.to_server.reader.take()]
            .into_iter()
            .flatten()
            .collect()
    }

    /// Tell if the connection still carries bytes.
    pub(crate) fn open(&self) -> bool {
        !self.reset && !self.closed
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

/// Break `conn`: both ends see a reset. Return the wakers to call.
pub(crate) fn reset(state: &mut State, id: u64) -> Vec<Waker> {
    let Some(conn) = state.conns.get_mut(&id) else {
        return Vec::new();
    };
    if conn.reset {
        return Vec::new();
    }
    conn.reset = true;
    conn.wakers()
}

/// Return the time when a frame that a connection of `client` sends now
/// arrives, after `latency`. A frame of a client that a partition cuts
/// arrives after the partition ends.
fn arrival(state: &State, client: usize, latency: Duration) -> Duration {
    let at = state.now + latency;
    match state.partitioned(client) {
        Some(until) => at.max(until + latency),
        None => at,
    }
}

/// Start the faults that the plan puts at the frame `frame`.
fn start_faults(state: &mut State, frame: u64) {
    for fault in state.fault_plan.remove(&frame).unwrap_or_default() {
        match fault {
            NetFault::Partition => partition(state),
            NetFault::Close => close(state),
        }
    }
}

/// Cut one client, chosen by the seed, from the server for a time.
fn partition(state: &mut State) {
    let clients = state.clients.max(1) as u64;
    let client = state.rng.below(clients) as usize;
    let length = state.rng.between(PARTITION.0, PARTITION.1);
    let until = state.now + length;
    let until = state
        .partitioned(client)
        .map_or(until, |end| end.max(until));
    state.partitions.insert(client, until);
    state.trace(format!(
        "net cuts c{client} from the server for {} ms",
        (until - state.now).as_millis()
    ));
    state.at(until, Timer::Heal { client });
}

/// Close one open connection, chosen by the seed.
fn close(state: &mut State) {
    let open: Vec<u64> = state
        .conns
        .iter()
        .filter(|(_, conn)| conn.open())
        .map(|(id, _)| *id)
        .collect();
    if open.is_empty() {
        return;
    }
    let id = open[state.rng.below(open.len() as u64) as usize];
    let now = state.now;
    let latency = state.latency();
    let Some(conn) = state.conns.get_mut(&id) else {
        return;
    };
    conn.closed = true;
    let client = conn.client;
    let mut ends = Vec::new();
    for to_client in [true, false] {
        let pipe = conn.pipe(to_client);
        let at = (now + latency).max(pipe.last);
        pipe.last = at;
        ends.push((at, to_client));
    }
    for (at, to_client) in ends {
        state.at(
            at,
            Timer::Eof {
                conn: id,
                to_client,
            },
        );
    }
    state.trace(format!("net closes connection {id} of c{client}"));
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
        let Some(conn) = state.conns.get_mut(&self.conn) else {
            return;
        };
        let client = conn.client;
        let names = conn.pipe(to_client).frames(&bytes);
        let (from, to) = if to_client {
            ("s".to_owned(), format!("c{client}"))
        } else {
            (format!("c{client}"), "s".to_owned())
        };
        let mut planned = Vec::new();
        for name in names {
            state.frames += 1;
            planned.push(state.frames);
            state.trace(format!("net {from}>{to} {name}"));
        }
        let latency = state.latency();
        let lost = {
            let loss = state.faults.loss;
            state.rng.chance(loss)
        };
        let arrives = arrival(state, client, latency);
        let Some(conn) = state.conns.get_mut(&self.conn) else {
            return;
        };
        let pipe = conn.pipe(to_client);
        let at = arrives.max(pipe.last);
        pipe.last = at;
        let timer = if lost {
            state.trace(format!(
                "net loses a frame of connection {} of c{client}",
                self.conn
            ));
            Timer::Reset { conn: self.conn }
        } else {
            Timer::Deliver {
                conn: self.conn,
                to_client,
                bytes,
            }
        };
        state.at(at, timer);
        for frame in planned {
            start_faults(state, frame);
        }
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
            Some(conn) if conn.reset => {
                return Poll::Ready(Err(io::ErrorKind::ConnectionReset.into()));
            }
            Some(conn) if conn.closed => {
                return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
            }
            Some(_) => {}
            None => return Poll::Ready(Err(io::ErrorKind::ConnectionReset.into())),
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
        let Some(client) = state.conns.get(&self.conn).map(|conn| conn.client) else {
            return Poll::Ready(Ok(()));
        };
        let arrives = arrival(&state, client, latency);
        if let Some(conn) = state.conns.get_mut(&self.conn) {
            let pipe = conn.pipe(to_client);
            let at = arrives.max(pipe.last);
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

/// Connect `client` to the server. The server gets a connection task of
/// its current generation, as an accept gives it.
pub(crate) async fn connect(shared: &Arc<Shared>, client: usize) -> io::Result<SimStream> {
    let delay = shared.lock().latency();
    sleep(shared, delay).await;
    let cut = {
        let state = shared.lock();
        state.partitioned(client).map(|until| until - state.now)
    };
    if let Some(left) = cut {
        sleep(shared, left.min(CONNECT_TIMEOUT)).await;
        shared.lock().trace(format!("c{client} connect times out"));
        return Err(io::ErrorKind::TimedOut.into());
    }
    let mut state = shared.lock();
    let storages = match state.server.storages.clone() {
        Some(storages) if state.server.up => storages,
        _ => {
            state.trace(format!("c{client} connect refused"));
            return Err(io::ErrorKind::ConnectionRefused.into());
        }
    };
    let generation = state.server.generation;
    let conn = state.new_conn(Conn::new(client, generation));
    state.trace(format!("c{client} connects: connection {conn}"));
    let end = |client_side| SimStream {
        shared: Arc::clone(shared),
        conn,
        client_side,
    };
    let task = server::accept(shared, client, conn, generation, end(false), storages);
    state.spawn(&shared.ready, Owner::Server(generation), task);
    Ok(end(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(payload: &[u8]) -> Vec<u8> {
        crate::protocol::frame::encode(payload).unwrap()
    }

    fn message(id: i64, flags: i64, method: &str) -> Vec<u8> {
        let message = Value::Array(vec![
            Value::from(id),
            Value::from(flags),
            Value::from(method),
            Value::Array(Vec::new()),
        ]);
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &message).unwrap();
        bytes
    }

    #[test]
    fn the_trace_names_each_frame_that_the_bytes_complete() {
        let mut pipe = Pipe::default();
        let bytes = [
            frame(b"M5"),
            frame(&message(1, 0, "register")),
            frame(&message(-1, 0, ".reply")),
            frame(&message(2, 2, ".reply")),
        ]
        .concat();
        // The bytes come in two writes that cut a frame.
        let (first, second) = bytes.split_at(9);
        assert_eq!(pipe.frames(first), ["M5"]);
        assert_eq!(
            pipe.frames(second),
            ["register", "heartbeat", ".reply error"]
        );
        assert!(pipe.framing.is_empty());
        assert_eq!(name(&[0xff, 0x00, 0x01]), "?");
    }
}
