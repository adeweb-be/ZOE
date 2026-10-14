//! A Rust client for the ZEO protocol, with the ZOE extensions. The native
//! Python client `zoe.zeo.ClientStorage` and the server mode of Loupe use it.
//!
//! The client speaks the ZEO 6 msgpack protocol as `ZEO.ClientStorage`
//! does. Its methods are `async fn` on tokio's I/O traits. It reaches the
//! network, its tasks and the time only through a `Runtime`, so a
//! simulator can run it.

mod client;
mod error;
mod protocol;
pub mod runtime;

pub use client::{
    BLOB_CHUNK, Client, HEARTBEAT, HistoryEntry, Invalidation, Invalidations, Options, Record,
    Transaction, traced,
};
pub use error::Error;
pub use runtime::{Runtime, Tokio};
