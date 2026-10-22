//! The names of the ZOE extensions. A ZOE client declares them in the
//! third argument of `register`, and `get_info` gives back the names that
//! the server accepted.

/// The sequences: `sequence_next(name)`.
pub const SEQUENCES: &str = "sequences";

/// The KV namespaces: three reads, and two writes in the transaction of
/// the connection.
pub const KV: &str = "kv";

/// The early write-intent locks: `lock_intent(oids)` and `unlock_intent()`.
pub const LOCK_INTENT: &str = "lock_intent";

/// The trace context of a call: a fifth element, a map with the W3C
/// `traceparent` of the client.
pub const TRACE_CONTEXT: &str = "trace_context";

/// The vote of the connection can write a record that its transaction did
/// not store (ADR 0049).
pub const SERVER_WRITES: &str = "server_writes";
