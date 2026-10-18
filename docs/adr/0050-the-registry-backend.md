# 0050. The registry backend: a moved registry keeps its records in a durable namespace of its own

Status: accepted. The checkpoint of ZOE-217 (step 1), revised after its adversarial review on the pull request (#472). The human approved it with the recommendation of each of its 15 questions. It amends ADR 0043: item 2 of "Claude's choices" (the registry backend is not built) and the consequence on a copy through the ZODB iterator. It adds an opt-in backend to `zoe.plone` and a public Python API (`zoe.plone.registry`). It changes no on-disk format, no wire protocol and no code of the core.

This is a new ADR, not an edit of ADR 0043. ADR 0043 is accepted with the answers of the human: the key spaces, the protocol extension, `zoe.kv` and the rules of the namespaces. This ADR changes none of them. It builds a backend of `zoe.plone` on them, as ADR 0047 does for queued indexing. When the human accepts it, the status of ADR 0043 names it, and its item 2 points here.

## Context

The card asks for `plone.registry` in the durable namespace of ADR 0043, with no way for a registry to read as empty, to overwrite another registry, or to lose its rows in a copy.

What plone.registry 2.0.2, plone.app.registry 2.0.7 and the other eggs of the build do (paths in the eggs of `target/plone/eggs/cp313`):

- `portal_registry` is a `plone.app.registry.registry.Registry`, a `plone.registry.registry.Registry` and an OFS `SimpleItem`. Its attribute `_records` is a `_Records`, which is not persistent: it is in the pickle of the registry. It holds two `OOBTree`s, `_fields` and `_values` (`plone/registry/registry.py:140-158`). A field is a `PersistentField` (`plone/registry/field.py:104`), so each field is a record of its own. A field of a `FieldRef` is a str in `_fields`: the name of the record that holds the field (`registry.py:243-266`).
- `Registry.__getitem__`, `get` and `__contains__` read `self.records._values` (`registry.py:33-50`). `Record.value` reads and writes `self.__parent__.records._values[name]` (`plone/registry/record.py:66-90`). `_Records` reads and writes `self._fields` and `self._values` only. No other egg of the build reads `_fields` or `_values`.
- Code can change a field in place. The upgrade step `change_interface_on_lang_registry_records` of plone.app.upgrade sets `record.field.interfaceName` (`plone/app/upgrade/v52/final.py:125-135`). With the stock trees, the field is a persistent object, so the change is written.
- `Registry.records` is a property. It migrates the old persistent `Records` class, and only that class (`registry.py:52-57`, `:123-138`).
- `ZODB.broken.PersistentBroken.__reduce__` raises `BrokenModified` (`ZODB/broken.py:335-336`). A site often keeps records of an add-on that is gone; their fields load as such broken objects.
- On the layers of plone.app.testing over a ZOE store, the registry of a site and the objects that it reaches hold 1.7 MB.
- plone.protect checks the objects that the connections registered (`plone/protect/auto.py:215-237`). A namespace write registers no object (ADR 0043, Consequences).

What the review of ZOE-207 (#420) found in a first design, which kept every registry of a store in one namespace, as views of `_fields` and `_values`:

1. Two Plone sites of one Zope root overwrote each other, and the uninstall of one emptied the other.
2. A copy into another ZOE store (`copyTransactionsFrom`, zodbconvert, a zexp import, `zoe convert` and back) read as empty and took writes.
3. Before its first write, a new registry kept all its records in its own pickle: 346 KB, and 208 KB more for each write on FileStorage.

What ADR 0043 gives: a durable write commits or aborts with the transaction and takes no lock, and the last writer of a key wins; a read gives the last committed value, not the value at the snapshot of the connection; the generation row of a namespace is the tid of the last commit that wrote it; a copy through the ZODB iterator and `zoe convert` to a Data.fs leave the rows out (answer 13); `zoe backup` and `zoe restore` keep them; pack never reads them; undo gives back no key. A `DemoStorage` over a ZOE store has namespaces in an overlay.

## Decision

### 1. The option and the guard

- `zoe.plone.registry` is a backend of `zoe.plone`, opt-in, as `locking` and `indexing` are: `ZOE_PLONE_BACKENDS=registry`, or the ZCML file `zoe.plone:registry.zcml`. Both call `install()` when the ZCML loads. `uninstall()` takes it out of the process. `installed()` tells if it is installed. A second call of either does nothing. Neither moves a registry.
- **The move is a step of its own**, explicit: `move(registry)` (section 5). A registry keeps its stock `OOBTree`s until a move.
- `install()` lets `move`, `copy_rows` and `reset` run. Without it they raise `RuntimeError`. That is all it does. `uninstall()` also drops the cache of rows (section 4).
- A moved registry reads and writes in every process that has this version of `zoedb`, with or without the option. The data decides, not the process. So an instance that misses the option still serves the site.
- **The guard of a stock registry** runs whenever `zoe.plone` loads, with or without the option: `configure.zcml` installs it, as it installs the other handlers of `zoe.plone`. When code reads `records` of a stock registry whose storage has namespaces, the guard loads the registry if it is a ghost, then calls `readCurrent` on it. A registry without a connection or an oid, or in a storage without namespaces, is skipped. So a write that began before a move, in any instance, conflicts at its commit, and Zope retries it on the moved registry (section 7).
- **The cost of the guard, on every ZOE site with `zoe.plone`**, moved or not: each transaction that reads the registry and writes an object checks the serial of the registry at its vote (`checkCurrentSerialInTransaction`, a shared lock). A transaction that changes the registry object itself (a move, a move back, an edit of `portal_registry` in the ZMI) makes such a transaction conflict and retry. The stock behaviour otherwise stays.

### 2. What moves, and the rows

- **One namespace for each registry.** A move draws an id: 16 random bytes from `secrets`, as 32 lowercase hex digits. The rows of the registry go to the durable namespace `zoe.plone.registry:` followed by the id (51 bytes). Two registries never share a namespace, also in one store, also when their oids are the same in two stores.
- **The rows.**

  | Key | Value |
  | --- | --- |
  | `f` followed by the name of the record | the pickle of its field, or of the str of a `FieldRef` |
  | `v` followed by the name of the record | the pickle of its value |
  | `m` | the marker: the version of the layout (1 byte, 1), the oid of the registry (8 bytes), and the write counter (8 bytes, big-endian) |

  A record name passes `_validkey` of plone.registry, so it starts with a letter: an `f` or `v` key has at least 2 bytes, and the marker key has 1. A key holds at most 1,024 bytes, so a name holds at most 1,023 bytes. A longer name raises `ValueError` at its write. The keys of one kind are in the order of the names, as in an `OOBTree` of str keys (ADR 0043, section 1).
- **The write counter** is an int in the state of the registry and in the marker. A move sets it to 1. Each transaction that writes the registry adds 1 to both, once (section 7). The check of section 6 compares them.
- **The registry after the move.** Its `_records` is a `zoe.plone.registry.MovedRecords`. This class is a subclass of `_Records`. Its `_fields` and `_values` are views of the rows of its namespace. They give the methods of an `OOBTree` that `_Records` and `Registry` call: `__getitem__`, `get`, `__setitem__`, `__delitem__`, `__contains__`, `__len__`, `__iter__`, `keys(min, max)`, `minKey`, `maxKey` and `clear`. `keys(min, max)` includes `max`, and a scan excludes its `stop`, so the view scans to the key after `max`. The pickled state of `MovedRecords` is the registry without its acquisition wrapper (`__parent__`), the id and the counter: about a hundred bytes in the pickle of the registry. The class name is in the pickles of the store, so it stays importable.
- **The trees** stay in the store as they were. The registry no longer reaches them, so a garbage-collecting pack removes them. Until then, an undo of the move can give them back (section 9).

### 3. The pickles

- **Why pickles.** ADR 0047 keeps JSON in its keys: an operation of the queue is a path and names, which JSON holds. A record holds a field, an instance of a class of plone.registry, and a value that can be a tuple, a set, a frozenset, a `Decimal`, a `datetime` or bytes. JSON holds none of them without a codec of our own. ZODB keeps the same objects as pickles in the same store, so the rows add no new trust.
- The backend pickles with protocol 3, as ZODB does, by value. A field is a `Persistent` that provides `IPersistentField`; it pickles with its state as a plain object, and it unpickles as a new object with no connection. A field of a collection holds its `value_type` field in its state, the same way.
- **The pickler refuses a persistent object that is not an `IPersistentField`**, with `TypeError` at the write. The types of the fields of plone.registry hold no such object, so a value with one is a bug of its writer, which the stock trees would keep as a reference.
- **A broken field**, of an add-on whose class is gone, loads from the trees as a `PersistentBroken`, whose pickle raises. The pickler writes it as ZODB writes a broken object: `ZODB.broken.rebuild` with the module and the name of its class and its state. A read unpickles with `ZODB.broken.find_global`, so it gives a broken object while the class is gone, and the field itself when the class comes back. The move and the move back keep such records as they are. A read of such a record fails as with the stock trees.

### 4. A read, and the cache

- Each method of the views first runs the check of section 6, a write too. Then it reads the writes of the current transaction (`zoe.kv`), then the rows of a cache of the process.
- **The cache.** For each storage of a database (held through a weak reference) and each namespace, the process keeps the rows as bytes, with a label: the write counter of the marker of those rows. One lock guards the cache, for the free-threaded interpreter.
  - A view whose registry has a counter at or below the label takes the cached rows. Else it scans the namespace.
  - **A scan is atomic.** It reads the generation of the namespace, scans every row, and reads the generation again. When the generation changed, a commit wrote the namespace during the scan, and it scans again; after 3 tries, it raises `ConflictError`, and Zope retries the request. The scan then puts its rows and its label in the cache together, under the lock, and only when its label is above the label in the cache.
  - The cache keeps at most 16 namespaces for each storage; the namespace that was read least recently goes first. So the rows of a deleted site (section 8) leave the memory.
- Each write of a moved registry changes the state of the registry object (its counter), so ZODB invalidates the registry in each connection. A request then makes no storage call for the registry while nobody writes it, as with the pickle cache of the stock trees.
- **What a read gives.** The rows are never older than the snapshot of the registry, but they can be newer: a read gives the last committed rows (ADR 0043). A connection opened at a past time (`db.open(at=...)`) reads the current rows too.
- **In a transaction.** A transaction takes the cached rows once, at its first read, and keeps them to its end, with its own writes over them. It decodes a row once and keeps the object. A data manager of `zoe.plone.registry` joins each transaction that decodes a row. At a rollback to a savepoint, it drops the decoded rows, so the next read decodes them again from the rows that the savepoint kept. At the end of the transaction it drops them all. An in-place change of a value is not written, as with the stock trees, where a bucket that nothing else changes keeps no in-place change.

### 5. `move`, `move_back`, `copy_rows` and `reset`

Each one changes the current transaction of the connection of its registry. The caller commits. `move`, `copy_rows` and `reset` need `install()`.

- **`move(registry)`.** `registry` is a registry of plone.registry in a database. A registry that is moved already stays as it is. A registry with no connection, or in a storage without namespaces, raises `Unsupported` and stays as it is. The move draws the id, writes a row for each field and each value, and the marker with the oid of the registry and the counter 1, and puts a `MovedRecords` in `_records`. It calls `readCurrent` on each object of the trees that it reads (the `OOBTree`s, their internal nodes and their buckets), so its commit fails with a `ConflictError` when a write changed them after its snapshot. On a site of Plone the move writes about 1.7 MB of rows in one transaction.
- **`move_back(registry)`.** `registry` is a moved registry that passes the check of section 6, else `Unsupported`. It puts a `_Records` with new `OOBTree`s in `_records`, with each field and each value of the rows: the fields become new persistent objects. It deletes each row of the namespace, the marker too. The generation row stays (ADR 0043 deletes no generation row), so the namespace keeps 1 row. Nothing is lost: each record of the rows goes back to the trees.
- **`copy_rows(source, copy)`.** `source` is a moved registry that passes the check. `copy` is a moved registry with the id of `source` that fails it: a copy of `source` in another store or in the same store, with no rows, with the rows of another oid, or with stale rows. Else it raises `ValueError`. It reads the rows of `source` with an atomic scan (section 4), with the writes of the current transaction of `source`, and calls `readCurrent` on `source`. It draws a new id for `copy`, writes the rows in the namespace of the new id in the store of `copy`, with the oid of `copy` and the counter of `source` in the marker, and puts a `MovedRecords` of the new id and that counter in `_records` of `copy`. When the old namespace of `copy` in its store has a marker with the oid of `copy` (stale rows), it deletes those rows. A marker with another oid belongs to another registry, and stays. With a new id, a copy in the same store never shares the rows of its source.
- **`reset(registry)`**, the recovery when no source is left. `registry` is a moved registry that fails the check. `reset` puts a `_Records` with new empty `OOBTree`s in `_records`, and reads no row. The operator then imports the registry step (`plone.app.registry`) of each profile that the site has installed, through `portal_setup`. The records come back with the values of the profiles: the values that the site changed are lost. A registry that passes the check raises `ValueError`: it moves back instead.

**Two stores in one process.** `copy_rows` needs a connection to each store. A script, run for example with `bin/instance run`, opens the store of the copy as a `zoe.Storage` and the store of the source through a `zoe.zeo.ClientStorage` of the `zoe serve` of the source, or as a second `zoe.Storage` when no instance holds it (an in-process store is open in one process at a time). It calls `copy_rows` and commits the transaction of the copy.

The helpers of the acceptance test keep their calls: `install()` and `uninstall()`, `move`, `move_back`, and `copy_rows` for `bring_rows_back`.

### 6. The check: a registry never reads as empty

At its first read or write in a transaction, a view of a moved registry checks its rows. It raises `ZODB.POSException.Unsupported` when:

- the registry has no connection;
- the storage of its database has no namespaces: a FileStorage, a stock ZEO client, RelStorage, a `zoe.zeo.ClientStorage` whose server did not accept `kv`, a `DemoStorage` over one of them (`zoe.kv` raises it);
- its namespace has no marker, or a marker of another version of the layout;
- the oid of the marker is not the oid of the registry;
- the counter of the marker is below the counter of the registry: the rows are older than the registry.

A counter of the marker above the counter of the registry passes: a later commit wrote the rows, and a read gives the last committed rows (section 4).

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

### 7. A write, plone.protect and conflicts

- **The first write of a transaction** to a moved registry adds 1 to the counter of `MovedRecords` and stages the marker with the new counter. So it changes the state of the registry object, which ZODB writes. Each write (`__setitem__`, `__delitem__`, `clear` of the views) stages its row through `zoe.kv`.
- **A field changed in place.** The data manager of section 4 keeps the pickle of each field that the transaction decoded. Before the commit (a hook, as the locking backend has) and before each savepoint (`flush_before_savepoint` of `zoe.kv`), it pickles each such field again. When the pickle changed, it stages the row as a write does. So an upgrade step that sets `record.field.interfaceName` writes the field, as with the stock trees.
- **plone.protect** sees the registry among the objects of the connection. So a POST that writes a moved registry needs the token, as a POST that writes the stock trees does, and a GET that writes it without a token aborts, as with the stock trees: neither the registry nor an `OOBBucket` is a safe type of plone.protect.
- **Conflicts.** Two transactions that write a moved registry both write the registry object. The second one gets a `ConflictError`, and Zope retries it. So the rule of ADR 0043 that the last writer wins never applies to two writes of a registry: a retry reads the newer rows. With the stock trees, two writes of different records in one bucket merge. With the backend, they conflict and retry. Writes of the registry are rare: control panels, upgrade steps, the install of an add-on. Before a move, look at the writes-on-GET report and the conflict log of the site: a site whose requests often write the registry gets a retry for each pair of such writes.
- **The window of a move.** A write that began before a move writes into the trees, which the move leaves. The guard of section 1 called `readCurrent` on the registry, so its commit fails with a `ConflictError`, and its retry writes the rows. A write that began before a move back writes the registry object, which the move back writes too, so it conflicts the same way.
- A savepoint covers the writes: `zoe.kv` keeps the staged rows, ZODB keeps the state of the registry, and section 4 drops the decoded rows at a rollback.

### 8. Two sites in one database

Each registry has its own id, so its own namespace, its own marker and its own cache entry. A write of one registry writes no row of another. `move_back` of one deletes only its own rows. `uninstall()` moves nothing and deletes nothing. An OFS copy of a site in the same store holds the id of its source, and the check refuses it (section 6) until `copy_rows` gives it its own id.

- **An OFS paste of a site.** The handlers of the paste in Plone can read the registry of the copy. They then raise `Unsupported`, and the paste aborts: nothing is copied. Move the registry back, copy the site, then move both registries.
- **A deleted site.** Its rows stay in the store: pack never reads them. Move the registry back before the delete of a site. The cache forgets the namespace (section 4).

### 9. Pack, undo, backup and the other tools

- **Pack** never reads the rows (ADR 0043). After a move, the trees are garbage, and a garbage-collecting pack removes them. After a move back, the namespace keeps its generation row only.
- **Undo** gives back no key (ADR 0043). ZODB undoes a transaction only when no later transaction wrote its objects.
  1. **A write of a moved registry.** The undo gives back the registry object with the counter before the write. The rows stay, with the higher counter, so the check passes and the undo changes no record. A site writes the old value again. Only the last write of the registry can be undone this way.
  2. **A stock write before the move.** The undo gives back a bucket of the old trees, which the registry no longer reaches. It changes no record.
  3. **The move.** The undo gives back the stock trees, with the values before the move, when no write of the registry followed the move and no pack removed the trees. Else ZODB refuses it. Its rows stay in their namespace, which nothing reads.
  4. **The undo of an undo of a move.** It gives back the moved registry with the rows of the move. A stock write between the two undos went to the trees, and is lost. Do not undo such an undo: move again.
  5. **A move back.** The undo gives back a moved registry whose rows are gone. **Each request of the site then raises `Unsupported`** until the undo of that undo gives back the stock registry. Never undo a move back.
- **`zoe backup` and `zoe restore`** keep the rows and the oids (ADR 0030, ADR 0043). A restored site reads its registry.
- **`zoe convert`** to a Data.fs leaves the rows out and prints a warning (ADR 0043). The registry of the Data.fs raises `Unsupported`. Move it back before the conversion.
- **`zoe verify`** checks the rows as it checks every durable row.

### 10. A site without the backend, on ZEO, or leaving ZOE

- A site whose registry never moves keeps the stock behaviour, with or without the option, except for the guard of section 1 on a store with namespaces.
- On a storage without namespaces, `move` raises `Unsupported` and changes nothing. So the option on a ZEO site, a FileStorage or RelStorage refuses only the move.
- A moved registry needs a storage with namespaces in each instance: `zoe.Storage`, or `zoe.zeo.ClientStorage` with a server that accepts `kv`. A stock `ZEO.ClientStorage` on `zoe serve` reads it as `Unsupported`.
- **The way back.** `move_back(site.portal_registry)` and a commit, for example in a script of `bin/instance run`. Then remove the option. Nothing is lost.
- **Leaving ZOE** (`zoe convert`, zodbconvert, a zexp into another database) needs the move back first. A copy without it raises `Unsupported` on each request. While the source store exists, `copy_rows` brings the rows into a ZOE copy; a copy outside ZOE has no namespaces, so the operator moves back in the source and copies again. When the source store is gone, `reset` and the import of the profiles bring the site back, without the values that it changed.
- **A downgrade of `zoedb`** to a version without this backend needs the move back first. Else `_records` unpickles as a broken object, and each read of the registry raises.
- **A later card** (proposed): `zoe convert` refuses a store that holds a namespace `zoe.plone.registry:<id>` with a marker, unless an option forces it. `zoe convert` is in the core and reads no pickle; a namespace name is enough. zodbconvert is not ZOE's, so it gets no such check.

### 11. The test layers

- **Integration layers.** The acceptance test moves the registry of the site in the transaction of the test, which the layer aborts. The rows stay in the data manager of `zoe.kv`, and the check finds the staged marker. The abort drops the rows and the new `_records`.
- **Functional layers.** The commits go to the `DemoStorage` of the test and to its `zoe.kv` overlay. Both go with the `DemoStorage`. Each `DemoStorage` is a storage of its own for the cache. Section 6 says when the overlay takes the rows.
- **`test_exportimport`** runs on a `Registry()` in no database. It never moves, and the guard skips a registry without a connection or an oid. So the backend leaves it as it is.
- The guard runs in each test on a ZOE layer, also in the suites of Plone (`just plone zoe suites`), because `zoe.plone` loads there.
- The tests of the backend install it themselves. `just plone zoe backends` keeps `ZOE_PLONE_BACKENDS=locking`.

## Consequences

- A moved registry writes about a hundred bytes of the registry object and the rows that change, in place of the buckets of the trees: the 208 KB for each write of the first design are gone. The objects that the registry reaches hold less than a tenth of their bytes.
- A request reads the registry from the cache of the process, with no storage call while nobody writes it. A transaction decodes each record that it reads once.
- Two writes of a moved registry always conflict, and one retries. With the stock trees, they conflict only in one bucket.
- On every ZOE site with `zoe.plone`, a write transaction that read the registry checks its serial at the vote, and it conflicts with a change of the registry object.
- A copy of a moved registry raises `Unsupported` until `copy_rows`, a move back in the source, or `reset`. An OFS copy of a site is such a copy.
- The undo of a write of a moved registry changes no record. The undo of a move back takes the site down until it is undone.
- Each instance of a site with a moved registry uses a storage with namespaces and a `zoedb` with this backend.
- Step 3 of the card updates F31 and the paragraph on `zoe.plone` of the specification, and the status of ADR 0043.

## Alternatives

- **One namespace for every registry** (the first design). Rejected by the review of ZOE-207: two sites overwrote each other.
- **The oid of the registry as the name of its namespace**, as queued indexing names its queue. An in-store copy gets a new oid and finds no rows, so it is detected too. But an oid is an address in one store: two sites created the same way have the same oids in two stores, so a copy from one store into a restore of the other reads the rows of another registry. A random id is unique in every store.
- **One namespace with the id as a key prefix.** It works, but the registries then share one generation row and one scan range.
- **JSON values.** See section 3.
- **Only the values move.** The fields are most of the bytes, and each is a record of its own.
- **A cache on the generation of the namespace** (ADR 0043, answer 6), read at the start of each transaction. It costs one storage call for each transaction; the counter of the registry costs none, because ZODB invalidates the registry. The scan still reads the generation, to find a commit during the scan.
- **No write counter**, only the oid in the marker. Simpler, but a copy into a store restored from a backup of the source reads the older rows of the restore, and an undo of a write leaves the state of the registry the same. The check accepts a counter of the marker above the counter of the registry, so a reader whose snapshot is older than the last write passes.
- **A small persistent stamp object, marked at each write, in place of the registry object.** It saves the write of the registry pickle, which is small after the move. It adds an object to each registry.
- **An automatic move**, at the install or at the first write. The card makes the move an explicit step.
- **The guard only with the option.** Then an instance without the option can lose a write that races the move. The guard costs one serial check for each write transaction, so it runs always.
- **No guard; move only when no request writes the registry.** Simpler, but a write that races the move is lost.
- **List the in-place change of a field as a break.** Simpler, but an upgrade step of plone.app.upgrade does it, and its change would be lost with no error.

## Questions for the human

Each question gives Claude's recommendation first. The recommendations of questions 4, 5, 6, 9 and 12 follow the adversarial review of the pull request.

1. **A new ADR or an edit of ADR 0043.** Recommended: this new ADR 0050, and a pointer in the status of ADR 0043 when it is accepted. Other choice: an amendment section in ADR 0043.
2. **What `install()` does.** Recommended: it lets `move`, `copy_rows` and `reset` run, and nothing else; the guard and a moved registry work in every process (section 1). Other choices: no option at all, the move is the only opt-in (the helper `backend_on` then calls nothing); or a moved registry raises in a process without the option.
3. **The key layout.** Recommended: one namespace `zoe.plone.registry:<id>` for each registry, with the keys `f<name>`, `v<name>` and `m`, and a marker of the version, the oid and the counter (section 2). Other choice: one namespace with the id as a key prefix.
4. **The pickles.** Recommended: protocol 3, by value; refuse a persistent object that is not an `IPersistentField`; keep a broken field through `ZODB.broken.rebuild`, and unpickle with `ZODB.broken.find_global` (section 3). Other choice: pickle any persistent object by value.
5. **The cache.** Recommended: the rows of each namespace as bytes, labelled with the counter of their marker; an atomic scan that reads the generation before and after; a fill under a lock that only raises the label; a weak reference to the storage; at most 16 namespaces for each storage (section 4). Other choice: a read of the generation at the start of each transaction, as ADR 0043 answer 6 foresaw.
6. **The write counter, and each write marks the registry.** Recommended: the first write of a transaction adds 1 to the counter in the registry and in the marker, so the registry object changes, plone.protect sees it, two writes conflict, and a stale copy fails the check (sections 2, 6, 7). Other choices: no counter, only the oid (the gap of a store restored from a backup stays); or a stamp object in the registry.
7. **The errors of the check.** Recommended: `Unsupported` for a copy or for stale rows, and `ReadConflictError` when the store, or the server of a `zoe.zeo.ClientStorage`, has a newer serial of the registry (section 6). Other choice: `Unsupported` in both cases.
8. **`copy_rows`.** Recommended: keep the name; it draws a new id for the copy, refreshes stale rows, deletes the old rows of the copy only when their marker holds the oid of the copy, and refuses a copy that does not hold the id of its source (section 5). Other choices: keep the id of the source for a copy in another store; or no `copy_rows`, only "move back before the copy", and the last part of the acceptance test changes.
9. **`move_back` deletes the rows, and `reset` is the recovery.** Recommended: `move_back` deletes the rows, so stale rows never read; `reset` gives a registry with no source left empty stock trees, and the import of the profiles gives back the records with their default values (sections 5, 10). Other choices: keep the rows at the move back; or no `reset`, and the operator writes the trees by hand.
10. **Undo.** Recommended: state the five cases of section 9, with "never undo a move back" and "never undo the undo of a move". Other choice: refuse such undos; ZODB gives the storage no hook for it, so it needs a change of `zoe.Storage`.
11. **An OFS copy of a site.** Recommended: it raises `Unsupported` until an operator runs `copy_rows`, and a paste whose handlers read the registry aborts (section 8). Other choice: a subscriber of `IObjectCopiedEvent` calls `copy_rows` for each copied registry; it is a feature that the card does not name.
12. **The guard.** Recommended: it runs whenever `zoe.plone` loads on a store with namespaces, with or without the option, and `move` calls `readCurrent` on each object of the trees (sections 1, 5). It costs one serial check at the vote of each write transaction that read the registry. Other choices: the guard only with the option, and the risk that an instance without it loses a write during a move; or no guard, and the move runs when no request writes the registry.
13. **The helpers of the acceptance test.** Recommended: keep them as the planner wrote them; the names of this ADR fit them.
14. **A field changed in place** (new). Recommended: the data manager pickles each decoded field again before the commit and before each savepoint, and stages the row when it changed (section 7). Other choice: list the break; the upgrade step of plone.app.upgrade then loses its change with no error.
15. **A later card for `zoe convert`** (new). Recommended: open it: `zoe convert` refuses a store with a registry marker unless an option forces it (section 10). Other choice: the warning of ADR 0043 stays the only signal.
