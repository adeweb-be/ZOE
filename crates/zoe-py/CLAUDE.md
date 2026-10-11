# zoe-py

The Python extension module `zoe._zoe`, built by maturin into the `zoedb` wheel. Specification: "In-process mode".

## Layout

- `src/storage.rs`: `zoe._zoe.Storage`, a frozen pyclass that Python can subclass. It implements the `IStorage` methods that ZODB's `MVCCAdapter` calls.
- `src/errors.rs`: the mapping of core errors to the exceptions of ZODB and to `zoe.WrongThread`, and of the errors that stop a pack.
- `src/publish.rs`: the publication of commits, in commit order.
- `python/zoedb/src/zoe/storage.py`: `zoe.storage.Storage` subclasses `zoe._zoe.Storage` and ZODB's `ConflictResolvingStorage`, declares `IStorage` and adds `tpc_vote` with the conflict resolution, `history`, `pack`, `registerDB` and `supportsUndo`. It defines `WrongThread`.

## Invariants

- The module declares that it does not use the GIL, so that the GIL stays off when CPython 3.14t loads it.
- `import zoe` does not import ZODB. `zoe.Storage` and `zoe.WrongThread` load `zoe.storage` at first use. The extension imports an exception class of ZODB only when it raises it.
- Every call into the core runs inside `py.detach`: it releases the GIL, and on a free-threaded build it holds nothing.
- No mutex of the core or of this module is held while Python code runs. A lookup under a mutex binds its result to a local before it builds an exception, because building an exception runs Python code. A mutex that can wait, such as the mutex of a transaction entry during a vote, is taken only inside `py.detach`. A Python reference is dropped outside every mutex. The per-OID locks of a vote are not mutexes: they stay held while `_p_resolveConflict` runs.
- The last reference to the store drops inside `py.detach`, so the database closes without the GIL: each call moves its reference into the detached closure, `close` drops the store there, and a storage that Python frees without `close` closes its store detached too.
- `zoe.Storage` is thread-safe and all Connections of a process share it.
- A table keyed by the address of the ZODB transaction object holds one core transaction (`Txn<Arc<Store>>`) per `tpc_begin`. The entry keeps the object alive, so no other object gets its address while the entry exists.
- A second `tpc_begin` of a transaction, or a call with a transaction that the storage did not begin, raises `StorageTransactionError`. `tpc_abort` of an unknown transaction does nothing.
- The thread that calls `tpc_begin` drives the transaction. `store`, `checkCurrentSerialInTransaction` and `tpc_vote` from another thread raise `zoe.WrongThread`, a subclass of `StorageTransactionError`. `restore` and `storeBlob` follow the same rule when they are built. The core checks the thread, so a refused call changes nothing. `tpc_finish` and `tpc_abort` work from any thread.
- `zoe.Storage` resolves conflicts in this process, as ZODB's `ConflictResolvingStorage` does. `tpc_vote` calls `_vote_resolving`: the core takes the locks and, when each stale written oid can be resolved, keeps them and returns each oid with its committed serial and pickle. For each oid, `tryToResolveConflict` runs the `_p_resolveConflict` method of the class, and `tpc_vote` stores the result over the committed serial. Then it calls `tpc_vote` of the extension, which checks the serials again with the same locks. `tpc_vote` returns the resolved oids, or an empty list.
- A conflict that does not resolve raises the `ConflictError` of `tryToResolveConflict`, with `oid`, `class_name` and `serials`. If ZODB cannot read the class name from the pickle, the error comes without it. A read-current conflict is never resolved: it raises `ReadConflictError`.
- A staged pickle that the core cannot read back from its spill file fails `_vote_resolving` with `StorageError`, whose cause is the error of the core. `_vote_resolving` aborts the transaction first, so the locks are free when the error leaves `tpc_vote`. The error is not a `ConflictError`, so ZODB does not retry the transaction.
- Any error between the first vote and the end of the second vote, an asynchronous one included, makes `tpc_vote` call `tpc_abort` before it raises. So the locks are free when the error leaves `tpc_vote`, and the transaction is never left voted. The later `tpc_abort` of ZODB does nothing.
- While `_p_resolveConflict` runs, `tpc_abort` from another thread and `close` take the transaction, because no call holds its mutex. They release the locks at once. The resolving `tpc_vote` then raises `StorageTransactionError`.
- `registerDB` hands the database to `ConflictResolvingStorage`, so the resolution uses the record transforms of the database.
- `tpc_finish` runs its callback after the commit is durable. A commit is published when its callback has run. Callbacks run in commit order: a commit waits until the commit before it is published. A failed or panicking callback still publishes its commit.
- A callback cannot commit in its own storage: that `tpc_finish` raises `StorageTransactionError` before it commits, because its commit would wait for the callback that runs it. A thread-local list of the storages whose callbacks run tells this.
- A `ConflictError` on a stored object gets the stored pickle as `data`, so ZODB fills its class name. If ZODB cannot read the pickle, the error comes without `data`. `_vote_resolving` first applies the `_crs_untransform_record_data` of the database to the pickle, so the class name comes from the pickle and not from the transformed record. A record that does not untransform gives no `data`.
- `lastTransaction` gives only a published tid. It waits until the last tid of the core is published. A callback that calls it does not wait and gets the tid before its own commit.
- A read-only storage refuses `tpc_begin`, `new_oid`, `store` and `pack` with `ReadOnlyError`. It does not create a missing directory. It still opens the store for writes and takes the exclusive lock, until the core has a read-only open.
- `close` marks the table closed and aborts the open transactions. A `tpc_begin` that read the store before the close finds the mark when it inserts its entry, aborts its transaction and raises `StorageError`. So no transaction keeps the store open after `close`, and the directory lock is free when no call is in progress. A second `close` does nothing. A call on a closed storage raises `StorageError`.
- `close` aborts the transactions that no call uses first. Their locks go, so a vote that waits for one of them ends at once, and `close` does not wait for a lock timeout.
- `__len__` returns 0: IStorage asks only for an approximate length, and the store keeps no object count. `getSize` is the size of the files in the store directory. `getName` and `sortKey` are the canonical path of the directory.
- `pack(t, referencesf)` turns `t` into a tid as ZODB's own storages do: `TimeStamp(*time.gmtime(t)[:5] + (t % 60,))`. Then `_pack` runs `Store::pack` at that tid. The pack keeps every record after the tid and, for each object that a reader at the tid or later can reach, the newest record at or before it. `pack` never calls `referencesf`: the core reads the references in the pickles.
- `_pack` runs the whole pack inside one `py.detach`, so the GIL is free from start to end. The pack takes no per-OID lock and does not wait for a publication. Other threads read and commit while it runs: a commit waits for at most one batch of the pack. A second pack waits for the first. A pack can run from any thread, also while a transaction of that thread is open.
- A pack at a time before the last pack does nothing. A pack at a time after the last tid packs at the last tid.
- A closed storage refuses `pack` with `StorageError`. A pack in progress keeps the store open after `close` until it ends, as any call in progress does.
- `supportsUndo()` is false.
- The wheel never imports Plone. Only `zoe.plone`, through the `plone` extra, does.
- The version of the wheel equals the version of the crates.
- A change to a public Python API is a stop-and-ask.
- `_lock_table_is_empty()` tells if no transaction of the core holds or waits for a lock. It is a private hook for the tests, not a public API.

## Errors

| Core error | Python exception |
| --- | --- |
| `NotFound` | `POSKeyError(oid)` |
| `Conflict` | `ConflictError` with `oid`, and `serials == (current, read)` for a stale serial; a lock-wait timeout gives no serials. A conflict on a stored object also gives `class_name`, whether the core or `_p_resolveConflict` does not resolve it |
| `ReadConflict` | `ReadConflictError` with `oid` and `serials == (current, read)` |
| `WrongThread` | `zoe.WrongThread` |
| `Protocol`, `LockOrder` | `StorageTransactionError` |
| `Halted`, `CorruptRecord`, `CorruptBlob`, `CorruptTxn`, `Backend` | `StorageSystemError` |
| `Locked`, `Closed`, `Format`, `ReadOnly`, `NotClosedCleanly` | `StorageError` |
| `Io` | `OSError` |

A pack maps two errors in its own way. Each stops the pack and raises `StorageSystemError`, whose text starts with "pack stopped" and gives the text of the core:

| Core error of a pack | Cause | The text names |
| --- | --- | --- |
| `CorruptRecord` | a record fails its checksum. The pack keeps it, also when nothing can reach it. | the oid and the tid |
| `Format` | a record does not decode | the oid, the tid and the reason |
| `Format` | a row is in a format that this build cannot read or rewrite | the row |

## Tests

- `python/zoedb/tests/acceptance/test_storage.py` and `python/zoedb/tests/acceptance/test_conflict_resolution.py` are the acceptance tests of the storage and of conflict resolution.
- `python/zoedb/tests/acceptance/test_pack.py` is the acceptance test of pack. It also runs ZODB's `PackableStorage` suites.
- `python/zoedb/tests/acceptance/test_free_threaded_commits.py` is the acceptance test of commits from many threads. It reports the GIL state after its imports.
- `python/zoedb/tests/test_zoe_storage.py` runs the storage suites of ZODB and the unit tests of this crate.
- On CPython 3.14t, ZODB loads the stock `persistent`, which turns the GIL back on in the test process. The test that the extension keeps the GIL off runs in a new interpreter that imports only `zoe`. The parallel-commit test runs in a child interpreter with `-X gil=0`.
