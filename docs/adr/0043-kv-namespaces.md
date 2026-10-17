# 0043. KV namespaces: an ephemeral namespace with a TTL, and a durable namespace

Status: accepted. The human approved it on the pull request of ZOE-207 (#420), with the answers of its draft to each question below. It changes the on-disk format (a red zone), the wire protocol and the public Python API.

## Context

F31 asks for KV namespaces. The ephemeral namespace has a TTL, no history and no pack, for sessions, CSRF keys and locks. The durable flavour is for `plone.app.registry`. ZOE-207 takes sessions, locks and the registry out of the object graph. Then their writes conflict with no content write, and an expired key goes without a pack. The data model has no key space for them, so the namespaces are a format change.

What Plone 6.1.5 does today (paths in the eggs of `target/plone/eggs/cp313`):

- **Sessions.** Products.Sessions 6.0, a dependency of Products.PluggableAuthService, adds `session_data_manager` to the Zope root. Its container is `/temp_folder/session_data` (`Products/Sessions/__init__.py:41-54`, `:97-100`), and it sets `request.SESSION` lazily (`SessionDataManager.py:442`). Zope 5 has no `/temp_folder`: Products.TemporaryFolder is only a test dependency of Products.Sessions, and no instance under `plone/` mounts a temporary storage. So no container exists, and `request.SESSION` is `None` (`SessionDataManager.py:255-259`, `:297-330`). Plone itself keeps nothing in a session: `logoutUser` of PlonePAS invalidates a session only if one exists (`Products/PlonePAS/tools/membership.py:667-679`). The login of Plone is `plone.session`, a signed cookie with no state on the server (`plone/session/plugins/session.py:155-167`). With a temporary folder, the container is a `TransientObjectContainer` of Products.Transience: 20 minutes by default (`SessionDataManager.py:49-51`), and its conflict resolver keeps the session that changed last (`Products/Transience/TransientObject.py:210-264`).
- **Locks.** plone.app.lockingbehavior locks an item when its edit form begins (`plone/app/lockingbehavior/configure.zcml:29-33`, `plone/locking/events.py:8-12`). `TTWLockable.lock` puts a WebDAV `LockItem` of 10 minutes (`plone/locking/interfaces.py:11`) in `_dav_writelocks`, a `PersistentMapping` attribute of the content object that the first lock adds (`OFS/Lockable.py:41-56`, `:100-104`). It puts the token in the annotation `plone.locking`, another `PersistentMapping` (`plone/locking/lockable.py:45-65`, `:165-175`). `lock_info` deletes expired locks when it reads them (`lockable.py:137-146`, `OFS/Lockable.py:57-65`). So the GET of an edit form writes the content object or its lock mappings. In B5 `edit`, the transactions that lock or unlock an edit form are half of all transactions (`docs/findings/plone-end-to-end.md`, item 27).
- **CSRF keys.** plone.protect signs its tokens with the keys of a `KeyManager` of plone.keyring. The site has one as a persistent local utility (`plone/keyring/profiles/default/componentregistry.xml`, `plone/keyring/keymanager.py:8-25`), and the Zope root has one in `_key_manager` (`plone/protect/utils.py:120-129`). The keys rotate only at a login, daily or weekly, in the login transaction (`plone/protect/subscribers.py:19-56`). No GET writes them.
- **Registry.** `portal_registry` is a `Registry` object in the site (`plone/app/registry/registry.py:5`). Its `_records` holds two OOBTrees, `_fields` and `_values` (`plone/registry/registry.py:25-36`, `:140-167`). A request reads many records from the pickle cache of its connection, with no storage call.

The sequences of ADR 0024 are the closest primitive: a key space of their own, an extension negotiated through `register` and `get_info` (F13), and `storage.sequence(name)` in both modes. But a sequence is outside the two-phase commit. A namespace write must commit or abort with its ZODB transaction, so it goes through the commit path.

## Decision

### 1. Two flavours, three key spaces

| | ephemeral | durable |
| --- | --- | --- |
| for | sessions, locks | the registry |
| TTL | every key has one | none |
| history | none: one row per key | none: one row per key |
| pack | never reads it | never reads it |
| a crash | loses no acknowledged write | loses no acknowledged write |
| dumps, `zoe convert`, the change feed | leave it out | carry it |

| Key space | Tag | Key | Value |
| --- | --- | --- | --- |
| `kv_ephemeral` | 7 | the length of the name (1 byte), the name, the key | a frame; its payload is the expiry, a little-endian `u64` of milliseconds since the Unix epoch in UTC, then the value |
| `kv_expiry` | none | the expiry, a big-endian `u64`, then the key of the row | empty: not a frame, as `txn_oids` |
| `kv_durable` | 8 | the length of the name, the name, the key | a frame; its payload is the value. The row with the empty key of a namespace is its generation row: its payload is the tid of the last commit that wrote the namespace, a little-endian `u64` |

- A name is 1 to 255 bytes of UTF-8. A key is 1 to 1,024 bytes: the empty key of `kv_durable` is the generation row. A value is bytes. It counts toward `max-record-size` and `max-transaction-size` as a pickle does.
- The keys of the tables are bytes, not `&str`. redb decodes a `&str` key with `unwrap`, so a flipped bit can make a lookup panic (the known limits of `zoe-core`).
- UTF-8 byte order is the code point order of a Python `str`. So a scan of str keys gives the order of an OOBTree.
- Each frame has frame version 1, no flags, codec 0, branch id 0, previous serial 0 and no typed-field slots, as a sequence row. Its checksum covers the tag, the whole key of the row, header bytes 0 to 23 and the payload (ADR 0029). The tags 7 and 8 are the next free tags: 6 stays reserved for `vectors`.
- A read verifies the checksum before it reads the expiry. So a flipped bit in an expiry is a `CorruptKv` error, never an expired key. `CorruptKv` names the flavour, the namespace and the length of the key, never the key: a key can be a session id, which is a credential.
- The first write of a flavour creates its tables. A store that never writes a namespace keeps its tables and its bytes. The format name stays `ZOE format 2`.

### 2. Writes join the two-phase commit

- **Staging.** `Txn::kv_set(flavour, name, key, value, ttl)` and `Txn::kv_delete(flavour, name, key)` stage a write in the transaction, as `store` stages a pickle: between the begin and the vote, from the thread that drives the transaction. A later write of the same key replaces the staged one. A restore transaction refuses them with `Protocol`. The staged values share the staging of the pickles: `staging-memory` in memory, then the spill file.
- **No lock; the last writer wins.** A namespace write takes no per-OID lock and no lock of its own. The vote checks nothing for it. When two transactions write one key, the commit with the later tid wins. This is the simplest rule. A version check needs a version in each row and makes two writers of one key conflict. Last writer wins is already the rule of the session resolver of Products.Transience. A lock and a registry value are set, not merged.
- **Finish.** The commit-batch leader writes the namespace writes of each member in the backend write of the batch, after the records of the member. So they commit with the records, the transaction row and the `meta` update: one write, one sync. An ephemeral write gets its expiry there, the time of the batch write plus its TTL, and also writes its key in `kv_expiry`. A durable write also sets the generation row of its namespace to the tid of its member. A delete removes the row. A delete of an absent key does nothing.
- **A transaction with only namespace writes** is a commit without a record. It gets a tid and a transaction row with an oid count of 0. The other clients get `invalidateTransaction(tid, [])`, as ZEO 6.2 sends for an empty commit. It holds no oid lock, so it shares the sync of its batch.
- **Abort.** An abort, a failed vote and a dropped transaction drop the staged namespace writes with the pickles.
- **Durability.** An acknowledged write of either flavour is as durable as the records of its commit. A crash after the acknowledgement loses none of it. A crash before it leaves none of it. An ephemeral key goes only by its TTL or by a delete.
- **TTL.** A TTL is a number of seconds, more than 0 and at most 2^32 − 1, the maximum timeout of a WebDAV lock (`OFS/LockItem.py:26`). Each write of a key sets its TTL again. No call refreshes a TTL alone: the caller writes the value again.
- **Reads.** A read gives the last committed value of a key, not the value at the snapshot of the connection: the namespaces keep no history. An expired key reads as absent: a get gives nothing, and a scan skips it. A scan gives keys in byte order, at most 1,000 for each call. Reads run on any thread and on a read-only store. The generation of a durable namespace is the zero tid until a commit writes the namespace. A client cache of a namespace stays valid while its generation stays the same.

### 3. Expiry without a pack

- **On read.** A read compares the expiry with the time of the store clock (`Clock::now`).
- **The sweep step.** A step is a backend write of its own, outside the commit sequencer, as a sequence reservation is. It takes the keys of `kv_expiry` with an expiry at or before now, at most 1,000, in key order. In the same write, it removes each index key, and it removes the row when the row passes its checksum and its own expiry is at or before now. So a row that a later write refreshed stays, and a damaged index key never removes a live key. A row that fails its checksum stays, as evidence. The step commits with a sync.
- **Stale index keys.** A write leaves the index key of the expiry that it replaces, and a delete leaves the index key of its row. The sweep removes these keys when their time comes. So a write reads no row in the commit path.
- **When it runs.** `zoe serve` runs a step for each writable store that has the ephemeral table, once a minute, on a blocking thread. When a step takes 1,000 keys, the next step runs at once. `zoe.Storage` runs the steps the same way, on a thread of the extension for each such store, which runs no Python and holds no GIL. The thread starts at the open of the store, or at its first ephemeral write, and stops at the close. A read-only or halted store runs no step. `Store::sweep(limit)` runs one step for the tests and the tools. redb has one writer, so a commit waits for at most one step.
- **A crash.** A crash can lose a step. It then gives back only expired rows, which reads hide and the next step removes.

### 4. The protocol extension `kv`

The extension `kv` is negotiated as `sequences` is: the client declares it in the third argument of `register`, and `get_info` gives it back under `extensions` (F13). `flavour` is the str `"ephemeral"` or `"durable"`, `name` is a str, and `key`, `start`, `stop` and `value` are bin.

| Call | Reply | Runs |
| --- | --- | --- |
| `kv_get(flavour, name, key)` | the value, or nil | on the read pool, outside the transaction |
| `kv_scan(flavour, name, start, stop, limit)` | `[[key, value], ...]` in key order, from `start` (included) to `stop` (excluded), nil for an open end, at most `limit` and at most 1,000 pairs | on the read pool |
| `kv_generation(name)` | the generation of the durable namespace, 8 bytes | on the read pool |
| `kv_set(flavour, name, key, value, ttl)` | none, as `storea` | in the lane of the transaction; `ttl` is an int or a float for `"ephemeral"`, and nil for `"durable"` |
| `kv_delete(flavour, name, key)` | none | in the lane of the transaction |

An error of `kv_set` or `kv_delete` fails the vote, as an error of `storea` does. A read-only connection or store fails them at the vote with `ReadOnlyError`. A bad argument of a read fails the read with `TypeError` or `ValueError`, and the connection stays open. A stock client declares nothing: its `get_info` has no `extensions` key, and a `kv_*` call fails with `ValueError`, as for every extension. It sees the commits of namespace writes as transactions without records, which ZODB allows. It sees no key.

### 5. The Python API

`zoe.Storage` and `zoe.zeo.ClientStorage` get the methods of the calls: `kv_get(flavour, name, key)`, `kv_scan(flavour, name, start=None, stop=None, limit=1000)`, `kv_generation(name)`, `kv_set(flavour, name, key, value, ttl, transaction)` and `kv_delete(flavour, name, key, transaction)`. `kv_set` and `kv_delete` follow the rules of `store`: between `tpc_begin` and `tpc_vote` of `transaction`, from the thread that began it. On `zoe.zeo.ClientStorage`, each method raises `Unsupported` when the server did not accept `kv`.

ZODB gives no hook for writes beside the objects of a connection, so the module `zoe.kv` gives one. `zoe.kv.Namespace(connection, flavour, name)` has `get`, `scan`, `generation`, `set(key, value, ttl=None)` and `delete(key)`.

- `set` and `delete` keep the write in a data manager that joins the current transaction of the connection. `get` and `scan` see the writes of that transaction first. The data manager supports savepoints.
- The `transaction` package calls `tpc_begin` of every resource before the commit phase (`transaction/_transaction.py:427-430`). In the commit phase, the data manager stages its writes in the storage transaction that the connection began. ZODB keeps it as `transaction.data(connection)` (`ZODB/Connection.py:465-477`). So the writes commit in one write with the records.
- When the connection began none, because it changed no object, the data manager begins a storage transaction of its own and drives its vote, its finish and its abort.
- A storage that is not `zoe.Storage` or `zoe.zeo.ClientStorage` raises `Unsupported`.

`zoe.plone` builds its session data manager, its locking backend and its registry storage on `zoe.kv`, each opt-in (step 4 of ZOE-207).

### 6. `zoe verify` and the bit-flip harness

`zoe verify` reads every row of `kv_ephemeral` and `kv_durable`. Each scan checks the range and the order of its keys, as the scans of `records` do. These are findings in `checksums`, as for a sequence row: a frame that fails its checksum, a key whose name length runs past the key, an empty name or a name that is not UTF-8, and an ephemeral payload shorter than 8 bytes. These are findings too: an ephemeral row without the key of its expiry in `kv_expiry` (no step would remove it), a durable namespace with rows and no generation row, and a generation after the last tid. A finding names the namespace and the xxh3-64 hash of the key, never the key. An index key without its row, or with another expiry, is no finding: writes leave such keys. The report counts the keys of each flavour and the expired keys that wait for a step.

The bit-flip harness writes rows of both flavours and adds their frames to the gate. A flipped bit in a frame makes each read of it raise `CorruptKv`: never an absent key, an expired key or another value. `kv_expiry` has no frame, as `txn_oids`, so a flip there is outside the gate. The rule of the sweep step keeps such a flip from removing a live key.

### 7. The other tools

| Tool | ephemeral | durable |
| --- | --- | --- |
| pack | never reads it, nor `kv_expiry` | never reads it. It removes the transaction rows of namespace-only commits, as every row that keeps no record |
| iterator, `zodbconvert`, `copyTransactionsFrom` | gives the commits without records | the same: the iterator gives records only, so such a copy leaves the rows out |
| `zoe convert` | leaves it out | ZOE → ZOE copies the rows after the transactions. ZOE → Data.fs leaves them out, and its report counts them |
| `zoe backup`, `zoe restore` (ADR 0030) | leave it out | each dump holds every row, generation rows included, read in the snapshot of its last tid, as it holds the sequence ends. The rows are a new entry kind, so the dump version becomes 2, and a restore reads versions 1 and 2. A restore writes the rows of the last dump of the chain in one backend write. `zoe verify --backup` compares them when the store has no commit after the dumps |
| the change feed (F23, later) | gives no key | gives no key and no value. It can name the durable namespaces that a commit wrote |
| replication (later) | goes with its commit; a follower runs no step until its promotion | goes with its commit |
| undo (F11) | gives back no key | gives back no key |

Every durable write has a tid, so a backup with nothing new writes nothing and misses no change of a namespace.

## Consequences

- With the backends of `zoe.plone`, the GET of an edit form, or a session write, writes no object. It causes no `ConflictError` with a content write. plone.protect does not see these writes: it checks only the objects registered on the connections (`plone/protect/auto.py:215-237`). It already skips the writes of the `temporary` connection, which holds the sessions.
- A namespace-only commit costs a tid, a transaction row and a share of a sync. A site that writes a session on each request syncs as often as it commits.
- Two editors who open one edit form at the same moment can both take its lock, and the later write wins. With ZODB, one of them gets a `ConflictError` and a retry.
- A copy through the ZODB iterator leaves the durable rows out. A site that keeps its registry in the namespace moves it back into the object graph before such a copy: `zoe.plone` does this when the site switches the opt-in off.
- The writes-on-GET report (F32) counts records by class, so it counts no namespace write.
- A build without this change opens a store with namespaces and ignores them: its backups leave the durable rows out, and it runs no sweep.
- `Backend` and `WriteTxn` get new required methods. No existing signature changes. The sweep step and the restore of durable rows join the backend writes that run outside the commit sequencer.

## Alternatives

- **Keep sessions, locks and the registry in ZODB objects, with merges.** Merges reduce the conflicts, but two writes of one key still conflict. A lock still writes the content object, and every version of a session stays until a pack.
- **A separate store file.** A second redb file has its own sync, so its writes do not commit atomically with the records. It doubles the open, the lock, the backup and the verify.
- **An in-memory cache with no durability.** Zope's temporary folder does this for sessions. A restart then loses every session and every lock, nothing commits atomically with the records, and the registry cannot live there.
- **Redis.** It adds a service to deploy and a network round trip, and nothing commits atomically with the records. The registry still needs a durable store.
- **Reuse the `sequences` key space.** A namespace row is another kind of frame, and ADR 0029 gives each kind its own tag. Sequence keys are `&str`. A sequence is outside the two-phase commit by design (ADR 0024).
- **History for the durable flavour**, a row for each `(name, key, tid)`. Reads would follow the snapshot and undo would work. But each read becomes a range scan, and pack must then read the namespace and remove old rows.
- **A version check for each key.** It gives an exact lock of an edit form, but two writers of one key then conflict, and each row needs a version.
- **Ephemeral-only commits without a sync and without a tid.** A session write would cost no sync, but a crash would lose the acknowledged writes after the last durable commit, and the store would need a second commit path. A deferred commit with a tid is not possible: after a crash, a ZEO client that saw the lost tid refuses the server (`ZEO/asyncio/client.py:598-605`).
- **A sweep by a cursor over every row, without `kv_expiry`.** It saves a key space, but each step reads live rows, and an expired key goes only when the cursor reaches it.

## Answers of the human

The human approved each choice of this draft:

1. The ephemeral namespace is as durable as the records. Ephemeral-only commits have a sync.
2. Three key spaces: `kv_ephemeral` (tag 7), `kv_durable` (tag 8), and `kv_expiry` (no tag); their names and their key layout.
3. No history and no pack for both flavours. Reads give the last committed value, not the snapshot value, and undo gives back no key.
4. Last writer wins, with no lock and no version check, also for the locks of edit forms.
5. A namespace-only transaction is a commit with a tid and an empty transaction row, which stock clients see as an empty transaction.
6. The generation row of each durable namespace, for a client cache of the registry.
7. The name `kv`, its five calls, the argument `flavour`, and the module `zoe.kv`.
8. The sweep: a step of at most 1,000 keys, once a minute, with a sync, and no option.
9. The limits: a name of at most 255 bytes, a key of at most 1,024 bytes, a TTL of at most 2^32 − 1 seconds, and values under `max-record-size`.
10. No new format name: an older build opens a store with namespaces, leaves the durable rows out of its backups and runs no sweep.
11. The dump version 2, with a full copy of the durable rows in each dump.
12. The CSRF keys stay in plone.keyring: they change only at a login, and the card names no backend for them.
13. A copy through the ZODB iterator, and `zoe convert` to Data.fs, leave the durable rows out. A site moves its registry back into the object graph before such a copy.
