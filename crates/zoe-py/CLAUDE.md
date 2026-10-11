# zoe-py

The Python extension module `zoe._zoe`, built by maturin into the `zoedb` wheel. Specification: "In-process mode".

## Layout

- `src/storage.rs`: `zoe._zoe.Storage`, a frozen pyclass that Python can subclass. It implements the `IStorage` methods that ZODB's `MVCCAdapter` calls.
- `src/errors.rs`: the mapping of core errors to the exceptions of ZODB and to `zoe.WrongThread`.
- `src/publish.rs`: the publication of commits, in commit order.
- `python/zoedb/src/zoe/storage.py`: `zoe.storage.Storage` subclasses `zoe._zoe.Storage`, declares `IStorage` and adds `history`, `pack`, `registerDB` and `supportsUndo`. It defines `WrongThread`.

## Invariants

- The module declares that it does not use the GIL, so that the GIL stays off when CPython 3.14t loads it.
- `import zoe` does not import ZODB. `zoe.Storage` and `zoe.WrongThread` load `zoe.storage` at first use. The extension imports an exception class of ZODB only when it raises it.
- Every call into the core runs inside `py.detach`: it releases the GIL, and on a free-threaded build it holds nothing.
- No lock of the core or of this module is held while Python code runs. A lock that can wait, such as the lock of a transaction entry during a vote, is taken only inside `py.detach`. A Python reference is dropped outside every lock.
- `zoe.Storage` is thread-safe and all Connections of a process share it.
- A table keyed by the address of the ZODB transaction object holds one core transaction (`Txn<Arc<Store>>`) per `tpc_begin`. The entry keeps the object alive, so no other object gets its address while the entry exists.
- A second `tpc_begin` of a transaction, or a call with a transaction that the storage did not begin, raises `StorageTransactionError`. `tpc_abort` of an unknown transaction does nothing.
- The thread that calls `tpc_begin` drives the transaction. `store`, `checkCurrentSerialInTransaction` and `tpc_vote` from another thread raise `zoe.WrongThread`, a subclass of `StorageTransactionError`. `restore` and `storeBlob` follow the same rule when they are built. The core checks the thread, so a refused call changes nothing. `tpc_finish` and `tpc_abort` work from any thread.
- `tpc_finish` runs its callback after the commit is durable. A commit is published when its callback has run. Callbacks run in commit order: a commit waits until the commit before it is published. A failed or panicking callback still publishes its commit.
- `lastTransaction` gives only a published tid. It waits until the last tid of the core is published. A callback that calls it does not wait and gets the tid before its own commit.
- A read-only storage refuses `tpc_begin`, `new_oid` and `store` with `ReadOnlyError`.
- `close` aborts the open transactions and drops the store, so the directory lock is free when no call is in progress. A call on a closed storage raises `StorageError`.
- `__len__` returns 0: the store keeps no object count. `getSize` is the size of the files in the store directory. `getName` and `sortKey` are the canonical path of the directory.
- `pack` raises `NotImplementedError` until pack is built. `supportsUndo()` is false.
- The wheel never imports Plone. Only `zoe.plone`, through the `plone` extra, does.
- The version of the wheel equals the version of the crates.
- A change to a public Python API is a stop-and-ask.

## Errors

| Core error | Python exception |
| --- | --- |
| `NotFound` | `POSKeyError(oid)` |
| `Conflict` | `ConflictError` with `oid`, and `serials == (current, read)` for a stale serial; a lock-wait timeout gives no serials |
| `ReadConflict` | `ReadConflictError` with `oid` and `serials == (current, read)` |
| `WrongThread` | `zoe.WrongThread` |
| `Protocol`, `LockOrder` | `StorageTransactionError` |
| `Halted`, `CorruptRecord`, `CorruptTxn`, `Backend` | `StorageSystemError` |
| `Locked`, `Closed`, `Format` | `StorageError` |
| `Io` | `OSError` |

## Tests

- `python/zoedb/tests/acceptance/test_storage.py` is the acceptance test.
- `python/zoedb/tests/test_zoe_storage.py` runs the storage suites of ZODB and the unit tests of this crate.
- On CPython 3.14t, ZODB loads the stock `persistent`, which turns the GIL back on in the test process. The test that the extension keeps the GIL off runs in a new interpreter that imports only `zoe`. The parallel-commit test runs in a child interpreter with `-X gil=0`.
