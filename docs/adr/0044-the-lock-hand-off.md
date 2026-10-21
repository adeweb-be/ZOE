# 0044. The locks of hot objects: the lock hand-off at the vote, and early write-intent locks before the read

Status: accepted. Part A changes the invariant of group commit in section 5 of the specification. Part B adds the extension `lock_intent` to the wire protocol, the keys `early-locks` and `early-lock-classes`, and a public Python API.

## Context

### The writers of a hot object

F7 and N5 ask that the writers of a hot object stop going one by one. B5 shows why they do (item 2 of the ranked table of `docs/findings/plone-end-to-end.md`): each Plone save writes the same `BTrees.Length` counters and the same buckets of the catalog, so the saves conflict on them, and `zoe_fsync_batch_size` stays at 1.

How a commit takes its locks (`crates/zoe-core/src/lock/mod.rs`, `crates/zoe-core/src/commit/`):

- The vote takes an exclusive lock on each written oid and a shared lock on each read-current oid, in ascending oid order. A request waits behind each earlier request on its oid. The waits of one vote last at most `lock-timeout`.
- After it holds every lock, the vote compares each read serial with the current serial. Tier 1 (ADR 0042) merges a stale oid of a class that it knows; a refusal fails the vote with `ConflictError`; no answer lists the conflict for the client.
- `tpc_finish` gives the transaction to group commit (`commit/group.rs`). The finishes that wait form the next batch, in arrival order. The leader of a batch gives each member its tid in batch order, writes every member in one backend write transaction under the commit sequencer, syncs once, and wakes the members. A failed write or sync halts the store: each member of the batch and each later finish fail with `Halted`.
- A transaction releases its locks after its finish returns, so after its commit is durable.

Without a hand-off, a writer B that conflicts with a writer A on a hot `Length` waits for the lock until A is durable, then merges against A's record and joins a later batch. Each commit on the hot oid waits for one whole sync of the commit before it: the commits on that oid go one per batch.

Two facts make an earlier merge safe:

- Once A's finish has started, A cannot abort. Its staged records are final: they are the records that its batch writes. A's commit then becomes durable, or the store halts.
- Group commit writes in arrival order, in one backend write transaction per batch, and batches never overlap. A finish that starts after A's finish is in A's batch after A, or in a later batch. So it is never durable before A.

### The objects that tier 1 does not merge

Tier 1 and the lock hand-off serve the objects that tier 1 merges, such as `BTrees.Length.Length`. Early locks are for the other ones. A lock in `Connection.register`, when a transaction first changes an object, does not stop their conflicts. The spike `docs/spikes/early-lock-hook.md` shows it, and leaves twelve questions that sections B1 to B9 answer:

- ZODB reads each object at the snapshot of the start of the transaction, and `register` comes after the load of the object. The snapshot moves only between transactions. So a writer that waits in `register` for a holder holds a stale state when it gets the lock, and its vote fails as it fails without the lock (finding 4).
- A refresh at the grant is not safe: the change is often in the state already when `register` runs, so a reload loses it or writes a value that came from the old state.
- In a model of four writers and 20 increments of one counter that cannot merge, locks in `register` raised 102 `ConflictError`s, vote-time locks 89 and 104. Only two mechanisms changed the outcome: fail fast and keep the lock for the retry (79 errors, one per wait, no failed increment), and the lock before the read, with the read past the snapshot (0 errors).

The acceptance test asks: 64 writers on one object that cannot merge give 0 `ConflictError` with early locks on, and `ConflictError` as without early locks with early locks off. The acceptance tests are `python/zoedb/tests/acceptance/test_hot_objects.py` and `test_early_locks.py`. In `test_hot_objects.py`, a writer never retries, and each `ConflictError` that a transaction raises counts. Section B10 gives their helpers.

## Decision

### A. The lock hand-off

#### A1. When the hand-off applies

A vote B waits for the exclusive lock of an oid X that a transaction A holds. When A's finish starts (A gives its member to group commit), the lock manager hands the lock of X to the request at the head of the queue of X, when that request is the exclusive request of a vote. The hand-off gives B the staged record of X of A as the committed state of X. B then goes on with its other locks, in oid order.

- The holder hands off only an oid whose staged record is of a class that tier 1 merges in mode `on`. For another oid, the new owner could only wait for the durability of the holder, which is what the queue gives without a hand-off, so the holder keeps the lock and copies nothing.
- The hand-off comes after the member of the holder is in the queue of group commit, so a finish that starts after the hand-off is later in the commit order.
- A shared request (read-current) at the head of the queue does not take a hand-off: it waits until A releases, because its check needs the committed serial. A restore is never a holder of a hand-off: its finish can fail alone (a tid that fails its check), so its records are not final when the finish starts.

#### A2. What the vote does with a handed-off oid

At the check of the serials, the committed record of X is the staged record of A, not the current record of the store.

- Tier 1 merges: B stages the merged record over the commit of A. The reply of the vote gives X as resolved (the bare oid of ZEO 6), as for any merge of tier 1.
- Tier 1 refuses: the vote fails with `ConflictError` at once, with the reason of BTrees, as for any refusal. When the holder has no tid yet, the refusal gives no `serials`, as a lock timeout does.
- Tier 1 gives no answer, or does not run (class `off`, or `shadow`): B keeps the lock of X and waits until the commit of A is durable, then goes on with the tid of A as the committed serial. The client can then load the committed record to resolve the conflict. This wait counts in `lock-timeout`.

A second vote of B (after the client resolved other conflicts) does not check X again: B holds X, and nothing committed X between. A line of the resolution log of a merge onto a commit in flight gives the user and the description of the holder, and its tid, or 0 when its batch did not give it one yet.

#### A3. Order and durability

B's finish starts after its vote, so after A's finish: B is in A's batch after A, or in a later batch. Its tid is after the tid of A. Its client gets the acknowledgement when B's batch is durable. A failed write or sync of A's batch halts the store, so B fails with `Halted` too. So:

- An acknowledged commit is durable (N1).
- A commit is never durable before the commit that it merged onto.
- Readers see A after A's batch publishes, and B after B's batch publishes: no reader sees a state that is not durable. Invalidations go out after the sync of their batch.

The commits of a hot oid can share a batch: A, then B merged onto A, then C merged onto B, each one taking the lock when the finish before it starts. Their finishes wait in the same queue, so the next leader writes them together, with one sync. Group commit puts no limit on the commits of one batch: it takes every finish that waits.

The merged record of the new owner gets the tid of the holder as its previous serial when its batch writes it. A member whose holder got no tid fails with `Protocol`: the holder then failed alone, which happens only when no tid is left, and the member gets no tid either.

#### A4. The lock manager

- A finish that starts marks the exclusive locks of its transaction as committing, and hands each one that has an exclusive vote at the head of its queue to that vote, under the mutex of the lock table, as a release grants a lock.
- The holder makes the staged record of each handed-off oid available to the new owner. The new owner reads a copy, never the spill file of the holder, which goes away when the transaction of the holder ends.
- The release after the finish releases only the locks that the transaction still owns. A handed-off lock belongs to its new owner.
- A new owner that releases the lock before the holder is durable (a failed vote, an abort) leaves the hand-off in the entry: the next exclusive request takes the lock with it, and a shared request waits until the holder is durable. A new owner whose own commit is durable ends the hand-off that it got before it releases the lock, because the store then holds the last commit of the oid, also when the finish of the holder did not end the hand-off yet. So no vote reads the oid from the store while the store does not hold its last commit, and no vote merges onto a record that a later commit replaced. So a lock that is free, with no waiter and no hand-off, means that every commit of its oid is durable.
- A finish that waits for the end of a commit batch waits on a waiter of the store clock, as a lock wait does, so the simulator runs other threads during the sync of a batch.
- Locks stay in oid order: a hand-off only grants a lock sooner than a release would, so it adds no cycle. A wait still ends at `lock-timeout` or at a `LockCancel`.

#### A5. Both modes, and the clients

The hand-off is in `zoe-core`, so `zoe.Storage` and `zoe serve` both have it. It needs no change of the wire protocol: a stock `ZEO.ClientStorage` sees a vote that resolved an oid, as from a ZEO 6 server that resolved a conflict. A commit that writes no hot object takes the same path as without the hand-off: no waiter, no hand-off.

#### A6. Measurement

A counter `zoe_lock_handoffs_total` counts the hand-offs, and `zoe_fsync_batch_size` shows the commits per sync. The acceptance test `test_writers_on_a_hot_length_raise_no_conflict_and_lose_nothing` runs 64 writers on one `Length` and checks 0 `ConflictError` and the exact counter, on every platform. The batching is a check of the simulator: `writers_of_a_hot_length_share_the_syncs` of `crates/zoe-server/tests/simulated_hot_length.rs` asserts a mean above 2 commits for each sync, with syncs slower than a round trip. On macOS a sync is `fsync(2)` (ADR 0006), which is faster than one round trip of a client, so there the commits of the hot oid never wait in one batch, with or without the hand-off. The kill loop under the simulator, with hot `Length` writers and the hand-off on, gives 0 losses and no stale read.

### B. Early write-intent locks

#### B1. The lock before the read

The acceptance test counts every `ConflictError` that an attempt raises, not only the ones that the retries of Zope leave: the writers of `test_hot_objects.py` never retry. Only the lock before the read meets that reading; it meets the other one too.

With early locks on, an object of a class that the configuration declares hot (section B2) takes its intent lock when its state loads, before the transaction sees any byte of it:

- `zoe.zodb.LockingConnection.setstate(obj)`, for an object of a hot class, calls `lock_intent([oid])` and waits for the grant. Then it loads the current record of the oid, not the record before the snapshot, and sets the state from it. When a savepoint of the transaction holds a change of the oid, the load reads the savepoint, under the lock, as ZODB does. So a hot object that turned into a ghost after a savepoint keeps the change of the transaction.
- At the end of each transaction (`newTransaction`, `afterCompletion`) and at the close of the connection, the connection turns the hot objects of its cache into ghosts, so the first access of the next transaction loads them again, with the lock.
- The transaction holds the lock until its end: `tpc_finish`, `tpc_abort`, an abort of ZODB before `tpc_begin`, the end of a transaction of an explicit transaction manager (through `afterCompletion`), or the close of a connection that no transaction joined, which also turns its hot objects into ghosts. The close of a joined connection fails as in ZODB, and keeps the locks.
- A historical connection (`DB.open(before=...)`) takes no lock: it reads as ZODB does.

So a second writer waits before it reads, and reads the commit of the first one: it never holds a stale state of a hot object, and its vote passes the serial check. The costs, which the documentation of `early-locks` names:

- A transaction sees a hot object at a state newer than its snapshot: a read skew against the other objects. A hot class is one whose invariants do not span other objects (a counter, a queue head, a lock record).
- Every transaction that reads a hot object waits for the lock, also one that does not write it. A hot class is one that its readers nearly always write.
- A hot class must be a leaf record: a class whose state refers to no other persistent object. A hot object read past the snapshot can name a child that the snapshot does not hold, or that it holds at an older state, and the load of that child can raise `ReadConflictError`.

Fail fast, and fail fast with the lock kept for the retry, are not built: the first does not change the number of `ConflictError`s, and the second needs a lock that outlives its transaction and a change of the retry of Zope (finding 2).

#### B2. The hot classes

The client takes intent locks only for the classes that the key `early-lock-classes` names, by `module.name`, the class that ZODB pickles (`type(obj)`). The key is a list in `<zoe>` and in `<zoeclient>`, empty by default. `early-locks on` with an empty list takes no intent lock.

- A class of tier 1 in the list fails the open with `ValueError`: tier 1 and the lock hand-off serve it, and a lock before each read would only make its readers wait.
- A subclass is not hot unless the list names it, as tier 1 does.
- No class is hot by default. `zoe.plone` can name hot classes of a site later, from the findings of B5.
- The client does not learn hot oids from the conflicts that it sees: a learned set raises the first conflicts anyway, so it does not meet the acceptance test. A later card can add it.
- `early-locks on` alone does not make every class hot. With every class that tier 1 does not merge hot, every transaction would lock the root of the database at its load, so every request would wait for every other one.

#### B3. The owner of the intent locks

An intent owner holds the intent locks before `tpc_begin`. The session owns them on the server (option A1 of the spike), and the wire methods keep the form of the specification. On the client, the owner is the storage object by default, or the owner that a connection names.

- `lock_intent(oids)` takes the exclusive intent lock of each oid of `oids`, in oid order, and returns when the server holds them all for the session. `unlock_intent()` releases every intent lock of the session. Both are methods of the extension `lock_intent`: a connection that did not declare it in `register` gets `ValueError`, as for `sequences`.
- `tpc_begin` of the session adopts the intent locks of the session: the vote of that transaction counts them as its own, and `tpc_finish`, `tpc_abort` and the end of the connection release them.
- `zoe.Storage.lock_intent(oids, owner=None)` and `zoe.Storage.unlock_intent(owner=None)`, and the same methods of `zoe.zeo.ClientStorage`. Without `owner`, the locks belong to the default owner of the storage: a program that runs one transaction at a time on a storage, as the client of `test_early_locks.py`, calls them as the specification names them, from any thread, and its next `tpc_begin` adopts them.
- `zoe.zodb.LockingConnection` names an owner of its own, one for each connection, because many connections share one storage. Its `tpc_begin` binds that owner to the calling thread around the `tpc_begin` of ZODB, and the `tpc_begin` of the storage adopts the owner that the calling thread bound, or else the default owner. ZODB runs the commit of a connection on one thread. A connection on a storage with hot classes always binds its own owner, also when it took no lock: so a commit of a connection never adopts the locks of the default owner, which belong to a program that calls the storage itself.
- The native client (`zoe.zeo.ClientStorage` with `native true`) leases a pool connection to an owner at its first `lock_intent`, not at `tpc_begin`, and keeps the lease to the end of the transaction that adopts the owner, or to `unlock_intent`. So the bound of 64 connections bounds the owners that hold intent locks at once. An owner whose `lock_intent` fails when it holds no lock, or whose connection is lost, gives the connection back at once. The release of an owner that the program drops sends `unlock_intent` without a wait for the reply, and gives the connection back only after the call is in the channel of the connection, so the next transaction of the connection comes after the release.
- An end whose `unlock_intent` fails clears the state of the connection first, and logs the failure: the end of the session on the server, or the hold bound, releases the locks.
- A storage or a client that is read-only has no hot class: it writes nothing. A client whose server did not accept the extension `lock_intent`, as a stock ZEO 6 server, has no hot class either, and logs one WARN event at each connection; so does a client that runs the client of ZEO inside, on a server that does not speak M5.

The intent locks are advisory: the vote still takes its locks and checks every serial. So the server may drop an intent lock at any time, and nothing becomes wrong; at worst the transaction gets the `ConflictError` that it gets without early locks. So the clients without a hot class go on without the locks.

#### B4. The order of the locks

Intent locks come in oid order, as the specification says.

- A `lock_intent` of an oid below an intent lock that the session holds fails with `LockOrder` at once, also when the lock is free, and takes nothing. The client gets `StorageTransactionError`, the exception of ZODB to which the specification maps `LockOrder`. The caller releases its set and asks again for the whole set in oid order.
- Intent locks before the read come in the order of the loads. On a `LockOrder`, `LockingConnection` releases the intent locks of the transaction and takes them again with the new oid, in one `lock_intent`, in oid order. When the serial of each hot object that the transaction read is unchanged, the load goes on. Else it raises `ConflictError`, which ends the attempt, and the transaction manager of the connection keeps the hot oids of the attempt as a retry hint. The first load of a hot object in the next transaction of that manager takes them all with its own oid, in oid order, in one `lock_intent`, so the retry never loads out of order again. The hint is on the transaction manager, not on the connection: Zope closes the connection after an attempt, and its retry can get another one. On `transaction.manager`, the manager of the thread, the hint is the hint of the thread. The next transaction uses the hint or drops it at its end, and the transaction that kept it never uses it; so no lock and no hint outlives the next transaction.
- Without the hint, the relock nearly livelocks under contention: the release gives the locks to the transactions that wait at the head, they commit first, so the serials rarely stay unchanged, and each retry loads in the same order. In a probe of 8 threads, 25 requests each, two hot objects loaded in descending oid order and the three retries of Zope, 98 or 99 requests of 200 gave up without the hint, about 75 with early locks off, and none with it.
- A `lock_intent` of oids above every intent lock of the session waits in the queue of each oid, as a vote waits, within `lock-timeout`. A timeout fails it with `ConflictError`, and the call keeps no lock that it took.
- The vote counts the intent locks of its transaction as held, and takes the other locks in oid order. Before it waits for an oid below an intent lock that it holds, it releases the intent locks above that oid, waits, and takes them again in oid order. The serial check of the vote then catches a commit that came in between. So no two transactions wait for each other.

The lock hand-off (part A) goes only to the vote of a transaction, never to a `lock_intent`: an intent request reads the oid from the store, so it waits until the store holds the last commit of the oid, as a shared request does. An oid that a transaction holds as an intent lock and writes is an exclusive lock of its vote, and its finish can hand it off to a vote.

#### B5. Where a wait runs, and how long a hold lasts

- In `zoe serve`, a `lock_intent` runs on the thread of the transaction of the session, which starts at the first `lock_intent` of a transaction, as it starts at `tpc_begin`. It never runs on the read pool, so its wait holds no thread that the loads of other connections share. A `LockCancel` ends its wait when the connection ends, as for a vote.
- A session that holds intent locks without a `tpc_begin` for 60 s loses them: the server releases them and logs one WARN event with the oids and the label of the client. The bound is fixed, with no option. Because the locks are advisory, the drop changes no outcome but the chance of a conflict.
- A `tpc_begin` that fails, or whose adoption of the intent locks fails, releases the intent locks of the session: no transaction adopts them.

#### B6. No batch

A lock before the read must be granted before the load returns, so there is no batch of 50 ms: `setstate` sends one `lock_intent` for the object that it loads, at once, and waits for it. The hot classes are few, so a transaction sends few calls. The new request after a `LockOrder` sends the whole set in one call.

#### B7. How `early-locks on` selects the connection class

A patch of the class (option D1 of the spike). When a storage with `early-locks on` and a non-empty `early-lock-classes` opens, `zoe.zodb` installs its methods on `ZODB.Connection.Connection` once per process: `setstate`, `newTransaction`, `afterCompletion`, `tpc_begin`, `tpc_finish` and `close`. The install keeps the methods that it replaces as `Connection` has them at the install, not at the import of `zoe.zodb`, so a patch that another package made before stays in the chain. Each patched method checks `self._db.storage` first, and does exactly what the method that it replaced does for a storage without hot classes and for a historical connection. So it works in `<zoe>`, in `<zoeclient>`, under Zope and under a plain `<zodb>`, and it leaves no stock connection in a pool. `zoe.zodb.LockingConnection` is the same methods as a subclass, for a program that makes its `DB` itself.

The patch reads these internals of ZODB, and no other: of the connection, `_db`, `before`, `opened`, `_savepoint_storage` and the `index` of the savepoint, `_reader.setGhostState`, `_cache` (`get`, `invalidate`, `update_object_size_estimation`), `_load_count`, `_modified`, `_creating` and `_needs_to_join`; of the object, `type(obj)`, `_p_oid`, `_p_serial`, `_p_estimated_size`, and for a blob `_p_blob_uncommitted` and `_p_blob_committed`.

#### B8. `native false` and `readCurrent`

- `early-locks on` with `native false` in `<zoeclient>` fails the open with `ValueError`: `ZEO.ClientStorage` has one session and one commit lock for every thread of the process, so the intent locks of one thread would belong to all of them, and a deadlock would end only at `lock-timeout`.
- `readCurrent` takes its shared lock at the vote. A hot object is locked at its load already, and a shared intent lock of every node that a BTree passes would make the readers of a tree wait.

#### B9. The versions of ZODB

The patch is tested on the versions of ZODB that the lock files of the repository install (6.2 to 6.4). The spike found `Connection.py` the same in these three versions. ZODB 6.0 and 6.1 stay supported targets, as for the rest of ZOE, without a test of their own until a lock file installs them.

#### B10. The helpers of the acceptance tests

- `test_early_locks.Client.lock` and `Client.release` call `self.storage.lock_intent(list(oids))` and `self.storage.unlock_intent()`, with the default owner of the storage (section B3).
- `test_early_locks.lock_order()` gives `StorageTransactionError` (section B4).
- `test_hot_objects.database()` adds the hot class of the test with `early-locks on`. The hot object of the test is a `PersistentMapping`, which tier 1 does not merge:

```python
    lines = section.strip().splitlines()
    if early_locks is not None:
        lines.insert(-1, f"  early-locks {early_locks}")
    if early_locks == "on":
        lines.insert(-1, "  early-lock-classes persistent.mapping.PersistentMapping")
```

The root of the database is a `PersistentMapping` too, so the writers of that test also lock the root at its load. The test of the hot `Length` passes `early-locks on` with no hot class of its own: `Length` is a class of tier 1, and the lock hand-off serves it (section A6).

#### B11. What the build adds

- The core: an intent owner (`IntentLocks`), `Store::lock_intent(owner, oids)` with the rules of section B4, its adoption by `Txn::begin`, and the release at the end of the transaction, at a drop of the owner and at the hold bound of the server.
- `zoe serve`: the extension `lock_intent`, the methods `lock_intent` and `unlock_intent`, the thread of the transaction from the first `lock_intent`, the hold bound, and the counters `zoe_intent_locks_total` and `zoe_intent_lock_wait_seconds`.
- `zoe-client`, the native client: the methods, the lease at the first `lock_intent`.
- `zoe.Storage` and `zoe.zeo.ClientStorage`: `lock_intent`, `unlock_intent`; the keys `early-locks` and `early-lock-classes` and their `ZOE_*` twins.
- `zoe.zodb`: the patch and `LockingConnection`.
- `zoe-bench hot` counts a `lock_intent` that times out as a conflict, and names the mean commits of a sync `commits_per_sync`.
- The specification: section 5 ("Early write-intent locks"), section 9 (lever 3), the protocol table, the configuration table, F7 and the testing table.

## Consequences

- Section 5 of the specification states the invariant of group commit with the hand-off: a lock passes from a commit whose finish started to a vote that merges onto it, and the merge commits after it.
- The vote has a wait for the durability of a holder, when the conflict on a handed-off oid goes to the client. It is the wait for the release, moved after the lock.
- A long chain of merges on one oid fills one batch. The batch grows with the number of writers, and its write takes longer.
- The hand-off needs no on-disk format change, no wire change and no new key: it follows from tier 1 being on.
- F7 holds for a declared hot object that cannot merge: its writers wait instead of failing. It does not hold for an object that no configuration declares.
- A hot object costs a lock round trip at each load and a reload at each transaction, and a read skew within the transaction. The documentation of `early-lock-classes` says so.
- Section 5 of the specification holds the rules of section B4 for the order of intent locks.
- Stock clients see nothing new: they declare no extension, and their votes wait for an intent lock as for a vote lock.
- Vote-time locking stays the default: `early-locks` is off, and `early-lock-classes` is empty.

## Alternatives

- **The hand-off at the vote of the holder.** A has voted but can still abort: a merge onto its state would hold a change that never commits.
- **The hand-off when the batch of the holder is written** (its tid known). Then B goes to the next batch, and each batch holds one commit of the hot oid: the commits of the hot oid stay one per sync.
- **Merge in the batch, by the leader.** The leader would run tier 1 on the members that conflict. It takes Rust merges into the write path under the sequencer, and a refusal would fail a member after its vote passed.
- **No lock at all for the classes of tier 1.** The commits would merge at the finish. Same objection.
- **The intent lock in `register`.** It gives as many `ConflictError`s as vote-time locks on an object that cannot merge (the spike).
- **Fail fast at the grant.** It saves the work after the change, not the conflict.
- **Fail fast and keep the lock for the retry.** One `ConflictError` per wait, so it meets only the reading of the acceptance test that counts what the retries leave. It needs a lock that outlives its transaction, a thread that owns it across connections of the pool, and a shorter sleep in the retry of Zope.
- **A hot set that the client learns from its conflicts.** Its first conflicts still raise. A later card can add it to the declared classes.
- **A class of the configuration (`connection-class`) or a database section of ZOE.** `connection-class` leaves a stock connection in the pool, and `<zodb>` has no such key; a database section is a second place for one setting.

## Merged ADRs

- ADR 0045, early write-intent locks: Decision, part B. Its section N is section BN here, for N from 1 to 11. The changes of its section 12 are in the sections B1, B3, B4, B5, B7 and B11 that hold their subject.
