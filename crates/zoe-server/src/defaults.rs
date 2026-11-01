//! The defaults of `zoe serve` that the server (`serve`) and the command
//! line (`cli`) share. The module compiles without the feature `serve`, so
//! the command line of each build has the same defaults.

/// The most transactions that `getInvalidations` covers.
pub(crate) const INVALIDATION_QUEUE_SIZE: usize = 100_000;

/// The most invalidations that wait for one client.
pub(crate) const CLIENT_QUEUE_SIZE: usize = 1_000;

/// The longest time that a stop takes, in whole seconds.
pub(crate) const SHUTDOWN_TIMEOUT: &str = "30";

/// The longest time that a connection sends no frame while no call of the
/// connection runs, in whole seconds. A stock client sends a heartbeat
/// every 60 seconds, so it keeps its connection.
pub(crate) const IDLE_TIMEOUT: &str = "300";

/// A store is served for writes.
pub(crate) const READ_ONLY: bool = false;

/// A pack collects garbage.
pub(crate) const PACK_GC: bool = true;
