//! A Rust client for the ZEO protocol, with the ZOE extensions. The native
//! Python client `zoe.zeo.ClientStorage` and the server mode of Loupe use it.
//!
//! The client speaks the ZEO 6 msgpack protocol as `ZEO.ClientStorage`
//! does, over TCP or a unix domain socket, with TLS or without. Its methods
//! are `async fn` on tokio's I/O traits. It reaches the network, its tasks
//! and the time only through a `Runtime`, so a simulator can run it.
//!
//! `Pool` runs several transactions at once on a pool of connections, and
//! delivers the tids of its listener connection in one order (ADR 0036).
//! It can hold a `Cache`, the client cache of ADR 0034, with the semantics
//! of the cache of ZEO. A pool of `Pool::with_reconnect` connects again
//! after a lost connection, and verifies its cache, as `ZEO.ClientStorage`
//! does.

mod cache;
mod client;
mod endpoint;
mod error;
mod pool;
mod protocol;
pub mod runtime;
mod tls;

pub use cache::{Cache, CacheCounts, Copier};
pub use client::{
    BLOB_CHUNK, Client, HEARTBEAT, HistoryEntry, Invalidation, Invalidations, Listed, Options,
    Record, Transaction, Voted, traced,
};
pub use endpoint::{Address, Endpoint};
pub use error::Error;
pub use pool::{
    Connect, Counters, Delivery, Dropping, Event, FileCheck, Finished, Open, Pool, PoolIntents,
    PoolTransaction, REFUSED_POLL, Reconnect, Turn, Verification,
};
pub use protocol::recycle;
pub use runtime::{Runtime, Tokio};
pub use tls::{Tls, TlsOptions};
