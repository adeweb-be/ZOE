# zoe-core

The storage core. It owns correctness: the data model, two-phase commit, per-OID locks, conflict handling, pack and blobs. Backends own bytes. Specification: "Storage core" and "Async and free-threading".

Red zones in this crate: `src/format/` and `src/pack/`. A change to the data model is a format change wherever its code lives.

Every test of this crate also runs under ThreadSanitizer with `just tsan`. A data race fails it. The ignored test in `tests/tsan_canary.rs` holds a deliberate race: `just tsan` runs it and passes only when ThreadSanitizer reports that race.

## Invariants

- The core is synchronous. It has no tokio, no async code and no Python inside.
- The core, not the backend, implements locking, two-phase commit, conflict resolution, invalidations and blobs.
- The backend never allocates tids. The core allocates the tid at `tpc_finish` under the commit sequencer: `max(now, last_tid + 1 tick)`.
- Tids always increase.
- A client passes a tid only through `restore`, never through a normal commit.
- Locks are acquired in ascending oid order. A request out of order is refused with `LockOrder`, never granted.
- A transaction that must wait releases nothing and queues once.
- A lock wait longer than `lock-timeout` (default 10 s) becomes a `ConflictError`.
- Written oids take exclusive locks. Oids passed to `checkCurrentSerialInTransaction` take shared locks.
- Conflict detection and resolution happen at vote, before any disk write. A failed vote releases its locks.
- No conflict tier runs Python on the server.
- One backend write transaction holds the records, the transaction row and the meta update of a commit. It is durable before the client gets the acknowledgement.
- A crash before the fsync leaves nothing. A crash after it leaves a complete transaction. No repair step exists.
- Invalidations go out only after the acknowledgement.
- Locks that group-commit members hold stay held until the batch is durable. A failed fsync fails every member.
- Only the commit-batch leader opens a backend write transaction. Readers use snapshots and never block it.
- One thread drives a transaction from `tpc_begin` to `tpc_finish` or `tpc_abort`. A call from another thread is refused with `WrongThread`.
- The lock manager and group commit are the only shared mutable state. The core is `Send + Sync`.
- Every read verifies the record checksum. A mismatch raises `CorruptRecord` with the oid and the tid, never `POSKeyError`.
- An error in `finish` leaves the outcome unknown until the store is reopened. Until then, every later finish is refused with `Halted`.
- Every record carries a codec byte, a branch id and a typed-field-slot header. Every blob record carries its BLAKE3 hash. This holds from the first store written.
- `max-record-size`, `max-transaction-size` and `max-blob-size` refuse a `store` before anything is written.
- Pack never blocks readers. Writers pause only for the final meta update. A blob file is deleted after its record.
- A blob file is immutable per `(oid, tid)`. `storeBlob` renames the file into place inside `tpc_finish`.
- Nothing is undoable past the last pack.
- A store creates its database as `zoe.redb.new`, syncs it, renames it to `zoe.redb` and syncs the directory. Opening never repairs a store; it only removes a `zoe.redb.new` that a crash left before the rename.
- `next_oid` in `meta` is always greater than every committed oid.
- The core reads the time only through the `Clock` trait and the filesystem only through the `Vfs` trait, so the crash simulation and the deterministic simulator can replace them.
- A change to the `Backend` or `WriteTxn` signatures is a stop-and-ask: other crates implement and call them.
