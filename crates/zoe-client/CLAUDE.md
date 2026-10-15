# zoe-client

A Rust client for the ZEO protocol, with the ZOE extensions. `zoe.zeo.ClientStorage`, `zoe-bench`, the server mode of Loupe, `zoe mcp` against a server and the simulator use it. Specification: "Async and free-threading", F2 (client side), F28 and N18.

## Layout

- `src/client.rs`: `Client`, `Options`, `Transaction`, `Invalidations`, and the three tasks of a connection (reader, writer, heartbeat).
- `src/protocol/`: the frames and the messages of ZEO 6 with msgpack. It has its own codec: the codec of `zoe-server` is private to that crate.
- `src/runtime.rs`: the trait `Runtime` (connect, spawn, sleep) and `Tokio`, its implementation for production.
- `src/error.rs`: `Error`.

## Invariants

- The client speaks the same ZEO 6 msgpack protocol as `ZEO.ClientStorage`: `M5`, then `register(storage, read_only)`, then calls with int ids from 1 and the flags `false`, and calls without a reply with the id 0 and the flags `true`. `loadBefore` has an int id too, not the `[oid, tid]` id of a stock client: the server echoes any id. `Client::connect` registers with `false` and two arguments.
- `Client::open` declares `Options::extensions` in a third argument of `register`, then calls `get_info`. A stock server refuses the third argument with `TypeError` (any module, such as `builtins.TypeError`) and keeps the connection open and unregistered: the client then registers again with two arguments. With no extension in `Options`, `register` has two arguments. `Client::info` gives the reply of `get_info`, and `Client::extensions` the names of its key `extensions` that the client declared.
- The client uses an extension only after the server accepted it in `get_info`. `sequence_next` needs `sequences`: without it, it fails with `Error::Unsupported` and sends nothing.
- Each reply goes to its call by its message id, whatever the order of the replies. A reply with an id that no call waits for is dropped.
- A lost connection fails every call that waits with `Error::Disconnected`, and every later call too. The end of the stream, an error of the stream, a frame that does not decode, an `invalidateTransaction` in a bad shape, `Client::close` and the drop of the `Client` all close the connection.
- The client reaches the network, its tasks and the time only through `Runtime`: never `tokio::net`, `tokio::spawn` or `tokio::time` outside `Tokio`. It uses tokio's I/O traits and `tokio::sync`. Every `select!` is `biased;`, so a simulator runs it in one order.
- The client sends a heartbeat `[-1, 0, ".reply", nil]` every 60 seconds (`HEARTBEAT`), busy or idle, as a stock client does. So the `--idle-timeout` of `zoe serve` never closes it.
- One transaction at a time for each client, as the commit lock of `ZEO.ClientStorage`. A vote that fails, by an error or by a conflict that the vote lists, sends `tpc_abort` and waits for its reply, because the server keeps the locks of a listed conflict. A `Transaction` dropped before the server confirmed `finish` or `abort` sends `tpc_abort` and does not wait, so a cancelled `vote` or `abort` still releases the locks. It keeps the commit lock of the client until its `tpc_abort` is in the outgoing channel, also when the channel is full, so the `tpc_begin` of the next transaction never reaches the server before that abort. After a failed vote or an abort, `store`, `vote` and `finish` fail with `Error::Invalid` and send nothing: `zoe serve` closes the connection on a `storea` for a transaction that is not open.
- `Transaction::store_blob` uploads a blob as `ZEO.ClientStorage` does without a shared blob directory: `storea`, `storeBlobStart`, one `storeBlobChunk` per chunk of `BLOB_CHUNK` (59,000) bytes, and `storeBlobEnd`, all without a reply. The client downloads no blob.
- The client resolves no conflict. A `ConflictError` and a conflict that the vote lists are `Error::Conflict`. A `ReadConflictError` with an oid, as the vote gives after `Transaction::check_current_serial`, is `Error::ReadConflict`. Every other error of the server is `Error::Server` with its class; its message is empty without an argument, the text of its one argument, or else the value.
- `Client::begin` sends `tpc_begin` with the tid nil and the status `" "`. `Client::begin_with` sends a tid and a status: a transaction with a tid commits under it.
- `Transaction::vote_resolved` gives the oids that a stock server lists as resolved: their pickle on the server is not the pickle of the client. `vote` drops them.
- `Client::disconnect` closes the connection without waiting for its tasks, so a caller that does not own the `Client` can close it. `Client::is_connected` is false once the connection closed.
- `last_transaction` only goes up: it takes the maximum of the tid of `register`, of each invalidation and of each commit of the client.
- The invalidations go to a broadcast channel of 1,024 entries. A receiver that falls further behind gets `Error::Lagged`, and must treat its cache as stale. A consumer subscribes first, then reads `last_transaction`: the reader raises `last_transaction` before it sends the invalidation. A commit of the client gives no invalidation to the client, as from ZEO. `Invalidations::try_recv` reads without a wait. The reader dispatches the frames in order, so when the reply of `tpc_finish` comes back, each invalidation that the server sent before it is in the channel.
- tokio is allowed in this crate. `zoe-py` depends on it for the native client of `zoe.zeo.ClientStorage` (ADR 0035), so `cargo tree -p zoe-py -e normal` lists tokio. This crate never depends on rmcp.
- `zoe.zeo.ClientStorage` is a drop-in for `ZEO.ClientStorage`, never a requirement.
- A change to the wire protocol is a stop-and-ask.

## Tests

- `tests/client.rs` is the acceptance test, against `zoe_server::serve::Server` in the test process.
- `tests/runtime.rs` runs the client on a scripted server through a `Runtime` of the test: replies out of order, a lost connection, a bad frame, a listed conflict, a dropped transaction, the heartbeat, the declaration of `Client::open` and its fallback to two arguments, a read conflict, the resolved oids of a vote and `disconnect`.
- `crates/zoe-server/tests/simulation.rs` and `crates/zoe-server/tests/simulated_kill_loop.rs` run the client under the deterministic simulator of `zoe-server`, through its `Runtime`, against the server code, with network faults, blobs and crashes of the server.
