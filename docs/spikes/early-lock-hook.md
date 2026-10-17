# Spike: the early-lock hook in ZODB

Card ZOE-204, step 1; requirements F7 and N5, and F13 for the negotiation. The ADR of the lock hand-off (step 2) and the ADR of `lock_intent` (step 4) decide the open questions at the end.

**Verdict.** The hook exists, and it is the same on each ZODB that this spike could read. `ZODB/Connection.py` is byte for byte the same in ZODB 6.2, 6.3 and 6.4, and persistent calls `register` in the same places in 5.1, 6.5, 6.8 and the fork 6.8+zoe.1. A subclass of `Connection` sees the first change of each object, on the thread that changes it, after the object got its state. But a lock taken in `register` does not prevent a conflict on an object that cannot merge. ZODB reads each object at the snapshot of the start of the transaction, and nothing moves that snapshot before the next transaction. So a writer that waits for the intent lock of another writer holds a stale state when it gets the lock, and its vote fails as it fails without the lock. In a model with four writers that each increment one counter 20 times, locks in `register` raised 102 `ConflictError`s, and 24 of the 80 increments failed after four attempts. Vote-time locks raised 89 and 104, with 20 and 24 failures. Two additions remove the failures: the waiter fails at once and keeps its lock for its retry (no failure, one `ConflictError` for each wait), or the transaction takes the lock before it reads a known hot object and reads that object past its snapshot (no `ConflictError`). The specification names neither. So the acceptance test "64 writers on one object that cannot merge give 0 `ConflictError`" needs a decision of the human before the build. Three more points need a decision. Nothing names a ZODB transaction to the storage before `tpc_begin`, so the owner of an intent lock is new state in the client and in the server. `<zoe>` opens the storage before the DB exists, so `early-locks on` cannot set `DB.klass` alone. Zope's `connection-class` sets `DB.klass` too late for the first connection of the pool.

## Question

Step 1 of the card: the early-lock hook in ZODB (`Connection.register`) on each supported ZODB version (ZODB 6.0 or later, ZEO 6.0 or later, Plone 6.1 or later). The specification (section 9, lever 3) says that `zoe.zodb.LockingConnection` overrides `register` and calls `storage.lock_intent([oid])` for each changed object that has an oid, in one call per 50 ms or at the vote. Step 5 of the card adds that the client skips the classes that tier 1 merges. The spike answers:

1. When does persistent call `jar.register(obj)`, on which thread, and in which cases?
2. How do `Connection.register` and `_register` work, how does a subclass hook in, how does a DB use a subclass, can ZConfig select it, and how do Zope and Plone open connections?
3. Which storage object does a connection talk to, how does `lock_intent` reach `zoe.Storage` and `zoe.zeo.ClientStorage`, and which server-side transaction owns an intent lock taken before `tpc_begin`?
4. How does the hook meet `readCurrent`, savepoints, the classes of tier 1, new objects, `Connection.add` and cross-database references?
5. What differs between the versions that are installed?
6. What is the smallest hook that the specification allows, how does it fail, and what must the ADRs of the card decide?

## Method

- **Source.** The spike read the source of the installed packages. A path of a Python package is relative to its `site-packages`. A line number is that of the first version in the table below, unless the text names another version. A path of the repository is relative to its root.

| Package | Versions read | Where |
| --- | --- | --- |
| ZODB | 6.3; 6.2; 6.4; 5.8.1 for contrast only | the virtual environment of the Python tests; the eggs of the Plone instance (`target/plone`); the uv cache |
| persistent | 6.8+zoe.1; 6.5; 6.8; 5.1 | the virtual environment; the Plone eggs; the uv cache |
| BTrees | 6.5+zoe.3; 6.3 | the virtual environment; the Plone eggs |
| transaction | 5.1 | the virtual environment |
| ZEO | 6.2 | the virtual environment |
| Zope, Products.CMFCore, plone.testing | 5.14.2, 3.7, 9.0.7 | the Plone eggs (Plone 6.1.5) |

- **Probes.** Four small scripts ran on CPython 3.13.15 with the GIL, in the virtual environment of the Python tests. They stay outside the repository.
  - `probe_register.py` records each call of `register` from a `Connection` subclass: the class, the oid, `_p_serial == z64`, `oid in _added`, the state at the call and the thread. It runs fourteen cases on a `MappingStorage`, with the C persistent and with `PURE_PYTHON=1`.
  - `probe_snapshot.py` opens two connections on one `zoe.Storage` in a scratch directory. The second writer reads a counter, the first writer commits a change of it, and the second writer tries to read the change.
  - `probe_klass.py` sets `DB.klass` after the DB exists, as Zope does, and opens connections. It also gives `<zodb>` a `connection-class` key, and changes an object of a connection from a second thread.
  - `probe_model.py` is a model of early locks. Each writer has its own connection and increments one counter without conflict resolution in `tm.attempts(4)`, with 2 ms of work after the change, as a request does work after its change. The lock table is a stand-in in Python for the lock manager of the store: one exclusive lock for each oid, owned by a connection, a queue in arrival order, and a release that gives the lock to the head of the queue before the waiter wakes up (`crates/zoe-core/src/lock/mod.rs:1-19`). This hand-off matters: a lock that the releasing thread can take again before the waiters wake up hides the contention. The model has five modes: `vote` (no intent lock), `early` (the lock in `register`, after `super().register`), `failfast` (as `early`, and after a wait it raises `ConflictError` when the current serial of the oid is not `obj._p_serial`), `retain` (as `failfast`, and the connection keeps that lock for its next transaction) and `lockread` (hot objects become ghosts at each `newTransaction`; `setstate` takes the lock and then loads the current record, past the snapshot).
- **Machine.** Apple M3 Pro. Another job measured performance on the machine at the same time, so each probe ran on one core for less than a second. The counts of the model are the evidence; its times are indicative only.

## Findings

### 1. When persistent calls `register`

- **On a change of a loaded object, once.** The C function `changed` calls `jar.register(self)` only when the state is up to date or sticky and the object has a jar, and then sets the state to changed (`persistent/cPersistence.c:317-348`). A ghost never calls it directly: `__setattr__` loads the state first (`cPersistence.c:1065`), and so does `_p_changed = True` on a ghost (`cPersistence.c:1213-1219`, probe case 12). A second change in the same transaction does not call it (probe case 2). Names that start with `_p_` or `_v_` never call it (`cPersistence.c:1063-1073`).
- **Not during a load.** `unghostify` sets the state to changed while `jar.setstate` runs (`cPersistence.c:127`), so a `__setstate__` that sets attributes does not register. The Python implementation does the same (`persistent/persistence.py:390-422`, `412`).
- **Again after the state goes back to up to date.** `_p_changed = False` sets the state to up to date without an unregister (`cPersistence.c:1225-1227`), so the next change calls `register` again (probe case 3). A savepoint stores each changed object and sets `_p_changed = 0` (`ZODB/Connection.py:618-620`), so a change after a savepoint calls `register` again (probe case 4). A savepoint rollback invalidates the objects (`Connection.py:998-1008`), so a change after it calls `register` again too (probe case 5). The hook gets the same oid more than once in one transaction.
- **Before or after the change.** The C `__setattr__` calls `register` before it sets the attribute (`cPersistence.c:1068-1075`): in probe case 1, `register` saw the old value. The Python implementation sets the attribute first (`persistence.py:305`) and then registers (`persistence.py:314`). Many mutators change the state before they call `register` in both implementations: `PersistentMapping.__setitem__` (`persistent/mapping.py:80-82`), each change of a bucket (`BTrees/BucketTemplate.c:395-399`, `404-433`, `446-473`) and a change of a BTree node (`BTrees/BTreeTemplate.c:880`). Probe case 8 saw the new key at the call.
- **So `register` must not fail before `super().register(obj)`.** An exception there leaves a changed object in the cache that is not in `_registered_objects`, and the abort invalidates only those (`Connection.py:423-457`). The object then keeps the change into the next transaction. Stock ZODB has the same exposure when `join` raises: with an explicit transaction manager, a change outside a transaction raises `NoTransaction` from `_register` (probe case 14).
- **On the thread that changes the object.** `register` runs on the thread that changes the object. `_register` joins the transaction of the transaction manager of the connection, not a transaction of the calling thread (`Connection.py:825-837`; `probe_klass.py`: a change from a second thread registered on that thread and joined the manager of the connection). ZODB lets one thread at a time use a connection, so in Zope this is the thread of the request.
- **Outside a transaction.** With a manager that is not explicit, as Zope's `transaction.manager`, `get()` creates a transaction (`transaction/_manager.py:84-90`). So a change between two transactions begins an implicit transaction (probe case 13), and the next `begin()` aborts it (`_manager.py:71-79`).
- **New objects.** `Connection.add` registers through `_register`, not through `register` (`Connection.py:218-228`). A later change of the added object calls `register`, which returns at once for an oid in `_added` (`Connection.py:820-822`, probe case 6). A new object that a savepoint stored has an oid and `_p_serial == z64`, and it is not in `_added` (probe case 7). So the test for a new object is `obj._p_serial == z64`. A new object that only the commit reaches gets its oid and jar during the commit and calls no `register` before.
- **In Plone, the catalog registers last.** The indexing queue of CMFCore indexes in a before-commit hook (`Products/CMFCore/indexing.py:304`, `316-318`), and the hooks run before `tpc_begin` (`transaction/_transaction.py:267-273`). So the changes of the catalog register just before the vote. Only the content object and its annotations register earlier in the request.

### 2. `register`, the subclass, `DB.klass`, ZConfig and Zope

- **The hook point.** `Connection.register` (`Connection.py:805-823`) asserts that the object belongs to the connection, refuses an object without an oid, returns for an object in `_added`, and calls `_register`. Its docstring says that a subclass can override it (`Connection.py:808-809`). A subclass that calls `super().register(obj)` keeps every behaviour of ZODB.
- **`DB.klass`.** `klass` is a class attribute of `DB` (`ZODB/DB.py:343`). `DB.__init__` takes no `klass` argument (`DB.py:349-364`). `DB.open` makes a new connection from `self.klass` only when the pool has no free connection (`DB.py:775-783`). `DB.__init__` opens a connection to create the root and leaves it in the pool (`DB.py:479-486`). So a `klass` set after the DB exists leaves one stock `Connection` in the pool, and the next `open()` returns it (`probe_klass.py`: "first open after klass: Connection"). The pool gives out the free connection with the most objects in its cache (`DB.py:126-135`, `196-208`), so the stock connection can serve requests for the life of the process. A subclass of `DB` with its own `klass`, or `db.pool.clear()` after the change, gives only the subclass (`probe_klass.py`).
- **ZConfig.** `<zodb>` has no key for the connection class in any version read. ZODB 6.3 added only `class-factory` (`ZODB/component.xml:327-335`), and `probe_klass.py` got `ConfigurationError: 'connection-class' is not a known key name`. Zope's `<zodb_db>` has `connection-class` (`Zope2/Startup/wsgischema.xml:56-62`). `ZopeDatabase.open` sets `DB.klass` after `ZODBDatabase.open` made the DB (`Zope2/Startup/datatypes.py:140-147`), so the first connection of the pool stays stock.
- **`<zoe>` cannot reach the DB.** `ZODBDatabase.open` opens the storage section first, and then makes the DB (`ZODB/config.py:163-195`). The storage sees the DB only as the `MVCCAdapter` that `registerDB` gets (`ZODB/mvccadapter.py:53-54`), and the adapter has no reference to the DB. So `early-locks on` in `<zoe>` or `<zoeclient>` can switch the storage, but it cannot set `DB.klass` by itself.
- **Zope opens each connection through `db.open()`.** `ZApplicationWrapper.__call__` (`App/ZApplication.py:39-49`), the `load_app` of each attempt of a request (`ZPublisher/WSGIPublisher.py:298-310`) and the startup (`Zope2/App/startup.py:104-111`) call `db.open()`. So the class in `DB.klass` applies to each connection that the pool makes. The startup sends `DatabaseOpened` after it opened each database once (`startup.py:114`).
- **Mount points and other databases.** The Plone 6.1 eggs have no mount-point product. `Connection.get_connection` opens another database with that database's own `open()` (`Connection.py:350-361`), so each DB applies its own `klass`. In the eggs, only `five.intid` calls `get_connection` outside ZODB. `plone.testing` puts a `DemoStorage` over the storage of a test layer (`plone/testing/zodb.py:25-29`), so `db.storage` is not a ZOE storage in the tests of Plone.
- **The retry of a request.** Zope runs each attempt in its own `load_app`, which opens a connection and closes it at the end (`WSGIPublisher.py:298-310`, `380-403`). Before a retry, it sleeps a random time up to `2 ** retry_count` seconds (`ZPublisher/HTTPRequest.py:180-182`). The retry can get another connection from the pool.

### 3. The storage behind a connection, and the owner of an intent lock

- **The path to `lock_intent`.** `conn._storage` is an `MVCCAdapterInstance` (`Connection.py:124-137`). It forwards only a fixed list of methods (`mvccadapter.py:23-41`, `106-109`), so `conn._storage.lock_intent` raises `AttributeError` (`probe_snapshot.py`: the instance has no `sequence` either). During a savepoint, `conn._storage` is a `TmpStore` (`Connection.py:976-978`). The stable path is `conn._db.storage`, the storage object itself (`probe_snapshot.py`: `conn._db.storage is conn._storage._storage`). All the connections of a DB share that one object, for `zoe.Storage` and for `zoe.zeo.ClientStorage`.
- **Nothing names the transaction before `tpc_begin`.** ZODB calls the `tpc_begin` of the storage only from `Connection.tpc_begin`, with a new `TransactionMetaData` each time (`Connection.py:465-477`). `zoe.Storage` keys its open transactions by the address of that object (`crates/zoe-py/src/storage.rs:722-735`). The native client keys them by the same object (`python/zoedb/src/zoe/zeo.py:1558-1584`), and the pool leases a connection to the server only in `begin_with`, at `tpc_begin` (`crates/zoe-client/src/pool.rs:844-861`). So no object exists before `tpc_begin` that the storage can attach an intent lock to.
- **The server keeps no lock between transactions.** A session holds its registration, its iterators, its label and its `LockCancel` (`crates/zoe-server/src/protocol/session.rs:485-507`). The transaction of a session exists from `tpc_begin` to the end of the transaction (`session.rs:1466-1480`, `1680`, `1708`, `1720`). The core keeps the locks of a vote in its `Txn` (`crates/zoe-core/src/commit/mod.rs:125`, `758-786`). The lock manager counts the holders of an oid and does not know who holds it (`crates/zoe-core/src/lock/mod.rs:169-176`, `263-266`). So a transaction that asks again for a lock that it holds waits for itself until the lock timeout, and the vote must know which intent locks belong to its transaction.
- **Where a waiting call runs.** The server runs the calls of a transaction in order on one thread, and the other calls on the read pool (specification, "Connection handling"). A `lock_intent` before `tpc_begin` is not a call of a transaction today. On the read pool, its wait holds a thread that the loads of every connection share.
- **One connection for all the threads.** `ZEO.ClientStorage` runs one transaction at a time and holds a commit lock from `tpc_begin` to the end (`ZEO/ClientStorage.py:344`, `907-911`). `zoe.zeo.ClientStorage` with `native=False` keeps it. Its one session would then hold the intent locks of every thread of the process. The native client runs each transaction on its own connection of a pool of at most 64 (`zeo.py:441`; ADR 0036).

### 4. A writer that waits cannot read the change of the holder

- **The snapshot moves only between transactions.** `MVCCAdapterInstance.load` reads before `_start` (`mvccadapter.py:161-190`). Only `poll_invalidations` moves `_start` (`mvccadapter.py:143-159`), and only `newTransaction` calls it (`Connection.py:727-735`), at the start of a transaction and after its end. `register` comes after the load (finding 1). So a writer that waits in `register` for a holder holds the state of its snapshot, and the holder commits after that snapshot. When the holder changed the object, the writer is stale.
- **Probe on `zoe.Storage`.** Writer 2 read the counter at 0. Writer 1 committed 1. Writer 2 invalidated the object and read it again: 0. `conn._storage.load` gave the old serial, and the storage itself gave the new one. The snapshot did not move. The commit of writer 2 raised `ConflictError` (serial this transaction started with: the old one). Its retry read 1.
- **A refresh at the grant is not safe.** The writer read the value before the change, and the change is often in the state already when `register` runs (finding 1). A reload of the object at the grant then loses the change, or writes a value that came from the old state. Both are lost updates.
- **The model.** Conflicts are the `ConflictError`s that the attempts raised, retried or not. Failed increments are those that still failed after four attempts. No mode lost an update: each final value is the number of increments that succeeded.

| Mode | 4 writers × 20, run 1: conflicts / failed / waits | run 2 | 8 writers × 10: conflicts / failed / waits |
| --- | --- | --- | --- |
| `vote`: vote-time locks | 89 / 20 / 0 | 104 / 24 / 0 | 157 / 35 / 0 |
| `early`: the lock in `register` | 102 / 24 / 148 | 102 / 24 / 148 | 181 / 43 / 216 |
| `failfast`: `early`, then fail at once when stale | 102 / 24 / 148 | 102 / 24 / 148 | 181 / 43 / 216 |
| `retain`: `failfast`, and keep the lock for the retry | 79 / 0 / 79 | 79 / 0 / 79 | 79 / 0 / 79 |
| `lockread`: the lock before the read, the read past the snapshot | 0 / 0 / 79 | 0 / 0 / 79 | 0 / 0 / 79 |

- `early` and `failfast` give the same conflicts. `failfast` saves the work after the change: `early` took between two and five times as long as `vote` and `failfast` in each run, because each waiter waits, works and then fails.
- `retain` fails each waiter once. Its retry starts after the grant, so it reads the change of the holder, and it holds the lock, so it does not wait again. In the model, the retry runs on the same connection, through `tm.attempts`. In Zope, a retry closes its connection, sleeps, and can get another connection (finding 2).
- `lockread` makes every reader of a hot object wait, also a reader that does not write it, and it gives the transaction one object that is newer than its snapshot (a read skew against the other objects).

### 5. `readCurrent`, savepoints, tier 1, new objects, `add` and other databases

- **`readCurrent`.** A change of a BTree calls `readCurrent` on each node that it passes (`BTrees/BTreeTemplate.c:761`; Python `BTrees/_base.py:1051`, `1123`; BTrees 6.3 at `BTreeTemplate.c:754`). The connection keeps `{oid: serial}` (`Connection.py:839-843`), sends it with `checkCurrentSerialInTransaction` after its stores (`Connection.py:496-502`), and drops an oid that it writes (`Connection.py:617`). In probe case 10, an insert into a tree of 200 keys registered only its bucket and made the root read-current. `readCurrent` is a method of its own on the jar, so a shared intent lock at that time needs a second override.
- **Savepoints.** The hook gets an oid again after a savepoint and after a rollback (finding 1). `_commit_savepoint` stores the records of the savepoint at the commit (`Connection.py:1010-1046`). An oid whose change a rollback undid can keep its intent lock to the end of the transaction. The vote must not treat it as a write.
- **The classes of tier 1.** ZODB pickles `type(obj)` as the class of a record (`ZODB/serialize.py:383`). `type(obj)` gives `BTrees.OOBTree.OOBucket` and `BTrees.Length.Length` with the C and with the Python implementation (probe cases 9 and 10). Tier 1 merges a class by its exact module and name: the buckets, sets, BTrees and tree sets of each family but `fs`, and `BTrees.Length.Length` (`crates/zoe-pickle/src/btrees.rs:62-65`, `98-118`). So a subclass is not merged, and the client must test the exact class, not `isinstance`. A dict keyed by the class object gives one lookup for each call. A BTree of more than one bucket is a class of tier 1, but tier 1 merges a tree only when it holds one bucket inline (`crates/zoe-core/src/merge/mod.rs:9-12`). So a skip by class also skips the inner nodes of large trees, which tier 1 does not merge. The modes of tier 1 for each class (`structural-merges`, `structural-merge-classes`, shadow) belong to the store, and the client does not know them.
- **New objects and `add`.** The hook skips `obj._p_serial == z64` (finding 1). Their oids come from `new_oid`, so they are usually above the oids of the objects that exist, and the vote locks them after the intent locks.
- **Other databases.** An object of another database has the connection of that database as its jar, and `register` asserts it (`Connection.py:813`). Each DB needs its own connection class and its own storage with the extension.

### 6. The versions

| Package | What differs for the hook |
| --- | --- |
| ZODB 6.2, 6.3, 6.4 | `Connection.py` and `mvccadapter.py` have the same digest in the three versions. `DB.py` of 6.3 and 6.4 adds the `class_factory` argument. `config.py` and `component.xml` of 6.3 and 6.4 add `class-factory`. Nothing else. |
| ZODB 5.8.1 (contrast) | `Connection.py`, `DB.py` and `mvccadapter.py` differ only in the syntax of Python 2. |
| persistent 5.1, 6.5, 6.8, 6.8+zoe.1 | `changed` has the same body in all four. The fork interns the name `register` at the start of the module, and the others intern it in the function. Plone's 6.5 has `changed` at `cPersistence.c:251` and `Per_setattro` at `947`. |
| BTrees 6.3, 6.5+zoe.3 | `readCurrent` in `_BTree_set` at the same place. |
| ZODB 6.0, 6.1; ZEO 6.0, 6.1 | Not installed, not read. |

## Options

### The smallest hook that the specification allows

1. `zoe.zodb.LockingConnection`, a subclass of `ZODB.Connection.Connection`. It overrides `register`, `tpc_begin`, `tpc_finish`, `tpc_abort` and `abort`. It reads `_p_oid`, `_p_serial`, `type(obj)` and `_db.storage`, and no other internal of ZODB.
2. `register(obj)` calls `super().register(obj)` first. Then it skips the object when the storage did not negotiate `lock_intent`, when `obj._p_serial == z64`, when the exact class is a class of tier 1, or when the transaction sent the oid already. Else it adds the oid to the batch of the connection.
3. A batch goes out from `register`, on the thread of the transaction, in oid order, when its oldest oid waited 50 ms or more. The call waits for the grant. The vote locks the oids that are still in the batch, so `tpc_begin` sends none.
4. `tpc_begin` gives the intent locks of the connection to the transaction of the storage, so the vote does not ask for them again. `tpc_finish` and `tpc_abort` end them with the transaction. `abort` releases them when the transaction ends before `tpc_begin`.
5. A `LockOrder` releases the whole set, and the call asks again for the whole set in order. A lock timeout raises `ConflictError` from `register`, after `super().register`, so the abort invalidates the object.

How it fails:

- It does not meet F7 for an object that cannot merge: each waiter is stale (finding 4). It only changes who wins: the first to change, not the first to vote.
- A lock lasts from its batch to the end of the commit. In Zope, the commit comes after the response is made (`WSGIPublisher.py:172-186`), so the lock also covers the rendering.
- The vote of a stock client, or of any transaction that writes an oid that another transaction holds as an intent lock, waits behind the holder for at most `lock-timeout`, and holds the locks of its own vote while it waits.
- A thread that never ends its transaction holds its intent locks without a bound: in-process to the end of the process, through `zoe serve` until its connection closes.
- With `native=False`, a thread in its vote can wait for an intent lock of a thread that waits for the commit lock of the client. Only the lock timeout ends it.
- A batch with an oid below a held oid gets `LockOrder`. The hot objects of a Plone site are often old, with low oids, and its content is newer, with higher oids, so the second request can be frequent. Not measured.
- The vote takes its locks in oid order. Intent locks above an oid that the vote must wait for break that order, unless the vote releases them first.
- `<zoe>` alone cannot select the class, and Zope's `connection-class` leaves one stock connection in the pool (finding 2).
- In the tests of Plone, the storage of the DB is a `DemoStorage`, so the hook does nothing there.
- A `register` that raises before `super().register` corrupts the cache of the connection (finding 1).

### A. The owner of the intent locks before `tpc_begin`

- **A1. The session owns them.** As the protocol table of the specification gives the methods, `lock_intent(oids)` and `unlock_intent()` take no owner. The server keeps one intent set for each session and gives it to the next `tpc_begin` of that session. In-process, the connection holds an owner object of the core and gives it to its transaction after `tpc_begin`. The native client must lease its pool connection at the first batch, not at `tpc_begin`, and keep it to the end of the transaction. So a lease lasts most of a request, and the bound of 64 limits the transactions that hold intent locks. `native=False` cannot work this way: one session serves every thread.
- **A2. The client names the owner.** `lock_intent(owner, oids)` on any connection, the listener too, and `tpc_begin` names the owner. A session holds many owners, and the end of the connection releases them all. It changes the form of the methods in the specification.
- **A3. The storage transaction begins early.** The first batch begins the transaction of the storage. ZODB knows the user, the description and the extension only at the commit, and Zope sets them late in the request, so the protocol needs one more call to give them. The one transaction of a session is then busy for the whole request, and an abort of ZODB before `tpc_begin` must call `tpc_abort`.

In each option the intent locks are advisory. The vote still takes its locks and checks every serial. So the server can drop an intent lock at any time (a lost connection, a bound on the hold, a `LockOrder`), and nothing becomes wrong.

### B. A waiter that reads after the grant

- **B0. Nothing more.** The specification as written. The gain is the order of the writers. The acceptance test on an object that cannot merge fails.
- **B1. Fail fast.** The grant gives the current serial of each oid. When it differs from `obj._p_serial`, the hook raises `ConflictError` at once. It saves the work after the change, and gives the same conflicts (model).
- **B2. Fail fast and keep the lock for the retry.** No failed increment in the model, one `ConflictError` for each wait. The owner must outlive its transaction, and the retry must find it. In Zope, the retry gets a connection from the pool after a sleep (finding 2), so the owner belongs to the thread, and `zoe.plone` must shorten the sleep of the retry. The retry must start its snapshot after the commit of the holder: through `zoe serve`, the grant comes on a connection of the pool and the invalidation on the listener (ADR 0036). A request that does not retry must release the kept lock.
- **B3. The lock before the read.** Known hot objects become ghosts at each `newTransaction`, `setstate` takes the lock, and the load reads the current record. No `ConflictError` in the model. It needs a hot set, and overrides of `setstate` and `newTransaction`. Every reader of a hot object waits, and the transaction reads that object past its snapshot.

### C. The batch of 50 ms

- **C1. No thread.** A batch goes out at a `register` when its oldest oid waited 50 ms; the vote takes the rest. A transaction that changes one object never sends a batch, because no later `register` comes.
- **C2. A rate.** The first oid of a transaction goes out at once. Later oids go out at most once in 50 ms, at a `register`.
- **C3. A timer thread.** One thread of the process sends the batches that are due. The thread of the transaction waits for the grants at its next `register` or at `tpc_begin`. It needs a request that does not block, in the lock manager for the in-process mode and in `zoe-client`.

### D. How `early-locks on` selects the class

- **D1. A patch of the class.** When a storage with early locks opens, `zoe.zodb` patches the methods of `ZODB.Connection.Connection`, and each patched call checks `self._db.storage`. It reaches every DB of the process, with no change of the configuration. The specification mentions a patch as an alternative.
- **D2. The configuration names the class.** `connection-class zoe.zodb.LockingConnection` in `<zodb_db>`, and `zoe.plone` clears the free connections of the pool at `DatabaseOpened`. It does not work in `<zodb>`, which has no such key.
- **D3. A database section of ZOE.** A section that makes the DB with `klass` set before the first connection. A new section type, and a second place for one setting.

## Open questions for the human

1. What does the acceptance test "64 writers on one object that cannot merge give 0 `ConflictError`" count: every `ConflictError` that an attempt raises, or only those that remain after the retries? B2 meets the second reading only; B3 meets both; B0 and B1 meet neither.
2. Which mechanism lets a waiter read after the grant: B1, B2 or B3? B2 adds a lock that outlives its transaction and a change of the retry of Zope. B3 adds a read past the snapshot and waits for readers.
3. Who owns an intent lock before `tpc_begin` (A1, A2 or A3), and what is the form of `lock_intent` and `unlock_intent` on the wire?
4. How does the vote take its locks when the transaction holds intent locks: does it release the intent locks above the first oid that it must wait for, or may it take a free lock out of order and wait only in order? How does this meet the hand-off of step 2?
5. Where does a `lock_intent` that waits run in `zoe serve`: on the thread of the transaction of the session, from the first `lock_intent` on?
6. Is a hold without a vote bounded, so that a stuck request cannot keep a hot object? The drop is safe because the intent locks are advisory.
7. Which form of the batch of 50 ms (C1, C2 or C3)? Does the first oid of a transaction go out at once?
8. How does `early-locks on` select the class (D1, D2 or D3), in `<zoe>` and in `<zoeclient>`, with Zope and with a plain `<zodb>`?
9. Which classes does the client skip: the fixed list of tier 1, or the modes of the store, which `get_info` would then give?
10. What does `early-locks on` do with `native false` in `<zoeclient>`: refuse it with `ValueError`, or run without early locks?
11. Does `readCurrent` take a shared intent lock when BTrees call it, or only at the vote as now? Lever 3 says that it "takes a shared lock under the same rules".
12. Is ZODB 6.2 the oldest version for the hook, or does the test for each version add ZODB 6.0 and 6.1?

## Not measured

- No ZOE code runs early locks. The model is Python, on `MappingStorage`, with a stand-in for the lock manager, on the GIL build only. It did not run against `zoe serve`, the in-process store (except `probe_snapshot.py`) or 3.14t.
- No Plone run. How much of a save registers before the before-commit hook, how long Zope holds a transaction after its first change, and how often a batch gets `LockOrder` are not known.
- The cost of the overrides on each `register`, and the cost of a `lock_intent` round trip.
- The read skew of B3 on the objects of a Plone site.
- ZODB 6.0 and 6.1, and ZEO 6.0 and 6.1: not installed here.
- The times of the model: another job used the machine.
