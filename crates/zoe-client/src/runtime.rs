//! The network and the time of the client.
//!
//! The client reaches the network, its tasks and the time only through
//! the trait `Runtime`. `Tokio` is the runtime of production. A simulator
//! gives its own runtime, with its own streams and its own clock, so the
//! same client code runs on a simulated network and time.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};

/// A future that a runtime gives or runs.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// A byte stream to the server.
pub trait Stream: AsyncRead + AsyncWrite + Send + Unpin + 'static {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Stream for T {}

/// The network, the tasks and the time of a client.
pub trait Runtime: Send + Sync + 'static {
    /// Open a stream to `addr`.
    fn connect(&self, addr: SocketAddr) -> BoxFuture<io::Result<Box<dyn Stream>>>;

    /// Run `task` until it ends. The client does not wait for the task.
    fn spawn(&self, task: BoxFuture<()>);

    /// Return a future that ends after `duration`.
    fn sleep(&self, duration: Duration) -> BoxFuture<()>;
}

/// The runtime of production: TCP and the tasks and timers of the current
/// tokio runtime.
#[derive(Clone, Copy, Debug, Default)]
pub struct Tokio;

impl Runtime for Tokio {
    fn connect(&self, addr: SocketAddr) -> BoxFuture<io::Result<Box<dyn Stream>>> {
        Box::pin(async move {
            let stream = tokio::net::TcpStream::connect(addr).await?;
            // A call is a small frame. Send it at once.
            stream.set_nodelay(true)?;
            Ok(Box::new(stream) as Box<dyn Stream>)
        })
    }

    fn spawn(&self, task: BoxFuture<()>) {
        tokio::spawn(task);
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<()> {
        Box::pin(tokio::time::sleep(duration))
    }
}
