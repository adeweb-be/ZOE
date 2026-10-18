//! The runtime of the simulation, and the accept of a connection.
//!
//! The same runtime serves the server and the clients: its tasks are tasks
//! of the driver, its blocking threads run under the baton, and its sleeps
//! run on the simulated clock. A connection of the server runs
//! `protocol::serve_stream`, the code that `zoe serve` runs for each
//! connection, and the clients run `zoe-client`.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use super::net::{self, SimStream};
use super::world::{self, Owner, Shared, Task};
use crate::protocol::{self, Connection, Stopping, Storages};
use crate::runtime::{BoxFuture, Runtime};
use crate::serve::{DEFAULT_IDLE_TIMEOUT, DEFAULT_MAX_FRAME_SIZE};

/// The runtime of one actor of the simulation.
pub(crate) struct SimRuntime {
    shared: Arc<Shared>,
    owner: Owner,
    /// The client of the actor, or of the connection of the server.
    client: Option<usize>,
    /// The connection of the server that the actor serves.
    conn: Option<u64>,
}

impl SimRuntime {
    /// Return the runtime of `client`.
    pub(crate) fn client(shared: &Arc<Shared>, client: usize) -> Self {
        Self {
            shared: Arc::clone(shared),
            owner: Owner::Client(client),
            client: Some(client),
            conn: None,
        }
    }

    fn spawn_task(&self, task: Task) {
        let mut state = self.shared.lock();
        state.spawn(&self.shared.ready, self.owner, task);
    }

    fn sleep_for(&self, duration: Duration) -> BoxFuture<()> {
        Box::pin(world::sleep(&self.shared, duration))
    }
}

impl Runtime for SimRuntime {
    fn spawn(&self, task: BoxFuture<()>) {
        self.spawn_task(task);
    }

    fn spawn_blocking(&self, work: Box<dyn FnOnce() + Send>) {
        let Owner::Server(generation) = self.owner else {
            unreachable!("only the server starts a blocking thread");
        };
        world::spawn_thread(&self.shared, generation, self.client, self.conn, work);
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<()> {
        self.sleep_for(duration)
    }

    fn now(&self) -> Duration {
        self.shared.now()
    }
}

impl zoe_client::Runtime for SimRuntime {
    fn connect(
        &self,
        _addr: SocketAddr,
    ) -> zoe_client::runtime::BoxFuture<io::Result<Box<dyn zoe_client::runtime::Stream>>> {
        let shared = Arc::clone(&self.shared);
        let client = self.client.expect("a client connects");
        Box::pin(async move {
            let stream = net::connect(&shared, client).await?;
            Ok(Box::new(stream) as Box<dyn zoe_client::runtime::Stream>)
        })
    }

    fn spawn(&self, task: zoe_client::runtime::BoxFuture<()>) {
        self.spawn_task(task);
    }

    fn sleep(&self, duration: Duration) -> zoe_client::runtime::BoxFuture<()> {
        self.sleep_for(duration)
    }
}

/// Return the task that serves the connection `conn` of `client` to the
/// server of `generation` on `stream`, as the accept loop of `zoe serve` starts
/// it, with the default bounds of `zoe serve`.
pub(crate) fn accept(
    shared: &Arc<Shared>,
    client: usize,
    conn: u64,
    generation: u32,
    stream: SimStream,
    storages: Arc<Storages>,
) -> Task {
    let runtime = SimRuntime {
        shared: Arc::clone(shared),
        owner: Owner::Server(generation),
        client: Some(client),
        conn: Some(conn),
    };
    let connection = Connection {
        peer: format!("c{client}"),
        runtime: Arc::new(runtime),
        max_frame_size: DEFAULT_MAX_FRAME_SIZE.get(),
        idle_timeout: Some(DEFAULT_IDLE_TIMEOUT),
        read_only: false,
        // The stream of the simulator has no socket.
        socket: None,
    };
    // No stop of the server comes: the simulator stops a server by a
    // crash or at the end of a run.
    let (_, never) = Stopping::new();
    Box::pin(protocol::serve_stream(
        stream.clone(),
        stream,
        connection,
        storages,
        never,
    ))
}
