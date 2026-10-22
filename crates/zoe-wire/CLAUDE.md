# zoe-wire

The bytes and the names of the ZEO 6 msgpack protocol that `zoe serve` (`zoe-server`) and its clients (`zoe-client`, `zoe-py`, `zoe-bench`) share, and the reader of TLS certificates that the server and the client share. ADR 0055 records the decision.

## Layout

- `src/lib.rs`: the constants of the protocol (`PROTOCOL`, `ERROR`, `HEARTBEAT`, `REPLY`, `TRACEPARENT`, `MAX_TID`), and an oid or a tid on the wire: `p64` gives the bin of 8 big-endian bytes, as `ZODB.utils.p64`; `u64_of` reads a bin or a str of 8 bytes, as `ZODB.utils.u64`.
- `src/frame.rs`: the frame, a 4-byte big-endian length and the payload: `header`, `encode`, `length`, and `RESERVE`, the largest buffer that a read reserves before the bytes arrive.
- `src/msgpack.rs`: the head of a bin (written and read), the head of an array (read), and the big-endian read of a length.
- `src/class.rs`: the class names of the exceptions that the replies of `zoe serve` name.
- `src/extension.rs`: the names of the ZOE extensions.

## Invariants

- The crate is sans-IO. It reads and writes bytes in memory. It opens no socket, reads no stream, starts no thread and has no async function. It never depends on tokio, also not behind a feature. The read loops of the frames stay in `zoe-server` and `zoe-client`.
- It depends on no crate of ZOE. `zoe-core` never depends on it. `MAX_TID` is a `u64`, so a caller writes `Tid(MAX_TID)`. `MAX_TID` is the `maxtid` of ZODB (`i64::MAX`), not `Tid::MAX` of the core.
- `zoe-server` takes it only under its feature `serve`. `cargo tree -p zoe-py -e normal` lists it, lists tokio through `zoe-client`, and does not list rmcp.
- Each byte that it writes is a byte of the wire protocol. A change to one is a change of the wire protocol: stop and ask.
- Each caller maps the errors of the crate to its own error, with its own text.
- The tests of the server and of the client keep their own codecs. A test that built its frames with this crate would not see a change of it.
