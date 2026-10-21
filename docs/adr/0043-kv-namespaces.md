# 0043. KV namespaces: an ephemeral namespace with a TTL, a durable namespace, and the backends of zoe.plone on them

Status: accepted. Part A changes the on-disk format (a red zone), the wire protocol and the public Python API. Part B adds an opt-in backend to `zoe.plone` and a public Python API (`zoe.plone.registry`); it changes no on-disk format, no wire protocol and no code of the core.

## Context

### The namespaces

F31 asks for KV namespaces. The ephemeral namespace has a TTL, no history and no pack, for sessions, CSRF keys and locks. The durable flavour is for `plone.app.registry`. The locks of the edit forms, the registry and the sessions move out of the object graph. Then their writes conflict with no content write, and an expired key goes without a pack. The data model has no key space for them, so the namespaces are a format change.

What Plone 6.1.5 does (paths in the eggs of `target/plone/eggs/cp313`):

- **Sessions.** Products.Sessions 6.0, a dependency of Products.PluggableAuthService, adds `session_data_manager` to the Zope root. Its container is `/temp_folder/session_data` (`Products/Sessions/__init__.py:41-54`, `:97-100`), and it sets `request.SESSION` lazily (`SessionDataManager.py:442`). Zope 5 has no `/temp_folder`: Products.TemporaryFolder is only a test dependency of Products.Sessions, and no instance under `plone/` mounts a temporary storage. So no container exists, and `request.SESSION` is `None` (`SessionDataManager.py:255-259`, `:297-330`). Plone itself keeps nothing in a session: `logoutUser` of PlonePAS invalidates a session only if one exists (`Products/PlonePAS/tools/membership.py:667-679`). The login of Plone is `plone.session`, a signed cookie with no state on the server (`plone/session/plugins/session.py:155-167`). With a temporary folder, the container is a `TransientObjectContainer` of Products.Transience: 20 minutes by default (`SessionDataManager.py:49-51`), and its conflict resolver keeps the session that changed last (`Products/Transience/TransientObject.py:210-264`).
- **Locks.** plone.app.lockingbehavior locks an item when its edit form begins (`plone/app/lockingbehavior/configure.zcml:29-33`, `plone/locking/events.py:8-12`). `TTWLockable.lock` puts a WebDAV `LockItem` of 10 minutes (`plone/locking/interfaces.py:11`) in `_dav_writelocks`, a `PersistentMapping` attribute of the content object that the first lock adds (`OFS/Lockable.py:41-56`, `:100-104`). It puts the token in the annotation `plone.locking`, another `PersistentMapping` (`plone/locking/lockable.py:45-65`, `:165-175`). `lock_info` deletes expired locks when it reads them (`lockable.py:137-146`, `OFS/Lockable.py:57-65`). So the GET of an edit form writes the content object or its lock mappings. In B5 `edit`, the transactions that lock or unlock an edit form are half of all transactions (`docs/findings/plone-end-to-end.md`, item 27).
- **CSRF keys.** plone.protect signs its tokens with the keys of a `KeyManager` of plone.keyring. The site has one as a persistent local utility (`plone/keyring/profiles/default/componentregistry.xml`, `plone/keyring/keymanager.py:8-25`), and the Zope root has one in `_key_manager` (`plone/protect/utils.py:120-129`). The keys rotate only at a login, daily or weekly, in the login transaction (`plone/protect/subscribers.py:19-56`). No GET writes them.
- **Registry.** `portal_registry` is a `Registry` object in the site (`plone/app/registry/registry.py:5`). Its `_records` holds two OOBTrees, `_fields` and `_values` (`plone/registry/registry.py:25-36`, `:140-167`). A request reads many records from the pickle cache of its connection, with no storage call.

The sequences of ADR 0024 are the closest primitive: a key space of their own, an extension negotiated through `register` and `get_info` (F13), and `storage.sequence(name)` in both modes. But a sequence is outside the two-phase commit. A namespace write must commit or abort with its ZODB transaction, so it goes through the commit path.

### The registry

The registry must move into a durable namespace with no way for a registry to read as empty, to overwrite another registry, or to lose its rows in a copy.

What plone.registry 2.0.2, plone.app.registry 2.0.7 and the other eggs of the build do (paths in the eggs of `target/plone/eggs/cp313`):

- `portal_registry` is a `plone.app.registry.registry.Registry`, a `plone.registry.registry.Registry` and an OFS `SimpleItem`. Its attribute `_records` is a `_Records`, which is not persistent: it is in the pickle of the registry. It holds two `OOBTree`s, `_fields` and `_values` (`plone/registry/registry.py:140-158`). A field is a `PersistentField` (`plone/registry/field.py:104`), so each field is a record of its own. A field of a `FieldRef` is a str in `_fields`: the name of the record that holds the field (`registry.py:243-266`).
- `Registry.__getitem__`, `get` and `__contains__` read `self.records._values` (`registry.py:33-50`). `Record.value` reads and writes `self.__parent__.records._values[name]` (`plone/registry/record.py:66-90`). `_Records` reads and writes `self._fields` and `self._values` only. No other egg of the build reads `_fields` or `_values`.
- Code can change a field in place. The upgrade step `change_interface_on_lang_registry_records` of plone.app.upgrade sets `record.field.interfaceName` (`plone/app/upgrade/v52/final.py:125-135`). With the stock trees, the field is a persistent object, so the change is written.
- `Registry.records` is a property. It migrates the old persistent `Records` class, and only that class (`registry.py:52-57`, `:123-138`).
- `ZODB.broken.PersistentBroken.__reduce__` raises `BrokenModified` (`ZODB/broken.py:335-336`). A site often keeps records of an add-on that is gone; their fields load as such broken objects.
- On the layers of plone.app.testing over a ZOE store, the registry of a site and the objects that it reaches hold 1.7 MB.
- plone.protect checks the objects that the connections registered (`plone/protect/auto.py:215-237`). A namespace write registers no object (see Consequences).

A design that keeps every registry of a store in one namespace, as views of `_fields` and `_values`, fails a review in three ways:

1. Two Plone sites of one Zope root overwrite each other, and the uninstall of one empties the other.
2. A copy into another ZOE store (`copyTransactionsFrom`, zodbconvert, a zexp import, `zoe convert` and back) reads as empty and takes writes.
3. Before its first write, a new registry keeps all its records in its own pickle: 346 KB, and 208 KB more for each write on FileStorage.

B5 shows no GET that writes the registry.

## Decision

### A. The namespaces

#### A1. Two flavours, three key spaces

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
- Each frame has frame version 1, no flags, codec 0, branch id 0, previous serial 0 and no typed-field slots, as a sequence row. Its checksum covers the tag, the whole key of the row, header bytes 0 to 23 and the payload (ADR 0020). The tags 7 and 8 are the next free tags: 6 stays reserved for `vectors`.
- A read verifies the checksum before it reads the expiry. So a flipped bit in an expiry is a `CorruptKv` error, never an expired key. `CorruptKv` names the flavour, the namespace and the length of the key, never the key: a key can be a session id, which is a credential.
- The first write of a flavour creates its tables. A store that never writes a namespace keeps its tables and its bytes. The format name stays `ZOE format 2`: an older build opens a store with namespaces and ignores them.

#### A2. Writes join the two-phase commit

- **Staging.** `Txn::kv_set(flavour, name, key, value, ttl)` and `Txn::kv_delete(flavour, name, key)` stage a write in the transaction, as `store` stages a pickle: between the begin and the vote, from the thread that drives the transaction. A later write of the same key replaces the staged one. A restore transaction refuses them with `Protocol`. The staged writes share the staging of the pickles: the key of each row and its value count in `staging-memory` and toward `max-transaction-size`; a value that does not fit goes to the spill file, and its key stays in memory.
- **No lock; the last writer wins.** A namespace write takes no per-OID lock and no lock of its own. The vote checks nothing for it. When two transactions write one key, the commit with the later tid wins, also for the locks of edit forms. This is the simplest rule. A version check needs a version in each row and makes two writers of one key conflict. Last writer wins is already the rule of the session resolver of Products.Transience. A lock and a registry value are set, not merged.
- **Finish.** The commit-batch leader writes the namespace writes of each member in the backend write of the batch, after the records of the member. So they commit with the records, the transaction row and the `meta` update: one write, one sync. An ephemeral write gets its expiry there, the time of the batch write plus its TTL, and also writes its key in `kv_expiry`. A durable write also sets the generation row of its namespace to the tid of its member. A delete removes the row. A delete of an absent key does nothing.
- **A transaction with only namespace writes** is a commit without a record. It gets a tid and a transaction row with an oid count of 0. The other clients get `invalidateTransaction(tid, [])`, as ZEO 6.2 sends for an empty commit. It holds no oid lock, so it shares the sync of its batch.
- **Abort.** An abort, a failed vote and a dropped transaction drop the staged namespace writes with the pickles.
- **Durability.** An acknowledged write of either flavour is as durable as the records of its commit, with a sync. A crash after the acknowledgement loses none of it. A crash before it leaves none of it. An ephemeral key goes only by its TTL or by a delete.
- **TTL.** A TTL is a number of seconds, more than 0 and at most 2^32 − 1, the maximum timeout of a WebDAV lock (`OFS/LockItem.py:26`). Each write of a key sets its TTL again. No call refreshes a TTL alone: the caller writes the value again.
- **Reads.** A read gives the last committed value of a key, not the value at the snapshot of the connection: the namespaces keep no history. An expired key reads as absent: a get gives nothing, and a scan skips it. A scan gives keys in byte order, at most 1,000 for each call, and stops after the pair that brings its keys and values to 1 MiB, so a reply stays near that size. So a reply with fewer pairs than its limit is not the end: a reader scans again from after its last key until a reply is empty. Reads run on any thread and on a read-only store. The generation of a durable namespace is the zero tid until a commit writes the namespace. A client cache of a namespace stays valid while its generation stays the same.

#### A3. Expiry without a pack

- **On read.** A read compares the expiry with the time of the store clock (`Clock::now`).
- **The sweep step.** A step is a backend write of its own, outside the commit sequencer, as a sequence reservation is. It takes the keys of `kv_expiry` with an expiry at or before now, at most 1,000, in key order. In the same write, it removes each index key, and it removes the row when the row passes its checksum and its own expiry is at or before now. So a row that a later write refreshed stays, and a damaged index key never removes a live key. A row that fails its checksum stays, as evidence. The step commits with a sync.
- **Stale index keys.** A write leaves the index key of the expiry that it replaces, and a delete leaves the index key of its row. Such a stale key stays until its own expiry. Then a step takes it, finds no row with that expiry, and removes only the key. So a write reads no row in the commit path.
- **When it runs.** `zoe serve` runs a step for each writable store that has the ephemeral table, once a minute, on a blocking thread. When a step takes 1,000 keys, the next step runs at once. The sweep has no option. `zoe.Storage` runs the steps the same way, on a thread of the extension for each such store, which runs no Python and holds no GIL. The thread starts at the open of the store, or at its first ephemeral write, and stops at the close. `zoe.Storage` waits for its sweep thread at `close`, without the GIL, so the next open of the same store finds it free. A read-only or halted store runs no step. `Store::sweep(limit)` runs one step for the tests and the tools. redb has one writer, so a commit waits for at most one step.
- **A crash.** A crash can lose a step. It then gives back only expired rows, which reads hide and the next step removes.

#### A4. The protocol extension `kv`

The extension `kv` is negotiated as `sequences` is: the client declares it in the third argument of `register`, and `get_info` gives it back under `extensions` (F13). `flavour` is the str `"ephemeral"` or `"durable"`, `name` is a str, and `key`, `start`, `stop` and `value` are bin.

| Call | Reply | Runs |
| --- | --- | --- |
| `kv_get(flavour, name, key)` | the value, or nil | on the read pool, outside the transaction |
| `kv_scan(flavour, name, start, stop, limit)` | `[[key, value], ...]` in key order, from `start` (included) to `stop` (excluded), nil for an open end, at most `limit` and at most 1,000 pairs | on the read pool |
| `kv_generation(name)` | the generation of the durable namespace, 8 bytes | on the read pool |
| `kv_set(flavour, name, key, value, ttl)` | none, as `storea` | in the lane of the transaction; `ttl` is an int or a float for `"ephemeral"`, and nil for `"durable"` |
| `kv_delete(flavour, name, key)` | none | in the lane of the transaction |

An error of `kv_set` or `kv_delete` fails the vote, as an error of `storea` does. A read-only connection or store fails them at the vote with `ReadOnlyError`. A bad argument of a read fails the read with `TypeError` or `ValueError`, and the connection stays open; a scan limit below 1 is a `ValueError`. A stock client declares nothing: its `get_info` has no `extensions` key, and a `kv_*` call fails with `ValueError`, as for every extension. It sees the commits of namespace writes as transactions without records, which ZODB allows. It sees no key.

#### A5. The Python API

`zoe.Storage` and `zoe.zeo.ClientStorage` have the methods of the calls: `kv_get(flavour, name, key)`, `kv_scan(flavour, name, start=None, stop=None, limit=1000)`, `kv_generation(name)`, `kv_set(flavour, name, key, value, ttl, transaction)` and `kv_delete(flavour, name, key, transaction)`, and `kv_supported()`, which tells if the storage has namespaces: always for `zoe.Storage`, and for `zoe.zeo.ClientStorage` when its server accepted `kv`. While a native client has no server, `kv_supported()` gives the answer of the last server, so the locking backend never takes the stock path for that time. `kv_set` and `kv_delete` follow the rules of `store`: between `tpc_begin` and `tpc_vote` of `transaction`, from the thread that began it. On `zoe.zeo.ClientStorage`, each method raises `Unsupported` when the server did not accept `kv`.

ZODB gives no hook for writes beside the objects of a connection, so the module `zoe.kv` gives one. `zoe.kv.Namespace(connection, flavour, name)` has `get`, `scan`, `generation`, `set(key, value, ttl=None)` and `delete(key)`.

- `set` and `delete` keep the write in a data manager that joins the current transaction of the connection. `get` and `scan` see the writes of that transaction first. The data manager supports savepoints. It stages the rows of the locking backend before each of its savepoints, so a savepoint keeps a `LockItem` that changed in place, whatever the order in which the data managers joined.
- The `transaction` package calls `tpc_begin` of every resource before the commit phase (`transaction/_transaction.py:427-430`). In the commit phase, the data manager stages its writes in the storage transaction that the connection began. ZODB keeps it as `transaction.data(connection)` (`ZODB/Connection.py:465-477`). So the writes commit in one write with the records.
- When the connection began none, because it changed no object, the data manager begins a storage transaction of its own and drives its vote, its finish and its abort.
- With no write left, after a rollback, the data manager begins no storage transaction.
- A scan of a namespace fills its page: it reads the store again until it holds `limit` pairs or a read gives an empty page.
- A storage whose `kv_supported()` is not true raises `Unsupported`: a storage that is not `zoe.Storage` or `zoe.zeo.ClientStorage`, and a client whose server did not accept `kv`. `zoe.kv._supported` stays private: the specification does not list it.

`zoe.plone` builds its locking backend (section A8) and its registry backend (part B) on `zoe.kv`, both opt-in.

#### A6. `zoe verify` and the bit-flip harness

`zoe verify` reads every row of `kv_ephemeral` and `kv_durable`. Each scan checks the range and the order of its keys, as the scans of `records` do. These are findings in `checksums`, as for a sequence row: a frame that fails its checksum, a key whose name length runs past the key, an empty name or a name that is not UTF-8, and an ephemeral payload shorter than 8 bytes. These are findings too: an ephemeral row without the key of its expiry in `kv_expiry` (no step would remove it), a durable namespace with rows and no generation row, and a generation after the last tid. A finding names the namespace and the xxh3-64 hash of the key, as the check of a backup does, never the key; it names the hash of the whole row only when the row gives no key. An index key without its row, or with another expiry, is no finding: writes leave such keys. The report counts the keys of each flavour and the expired keys that wait for a step.

The bit-flip harness flips the frames of the records and of the transaction rows. The gate of the namespaces is the exhaustive test `every_flipped_bit_of_a_namespace_frame_raises_corrupt_kv` of `crates/zoe-core/tests/kv_namespaces.rs`: it flips each bit of a frame of each flavour, and each read of the frame raises `CorruptKv`, never an absent key, an expired key or another value. `kv_expiry` has no frame, as `txn_oids`, so a flip there is outside the gate. The rule of the sweep step keeps such a flip from removing a live key.

#### A7. The other tools

| Tool | ephemeral | durable |
| --- | --- | --- |
| pack | never reads it, nor `kv_expiry` | never reads it. A pack at T removes the transaction rows of namespace-only commits strictly before T, as every row that keeps no record. The row at T stays, also when the pack removes every record of the transaction at T, with an oid count of 0. So a pack at the last tid keeps the last transaction, which the dumps of a backup and the clients name: a backup after it continues its dumps, and a restore gives the same last tid |
| iterator, `zodbconvert`, `copyTransactionsFrom` | gives the commits without records | the same: the iterator gives records only, so such a copy leaves the rows out |
| `zoe convert` | leaves it out | ZOE → Data.fs leaves the rows out: its report counts them, and it prints a warning. It refuses a store that holds a moved registry, unless `--force` (section B9). `zoe convert` has no ZOE → ZOE direction: the dumps of `zoe backup` and `zoe restore` copy a store with its rows |
| `zoe backup`, `zoe restore` (ADR 0030) | leave it out | each dump holds every row, generation rows included, read in the snapshot of its last tid, as it holds the sequence ends. The rows are an entry kind of dump version 2, and a restore reads versions 1 and 2. A restore writes the rows of the last dump of the chain in one backend write. `zoe verify --backup` compares them when the store has no commit after the dumps |
| the change feed (F23, later) | gives no key | gives no key and no value. It can name the durable namespaces that a commit wrote |
| replication (later) | goes with its commit; a follower runs no step until its promotion | goes with its commit |
| undo (F11) | gives back no key | gives back no key |

Every durable write has a tid, so a backup with nothing new writes nothing and misses no change of a namespace.

#### A8. The backends of `zoe.plone`, and the writes-on-GET report

- **The locking backend.** `zoe.plone:locking.zcml`, an opt-in file of the site, installs `zoe.plone.locking` when the ZCML loads, as `ZOE_PLONE_BACKENDS=locking` does through `configure.zcml`. For an object that provides `ITTWLockable` and whose storage has namespaces, it replaces two mappings: `OFS.Lockable.LockableItem.wl_lockmapping`, so every `wl_*` method of Zope reads and writes the namespace, and `TTWLockable._locks` of plone.locking, the types of the locks. One row of the ephemeral namespace `zoe.plone.locks` holds all the locks of one object, under its oid: a pickle of its WebDAV `LockItem`s and of their types, with the TTL of its longest valid lock. The row is the unit of a write: when two transactions change the locks of one object, the later commit wins for all of them. A transaction decodes a row once. Each change of a mapping stages the row at once, so a savepoint covers it. A data manager that writes nothing joins each transaction that decodes a row, so a rollback to a savepoint drops the decoded rows, and the next read decodes them again. A hook before the commit stages the rows again for a `LockItem` that changed in place (`refresh`). A row without a lock is deleted. A lock that is no longer valid reads as absent. With `killinvalids`, `wl_lockmapping` deletes it from the row, and calls `__no_valid_write_locks__` of the object when no lock is left, as OFS does. The row expires with its longest valid lock, so a read writes only in the second after a lock ends, or for an object with several locks. The backend carries no lock over: the locks that an object kept itself before the install do not count while it is installed, and the locks of the namespace do not count after the uninstall. A lock lives for minutes. Every other object keeps the stock behaviour. The lock of an edit form then writes no object.
- **The install.** The backends install when the ZCML loads, through `zcml:customAction` of z3c.zcmlhook, as plone.protect patches Zope. So an instance, its scripts and `bin/instance run` see the same backends.
- **The test layers.** A `DemoStorage` whose chain of bases ends at a ZOE storage has namespaces: `zoe.kv` keeps its writes in an overlay in memory that belongs to the `DemoStorage` and goes with it, and a read looks at the overlays of the chain, then at the base. One lock guards the overlays, and a read copies what it needs under it. So the suite of plone.locking runs on the locking backend under the stacked `DemoStorage`s of plone.testing, and no write of a test reaches the base. plone.app.testing turns plone.autoinclude off, so the layer of `zoe.plone.testing` installs the backends that `ZOE_PLONE_BACKENDS` names; `just plone zoe backends` runs that suite with the locking backend, and the tests of `zoe.plone`. The test egg of `plone/base.cfg` and the stack `zoe-native-kv` of the bench, with its `--port-offset`, serve these runs.
- **The session data manager.** Not built: Plone 6.1 has no session container (see Context). F31 says so; a later card builds it when a site needs sessions.
- **The CSRF keys** stay in plone.keyring: they change only at a login.
- **The writes-on-GET report.** `zoe.plone.report.mark` sets the key `zoe.request_method` of the extension of the transaction. A subscriber of `PubStart` calls it for GET and HEAD requests only. The extension reaches the server with `tpc_begin`, for a stock `ZEO.ClientStorage` too, and stays in the transaction row. `zoe serve` counts the records of a marked commit by class in `zoe_writes_on_get_total{storage, class}`.

### B. The registry backend

#### B1. The option and the guard

- `zoe.plone.registry` is a backend of `zoe.plone`, opt-in, as `locking` and `indexing` are: `ZOE_PLONE_BACKENDS=registry`, or the ZCML file `zoe.plone:registry.zcml`. Both call `install()` when the ZCML loads. `uninstall()` takes it out of the process. `installed()` tells if it is installed. A second call of either does nothing. Neither moves a registry.
- **The move is a step of its own**, explicit: `move(registry)` (section B5). A registry keeps its stock `OOBTree`s until a move.
- `install()` lets `move`, `copy_rows` and `reset` run. Without it they raise `RuntimeError`. That is all it does. `uninstall()` also drops the cache of rows (section B4).
- A moved registry reads and writes in every process that has this version of `zoedb`, with or without the option. The data decides, not the process. So an instance that misses the option still serves the site.
- **The guard of a stock registry** runs whenever `zoe.plone` loads, with or without the option: `configure.zcml` installs it, as it installs the other handlers of `zoe.plone`. When code reads `records` of a stock registry whose storage has namespaces, the guard loads the roots of `_fields` and `_values` if they are ghosts, then calls `readCurrent` on them. The move empties these roots (section B5). A root that keeps its keys in its own state (an empty tree, or a tree of one bucket) is written by each write of a record, and the resolution of a conflict on it could merge such a write into the trees that the move left; for such a root, the guard also calls `readCurrent` on the registry object, which the move writes, and the move does not write the root. The move empties a root with children, and does not just write it again: rule 1 of the split merge (ADR 0048) lets a read-current node through when the other side changed it only by splits and replacements, and an unchanged node is such a node. An empty root is not. A registry without a connection or an oid, or in a storage without namespaces, is skipped. So a write that began before a move, in any instance, conflicts at its commit, and Zope retries it on the moved registry (section B7). A write of a record goes through the nodes of the tree, and BTrees calls `readCurrent` on each of them itself; a change of a field in place reads no node, so the guard is what makes it conflict.
- **The argument of ADR 0048 holds.** ADR 0048 assumes that only BTrees calls `readCurrent`: a program that marks a node read-current itself and writes a key under it loses the check of the other children of that node under rule 1. The guard and the move call `readCurrent` too, and neither loses that check.
  - The guard marks only the two roots, and it writes no key. The path children of a root are the children that the transaction stored or marked read-current; the guard marks no child, so it adds no path child. A write of a record goes through the API of BTrees, which marks the same root and the nodes of its path itself. A transaction that writes no key under a root, as a change of a field in place, has no path child in it, and rule 1 gives no answer: the check of the vote stays. For a root that keeps its keys, the guard also marks the registry object, which is not a tree: the rules never apply to it.
  - The move marks each node and each bucket of the trees, and writes no key into them. It stores each root that it empties. Rules 2 and 3 take only a node in which the transaction split a child, so they give no answer for a root of the move. Rule 1 takes a node that the transaction did not store, as an internal node below a root, but the move marks every child of such a node, so every child is a path child. A split of the other side then has a path child as its anchor, and a replacement replaces a path child: rule 1 gives no answer in both cases. So a write that commits during the move makes it conflict.
  - A root that the move emptied changed by more than splits and replacements. So the rules give no answer for a write that began before the move, also a write that splits a bucket and stores the root.
- **The early return of the guard.** The guard keeps, for each connection, the transaction of its last check and, for each registry that it checked in it, the oids of the two roots and whether it read the registry object current. It returns at once when it checked the same roots in the current transaction, both roots are still read current and unchanged, and the registry object is still read current or changed, if the check read it. It then loads no tree and reads no state: the second read of `records` in a transaction, and each later one, costs a few attribute reads and dict lookups. The mark of the guard itself counts, not only the objects read current: BTrees reads a root current at each set and delete, also a delete of a missing key or an insert of a key that is there, which change nothing. So a root of one bucket that code read current through `_records` before the first read of `records` gets the whole guard, and the registry object is read current. A savepoint that stores an object takes it out of the objects read current, and a rollback does not put it back. So after a rollback past a write of a root or of the registry object, the guard checks again. A changed root goes through the whole guard again too.
- **The cost of the guard, on every ZOE site with `zoe.plone`**, moved or not: each transaction that reads the registry and writes an object checks the serials of the two roots at its vote (`checkCurrentSerialInTransaction`, a shared lock). A move, or a write that changes a root (a split of the root), makes such a transaction conflict and retry. A write of an attribute of the registry object, as the subscribers of the resources of Products.CMFPlone and of plone.app.theming do (`__RESOURCE_REGISTRY_MTIME`, `_theme_cache_mtime`), does not, except on a registry whose roots keep their keys. The stock behaviour otherwise stays.

#### B2. What moves, and the rows

- **One namespace for each registry.** A move draws an id: 16 random bytes from `secrets`, as 32 lowercase hex digits. The rows of the registry go to the durable namespace `zoe.plone.registry:` followed by the id (51 bytes). Two registries never share a namespace, also in one store, also when their oids are the same in two stores.
- **The rows.**

  | Key | Value |
  | --- | --- |
  | `f` followed by the name of the record | the pickle of its field, or of the str of a `FieldRef` |
  | `v` followed by the name of the record | the pickle of its value |
  | `m` | the marker: the version of the layout (1 byte, 1), the oid of the registry (8 bytes), and the write counter (8 bytes, big-endian) |

  A record name passes `_validkey` of plone.registry, so it starts with a letter: an `f` or `v` key has at least 2 bytes, and the marker key has 1. A key holds at most 1,024 bytes, so a name holds at most 1,023 bytes. A longer name raises `ValueError` at its write. The keys of one kind are in the order of the names, as in an `OOBTree` of str keys (section A1).
- **The write counter** is an int in the state of the registry and in the marker. A move sets it to 1. Each transaction that writes the registry sets both, once, to 1 more than the highest of the counter of the registry, the counter of the marker that the transaction staged, and the counter of the stored marker (section B7). So a counter never repeats, also after an undo (section B9). The check of section B6 compares them.
- **The registry after the move.** Its `_records` is a `zoe.plone.registry.MovedRecords`. This class is a subclass of `_Records`. Its `_fields` and `_values` are views of the rows of its namespace. They give the methods of an `OOBTree` that `_Records` and `Registry` call: `__getitem__`, `get`, `__setitem__`, `__delitem__`, `__contains__`, `__len__`, `__iter__`, `keys(min, max)`, `minKey`, `maxKey` and `clear`. `keys(min, max)` includes `max`, as an `OOBTree` does. The view keeps the sorted names of the transaction, and `keys(min, max)`, `minKey` and `maxKey` find each end of the range with a bisect (`bisect_left` at `min`, `bisect_right` at `max`), with the bounds as the caller gives them: a range compares about log2(n) names, not each name. When no name is at or above `min`, the range is empty and `max` is not compared, as an `OOBTree` does. The pickled state of `MovedRecords` is the registry without its acquisition wrapper (`__parent__`), the id and the counter: about a hundred bytes in the pickle of the registry. The class name is in the pickles of the store, so it stays importable.
- **The trees** stay in the store, with their roots emptied (section B5) and their buckets as they were. The registry no longer reaches them, so a garbage-collecting pack removes them. Until then, an undo of the move can give them back (section B9).

#### B3. The pickles

- **Why pickles.** ADR 0047 keeps JSON in its keys: an operation of the queue is a path and names, which JSON holds. A record holds a field, an instance of a class of plone.registry, and a value that can be a tuple, a set, a frozenset, a `Decimal`, a `datetime` or bytes. JSON holds none of them without a codec of our own. ZODB keeps the same objects as pickles in the same store, so the rows add no new trust.
- The backend pickles with protocol 3, as ZODB does, by value. A field is a `Persistent` that provides `IPersistentField`; it pickles with its state as a plain object, and it unpickles as a new object with no connection. A field of a collection holds its `value_type` field in its state, the same way.
- **The pickler refuses a persistent object that is not an `IPersistentField`**, with `TypeError` at the write. The types of the fields of plone.registry hold no such object, so a value with one is a bug of its writer, which the stock trees would keep as a reference.
- **A broken field**, of an add-on whose class is gone, loads from the trees as a `PersistentBroken`, whose pickle raises. The pickler writes it as ZODB writes a broken object: `ZODB.broken.rebuild` with the module and the name of its class and its state. A read unpickles with `ZODB.broken.find_global`, so it gives a broken object while the class is gone, and the field itself when the class comes back. The move and the move back keep such records as they are. A read of such a record fails as with the stock trees.

#### B4. A read, and the cache

- Each method of the views first runs the check of section B6, a write too. Then it reads the writes of the current transaction (`zoe.kv`), then the rows of a cache of the process.
- **The cache.** For each storage of a database (held through a weak reference) and each namespace, the process keeps the rows as bytes, with a label: the write counter of the marker of those rows. One lock guards the cache, for the free-threaded interpreter.
  - A view whose registry has a counter at or below the label takes the cached rows. Else it scans the namespace.
  - **A scan is atomic.** It reads the generation of the namespace, scans every row, and reads the generation again. When the generation changed, a commit wrote the namespace during the scan, and it scans again; after 3 tries, it raises `ConflictError`, and Zope retries the request. The scan then puts its rows and its label in the cache together, under the lock, and only when its label is above the label in the cache.
  - The cache keeps at most 16 namespaces for each storage; the namespace that was read least recently goes first. So the rows of a deleted site (section B8) leave the memory.
  - **The limit of 16 is fixed**, with no setting. It costs only a process that reads more than 16 moved registries of one storage in turn, as a Zope root with more than 16 sites, each with a moved registry. Each read of a registry that is not in the cache then scans its namespace again (about 1.7 MB of rows on a site of Plone), and its rows take the place of the rows of another registry. A site that needs more takes a later change that makes the limit a setting.
- Each write of a moved registry changes the state of the registry object (its counter), so ZODB invalidates the registry in each connection. A request then makes no storage call for the registry while nobody writes it, as with the pickle cache of the stock trees.
- **What a read gives.** The rows are never older than the snapshot of the registry, but they can be newer: a read gives the last committed rows (section A2). A connection opened at a past time (`db.open(at=...)`) reads the current rows too.
- **In a transaction.** A transaction takes the cached rows once, at its first read, and keeps them to its end, with its own writes over them. A read in a transaction that is no longer active, as in an after-commit hook, joins nothing and keeps no state. It decodes a row once and keeps the object. A data manager of `zoe.plone.registry` joins each transaction that decodes a row. At a rollback to a savepoint, it drops the decoded rows, so the next read decodes them again from the rows that the savepoint kept. At the end of the transaction it drops them all. An in-place change of a value is not written, as with the stock trees, where a bucket that nothing else changes keeps no in-place change.

#### B5. `move`, `move_back`, `copy_rows` and `reset`

Each one changes the current transaction of the connection of its registry. The caller commits. `move`, `copy_rows` and `reset` need `install()`.

- **`move(registry)`.** `registry` is a registry of plone.registry in a database. A registry that is moved already stays as it is. A registry with no connection, or in a storage without namespaces, raises `Unsupported` and stays as it is. The move draws the id, writes a row for each field and each value, and the marker with the oid of the registry and the counter 1, and puts a `MovedRecords` in `_records`. It calls `readCurrent` on each object of the trees that it reads (the `OOBTree`s, their internal nodes and their buckets) and on each persistent field that it pickles (a field, and the `value_type` field of a collection), so its commit fails with a `ConflictError` when a write changed them after its snapshot, also a change of a field in place. It also empties each root that has children (`clear`, which writes the root and no bucket), so a write that began before the move fails on the `readCurrent` of the guard (section B1), also with the split merge on. On a site of Plone the move writes about 1.7 MB of rows in one transaction.
- **`move_back(registry)`.** `registry` is a moved registry that passes the check of section B6, else `Unsupported`. It puts a `_Records` with new `OOBTree`s in `_records`, with each field and each value of the rows: the fields become new persistent objects. It deletes each row of the namespace, the marker too, so stale rows never read. The generation row stays (part A deletes no generation row), so the namespace keeps 1 row. Nothing is lost: each record of the rows goes back to the trees.
- **`copy_rows(source, copy)`.** `source` is a moved registry that passes the check. `copy` is a moved registry with the id of `source` that fails it: a copy of `source` in another store or in the same store, with no rows, with the rows of another oid, or with stale rows. Else it raises `ValueError`. It reads the rows of `source` with an atomic scan (section B4), with the writes of the current transaction of `source`, and calls `readCurrent` on `source`. It draws a new id for `copy`, writes the rows in the namespace of the new id in the store of `copy`, with the oid of `copy` and the counter of `source` in the marker, and puts a `MovedRecords` of the new id and that counter in `_records` of `copy`. When the old namespace of `copy` in its store has a marker with the oid of `copy` (stale rows), it deletes those rows. A marker with another oid belongs to another registry, and stays. With a new id, a copy in the same store never shares the rows of its source.
- **`reset(registry)`**, the recovery when no source is left. `registry` is a moved registry that fails the check. `reset` puts a `_Records` with new empty `OOBTree`s in `_records`, and reads no row. The operator then imports the registry step (`plone.app.registry`) of each profile that the site has installed, through `portal_setup`: the records of Plone itself are in `profile-Products.CMFPlone:dependencies`. The records come back with the values of the profiles: the values that the site changed are lost. A registry that passes the check raises `ValueError`: it moves back instead.

  **`reset` on a ZEO client without namespaces.** A stock `ZEO.ClientStorage`, or a `zoe.zeo.ClientStorage` whose server did not accept `kv`, also as the base of a `DemoStorage`, fails the check of every moved registry. It cannot tell a live `zoe serve`, whose rows are intact, from a ZEO server of a FileStorage after leaving ZOE (section B10). So on such a client `reset` raises `Unsupported` and changes nothing. The message says to stop the ZEO server and open the storage directly to reset: a FileStorage, or a `zoe.Storage`. On a storage opened directly, `reset` goes on as above: a storage without namespaces holds no rows, and a storage with namespaces gives the answer of the check. A `zoe.zeo.ClientStorage` whose server accepts `kv` reads the rows, so its check is the answer too. `reset` takes no argument to force it.

  **The wrappers.** `reset` looks for the ZEO client under the wrappers of the storage of the database. It follows the attributes `base` (a `DemoStorage`, zc.zlibstorage), `storage` (zc.beforestorage), `_storage`, and the storage of `ZODB.blob.BlobStorage`, and it refuses when one of these storages is a ZEO client without namespaces. A wrapper that keeps its storage under another name hides the client: `reset` then goes on, and gives the registry empty trees while the server can hold its rows. So run `reset` only on a storage opened directly, as the message says.

**Two stores in one process.** `copy_rows` needs a connection to each store. A script, run for example with `bin/instance run`, opens the store of the copy as a `zoe.Storage` and the store of the source through a `zoe.zeo.ClientStorage` of the `zoe serve` of the source, or as a second `zoe.Storage` when no instance holds it (an in-process store is open in one process at a time). It calls `copy_rows` and commits the transaction of the copy.

The helpers of the acceptance test call `install()` and `uninstall()`, `move`, `move_back`, and `copy_rows` for `bring_rows_back`.

#### B6. The check: a registry never reads as empty

At its first read or write in a transaction, a view of a moved registry checks its rows. It raises `ZODB.POSException.Unsupported` when:

- the registry has no connection;
- the storage of its database has no namespaces: a FileStorage, a stock ZEO client, RelStorage, a `zoe.zeo.ClientStorage` whose server did not accept `kv`, a `DemoStorage` over one of them (`zoe.kv` raises it);
- its namespace has no marker, or a marker of another version of the layout;
- the oid of the marker is not the oid of the registry;
- the counter of the marker is below the counter of the registry: the rows are older than the registry.

A counter of the marker above the counter of the registry passes: a later commit wrote the rows, and a read gives the last committed rows (section B4).

The message names the registry and says what to do: `copy_rows` from its source, `move_back` in the source before a copy, or `reset`.

One failure is not a copy: a move back committed after the snapshot of the connection, which deleted the rows. When the check fails, the view asks the store for the current serial of the registry. On a `zoe.zeo.ClientStorage` it asks the server, not the cache of the client, so an invalidation that is still on its way does not count. When that serial is newer than `_p_serial`, the view raises `ReadConflictError` in place of `Unsupported`. Zope retries the request, and the retry reads the stock registry.

So each copy raises at its first read and at its first write:

| Copy | What the copy holds | The check |
| --- | --- | --- |
| `copyTransactionsFrom`, zodbconvert, the copy of the ZODB iterator, a restore of each record | the pickle of the registry, no row | no marker: `Unsupported` |
| the same into a store restored from a backup of the source (`zodbconvert --incremental`) | the pickle of a later write, the rows of the restore | the counter of the marker is below: `Unsupported` |
| `zoe convert` to a Data.fs, and back | the pickle, no row | no namespaces, then no marker: `Unsupported` |
| a zexp import into another database | the pickle, at a new oid, no row | no marker: `Unsupported` |
| a zexp export and import in one store, an OFS copy and paste | the pickle with the same id, at a new oid | the marker holds the oid of the source: `Unsupported`. The source keeps its values |
| `zoe backup` and `zoe restore` | the pickle and the rows of one tid, the same oids | passes |

Each method of the views checks before it reads or writes a row, a write that reads no field too (`records[name] = Record(...)`). So no write reaches the namespace of another registry, and a write to a copy raises before its commit.

**The overlay of a `DemoStorage`.** On a ZOE store, the rows and the records of a commit are in one backend write. On a `DemoStorage`, the data manager of `zoe.kv` puts the rows in the overlay at its own finish, after the connection committed the records. In that moment, another thread can load the new registry before its rows, and its check fails. The layers of plone.testing run each request in the thread of the test, so the tests do not meet it.

#### B7. A write, plone.protect and conflicts

- **The first write of a transaction** to a moved registry reads the stored marker (`kv_get`, not the cache) and the marker that the transaction staged, sets the counter of `MovedRecords` to 1 more than the highest of the three counters, and stages the marker with the new counter. So it changes the state of the registry object, which ZODB writes. Each write (`__setitem__`, `__delitem__`, `clear` of the views) stages its row through `zoe.kv`.
- **A field changed in place.** The data manager of section B4 keeps the bytes of the row of each field that the transaction decoded, and pickles no field at the decode. Before the commit (a hook, as the locking backend has) and before each savepoint (`flush_before_savepoint` of `zoe.kv`), it pickles each such field. A pickle equal to the kept bytes is no change. So a transaction that reads fields pickles each of them once, at its commit. When the pickle changed, it stages the row as a write does. So an upgrade step that sets `record.field.interfaceName` writes the field, as with the stock trees.
- **A row that is not a new pickle of its object.** A pickle of the move, of a write or of the check before the commit is what a new pickle of the decoded object gives, byte for byte (a test checks each field row of a site of Plone). A row can still differ: a pickle of another writer or of another protocol, or a pickle where two attributes share one str that the decode gives back as two, or the reverse. Such a row is not a change. When the pickle differs from the kept bytes, the data manager decodes the kept bytes again and pickles that new object. When the two pickles are equal, nothing changed: it stages nothing, and it keeps the new pickle for the next check of the transaction. Only a pickle that differs from both is a change. The row stays as it is on disk until a write: a read never writes a row, so a GET makes no write that plone.protect sees and no conflict. Such a row costs one more decode and one more pickle at each commit that read it.
- **plone.protect** sees the registry among the objects of the connection. So a POST that writes a moved registry needs the token, as a POST that writes the stock trees does, and a GET that writes it without a token aborts, as with the stock trees: neither the registry nor an `OOBBucket` is a safe type of plone.protect.
- **Conflicts.** Two transactions that write a moved registry both write the registry object. The second one gets a `ConflictError`, and Zope retries it. So the rule of part A that the last writer wins never applies to two writes of a registry: a retry reads the newer rows. With the stock trees, two writes of different records in one bucket merge. With the backend, they conflict and retry. Writes of the registry are rare: control panels, upgrade steps, the install of an add-on. Before a move, look at the writes-on-GET report and the conflict log of the site: a site whose requests often write the registry gets a retry for each pair of such writes.
- **The window of a move.** A write that began before a move writes into the trees, which the move leaves. The guard of section B1 called `readCurrent` on the roots, which the move empties (or on the registry object, for a root that keeps its keys), so its commit fails with a `ConflictError`, and its retry writes the rows. A write that commits during a move changes a node, a bucket or a field that the move read current, so the move fails. A write that began before a move back writes the registry object (its counter), which the move back writes too, so it conflicts the same way; the move back puts new trees, which nothing read before it.
- A savepoint covers the writes: `zoe.kv` keeps the staged rows, ZODB keeps the state of the registry, and section B4 drops the decoded rows at a rollback.
- **The counter after a rollback.** When the connection joins the transaction before the first read of the registry, its savepoint comes first. The savepoint of the data manager of section B4 then stages a field changed in place and the counter after the connection kept the state of the registry. A rollback to that savepoint keeps the staged rows, the marker with the new counter, and gives back the registry with its old counter. So before the commit, the data manager raises the counter of each registry with staged rows whose counter the transaction did not raise, when the staged marker holds another counter than the registry. A move or `copy_rows` stages a marker with the counter of the registry, so a move followed by a read in the same transaction commits the counter 1. It takes the counter as the first write does: 1 more than the highest of the three counters. A write after the rollback raises it the same way, and the commit then raises it no more. So the commit writes the registry object, and each cache of rows scans again.

#### B8. Two sites in one database

Each registry has its own id, so its own namespace, its own marker and its own cache entry. A write of one registry writes no row of another. `move_back` of one deletes only its own rows. `uninstall()` moves nothing and deletes nothing. An OFS copy of a site in the same store holds the id of its source, and the check refuses it (section B6) until `copy_rows` gives it its own id.

- **An OFS paste of a site.** The handlers of the paste in Plone can read the registry of the copy. They then raise `Unsupported`, and the paste aborts: nothing is copied. Move the registry back, copy the site, then move both registries.
- **A deleted site.** Its rows stay in the store: pack never reads them. Move the registry back before the delete of a site. The cache forgets the namespace (section B4).

#### B9. Pack, undo, backup and the other tools

- **Pack** never reads the rows (section A7). After a move, the trees are garbage, and a garbage-collecting pack removes them. After a move back, the namespace keeps its generation row only.
- **Undo** gives back no key (section A7). ZODB undoes a transaction only when no later transaction wrote its objects.
  1. **A write of a moved registry.** The undo gives back the registry object with the counter before the write. The rows stay, with the higher counter, so the check passes and the undo changes no record: every instance reads the rows of the write. A site writes the old value again. The next write takes its counter from the stored marker, so it gets a counter that no write had, and each cache of rows scans again. Only the last write of the registry can be undone this way.
  2. **A stock write before the move.** The undo gives back a bucket of the old trees, which the registry no longer reaches. It changes no record.
  3. **The move.** The undo gives back the stock trees, with the values before the move, when no write of the registry followed the move and no pack removed the trees. Else ZODB refuses it. Its rows stay in their namespace, which nothing reads.
  4. **The undo of an undo of a move.** It gives back the moved registry with the rows of the move. A stock write between the two undos went to the trees, and is lost. Do not undo such an undo: move again.
  5. **A move back.** The undo gives back a moved registry whose rows are gone. **Each request of the site then raises `Unsupported`** until the undo of that undo gives back the stock registry. Never undo a move back.
- **`zoe backup` and `zoe restore`** keep the rows and the oids (ADR 0030, section A7). A restored site reads its registry.
- **`zoe convert`** to a Data.fs refuses a store that holds a moved registry: a namespace `zoe.plone.registry:<id>` with its marker `m`. It fails with 1 before it writes anything, and its message names each registry by its oid and tells the way out: `move_back`, a commit, then the conversion. A marker of another layout also refuses, and the message names its namespace. `--force` (`ZOE_FORCE`) converts anyway: the Data.fs leaves the rows out (section A7), and the command writes a warning that names each registry, to stderr and in its report. The registry of such a Data.fs raises `Unsupported`.
- **`zoe verify`** checks the rows as it checks every durable row.

#### B10. A site without the backend, on ZEO, or leaving ZOE

- A site whose registry never moves keeps the stock behaviour, with or without the option, except for the guard of section B1 on a store with namespaces.
- On a storage without namespaces, `move` raises `Unsupported` and changes nothing. So the option on a ZEO site, a FileStorage or RelStorage refuses only the move.
- A moved registry needs a storage with namespaces in each instance: `zoe.Storage`, or `zoe.zeo.ClientStorage` with a server that accepts `kv`. A stock `ZEO.ClientStorage` on `zoe serve` reads it as `Unsupported`.
- **The way back.** `move_back(site.portal_registry)` and a commit, for example in a script of `bin/instance run`. Then remove the option. Nothing is lost.
- **Leaving ZOE** (`zoe convert`, zodbconvert, a zexp into another database) needs the move back first. A copy without it raises `Unsupported` on each request. While the source store exists, `copy_rows` brings the rows into a ZOE copy; a copy outside ZOE has no namespaces, so the operator moves back in the source and copies again. When the source store is gone, `reset` on a storage opened directly and the import of the profiles bring the site back, without the values that it changed (section B5).
- **A downgrade of `zoedb`** to a version without this backend needs the move back first. Else `_records` unpickles as a broken object, and each read of the registry raises.
- **The guard of `zoe convert`**: `zoe convert --to-data-fs` refuses a store that holds a namespace `zoe.plone.registry:<id>` with a marker, unless `--force` (section B9). It reads no pickle: it walks the durable rows and keeps the marker rows of the registry namespaces. zodbconvert is not ZOE's, so it gets no such check.

#### B11. The test layers

- **Integration layers.** The acceptance test moves the registry of the site in the transaction of the test, which the layer aborts. The rows stay in the data manager of `zoe.kv`, and the check finds the staged marker. The abort drops the rows and the new `_records`.
- **Functional layers.** The commits go to the `DemoStorage` of the test and to its `zoe.kv` overlay. Both go with the `DemoStorage`. Each `DemoStorage` is a storage of its own for the cache. Section B6 says when the overlay takes the rows.
- **`test_exportimport`** runs on a `Registry()` in no database. It never moves, and the guard skips a registry without a connection or an oid. So the backend leaves it as it is.
- The guard runs in each test on a ZOE layer, also in the suites of Plone (`just plone zoe suites`). plone.app.testing loads no ZCML of `zoe.plone`, so `zoe.plone.testing.install()` installs the guard, as `configure.zcml` does in an instance.
- The tests of the backend install it themselves. `just plone zoe backends` keeps `ZOE_PLONE_BACKENDS=locking`.

## Consequences

- With the locking backend of `zoe.plone`, the GET of an edit form writes no object. It causes no `ConflictError` with a content write. plone.protect checks only the objects registered on the connections (`plone/protect/auto.py:215-237`), so it does not see a namespace write. The lock of an edit form needs no check: plone.locking marks it as a safe write. A backend whose writes need the check of plone.protect, as the registry does, must also mark an object as changed (section B7).
- A namespace-only commit costs a tid, a transaction row and a share of a sync. A site that writes a session on each request syncs as often as it commits.
- Two editors who open one edit form at the same moment can both take its lock, and the later write wins. With ZODB, one of them gets a `ConflictError` and a retry.
- A copy through the ZODB iterator, and `zoe convert` to a Data.fs, leave the durable rows out. An application that keeps data in a durable namespace moves it back into the object graph before such a copy. A moved registry in such a copy raises `Unsupported` until `copy_rows`, a move back in the source, or `reset` (section B6).
- The writes-on-GET report (F32) counts records by class, so it counts no namespace write.
- A build without namespaces opens a store with namespaces and ignores them: its backups leave the durable rows out, and it runs no sweep.
- `Backend` and `WriteTxn` have required methods for the namespaces. The sweep step and the restore of durable rows join the backend writes that run outside the commit sequencer.
- A moved registry writes about a hundred bytes of the registry object and the rows that change, in place of the buckets of the trees: the 208 KB for each write of the first design do not occur. The objects that the registry reaches hold less than a tenth of their bytes.
- A request reads the registry from the cache of the process, with no storage call while nobody writes it. A transaction decodes each record that it reads once.
- Two writes of a moved registry always conflict, and one retries. With the stock trees, they conflict only in one bucket.
- On every ZOE site with `zoe.plone`, a write transaction that read the registry checks the serials of the two roots of its trees at the vote, and it conflicts with a move or a split of a root, not with a write of an attribute of the registry object.
- An OFS copy of a site with a moved registry raises `Unsupported` until `copy_rows`, a move back in the source, or `reset`.
- The undo of a write of a moved registry changes no record. The undo of a move back takes the site down until it is undone.
- Each instance of a site with a moved registry uses a storage with namespaces and a `zoedb` with the registry backend.

## Alternatives

- **Keep sessions, locks and the registry in ZODB objects, with merges.** Merges reduce the conflicts, but two writes of one key still conflict. A lock still writes the content object, and every version of a session stays until a pack.
- **A separate store file.** A second redb file has its own sync, so its writes do not commit atomically with the records. It doubles the open, the lock, the backup and the verify.
- **An in-memory cache with no durability.** Zope's temporary folder does this for sessions. A restart then loses every session and every lock, nothing commits atomically with the records, and the registry cannot live there.
- **Redis.** It adds a service to deploy and a network round trip, and nothing commits atomically with the records. The registry still needs a durable store.
- **Reuse the `sequences` key space.** A namespace row is another kind of frame, and ADR 0020 gives each kind its own tag. Sequence keys are `&str`. A sequence is outside the two-phase commit by design (ADR 0024).
- **History for the durable flavour**, a row for each `(name, key, tid)`. Reads would follow the snapshot and undo would work. But each read becomes a range scan, and pack must then read the namespace and remove old rows.
- **A version check for each key.** It gives an exact lock of an edit form, but two writers of one key then conflict, and each row needs a version.
- **Ephemeral-only commits without a sync and without a tid.** A session write would cost no sync, but a crash would lose the acknowledged writes after the last durable commit, and the store would need a second commit path. A deferred commit with a tid is not possible: after a crash, a ZEO client that saw the lost tid refuses the server (`ZEO/asyncio/client.py:598-605`).
- **A sweep by a cursor over every row, without `kv_expiry`.** It saves a key space, but each step reads live rows, and an expired key goes only when the cursor reaches it.
- **One namespace for every registry**, as views of `_fields` and `_values`. Two sites overwrite each other (see Context).
- **The oid of the registry as the name of its namespace**, as queued indexing names its queue. An in-store copy gets a new oid and finds no rows, so it is detected too. But an oid is an address in one store: two sites created the same way have the same oids in two stores, so a copy from one store into a restore of the other reads the rows of another registry. A random id is unique in every store.
- **One namespace with the id as a key prefix.** It works, but the registries then share one generation row and one scan range.
- **JSON values.** See section B3.
- **Pickle any persistent object by value.** A value with a persistent object that is not a field is a bug of its writer (section B3).
- **Only the values move.** The fields are most of the bytes, and each is a record of its own.
- **A cache on the generation of the namespace**, read at the start of each transaction. It costs one storage call for each transaction; the counter of the registry costs none, because ZODB invalidates the registry. The scan still reads the generation, to find a commit during the scan.
- **No write counter**, only the oid in the marker. Simpler, but a copy into a store restored from a backup of the source reads the older rows of the restore, and an undo of a write leaves the state of the registry the same. The check accepts a counter of the marker above the counter of the registry, so a reader whose snapshot is older than the last write passes.
- **A small persistent stamp object, marked at each write, in place of the registry object.** It saves the write of the registry pickle, which is small after the move. It adds an object to each registry.
- **`Unsupported` also when a move back committed after the snapshot.** The retry that a `ReadConflictError` gives reads the stock registry, so the request succeeds.
- **No option at all**, the move as the only opt-in, or a moved registry that raises in a process without the option. The data decides, so an instance that misses the option still serves the site.
- **An automatic move**, at the install or at the first write. The move is an explicit step.
- **Keep the id of the source for a copy in another store.** A new id never shares rows, in any store.
- **No `copy_rows`, only a move back in the source before each copy.** A copy made without the move back then gets its rows only when the source moves back and the copy is made again.
- **Keep the rows at the move back.** Stale rows could then read.
- **No `reset`**, and the operator writes the trees by hand.
- **Refuse the undos of section B9.** ZODB gives the storage no hook for it, so it needs a change of `zoe.Storage`.
- **A subscriber of `IObjectCopiedEvent` that calls `copy_rows`** for each copied registry. It is a feature that the specification does not name.
- **The guard on the registry object.** Simpler, but every transaction that read the registry then conflicts with a write of an attribute of the registry object, which the import of a profile makes through the subscribers of the resources and of the theme. A test of plone.restapi (`test_install_specific_profile`) fails so, as it commits with no retry.
- **The guard only with the option.** Then an instance without the option can lose a write that races the move. The guard costs one serial check for each write transaction, so it runs always.
- **No guard; move only when no request writes the registry.** Simpler, but a write that races the move is lost.
- **List the in-place change of a field as a break.** Simpler, but an upgrade step of plone.app.upgrade does it, and its change would be lost with no error.
- **Write the new pickle of a row once at its first read.** A read then writes the registry and its counter.
- **The warning of `zoe convert` as the only signal for a moved registry.** The Data.fs then holds a registry that raises `Unsupported` on each request.

## Merged ADRs

- ADR 0050, the registry backend: Decision, part B. Its section N is section BN here, for N from 1 to 11. Its context is in Context, "The registry", and its alternatives are in Alternatives.
