# 0045. Early write-intent locks: the lock before the read of a declared hot object

Status: accepted. A checkpoint of ZOE-204 (step 4): it adds the extension `lock_intent` to the wire protocol, the key `early-lock-classes` and a public Python API, and it changes the design of section 9 (lever 3) and section 5 ("Early write-intent locks"). The spike of step 1 (`docs/spikes/early-lock-hook.md`) left twelve questions without a proposal. Claude settled them, and the human approved this ADR as written, with the change of the helper `test_hot_objects.database()` of section 10. Each answer below says that it was Claude's choice.

## Context

The specification says: a client sends `lock_intent(oids)` when a transaction first changes an object (`Connection.register`), a second writer waits for the lock, "then re-reads the object after the first commit and writes a non-conflicting update". The spike shows that the second half does not hold:

- ZODB reads each object at the snapshot of the start of the transaction, and `register` comes after the load of the object. The snapshot moves only between transactions. So a writer that waits in `register` for a holder holds a stale state when it gets the lock, and its vote fails as it fails without the lock (finding 4).
- A refresh at the grant is not safe: the change is often in the state already when `register` runs, so a reload loses it or writes a value that came from the old state.
- In a model of four writers and 20 increments of one counter that cannot merge, locks in `register` raised 102 `ConflictError`s, vote-time locks 89 and 104. Only two mechanisms changed the outcome: fail fast and keep the lock for the retry (79 errors, one per wait, no failed increment), and the lock before the read, with the read past the snapshot (0 errors).

The acceptance test of the card asks: 64 writers on one object that cannot merge give 0 `ConflictError` with early locks on, and `ConflictError` as today with early locks off. Tier 1 (ADR 0042) and the lock hand-off (ADR 0044) already serve the objects that tier 1 merges, such as `BTrees.Length.Length`; early locks are for the other ones.

The acceptance tests are `python/zoedb/tests/acceptance/test_hot_objects.py` and `test_early_locks.py`, from the planner (`plan/ZOE-204`). In `test_hot_objects.py`, a writer never retries, and each `ConflictError` that a transaction raises counts. The tests leave four helpers to this ADR, and the human approves a change of them with it: `test_hot_objects.database()`, `test_early_locks.Client.lock`, `Client.release` and `lock_order()`. Section 10 gives them.

## Decision

### 1. The lock before the read (questions 1 and 2)

*Claude's choice, approved by the human.* The acceptance test counts every `ConflictError` that an attempt raises, not only the ones that the retries of Zope leave: the writers of `test_hot_objects.py` never retry. Only the lock before the read meets that reading; it meets the other one too.

With early locks on, an object of a class that the configuration declares hot (section 2) takes its intent lock when its state loads, before the transaction sees any byte of it:

- `zoe.zodb.LockingConnection.setstate(obj)`, for an object of a hot class, calls `lock_intent([oid])` and waits for the grant. Then it loads the current record of the oid, not the record before the snapshot, and sets the state from it.
- At the start of each transaction (`newTransaction`), the connection turns the hot objects of its cache into ghosts, so the first access of the next transaction loads them again, with the lock.
- The transaction holds the lock until its end: `tpc_finish`, `tpc_abort`, or an abort of ZODB before `tpc_begin`.

So a second writer waits before it reads, and reads the commit of the first one: it never holds a stale state of a hot object, and its vote passes the serial check. The costs, which the documentation of `early-locks` names:

- A transaction sees a hot object at a state newer than its snapshot: a read skew against the other objects. A hot class is one whose invariants do not span other objects (a counter, a queue head, a lock record).
- Every transaction that reads a hot object waits for the lock, also one that does not write it. A hot class is one that its readers nearly always write.

Fail fast, and fail fast with the lock kept for the retry, are not built: the first does not change the number of `ConflictError`s, and the second needs a lock that outlives its transaction and a change of the retry of Zope (finding 2).

### 2. The hot classes (questions 2 and 9)

*Claude's choice, approved by the human.* The client takes intent locks only for the classes that the key `early-lock-classes` names, by `module.name`, the class that ZODB pickles (`type(obj)`). The key is a list in `<zoe>` and in `<zoeclient>`, empty by default. `early-locks on` with an empty list takes no intent lock.

- A class of tier 1 in the list fails the open with `ValueError`: tier 1 and the lock hand-off serve it, and a lock before each read would only make its readers wait.
- A subclass is not hot unless the list names it, as tier 1 does.
- No class is hot by default. `zoe.plone` can name hot classes of a site later, from the findings of B5.
- The client does not learn hot oids from the conflicts that it sees: a learned set raises the first conflicts anyway, so it does not meet the acceptance test. A later card can add it.
- `early-locks on` alone does not make every class hot. With every class that tier 1 does not merge hot, every transaction would lock the root of the database at its load, so every request would wait for every other one.

### 3. The owner of the intent locks (question 3)

*Claude's choice, approved by the human.* An intent owner holds the intent locks before `tpc_begin`. The session owns them on the server (option A1 of the spike), and the wire methods keep the form of the specification. On the client, the owner is the storage object by default, or the owner that a connection names.

- `lock_intent(oids)` takes the exclusive intent lock of each oid of `oids`, in oid order, and returns when the server holds them all for the session. `unlock_intent()` releases every intent lock of the session. Both are methods of the extension `lock_intent`: a connection that did not declare it in `register` gets `ValueError`, as for `sequences`.
- `tpc_begin` of the session adopts the intent locks of the session: the vote of that transaction counts them as its own, and `tpc_finish`, `tpc_abort` and the end of the connection release them.
- `zoe.Storage.lock_intent(oids, owner=None)` and `zoe.Storage.unlock_intent(owner=None)`, and the same methods of `zoe.zeo.ClientStorage`. Without `owner`, the locks belong to the default owner of the storage: a program that runs one transaction at a time on a storage, as the client of `test_early_locks.py`, calls them as the specification names them, from any thread, and its next `tpc_begin` adopts them.
- `zoe.zodb.LockingConnection` names an owner of its own, one for each connection, because many connections share one storage. Its `tpc_begin` binds that owner to the calling thread around the `tpc_begin` of ZODB, and the `tpc_begin` of the storage adopts the owner that the calling thread bound, or else the default owner. ZODB runs the commit of a connection on one thread.
- The native client (`zoe.zeo.ClientStorage` with `native true`) leases a pool connection to an owner at its first `lock_intent`, not at `tpc_begin`, and keeps the lease to the end of the transaction that adopts the owner, or to `unlock_intent`. So the bound of 64 connections bounds the owners that hold intent locks at once.

The intent locks are advisory: the vote still takes its locks and checks every serial. So the server may drop an intent lock at any time, and nothing becomes wrong; at worst the transaction gets the `ConflictError` that it gets without early locks.

### 4. The order of the locks (question 4)

*Claude's choice, approved by the human.* The rule of the specification stays: intent locks come in oid order.

- A `lock_intent` of an oid below an intent lock that the session holds fails with `LockOrder` at once, also when the lock is free, and takes nothing. The client gets `StorageTransactionError`, the exception of ZODB to which the specification maps `LockOrder`. The caller releases its set and asks again for the whole set in oid order.
- Intent locks before the read come in the order of the loads. So `LockingConnection` turns a `LockOrder` into `ConflictError`, which ends the attempt, and keeps the hot oids of the attempt. The next attempt of the connection takes them all in one `lock_intent`, in oid order, at its first load of a hot object.
- A `lock_intent` of oids above every intent lock of the session waits in the queue of each oid, as a vote waits, within `lock-timeout`. A timeout fails it with `ConflictError`, and the call keeps no lock that it took.
- The vote counts the intent locks of its transaction as held, and takes the other locks in oid order. Before it waits for an oid below an intent lock that it holds, it releases the intent locks above that oid, waits, and takes them again in oid order. The serial check of the vote then catches a commit that came in between. So no two transactions wait for each other.

The lock hand-off (ADR 0044) goes only to the vote of a transaction, never to a `lock_intent`: an intent request reads the oid from the store, so it waits until the store holds the last commit of the oid, as a shared request does. An oid that a transaction holds as an intent lock and writes is an exclusive lock of its vote, and its finish can hand it off to a vote.

### 5. Where a wait runs, and how long a hold lasts (questions 5 and 6)

*Claude's choice, approved by the human.*

- In `zoe serve`, a `lock_intent` runs on the thread of the transaction of the session, which starts at the first `lock_intent` of a transaction, as it starts at `tpc_begin` today. It never runs on the read pool, so its wait holds no thread that the loads of other connections share. A `LockCancel` ends its wait when the connection ends, as for a vote.
- A session that holds intent locks without a `tpc_begin` for 60 s loses them: the server releases them and logs one WARN event with the oids and the label of the client. The bound is fixed, with no option. Because the locks are advisory, the drop changes no outcome but the chance of a conflict.

### 6. No batch (question 7)

*Claude's choice, approved by the human.* A lock before the read must be granted before the load returns, so there is no batch of 50 ms: `setstate` sends one `lock_intent` for the object that it loads, at once, and waits for it. The hot classes are few, so a transaction sends few calls. The retry after a `LockOrder` sends the hot oids of the failed attempt in one call.

### 7. How `early-locks on` selects the connection class (question 8)

*Claude's choice, approved by the human.* A patch of the class (option D1 of the spike). When a storage with `early-locks on` and a non-empty `early-lock-classes` opens, `zoe.zodb` installs its methods on `ZODB.Connection.Connection` once per process: `setstate`, `newTransaction`, `tpc_begin`, `tpc_finish`, `tpc_abort` and `abort`. Each patched method checks `self._db.storage` first, and does exactly what ZODB does for a storage without early locks. So it works in `<zoe>`, in `<zoeclient>`, under Zope and under a plain `<zodb>`, and it leaves no stock connection in a pool. `zoe.zodb.LockingConnection` is the same methods as a subclass, for a program that makes its `DB` itself. The patch reads `_p_oid`, `_p_serial`, `type(obj)`, `_db.storage` and `_storage`, and no other internal of ZODB.

### 8. `native false` and `readCurrent` (questions 10 and 11)

*Claude's choice, approved by the human.*

- `early-locks on` with `native false` in `<zoeclient>` fails the open with `ValueError`: `ZEO.ClientStorage` has one session and one commit lock for every thread of the process, so the intent locks of one thread would belong to all of them, and a deadlock would end only at `lock-timeout`.
- `readCurrent` takes its shared lock at the vote, as now. A hot object is locked at its load already, and a shared intent lock of every node that a BTree passes would make the readers of a tree wait.

### 9. The versions of ZODB (question 12)

*Claude's choice, approved by the human.* The patch is tested on the versions of ZODB that the lock files of the repository install (6.2 to 6.4 today). The spike found `Connection.py` the same in these three versions. ZODB 6.0 and 6.1 stay supported targets, as for the rest of ZOE, without a test of their own until a lock file installs them.

### 10. The helpers of the acceptance tests

The planner left four helpers to this ADR. With the decisions above:

- `test_early_locks.Client.lock` and `Client.release` stay as they are: `self.storage.lock_intent(list(oids))` and `self.storage.unlock_intent()`, with the default owner of the storage (section 3).
- `test_early_locks.lock_order()` stays as it is: `StorageTransactionError` (section 4).
- `test_hot_objects.database()` adds the hot class of the test with `early-locks on`. The hot object of the test is a `PersistentMapping`, which tier 1 does not merge. The human approved the change:

```python
    lines = section.strip().splitlines()
    if early_locks is not None:
        lines.insert(-1, f"  early-locks {early_locks}")
    if early_locks == "on":
        lines.insert(-1, "  early-lock-classes persistent.mapping.PersistentMapping")
```

The root of the database is a `PersistentMapping` too, so the writers of that test also lock the root at its load. The test of the hot `Length` passes `early-locks on` with no hot class of its own: `Length` is a class of tier 1, and the lock hand-off serves it.

- The human also approved a second change of `test_hot_objects.py`: the test of the hot `Length` runs on Linux only, with a reason. On macOS a sync is `fsync(2)` (ADR 0040), which is faster than one round trip of a client, so the commits of the hot oid never wait in one batch, with or without the hand-off. Linux CI gives its evidence; the simulator gives the evidence of the mechanism (`crates/zoe-server/tests/simulated_hot_length.rs`).

### 11. What the build adds

- The core: an intent owner (`IntentLocks`), `Store::lock_intent(owner, oids)` with the rules of section 4, its adoption by `Txn::begin`, and the release at the end of the transaction, at a drop of the owner and at the hold bound of the server.
- `zoe serve`: the extension `lock_intent`, the methods `lock_intent` and `unlock_intent`, the thread of the transaction from the first `lock_intent`, the hold bound, and the counters `zoe_intent_locks_total` and `zoe_intent_lock_wait_seconds`.
- `zoe-client`, the native client: the methods, the lease at the first `lock_intent`.
- `zoe.Storage` and `zoe.zeo.ClientStorage`: `lock_intent`, `unlock_intent`; the keys `early-locks` and `early-lock-classes` and their `ZOE_*` twins.
- `zoe.zodb`: the patch and `LockingConnection`.
- The specification: section 5 ("Early write-intent locks"), section 9 (lever 3), the protocol table, the configuration table, F7 and the testing table.

## Consequences

- F7 holds for a declared hot object that cannot merge: its writers wait instead of failing. It does not hold for an object that no configuration declares.
- A hot object costs a lock round trip at each load and a reload at each transaction, and a read skew within the transaction. The documentation of `early-lock-classes` says so.
- The rule "intent locks in oid order, `LockOrder` for a lower oid" of the specification changes to the rules of section 4.
- Stock clients see nothing new: they declare no extension, and their votes wait for an intent lock as for a vote lock.
- Vote-time locking stays the default: `early-locks` is off, and `early-lock-classes` is empty.

## Alternatives

- **The lock in `register`, as the specification says.** It gives as many `ConflictError`s as vote-time locks on an object that cannot merge (the spike). Refused.
- **Fail fast at the grant.** Saves the work after the change, not the conflict. Refused.
- **Fail fast and keep the lock for the retry.** One `ConflictError` per wait, so it meets only the reading of the acceptance test that counts what the retries leave. It needs a lock that outlives its transaction, a thread that owns it across connections of the pool, and a shorter sleep in the retry of Zope. Refused for now.
- **A hot set that the client learns from its conflicts.** Its first conflicts still raise. A later card can add it to the declared classes.
- **A class of the configuration (`connection-class`) or a database section of ZOE.** `connection-class` leaves a stock connection in the pool, and `<zodb>` has no such key; a database section is a second place for one setting. Refused for the patch.
