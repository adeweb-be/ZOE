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
- Locks are acquired in ascending oid order, over the written and the read-current oids together. A request out of order is refused with `LockOrder`, never granted.
- Written oids take exclusive locks. Oids passed to `checkCurrentSerialInTransaction` take shared locks. An oid that is both takes an exclusive lock. A shared lock is compatible only with shared locks.
- Each oid has a wait queue in arrival order. A lock request never passes a request that waits for the same oid, so a stream of readers cannot starve a writer.
- A transaction that must wait releases nothing and queues once.
- The thread that releases a lock grants it to the waiters at the head of the queue, under the lock-manager mutex, then wakes them. A waiter holds its lock before it wakes up.
- A lock wait longer than `lock-timeout` (default 10 s) becomes a `ConflictError` on the oid waited for. The timeout covers all the waits of one vote, measured on the monotonic clock from the start of its lock acquisition.
- Conflict detection and resolution happen at vote, before any disk write. A stale written serial is `Conflict`; a stale read-current serial is `ReadConflict`. The vote checks the written serials first.
- A failed vote releases every lock that it took. `finish` releases the locks after the commit is durable and the sequencer holds the new tid. `abort` releases them. Dropping a transaction aborts it.
- The lock table has an entry only for an oid that a transaction holds or waits for.
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
- The core reads the time only through the `Clock` trait and the filesystem only through the `Vfs` trait, so the crash simulation and the deterministic simulator can replace them. One exception: lock waits use `Instant`, because `Clock` gives wall time and a `Condvar` wait cannot run on a simulated clock.
- A change to the `Backend` or `WriteTxn` signatures is a stop-and-ask: other crates implement and call them.
