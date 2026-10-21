# 0055. zoe-wire: the ZEO frame codec and the TLS certificate reader, written once

Status: proposed. The checkpoint of step 1 of ZOE-260. It adds a crate name, so the human approves it before any code moves. It changes no byte on the wire, no on-disk format and no public Python API. Section "Questions for the human" lists the choices that the card left open; each one has a recommendation.

## Context

`zoe-server` and `zoe-client` speak the two sides of the ZEO 6 msgpack protocol. Each crate has its own codec: the codec of the server is private to its crate, and `crates/zoe-client/CLAUDE.md` says that the client keeps its own. The audit of the cleanup campaign found the same bytes, names and readers in both crates, and some of them in `zoe-py`, `zoe-bench` and the simulator too. The human decided: "also a shared zoe-wire crate".

The copies on `main` at ff1f8fdf:

| What | zoe-server | zoe-client | Elsewhere |
| --- | --- | --- | --- |
| The frame: `RESERVE`, the encode, the read of the length | `src/protocol/frame.rs:9-60` (52 lines) | `src/protocol/mod.rs:23-26, 62-92, 312-320` (44 lines) | `zoe-server/src/sim/net.rs:51-53`, the trace of the simulator |
| The msgpack heads of a bin and of an array, and the big-endian read of a length | `src/protocol/message.rs:165-178, 290-296` (21 lines) | `src/protocol/mod.rs:270-301` (32 lines) | |
| The protocol `M5` | `src/protocol/mod.rs:70` | `src/protocol/mod.rs:21` | |
| The flags of an error reply, the id of the heartbeat, `.reply`, the key `traceparent` | `src/protocol/message.rs:13-20`; `.reply` at 147, 164 and 214 | `src/protocol/mod.rs:32-33, 254, 322-323, 368, 451` | |
| The class names of the exceptions | `src/protocol/session.rs:74-86`, and 11 literals of `Fault` from line 166 to line 346 | `src/protocol/mod.rs:35-39, 516`; `src/pool.rs:750` | |
| The names of the extensions | `src/protocol/negotiation.rs:26, 34, 46, 50, 54` | `src/client.rs:1784-1793` | `zoe-py/src/client.rs:106-107`; `zoe-bench/src/hot.rs:179` |
| `MAX_TID`, the `maxtid` of ZODB | | `src/protocol/mod.rs:28-30` | `zoe-py/src/client.rs:103-104` |
| An oid or a tid as a bin of 8 bytes | `src/protocol/session.rs:2572-2575` (`p64`), `2610-2618` (`u64_of`) | `src/protocol/mod.rs:554-563` (`bin8`, `p64`) | |
| The PEM loader `certificates` | `src/tls.rs:164-176` (13 lines) | `src/tls.rs:375-387` (13 lines) | |
| `Pinned`, and the check of its validity period | `src/tls.rs:216-239, 283-296` (38 lines) | `src/tls.rs:244-264, 305-318` (35 lines) | |
| The DER reader and its date arithmetic | `src/tls.rs:327-452` (126 lines) | `src/tls/der.rs:1-214` (214 lines) | |

What drifted:

- The two functions `p64` do opposite things. The `p64` of the server gives the bin of the 8 big-endian bytes of a number, as `ZODB.utils.p64` does. The `p64` of the client reads the number of a bin of 8 bytes, as `ZODB.utils.u64` does. The client calls its encode `bin8`. `zoe-py` uses `p64` and `u64_of` as the server does, on Python bytes (`src/errors.rs:237-242`).
- The two decodes take different values. The `p64` of the client reads a bin or a str of 8 bytes (`as_slice` of rmpv). The `u64_of` of the server reads a bin only, and its `serial_of` also takes a str whose UTF-8 bytes are 8 bytes.
- The flags of an error reply are a `u8` on the server and an `i64` on the client.
- A payload longer than `u32::MAX` bytes fails with two texts: "frame too long" on the server, and "a frame of N bytes" on the client.
- The big-endian read of a cut length gives 0 on the server (`length`) and `None` on the client (`be`).
- The names of the extensions are in four crates. The server serves five. `zoe-client` names four of them, and `server_writes` comes only from `zoe/zeo.py`.
- The DER reader has two copies. 92 lines are byte for byte the same (server `tls.rs:361-452`, client `tls/der.rs:123-214`): the read of a TLV, the read of a time, and the date arithmetic. The copy of the client also reads the common names of the subject and the subjectAltName extension. The copy of the server reads only the validity period.

A fix in one copy does not reach the other copy.

## Decision

### 1. A new crate, `zoe-wire`

`zoe-wire` holds the bytes and the names of ZEO 6 that both sides of the protocol use, and the reader of TLS certificates that both sides use. Its users are `zoe-server` (feature `serve`), `zoe-client`, `zoe-py` and `zoe-bench`.

### 2. The crate is sans-IO

`zoe-wire` reads and writes bytes in memory. It opens no socket, reads no stream, starts no thread and has no async function. It does not depend on tokio, and no feature of it adds tokio. One function reads files: the PEM loader. It reads the certificate files of the configuration once, when TLS loads, and never a byte of a connection.

### 3. The read loop stays in the callers

*Claude's choice.* The card asks whether the async read loop goes into `zoe-wire` behind a feature, or stays in the callers. It stays in the callers, for four reasons:

- The two loops differ in policy, not in bytes. The server refuses a frame longer than `--max-frame-size` before it reserves memory. The client has no limit, and it reads a long reply in two parts, so that the bytes of a bin go once from the socket into their own buffer, with the spare buffers of `recycle`.
- What the loops share is about 20 lines on each side: the read of the 4 bytes of the length, and the read of the payload into a buffer that reserves at most `RESERVE`. `zoe-wire` gives them `frame::length` and `RESERVE`. So the bytes and the bound are written once, and only the loop stays on each side.
- A feature `tokio` puts tokio into the one crate whose rule is "no I/O". It also gives the crate two builds to test, and it changes the line of the root `CLAUDE.md` that names each crate that uses tokio.
- Without tokio, a caller without a runtime can use the crate: the trace of the simulator splits frames out of a buffer (`sim/net.rs:51-53`), and a fuzz target can call the crate directly.

### 4. What moves in

| Module | Item | It replaces |
| --- | --- | --- |
| `zoe_wire` | `PROTOCOL` (`M5`); `ERROR` (the flags 2 of an error reply, one type for both sides); `HEARTBEAT` (the id -1); `REPLY` (`.reply`); `TRACEPARENT`; `MAX_TID` (the `maxtid` of ZODB, `i64::MAX`, as a `u64`) | the constants of both sides, of `zoe-py` and of the literals of the client |
| `zoe_wire` | `p64(n) -> Value` and `u64_of(&Value) -> Option<u64>` (section 5) | `p64` and `u64_of` of the server; `bin8` and `p64` of the client |
| `zoe_wire::frame` | `RESERVE` (1 MiB); the 4 big-endian bytes of a length, which fail over `u32::MAX`; the encode of a frame; the length that 4 bytes give | `encode` and the length of `read` and `write_parts` of the server; `RESERVE`, `read_len` and `frame` of the client; the split of the simulator trace |
| `zoe_wire::msgpack` | the head of a bin for a length (bin 8, bin 16 or bin 32); the read of the head of an array and of a bin; the big-endian read of a number of 1, 2 or 4 bytes | the bin head of `load_reply` and `length` of the server; `array_len`, `bin_len` and `be` of the client |
| `zoe_wire::class` | the 12 class names that the replies of `zoe serve` name and that the client reads or builds: `ConflictError`, `ReadConflictError`, `POSKeyError`, `ReadOnlyError`, `StorageError`, `StorageSystemError`, `StorageTransactionError` and `MultipleUndoErrors` of `ZODB.POSException`; `ValueError`, `TypeError`, `KeyError` and `Exception` of `builtins` | the constants and the literals of `Fault`; `CONFLICT`, `READ_CONFLICT` and the literals of the client |
| `zoe_wire::extension` | `SEQUENCES`, `KV`, `LOCK_INTENT`, `TRACE_CONTEXT`, `SERVER_WRITES` | the names in `negotiation.rs`, `client.rs`, `zoe-py` and `zoe-bench` |
| `zoe_wire::tls` | `certificates(path)`, the PEM loader: one certificate at least, and an error that names the file | `certificates` of both sides |
| `zoe_wire::tls` | `Pinned<P>`: the certificates of a CA file that a peer certificate matches byte for byte, each with what a reader of its DER gave; `Validity`, with `check(now)`, which gives the `CertificateError` of rustls for a certificate that is not valid yet or that expired | `Pinned`, `pinned` and `pinned_of`, and the two copies of the check of the period |
| `zoe_wire::tls::der` | one TLV reader and one date arithmetic, with two readers: `validity`, which reads what the server reads today, and `parts`, which reads what the client reads today (the validity period, the common names of the subject, the subjectAltName extension) | `tls.rs:327-452` of the server and `tls/der.rs` of the client |

Each side keeps its reading of a certificate: the server pins with `validity`, and the client with `parts`. So no certificate pins on a side where it did not pin before.

Each side maps the errors of `zoe-wire` to its own error, with the text of today.

### 5. Two clear names for an oid or a tid on the wire

*Claude's choice.* `p64(n)` gives the bin of the 8 big-endian bytes of `n`, as `ZODB.utils.p64` does. `u64_of(value)` gives the number in 8 big-endian bytes, as `ZODB.utils.u64` does: it reads a bin or a str of 8 bytes, as the client reads them today, and gives `None` for another value. These are the names of the server and of `zoe-py` today. So the calls of the server (about 300) keep their name, and only the client renames (about 80 calls): its `bin8` becomes `p64`, and its `p64` becomes `u64_of`.

The server keeps its own checks on top of `u64_of`, with the texts of its `TypeError`s: an oid or a tid must be a bin ("not bytes", "not 8 bytes"), and a serial can also be a str (`serial_of`). So neither side reads a value in a different way.

### 6. Dependencies

- `rmpv`, for `p64` and `u64_of`, whose type is the `Value` of both sides.
- `rustls`, for the PEM reader of `pki_types`, `CertificateDer`, `UnixTime` and `CertificateError`.

Both come from the workspace. `zoe-server` with `serve`, `zoe-client` and `zoe-py` link both today. So no tree gains a crate other than `zoe-wire`.

`zoe-wire` depends on no crate of ZOE. It is a leaf. So `zoe-core` never depends on it, and it never depends on `zoe-core`. That is why `MAX_TID` is a `u64`: a caller writes `Tid(MAX_TID)`.

`zoe-server` takes `zoe-wire` only under its feature `serve`. The build of `zoe-server` without `serve`, which `zoe-py` uses, does not link it through `zoe-server`. `zoe-py` and `zoe-bench` depend on `zoe-wire` directly, for `MAX_TID` and the names of the extensions. So `cargo tree -p zoe-py -e normal` lists tokio through `zoe-client`, lists `zoe-wire`, and does not list rmcp (ADR 0035).

### 7. What stays out

- The sessions of the server and of the client. ZOE-261 and ZOE-262 hold them.
- The decoders of messages. The server decodes calls from a client that it does not trust, with its bounds (`MAX_VALUES`, `MAX_DEPTH`, the scan of `check_values`). The client decodes replies and calls of the server, and reads the head of a long reply (`lead`). The two decoders read different messages in different directions.
- The read loops (section 3), and the vectored write of the server (`write_parts`).
- The rustls verifiers (`ClientCa` of the server, `ServerCa` and `ChainOnly` of the client), `TlsOptions` and `Tls` of each side, and the handshakes. Each one is the policy of its side.
- The test of the client for `TypeError` and `ReadOnlyError` by the last part of the class name: a stock server can name any module.
- The methods of each extension (`negotiation::SERVED`). That table is the gate of the server.
- `Tid::MAX` of `zoe-core`, `u64::MAX`. It is a bound of the core. The `maxtid` of ZODB, `i64::MAX`, goes on the wire. These are two values with two purposes.
- The codecs of the tests: `crates/zoe-server/tests/wire/mod.rs` and the helpers of `crates/zoe-client/tests/`. They are a second copy of the bytes, on purpose: a test that builds its frames with `zoe-wire` does not see a change of `zoe-wire`. The rules of the campaign also forbid a change to what a test checks.
- The names of the extensions in `zoe/zeo.py`. `zoe-wire` is Rust, and the public Python API does not change.

## Consequences

- The workspace has 12 crates. The "Names" line of the root `CLAUDE.md` adds `zoe-wire`, in this pull request.
- The line of `crates/zoe-client/CLAUDE.md` that says that the client keeps its own codec changes, in this pull request: the client takes the shared bytes and names from `zoe-wire`. The line about `tls/der.rs` changes when the DER reader moves (step 3).
- The lines of `crates/zoe-server/CLAUDE.md` that describe `src/protocol/` and `src/tls.rs` name `zoe-wire` where the code moved.
- `crates/zoe-wire/CLAUDE.md` holds the invariants of the crate: sans-IO; no tokio; no crate of ZOE as a dependency; a change of a byte that it writes is a change of the wire protocol, so it is a stop-and-ask.
- The workspace `Cargo.toml` adds `zoe-wire`. `Cargo.lock` and `crates/zoe-server/fuzz/Cargo.lock` add the package. `just check` checks that the lock of the fuzz targets needs no change.
- The fuzz targets do not change. `frame` enters through `zoe_server::fuzz::read_messages`, which reads the length with `zoe-wire`, and `handshake` reaches it through the server. `just fuzz 600` is a check of the card. A target for the DER reader, or for the head of a long reply, would add a test, so it is not part of this card.
- No byte on the wire changes. The tests of the wire contract of the specification (for example `crates/zoe-server/tests/frames.rs`, `load_bytes.rs` and `zeo_compatibility.rs`, and the tests of the bytes in `crates/zoe-client/tests/`) and the conformance suites with the stock and the native client check it. They do not change.
- The unit tests of the code that moves go with it, with their names and their assertions. The count of tests does not drop: two tests have the same name (`the_days_count_the_leap_years`), and each one stays, in its own module.

## Alternatives

- **Keep the copies.** They already drifted (see Context), and a fix in one copy does not reach the other.
- **Put the code in `zoe-core`.** The core is the synchronous storage engine, and the wire is not its concern. The core would link `rmpv` and `rustls`, and each user of the core (the in-process storage, `zoe-convert`, Loupe) would link a TLS stack that it does not use.
- **Put the code in `zoe-client`, and let the server depend on it.** The `zoe` binary would link the client: its tokio tasks, its cache with its `unsafe` code, and `rustls-native-certs`, none of which the server runs.
- **Put the read loop into `zoe-wire` behind a feature `tokio`.** See section 3: tokio in a crate without I/O, two builds to test, and a change to the tokio line of the root `CLAUDE.md`, for about 20 lines on each side.

## Questions for the human

1. **The read loop.** In the callers, with the length and `RESERVE` from `zoe-wire` (section 3), or in `zoe-wire` behind a feature `tokio`? Recommendation: in the callers.
2. **The names of the oid and tid helpers.** `p64` and `u64_of`, the names of ZODB, of the server and of `zoe-py` (section 5), or two new names such as `to_bin8` and `from_bin8`, which change about 390 calls instead of about 80? Recommendation: `p64` and `u64_of`.
3. **Two constants more than the card names.** The card names `M5`, `RESERVE`, `ERROR` and `TRACEPARENT`. This ADR also moves `HEARTBEAT` and `REPLY`, which both sides write. Recommendation: move them.
4. **Two files outside the scope fence.** The "Rust crates" row of the naming table of the specification lists the crates, and `crates/zoe-server/CLAUDE.md` describes the moved code. Recommendation: update both in this pull request.
