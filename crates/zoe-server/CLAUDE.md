# zoe-server

The `zoe serve` process: a tokio server that speaks the ZEO 6 msgpack protocol. Specification: "Server mode".

## Invariants

- This is the only crate that uses tokio.
- The server has no Python inside and needs no Plone eggs. It ships as one static binary.
- Core calls run on the blocking pool, never on the async runtime threads.
- An unmodified `ZEO.ClientStorage` works. A stock client never sees an extension that it did not declare in `register`.
- Requests on one connection run concurrently. Every reply carries the message id of its request.
- The invalidation queue of a slow client is bounded (default 1,000 messages). On overflow the server closes that connection.
- `getInvalidations(tid)` returns `None` when the gap is longer than `invalidation-queue-size`.
- Commits on different named storages never wait for each other.
- A read-only storage refuses `tpc_begin` with `ReadOnlyError`.
- `storeBlobShared` is accepted only with `shared-blob-dir` on and a path inside the server's blob directory.
- On SIGTERM: in-flight `tpc_finish` calls complete, transactions in `Staged` or `Locked` abort, every client gets a clean close, within `shutdown-timeout`.
- A change to the wire protocol is a stop-and-ask.
