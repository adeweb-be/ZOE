# zoe-server

The `zoe` command: the binary `zoe`, its subcommands, and the `zoe serve` process, a tokio server that speaks the ZEO 6 msgpack protocol. Specification: "Server mode", the naming table, F8, F14 and F26. ADR 0028 records the decisions of the command.

## Layout

- `src/main.rs`: the binary `zoe`. It only calls `cli::main` and exits with its code.
- `src/cli/`: `zoe_server::cli`, the command line. `mod.rs` parses and runs, `convert.rs` holds `zoe convert`, `pack.rs` holds `zoe pack`.
- `src/cli/serve.rs`: `zoe serve`. Without the feature `serve` it fails with 1 and names the `zoe` binary.
- `src/cli/mcp.rs`: `zoe mcp`. It calls `zoe-mcp`. Without the feature `serve` it fails with 1 and names the `zoe` binary.
- `src/serve.rs`: `zoe_server::serve::{Server, ServeOptions}`, the server as a library. The binary and the Rust tests run it.
- `src/protocol/`: the frames, the messages, the extension pickle, the session of a connection, the status of a storage and the times that the protocol sends. It compiles only with the feature `serve`.

## Invariants of the command

- One binary, `zoe`, in this crate. The console script `zoe` of the `zoedb` wheel calls `zoe_server::cli::main` through `zoe._zoe.main`. So both run the same code. Never add a second parser.
- `cli::main(args: Vec<OsString>) -> i32` takes the program name first and returns the exit code. It never exits the process and never panics on a user error.
- The subcommands and their names follow the naming table of the specification.
- Every option has a `ZOE_*` environment twin in SCREAMING\_SNAKE: the option name without `--`, in capitals, with `_` for `-`. The help of the option names its twin. An option on the command line wins over its twin.
- The store is `--path` (`ZOE_PATH`) in every subcommand. The blob directory of a `Data.fs` is `--data-fs-blob-dir` (`ZOE_DATA_FS_BLOB_DIR`).
- The exit code is 0 on success, 1 on a failure, and 2 on a usage error. A conversion whose report lists a problem fails with 1, and so does the `Mismatch` of `zoe-convert`.
- The report goes to stdout. A message goes to stderr, and starts with `zoe <subcommand>:`. The help and the version go to stdout with the code 0. A reader that closes stdout before the end of the report, as `head` does, does not fail the command, because the work is done. A conversion that does not verify still fails.
- The command parses all its arguments before it opens a file. A usage error changes nothing on disk.
- `cli::main` flushes stdout before it returns, because the Python process does not flush the stdout of Rust when it ends.
- `cli::main` does not hold the lock of stdout. `zoe mcp` writes MCP messages to stdout from another thread, and writes no report.
- A command that only reads opens the store with `Store::open_read_only`: `zoe convert --to-data-fs`, `zoe pack --dry-run`, and `zoe mcp` through Loupe's `Folder`. A command that writes to an existing store refuses a directory without `zoe.redb` and creates nothing: `zoe pack`. Only `zoe convert --from-data-fs` creates a store. Before it opens the store, it checks that the `Data.fs` is a regular file that starts with `FS30`, so a wrong source creates no store.
- `--days N` gives the tid of the time of the clock minus N days of 86,400 seconds, by `zoe_core::timestamp`. The command reads the time only through the `Clock` trait, so a test gives its own time. `cli::main` uses the system clock.

## Invariants of the server

- This crate uses tokio only under the feature `serve`: the server code, and `zoe-mcp` for `zoe mcp`. The feature is on by default. `zoe-py` depends on this crate without it, so the extension never links tokio or rmcp: `cargo tree -p zoe-py -e normal` must list neither.
- The server has no Python inside and needs no Plone eggs. It ships as one static binary.
- Core calls run on the blocking pool, never on the async runtime threads. Each connection runs its calls in order on one thread of the pool, so one thread drives each core transaction and the core never answers `WrongThread`.
- The server announces `M5` and takes only `M5`. A frame is a 4-byte big-endian length, then the bytes. A reply carries the message id of its call, whatever its msgpack type. The id -1 is a heartbeat and gets no reply. An error goes out with the flags 2 and `[class, args]`, with the class names of ZODB's exceptions. An error of a call without a reply closes the connection, as in ZEO.
- A frame with the id -1 is a heartbeat whatever its other parts: it gets no reply and changes nothing.
- The decode of a frame has a depth bound of 128 steps of rmpv, about 60 levels of arrays or maps. A deeper frame closes its connection, and the server stays up. A stock server takes deeper frames, but a ZEO message nests at most about 3 levels, and a transaction extension nests as deep as its dict.
- `tpc_finish` replies only after the core made the commit durable.
- A client can send `ruok` in place of `M5`. It needs no `register` and is not a connection of the status. The server sends `M5`, then one JSON payload `{storage id: status}` as Python's `json.dumps` writes it, then closes. The status reads the store on the blocking pool.
- `server_status` and the `ruok` reply have the 13 keys of ZEO 6's server, in its order. All the connections of a storage share its counters through `NamedStorage`, and the session counts each call where ZEO counts it. The store has no storage-wide commit lock: `lock_time` is nil, `waiting` is 0 and `timeout-thread-is-alive` is `"stub"`.
- `get_info` says `supportsUndo` false and `supports_record_iternext` true. Its `interfaces` are those of `zoe.Storage`, in `__iro__` order: never `IStorageUndoable` or `IExternalGC`. Its `length` is 0, as `len(zoe.Storage)`, and its `size` is the size of the files in the store directory. `get_size_info` gives the same `length` and `size`; a stock ZEO server does not answer it.
- `history` is newest first, with at most `size` entries. A size of 0 or less gives none. The `time` of an entry is `TimeStamp(tid).timeTime()` to the last bit.
- stdout carries only the lines `admin <address>` and `listening <address>`. The logs go to stderr.
- SIGTERM closes the store and exits with 0. `Server::stop` and the drop of a `Server` close it too: they drop the runtime, which waits for the session threads, and then the store.
- An unmodified `ZEO.ClientStorage` works. A stock client never sees an extension that it did not declare in `register`.
- Every reply carries the message id of its request. Concurrent requests on one connection are not served yet: each connection runs its calls in order.
- The invalidation queue of a slow client is bounded (default 1,000 messages). On overflow the server closes that connection.
- `getInvalidations(tid)` returns `None` when the gap is longer than `invalidation-queue-size`.
- Commits on different named storages never wait for each other.
- A read-only storage refuses `tpc_begin` with `ReadOnlyError`.
- `storeBlobShared` is accepted only with `shared-blob-dir` on and a path inside the server's blob directory.
- On SIGTERM: in-flight `tpc_finish` calls complete, transactions in `Staged` or `Locked` abort, every client gets a clean close, within `shutdown-timeout`.
- A change to the wire protocol is a stop-and-ask.

## Tests

- `tests/cli.rs` is the acceptance test of the binary and of `cli::main`.
- `tests/serve.rs` and `python/zoedb/tests/server/test_walking_skeleton.py` are the acceptance tests of `zoe serve`. `tests/wire/mod.rs` is a raw ZEO 6 client and a `zoe serve` process for the Rust tests of the server; it is part of the acceptance tests. The Python tests find the binary through `ZOE_BINARY`, which `just test` sets. `python/zoedb/tests/acceptance/test_console_script.py` is the acceptance test of the console script.
- `tests/mcp.rs` is the acceptance test of `zoe mcp`. It compiles only with the feature `serve`.
