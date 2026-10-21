# 0044. The locks of hot objects: the lock hand-off at the vote, and early write-intent locks before the read

Status: accepted. Part A changes the invariant of group commit in section 5 of the specification. Part B adds the extension `lock_intent` to the wire protocol, the keys `early-locks` and `early-lock-classes`, and a public Python API.

## Context

F7 and N5 ask that the writers of a hot object stop going one by one. In B5, the Plone saves conflict on the same `BTrees.Length` counters and catalog buckets, and `zoe_fsync_batch_size` stays at 1 (`docs/findings/plone-end-to-end.md`, item 2). A transaction releases its locks after its commit is durable, so the merges onto a hot `Length` go one per sync.

An earlier merge is safe: a finish that started cannot abort, its staged records are final, and group commit writes in arrival order, so a later finish is never durable before it.

For an object that tier 1 does not merge, a lock in `Connection.register` does not stop the conflicts (`docs/spikes/early-lock-hook.md`): ZODB reads the object at the snapshot, before `register`, so a writer that waits there holds a stale state. In a model of four writers on a counter that cannot merge, locks in `register` gave 102 `ConflictError`s, vote-time locks 89 and 104, and a lock before the read 0.

The acceptance tests are `python/zoedb/tests/acceptance/test_early_locks.py` and `test_hot_objects.py`, where 64 writers on one object never retry and early locks on must give 0 `ConflictError`.

## Decision

### A. The lock hand-off

The rules: specification, "Storage core", "The lock hand-off (ADR 0044)".

#### A1. When the hand-off applies

A finish that starts hands the lock of each oid that it writes, of a class that tier 1 merges in mode `on`, to the exclusive vote at the head of its queue, with a copy of its staged record. For another class, the new owner could only wait for the holder. A `readCurrent` request never takes a hand-off: its check needs the committed serial. A restore never hands off: its finish can fail alone.

#### A2. What the vote does with a handed-off oid

The vote merges onto the staged record of the holder. A refusal fails the vote at once, with no `serials` when the holder has no tid yet. When tier 1 gives no answer or does not run (class `off` or `shadow`), the vote waits until the holder is durable. A second vote does not check the oid again. The resolution log line of a merge onto a commit in flight gives the user, the description and the tid of the holder, or 0 before its batch gives one.

#### A3. Order and durability

The new owner commits after the holder, in its batch or a later one, and a failed write or sync halts the store. So an acknowledged commit stays durable (N1), and no commit is durable before the commit that it merged onto. Group commit puts no limit on a batch, so a chain of merges on one oid shares one sync. A member whose holder got no tid fails with `Protocol`.

#### A4. The lock manager

A finish that starts marks the exclusive locks of its transaction as committing and hands them off under the mutex of the lock table. The new owner reads a copy of the staged record, never the spill file of the holder. The release after the finish releases only the locks that the transaction still owns. A new owner that releases the lock before the holder is durable leaves the hand-off for the next exclusive request. A new owner whose own commit is durable ends the hand-off before it releases the lock. So a lock that is free, with no waiter and no hand-off, means that every commit of its oid is durable. A hand-off only grants a lock sooner than a release, so it adds no cycle, and a wait still ends at `lock-timeout` or at a `LockCancel`.

#### A5. Both modes, and the clients

The hand-off is in `zoe-core`, so `zoe.Storage` and `zoe serve` both have it. It needs no wire change: a stock `ZEO.ClientStorage` sees a vote that resolved an oid, as from a ZEO 6 server.

#### A6. Measurement

`zoe_lock_handoffs_total` counts the hand-offs. `test_writers_on_a_hot_length_raise_no_conflict_and_lose_nothing` checks 0 `ConflictError` and the exact count with 64 writers on one `Length`. `writers_of_a_hot_length_share_the_syncs` (`simulated_hot_length.rs`) asserts a mean above 2 commits per sync, with syncs slower than a round trip; on macOS a sync is faster (ADR 0006), so the commits never share a batch there. The kill loop with hot `Length` writers gives 0 losses and no stale read.

### B. Early write-intent locks

The mechanics: specification, "Storage core", "Early write-intent locks (extension, ADR 0044)", and "ConflictError strategy", "Lever 3: early write-intent locks (ADR 0044)", which also lists the costs.

#### B1. The lock before the read

An object of a hot class takes its intent lock when its state loads, and loads the current record, past the snapshot. Only this mechanism gives 0 `ConflictError` when each attempt counts. The lock lasts until the end of the transaction: `tpc_finish`, `tpc_abort`, an abort before `tpc_begin`, `afterCompletion`, or the close of a connection that no transaction joined. The close of a joined connection fails as in ZODB and keeps the locks. A historical connection takes no lock.

#### B2. The hot classes

`early-lock-classes` names the hot classes by the `module.name` of `type(obj)`, in `<zoe>` and `<zoeclient>`. It is empty by default, and `early-locks on` with an empty list takes no lock. A class of tier 1 fails the open with `ValueError`. A subclass is not hot unless the list names it.

#### B3. The owner of the intent locks

An intent owner holds the intent locks before `tpc_begin`, which adopts them. `zoe.Storage.lock_intent(oids, owner=None)` and `unlock_intent(owner=None)`, and the same methods of `zoe.zeo.ClientStorage`, use the default owner of the storage. `LockingConnection` binds an owner of its own to the thread around `tpc_begin`, also when it took no lock, so it never adopts the locks of the default owner. The native client leases a pool connection to an owner from its first `lock_intent` to the end of the adopting transaction, so the pool bound of 64 also bounds the owners. An owner whose `lock_intent` fails while it holds no lock, or whose connection is lost, gives the connection back at once (`crates/zoe-py/CLAUDE.md`, the early write-intent locks). An end whose `unlock_intent` fails clears its state and logs the failure; the end of the session or the hold bound releases the locks.

#### B4. The order of the locks

The specification holds the order rules, the relock after `LockOrder` and the retry hint (see part B). On `transaction.manager`, the hint is the hint of the thread. No lock and no hint outlives the next transaction. Without the hint, a probe of 8 threads of 25 requests, with two hot objects loaded in descending oid order, gave up 98 or 99 requests of 200 (about 75 with early locks off); with the hint, none. An intent lock of an oid that the transaction writes becomes an exclusive lock of its vote, so its finish can hand it off.

#### B5. Where a wait runs, and how long a hold lasts

In `zoe serve`, `lock_intent` runs on the thread of the transaction of the session, never on the read pool. The hold bound of 60 s is fixed, with no option.

#### B6. No batch

The load needs its grant before it returns, so `setstate` sends one `lock_intent` at once, with no batch window. The hot classes are few, so the calls are few.

#### B7. How `early-locks on` selects the connection class

When a storage with hot classes opens, `zoe.zodb` patches `setstate`, `newTransaction`, `afterCompletion`, `tpc_begin`, `tpc_finish` and `close` of `ZODB.Connection.Connection`, once per process. So it works under `<zoe>`, `<zoeclient>`, Zope and a plain `<zodb>`, and leaves no stock connection in a pool. `zoe.zodb.LockingConnection` is the same methods as a subclass. The install keeps the methods that it replaces as they are at the install, so a patch of another package stays in the chain. Each patched method does exactly what the replaced one does for a storage without hot classes and for a historical connection. The patch reads only these internals of ZODB: of the connection, `_db`, `before`, `opened`, `_savepoint_storage` and the `index` of the savepoint, `_reader.setGhostState`, `_cache` (`get`, `invalidate`, `update_object_size_estimation`), `_load_count`, `_modified`, `_creating` and `_needs_to_join`; of the object, `type(obj)`, `_p_oid`, `_p_serial`, `_p_estimated_size`, and for a blob `_p_blob_uncommitted` and `_p_blob_committed`.

#### B8. `native false` and `readCurrent`

`<zoeclient>` with `native false` refuses `early-locks on` (specification, "ConflictError strategy", "Lever 3: early write-intent locks (ADR 0044)"). `readCurrent` keeps its shared lock at the vote: a shared intent lock of each node that a BTree passes would make the readers of a tree wait.

#### B9. The versions of ZODB

The tests run the patch on the versions of ZODB that the lock files install (6.2 to 6.4), whose `Connection.py` is the same. ZODB 6.0 and 6.1 stay supported targets without a test of their own.

#### B10. The helpers of the acceptance tests

`test_early_locks.Client.lock` and `Client.release` use the default owner of the storage (section B3), and `lock_order()` gives `StorageTransactionError` (section B4). `test_hot_objects.database()` declares `persistent.mapping.PersistentMapping` hot, so its writers also lock the root. Its hot `Length` test declares no hot class: the lock hand-off serves `Length` (section A6).

#### B11. What the build adds

The core owner `IntentLocks`, `Store::lock_intent(owner, oids)`, the metrics `zoe_intent_locks_total` and `zoe_intent_lock_wait_seconds`, and the twins `ZOE_EARLY_LOCKS` and `ZOE_EARLY_LOCK_CLASSES`. `zoe-bench hot` counts a `lock_intent` that times out as a conflict, and reports `commits_per_sync`.

## Consequences

- A conflict on a handed-off oid that tier 1 does not decide waits for the holder: the wait for the release, moved after the lock.
- A long chain of merges on one oid fills one batch, so the batch write takes longer.
- The hand-off needs no format change and no new key.
- F7 holds for a declared hot object that cannot merge, not for an undeclared one.
- A hot object costs a lock round trip at each load, a reload at each transaction, and a read skew.
- A vote of a stock client waits for an intent lock as for a vote lock.
- Vote-time locking stays the default: `early-locks` is off and `early-lock-classes` is empty.

## Alternatives

- **The hand-off at the vote of the holder.** The holder can still abort.
- **The hand-off when the batch of the holder is written.** Each batch then holds one commit of the hot oid.
- **Merge in the batch, by the leader, or no lock for the classes of tier 1.** It puts merges under the sequencer, and a refusal fails a member after its vote passed.
- **The intent lock in `register`.** As many `ConflictError`s as vote-time locks.
- **Fail fast at the grant.** It saves the work after the change, not the conflict.
- **A refresh of the object at the grant of a lock in `register`.** The change is often in the state already when `register` runs, so a reload loses it or writes a value from the old state.
- **Fail fast and keep the lock for the retry.** One `ConflictError` per wait (79 in the model), a lock that outlives its transaction, and a change of the retry of Zope.
- **A hot set that the client learns from its conflicts.** Its first conflicts still raise.
- **Every class hot with `early-locks on`.** Every transaction would lock the root at its load.
- **`connection-class` of the configuration, or a database section of ZOE.** The first leaves a stock connection in the pool and is not a key of `<zodb>`; the second is a second place for one setting.

## Merged ADRs

- ADR 0045, early write-intent locks: Decision, part B. Its section N is section BN here, for N from 1 to 11. The changes of its section 12 are in the sections B1, B3, B4, B5, B7 and B11 that hold their subject, or in the specification: "Storage core", "Early write-intent locks (extension, ADR 0044)", and "ConflictError strategy", "Lever 3: early write-intent locks (ADR 0044)".
