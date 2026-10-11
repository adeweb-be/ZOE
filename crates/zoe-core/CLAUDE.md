# zoe-core

The storage core. It owns correctness: the data model, two-phase commit, per-OID locks, conflict handling, pack and blobs. Backends own bytes. Specification: "Storage core" and "Async and free-threading".

Red zones in this crate: `src/format/` and `src/pack/`. A change to the data model is a format change wherever its code lives.

Every test of this crate also runs under ThreadSanitizer with `just tsan`. A data race fails it. The ignored test in `tests/tsan_canary.rs` holds a deliberate race: `just tsan` runs it and passes only when ThreadSanitizer reports that race.

## Invariants

- The core is synchronous. It has no tokio, no async code and no Python inside.
- The core, not the backend, implements locking, two-phase commit, conflict resolution, invalidations and blobs.
- The backend never allocates tids. The core allocates the tid at `tpc_finish` under the commit sequencer: `max(now, last_tid + 1 tick)`.
- Tids always increase.
- A normal commit never gets `Tid::MAX`, the tid that `load` reads before. When no tid below `Tid::MAX` is left after the last tid, `finish` fails with `Protocol`, and the store does not halt.
- A client passes a tid only through a restore transaction (`tpc_begin_restore`), never through a normal commit. A restore transaction takes both `store` and `restore`. `store` keeps its serial check there.
- A restored oid is a written oid: the vote takes an exclusive lock on it, as for `store`. `restore` reads the current serial of the oid, and the vote fails with `Conflict` if another transaction changed the oid since.
- A restore transaction (`tpc_begin_restore`) commits under the tid of the client and allocates no tid. The tid must be after the last tid: `tpc_begin_restore` checks it, and `finish` checks it again under the commit sequencer. A tid that is not after the last tid, or `Tid::MAX`, fails with `Protocol`, and the store does not halt.
- `store` refuses an empty pickle with `Protocol`. Only `restore` writes a deletion.
- `Oid(u64::MAX)` is reserved: `store` and `restore` refuse it with `Protocol`. `new_oid` fails with `Protocol` when no oid below `u64::MAX` is left. It never wraps.
- Locks are acquired in ascending oid order, over the written and the read-current oids together. A request out of order is refused with `LockOrder`, never granted.
- Written oids take exclusive locks. Oids passed to `checkCurrentSerialInTransaction` take shared locks. An oid that is both takes an exclusive lock. A shared lock is compatible only with shared locks.
- Each oid has a wait queue in arrival order. A lock request never passes a request that waits for the same oid, so a stream of readers cannot starve a writer.
- A transaction that must wait releases nothing and queues once.
- The thread that releases a lock grants it to the waiters at the head of the queue, under the lock-manager mutex, then wakes them. A waiter holds its lock before it wakes up.
- A lock wait longer than `lock-timeout` (default 10 s) becomes a `ConflictError` on the oid waited for. The timeout covers all the waits of one vote, measured on the monotonic clock from the start of its lock acquisition.
- Conflict detection and resolution happen at vote, before any disk write. A stale written serial is `Conflict`; a stale read-current serial is `ReadConflict`. The vote checks the written serials first.
- A failed vote releases every lock that it took. `finish` releases the locks after the commit is durable and the sequencer holds the new tid. `abort` releases them. Dropping a transaction aborts it.
- `vote_resolving` is the vote of a caller that resolves conflicts itself. A stale written oid can be resolved when `store` staged it with a serial other than `Tid::ZERO` and its committed record holds a pickle. When each stale written oid can be resolved, the vote keeps its locks and returns these oids with their committed records. Any other stale written serial fails it with `Conflict`, then a stale read-current serial fails it with `ReadConflict`. A read-current oid is never resolved.
- After `vote_resolving` gives conflicts, the transaction holds its locks and takes only a `store` of an oid in conflict, with the serial of the committed record. The next `vote` takes no lock. It checks every serial again with the locks held. An oid that is still stale fails it, and it releases the locks. So the locks are taken once, in ascending oid order. While the caller resolves, only an abort or a drop of the transaction releases them, from any thread: the in-process storage does this at `tpc_abort` and at `close`. The core runs no resolution code.
- The lock table has an entry only for an oid that a transaction holds or waits for.
- No conflict tier runs Python on the server.
- One backend write transaction holds the records, the blob records, the transaction row and the meta update of a commit. It is durable before the client gets the acknowledgement.
- A crash before the fsync leaves no record, no blob record and no transaction row. It can leave blob files in their place in the blob tree; an intent file in `blobs/tmp/` names them, and the next open removes them. A crash after the fsync leaves a complete transaction. No other repair step exists.
- Invalidations go out only after the acknowledgement.
- Locks that group-commit members hold stay held until the batch is durable. A failed fsync fails every member.
- Only the commit-batch leader opens a backend write transaction. Readers use snapshots and never block it.
- The thread that begins a transaction drives it. `store`, `store_blob`, `restore`, `check_current_serial` and `vote` from another thread fail with `WrongThread`, and the refused call changes nothing. A refused `store_blob` stages nothing and drops its blob file, which removes the file. `finish` and `abort` may run on any thread, because ZODB lets another thread finish or abort a transaction.
- `Txn` reaches its store through `S: Deref<Target = Store>`. `Txn<&Store>` borrows the store. `Txn<Arc<Store>>` shares it and borrows nothing, so a skin can keep it across calls and threads.
- A successful vote hands its locks to the transaction with `Locks::keep`. The transaction then releases them with `LockManager::release` at `finish`, at `abort` or when it is dropped. A failed vote drops its `Locks`, and that releases them.
- After a vote fails on a stale serial, `conflict_serials` gives the current serial and the serial that the transaction read, the `serials` of a ZODB `ConflictError`. After a lock-wait timeout it gives `None`. `staged` gives the read serial and the pickle that the transaction staged for an oid.
- `finish_after` commits as `finish` does and also returns `previous`, the last tid of the store when the commit started to write: the commit just before it in commit order. A skin runs its post-commit work in commit order with it, and holds no lock across commits.
- The lock manager and group commit are the only shared mutable state. The core is `Send + Sync`.
- Every read verifies the record checksum. A mismatch raises `CorruptRecord` with the oid and the tid, never `POSKeyError`.
- `load_before(oid, tid)` reads one snapshot: a reverse range scan from `(oid, tid - 1)` gives the record, and a forward scan from `(oid, tid)` gives the serial of the next record. Both records are verified. An oid with records, but none before `tid`, gives `None`. An oid with no record raises `NotFound`.
- A record with a 0-length payload is a deletion. `load`, `load_before` and `load_serial` of a deletion raise `NotFound`. `history` lists it with size 0. The iterator gives it with `data: None`. `record_iternext` skips the oid.
- The transaction iterator stops at the last tid at the time of the call. A later transaction always has a greater tid, so the iterator gives each transaction with all of its records. This is correct only because `finish` publishes `last_tid` after the commit is visible to readers. Group commit must keep this: it publishes `last_tid` after the whole batch is visible.
- `history` and the iterator read the transaction rows and the records in separate snapshots. This is correct only while nothing deletes a row or a record. Pack must keep the rows and the records consistent: the iterator raises `CorruptTxn` when the records of a transaction do not match the oid count of its row, and when records of a tid have no row. `history` raises `CorruptTxn` for a record that has no row.
- The transaction iterator does not borrow the store, so a skin can keep it across calls and threads. It holds only a weak reference to the store and keeps it open only during one call. After the `Store` is dropped, the next call gives `Closed`, and the iterator ends.
- The open database and the blob store live together in `Opened`. The blob store is the only owner of the directory, which holds the lock. The database closes before the directory releases the lock, because closing a redb database writes to the file.
- A blob file holds only a weak reference to `Opened`, so it never keeps a store open. After the `Store` is dropped, a write to the file fails with `Closed`, and the next open removes the file. `store_blob` refuses a blob file of another store with `Protocol`, because the same temporary name can exist in both stores.
- `Store::open_read_only` opens an existing store for reads only. It takes the store lock, so a writer gets `Locked`, and it opens the database file read-only, so it changes no byte. It creates the lock file only when the store has none, and nothing in a directory without a store. Every write call fails with `ReadOnly`: `new_oid`, `tpc_begin`, `tpc_begin_restore`, `Txn::begin_restore`, and `vote` and `vote_resolving` of a `Txn` that `Txn::begin` began. redb opens a read-only database only from a path, so this open does not go through the `Vfs`. A store that a crash left open fails with `NotClosedCleanly`: a writer open rebuilds the free-space map of its database.
- No key space maps a tid to its oids. `Backend::txn_oids` scans every key of `records`, so each iterator call reads all the keys of the store.
- An error in `finish` leaves the outcome unknown until the store is reopened. Until then, every later finish is refused with `Halted`.
- Every record carries a codec byte, a branch id and a typed-field-slot header. Every blob record carries its BLAKE3 hash. This holds from the first store written.
- `CODEC_NONE` is 0 and stores the data as it is. `CODEC_ZSTD` is 1 and stores one zstd frame that states the data length. These values never change. An unknown codec on read is a `Format` error, never data.
- The checksum and the payload length cover the payload as stored. A read verifies the checksum before it decompresses.
- `decode_frame` decodes every codec. A read path gets the data, never the stored form.
- Compression never changes what a load returns. A store reads every codec, whatever its `compression` option.
- Blob records and intent files are written with `encode_frame`, so they keep the codec `CODEC_NONE` whatever the `compression` option. A blob file is never compressed.
- A record is kept compressed only when zstd makes it smaller. Otherwise it is stored as it is, with codec 0. The zstd level is an option, 3 by default.
- Decompression is bounded. The length that a zstd frame states is at least 1, at most `u32::MAX` and at most 32,768 times the stored length, and the payload decodes to exactly that length. Any other zstd payload is `CorruptRecord`, never data and never a deletion.
- `max-record-size`, `max-transaction-size` and `max-blob-size` refuse a `store` before anything is written.
- Pack never blocks readers. Writers pause only for the final meta update. A blob file is deleted after its record.
- A blob file is immutable per `(oid, tid)` and read-only once committed. It lives at the BushyLayout path of `(oid, tid)` under `blobs/`, and `blobs/.layout` holds `bushy`.
- `store_blob` syncs the blob file. `finish` writes and syncs an intent file in `blobs/tmp/`, then renames each blob file into place and syncs its directory, all before the backend commit. A new directory is synced into its parent before anything goes into it. So a durable blob record always has its file.
- A directory that exists is not always durable: a killed process can stop between a mkdir and the sync of the parent. So the first blob file of a process syncs the parents of `blobs/` and `blobs/tmp/` also when they exist, and an open syncs the parent of each existing level of each oid directory that an intent file names, from the top down. A finish creates oid directories only after its intent file is durable, so the intent files name every such directory.
- Opening a store reads only `blobs/tmp/` and the oid directories that its intent files name, never the whole blob tree. It removes the blob files in the tree that an intent file names and that have no blob record, and syncs their directories. Then it removes the intent files and the temporary files: only regular files, and other entries stay.
- A read through `open_blob` checks the size and the BLAKE3 hash at the end of the file. A mismatch or a missing file is `CorruptBlob` with the oid and the tid.
- Every blob file operation goes through the `Vfs`, so the crash simulation covers it.
- A staged blob stays with its oid until `finish` or `abort`, also across the two votes of `vote_resolving`. A later `store` of the oid, the store of a resolved conflict too, and `restore` replace only the pickle and keep the blob, as ZODB's blob storages do. A restored deletion drops the blob. A second `store_blob` of the oid replaces the blob.
- A read-only open writes nothing under `blobs/`: no intent replay, no removal in `blobs/tmp/`, no directory sync. It only checks the layout file. A blob file that a crash left without its blob record stays invisible to it, because every blob read starts from the blob record, and the next writer open removes it. `new_blob_file` fails with `ReadOnly`.
- A staged blob holds only a weak reference to the store, as a blob file does. So it works the same in `Txn<&Store>` and in `Txn<Arc<Store>>`.
- Nothing is undoable past the last pack.
- A store creates its database as `zoe.redb.new`, syncs it, renames it to `zoe.redb` and syncs the directory. Opening never repairs a store; it only removes a `zoe.redb.new` that a crash left before the rename, and the leftovers of an unfinished commit: the blob files in the tree that an intent file names and that have no blob record, then the files in `blobs/tmp/`.
- `next_oid` in `meta` is always greater than every committed oid.
- `finish` raises the next oid of the store above every oid that it writes, before the write. So `new_oid` never gives an oid that a restore wrote.
- The core reads the time only through the `Clock` trait and the filesystem only through the `Vfs` trait, so the crash simulation and the deterministic simulator can replace them. One exception: lock waits use `Instant`, because `Clock` gives wall time and a `Condvar` wait cannot run on a simulated clock.
- A change to an existing `Backend` or `WriteTxn` signature is a stop-and-ask. A new method needs no stop while no crate outside `zoe-core` implements the traits, unless it needs a new key space, `meta` key or frame field: that is a data-model change, a red zone and a stop-and-ask.
