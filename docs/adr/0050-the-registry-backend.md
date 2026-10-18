# 0050. The registry backend: a moved registry keeps its records in a durable namespace of its own

Status: proposed. The checkpoint of ZOE-217 (step 1). It amends ADR 0043: item 2 of "Claude's choices" (the registry backend is not built) and the consequence on a copy through the ZODB iterator. It adds an opt-in backend to `zoe.plone` and a public Python API (`zoe.plone.registry`). It changes no on-disk format, no wire protocol and no code of the core.

This is a new ADR, not an edit of ADR 0043. ADR 0043 is accepted with the answers of the human: the key spaces, the protocol extension, `zoe.kv` and the rules of the namespaces. This ADR changes none of them. It builds a backend of `zoe.plone` on them, as ADR 0047 does for queued indexing. When the human accepts it, the status of ADR 0043 names it, and its item 2 points here.

## Context

The card asks for `plone.registry` in the durable namespace of ADR 0043, with no way for a registry to read as empty, to overwrite another registry, or to lose its rows in a copy.

What plone.registry 2.0.2 and plone.app.registry 2.0.7 do (paths in the eggs of `target/plone/eggs/cp313`):

- `portal_registry` is a `plone.app.registry.registry.Registry`, a `plone.registry.registry.Registry` and an OFS `SimpleItem`. Its attribute `_records` is a `_Records`, which is not persistent: it is in the pickle of the registry. It holds two `OOBTree`s, `_fields` and `_values` (`plone/registry/registry.py:140-158`). A field is a `PersistentField` (`plone/registry/field.py:104`), so each field is a record of its own. A field of a `FieldRef` is a str in `_fields`: the name of the record that holds the field (`registry.py:243-266`).
- `Registry.__getitem__`, `get` and `__contains__` read `self.records._values` (`registry.py:33-50`). `Record.value` reads and writes `self.__parent__.records._values[name]` (`plone/registry/record.py:66-90`). `_Records` reads and writes `self._fields` and `self._values` only. No other egg of the build reads `_fields` or `_values`.
- `Registry.records` is a property. It migrates the old persistent `Records` class, and only that class (`registry.py:52-57`, `:123-138`).
- On the layers of plone.app.testing over a ZOE store, the registry of a site and the objects that it reaches hold 1.7 MB.
- plone.protect checks the objects that the connections registered (`plone/protect/auto.py:215-237`). A namespace write registers no object (ADR 0043, Consequences).

What the review of ZOE-207 (#420) found in a first design, which kept every registry of a store in one namespace, as views of `_fields` and `_values`:

1. Two Plone sites of one Zope root overwrote each other, and the uninstall of one emptied the other.
2. A copy into another ZOE store (`copyTransactionsFrom`, zodbconvert, a zexp import, `zoe convert` and back) read as empty and took writes.
3. Before its first write, a new registry kept all its records in its own pickle: 346 KB, and 208 KB more for each write on FileStorage.

What ADR 0043 gives: a durable write commits or aborts with the transaction and takes no lock, and the last writer of a key wins; a read gives the last committed value, not the value at the snapshot of the connection; a copy through the ZODB iterator and `zoe convert` to a Data.fs leave the rows out (answer 13); `zoe backup` and `zoe restore` keep them; pack never reads them; undo gives back no key. A `DemoStorage` over a ZOE store has namespaces in an overlay.

## Decision

### 1. The option

- `zoe.plone.registry` is a backend of `zoe.plone`, opt-in, as `locking` and `indexing` are: `ZOE_PLONE_BACKENDS=registry`, or the ZCML file `zoe.plone:registry.zcml`. Both call `install()` when the ZCML loads. `uninstall()` takes it out of the process. `installed()` tells if it is installed. A second call of either does nothing. Neither moves a registry.
- **The move is a step of its own**, explicit: `move(registry)` (section 4). `install()` moves nothing. A registry keeps its stock `OOBTree`s until a move.
- `install()` does two things in the process:
  - It lets `move` and `copy_rows` run. Without it they raise `RuntimeError`. So the operator puts the option on each instance before a move.
  - It adds the guard of a stock registry (section 6): while installed, a stock registry in a store with namespaces calls `readCurrent` on itself when code reads its `records`. A write that began before a move then conflicts at its commit, and Zope retries it on the moved registry.
- A moved registry reads and writes in every process that has this version of `zoedb`, with or without the option. The data decides, not the process. So an instance that misses the option still serves the site.
- `uninstall()` removes the guard and drops the cache of rows (section 3).

### 2. What moves, and the rows

- **One namespace for each registry.** A move draws an id: 16 random bytes from `secrets`, as 32 lowercase hex digits. The rows of the registry go to the durable namespace `zoe.plone.registry:` followed by the id (51 bytes). Two registries never share a namespace, also in one store, also when their oids are the same in two stores.
- **The rows.**

  | Key | Value |
  | --- | --- |
  | `f` followed by the name of the record | the pickle of its field, or of the str of a `FieldRef` |
  | `v` followed by the name of the record | the pickle of its value |
  | `m` | the marker: the version of the layout (1 byte, 1), then the oid of the registry (8 bytes) |

  A record name passes `_validkey` of plone.registry, so it starts with a letter: an `f` or `v` key has at least 2 bytes, and the marker key has 1. A key holds at most 1,024 bytes, so a name holds at most 1,023 bytes. A longer name raises `ValueError` at its write. The keys of one kind are in the order of the names, as in an `OOBTree` of str keys (ADR 0043, section 1).
- **The pickles.** The backend pickles with protocol 3, as ZODB does, by value. A field is a `Persistent`; it pickles with its state as a plain object, and it unpickles as a new object with no connection. A value is of the types of the fields of plone.registry: these hold no persistent object. A read unpickles with `ZODB.broken.find_global`, so a class that is gone gives a broken object, as ZODB does.
- **The registry after the move.** Its `_records` is a `zoe.plone.registry.MovedRecords`. This class is a subclass of `_Records`. Its `_fields` and `_values` are views of the rows of its namespace. They give the methods of an `OOBTree` that `_Records` and `Registry` call: `__getitem__`, `get`, `__setitem__`, `__delitem__`, `__contains__`, `__len__`, `__iter__`, `keys(min, max)`, `minKey`, `maxKey` and `clear`. Its pickled state is the registry (`__parent__`) and the id: about a hundred bytes in the pickle of the registry. The class name is in the pickles of the store, so it stays importable.
- **The trees** stay in the store as they were. The registry no longer reaches them, so a garbage-collecting pack removes them. Until then, an undo of the move gives them back (section 8).

### 3. A read

- Each method of the views first runs the check of section 5. Then it reads the writes of the current transaction (`zoe.kv`), then the rows of a cache of the process.
- **The cache.** For each storage of a database and each namespace, the process keeps the rows as bytes, with a label: the serial of the registry object when it read them. A view whose registry has a serial after the label scans the namespace again (two scans of 1,000 pairs for a registry of Plone) and sets the label to that serial. Else it takes the cached rows.
- Each write of a moved registry also writes the registry object (section 6). So the serial of the registry changes with each write, and ZODB invalidates the registry in each connection. A request then makes no storage call for the registry while nobody writes it, as with the pickle cache of the stock trees. The cache needs no read of the generation of the namespace.
- A read gives the last committed rows (ADR 0043), never rows older than the snapshot of the registry. A transaction takes the cached rows once, at its first read, and keeps them to its end, with its own writes over them. It decodes a row once and keeps the object to its end. So an in-place change of a value that a read gave stays in the transaction, as with the stock trees it stays in the connection.

### 4. `move`, `move_back` and `copy_rows`

Each one changes the current transaction of the connection of its registry. The caller commits.

- **`move(registry)`.** `registry` is a registry of plone.registry in a database. A registry that is moved already stays as it is. A registry with no connection, or in a storage without namespaces, raises `Unsupported` and stays as it is. The move draws the id, writes a row for each field and each value, and the marker with the oid of the registry, and puts a `MovedRecords` in `_records`. It reads the trees with `readCurrent` on the `OOBTree`s and their buckets, so its commit fails with a `ConflictError` when a write changed them after its snapshot. On a site of Plone the move writes about 1.7 MB of rows in one transaction.
- **`move_back(registry)`.** `registry` is a moved registry that passes the check of section 5, else `Unsupported`. It puts a `_Records` with new `OOBTree`s in `_records`, with each field and each value of the rows: the fields become new persistent objects. It deletes each row of the namespace, the marker too. The generation row stays (ADR 0043 deletes no generation row), so the namespace keeps 1 row. Nothing is lost: each record of the rows goes back to the trees.
- **`copy_rows(source, copy)`.** `source` is a moved registry that passes the check. `copy` is a moved registry with the id of `source` that fails it: a copy of `source` in another store, or in the same store. Else it raises `ValueError`. It draws a new id for `copy`, writes the rows of `source` in the namespace of the new id in the store of `copy`, with the oid of `copy` in the marker, and puts a `MovedRecords` of the new id in `_records` of `copy`. The caller commits the transaction of `copy`. With a new id, a copy in the same store never shares the rows of its source. It reads the rows of `source` with the writes of the current transaction of `source`.

The helpers of the acceptance test keep their calls: `install()` and `uninstall()`, `move`, `move_back`, and `copy_rows` for `bring_rows_back`.

### 5. The check: a registry never reads as empty

At its first read or write in a transaction, a view of a moved registry checks its rows. It raises `ZODB.POSException.Unsupported` when:

- the registry has no connection;
- the storage of its database has no namespaces: a FileStorage, a stock ZEO client, RelStorage, a `zoe.zeo.ClientStorage` whose server did not accept `kv`, a `DemoStorage` over one of them (`zoe.kv` raises it);
- its namespace has no marker, or a marker of another version of the layout;
- the oid of the marker is not the oid of the registry.

The message names the registry and says what to do: `copy_rows` from its source, or `move_back` in the source before a copy.

One case is not a copy: a move back committed after the snapshot of the connection, which deleted the rows. When the check fails and the store holds a newer serial of the registry than its `_p_serial`, the view raises `ReadConflictError` in place of `Unsupported`. Zope retries the request, and the retry reads the stock registry.

So each copy raises at its first read and at its first write:

| Copy | What the copy holds | The check |
| --- | --- | --- |
| `copyTransactionsFrom`, zodbconvert, the copy of the ZODB iterator, a restore of each record | the pickle of the registry, no row | no marker: `Unsupported` |
| `zoe convert` to a Data.fs, and back | the same | no namespaces, then no marker: `Unsupported` |
| a zexp import into another database | the same, at a new oid | no marker: `Unsupported` |
| a zexp export and import in one store, an OFS copy and paste | the pickle with the same id, at a new oid | the marker holds the oid of the source: `Unsupported`. The source keeps its values |
| `zoe backup` and `zoe restore` | the pickle and the rows, the same oids | passes |

A write always reads the field of its record first, through the check. So no write reaches the namespace of another registry, and a write to a copy raises before its commit.

One gap stays: a copy through the iterator into a store that already holds the rows of that id with the same oid. That is a store restored from a backup of the source, then given the later transactions of the source through the iterator (`zodbconvert --incremental`). The copy reads the rows of the restore, which can be older. ADR 0043 already says it: move the registry back before a copy through the iterator.

### 6. A write, plone.protect and conflicts

- A write of a view (`__setitem__`, `__delitem__`, `clear`) stages the row through `zoe.kv` and marks the registry object as changed (`_p_changed = True`). After the move, the pickle of the registry is small.
- **plone.protect** sees the registry among the objects of the connection. So a POST that writes a moved registry needs the token, as a POST that writes the stock trees does, and a GET that writes it without a token aborts, as with the stock trees: neither the registry nor an `OOBBucket` is a safe type of plone.protect.
- **Conflicts.** Two transactions that write a moved registry both write the registry object. The second one gets a `ConflictError`, and Zope retries it. So the rule of ADR 0043 that the last writer wins never applies to two writes of a registry: a retry reads the newer rows. With the stock trees, two writes of different records in one bucket merge. With the backend, they conflict and retry. Writes of the registry are rare: control panels, upgrade steps, the install of an add-on.
- **The window of a move.** A write that began before a move writes into the trees, which the move leaves. With the guard of section 1, that write called `readCurrent` on the registry, so its commit fails with a `ConflictError`, and its retry writes the rows. A write that began before a move back writes the registry object, which the move back writes too, so it conflicts the same way. An instance without the option has no guard: a write of it that runs during the move can go to the trees and be lost. So the option goes on every instance before the move.
- A savepoint covers the writes: `zoe.kv` supports savepoints, and ZODB covers the mark of the registry.

### 7. Two sites in one database

Each registry has its own id, so its own namespace, its own marker and its own cache entry. A write of one registry writes no row of another. `move_back` of one deletes only its own rows. `uninstall()` moves nothing and deletes nothing. An OFS copy of a site in the same store holds the id of its source, and the check refuses it (section 5) until `copy_rows` gives it its own id.

### 8. Pack, undo, backup and the other tools

- **Pack** never reads the rows (ADR 0043). After a move, the trees are garbage, and a garbage-collecting pack removes them. After a move back, the namespace keeps its generation row only.
- **Undo** gives back no key (ADR 0043).
  - The undo of a write of a moved registry gives back the registry object, whose state is the same. The rows stay, so the undo changes no record. A site writes the old value again. With the stock trees, the undo gives back the bucket. ZODB undoes a transaction only when no later transaction wrote its objects, and each write writes the registry object, so only the last write of the registry can be undone this way.
  - The undo of a move gives back the stock trees, with the values before the move, when no write of the registry followed the move and no pack removed the trees. Else ZODB refuses it. Its rows stay in their namespace, which nothing reads.
  - The undo of a move back gives back a moved registry whose rows are gone: it raises `Unsupported`, never an empty registry. The undo of that undo gives back the stock registry.
- **`zoe backup` and `zoe restore`** keep the rows and the oids (ADR 0030, ADR 0043). A restored site reads its registry.
- **`zoe convert`** to a Data.fs leaves the rows out and prints a warning (ADR 0043). The registry of the Data.fs raises `Unsupported`. Move it back before the conversion.
- **`zoe verify`** checks the rows as it checks every durable row.

### 9. A site without the backend, or on ZEO

- A site whose registry never moves keeps the stock behaviour, with or without the option. The guard of section 1 runs only in a store with namespaces.
- On a storage without namespaces, `move` raises `Unsupported` and changes nothing. So the option on a ZEO site, a FileStorage or RelStorage refuses only the move.
- A moved registry needs a storage with namespaces in each instance: `zoe.Storage`, or `zoe.zeo.ClientStorage` with a server that accepts `kv`. A stock `ZEO.ClientStorage` on `zoe serve` reads it as `Unsupported`.
- **The way back.** `move_back(site.portal_registry)` and a commit, for example in a script of `bin/instance run`. Then remove the option. Nothing is lost.

### 10. The test layers

- **Integration layers.** The acceptance test moves the registry of the site in the transaction of the test, which the layer aborts. The rows stay in the data manager of `zoe.kv`, and the check finds the staged marker. The abort drops the rows and the new `_records`.
- **Functional layers.** The commits go to the `DemoStorage` of the test and to its `zoe.kv` overlay. Both go with the `DemoStorage`. Each `DemoStorage` is a storage of its own for the cache.
- **`test_exportimport`** runs on a `Registry()` in no database. It never moves, and the guard skips a registry without a connection or an oid. So the backend leaves it as it is.
- The tests of the backend install it themselves. `just plone zoe backends` keeps `ZOE_PLONE_BACKENDS=locking`.

## Consequences

- A moved registry writes about a hundred bytes of the registry object and the rows that change, in place of the buckets of the trees: the 208 KB for each write of the first design are gone. The objects that the registry reaches hold less than a tenth of their bytes.
- A request reads the registry from the cache of the process, with no storage call while nobody writes it. A transaction decodes each record that it reads once.
- Two writes of a moved registry always conflict, and one retries. With the stock trees, they conflict only in one bucket.
- A copy of a moved registry raises `Unsupported` until `copy_rows` or a move back in the source. An OFS copy of a site is such a copy.
- An in-place change of a field of a moved registry is lost: a field is a copy of the row. plone.registry and plone.app.registry replace a field; they do not change it in place.
- The undo of a write of a moved registry changes no record.
- Each instance of a site with a moved registry uses a storage with namespaces, and has the option during a move.
- Step 3 of the card updates F31 and the paragraph on `zoe.plone` of the specification, and the status of ADR 0043.

## Alternatives

- **One namespace for every registry** (the first design). Rejected by the review of ZOE-207: two sites overwrote each other.
- **The oid of the registry as the name of its namespace**, as queued indexing names its queue. An in-store copy gets a new oid and finds no rows, so it is detected too. But an oid is an address in one store: two sites created the same way have the same oids in two stores, so a copy from one store into a restore of the other reads the rows of another registry. A random id is unique in every store.
- **One namespace with the id as a key prefix.** It works, but the registries then share one generation row and one scan range.
- **JSON values.** A field is an object of a class of plone.registry; JSON holds no class.
- **Only the values move.** The fields are most of the bytes, and each is a record of its own.
- **A cache on the generation of the namespace** (ADR 0043, answer 6). It costs one storage call for each transaction; the serial of the registry costs none, because ZODB invalidates it.
- **A version check in the marker**, a counter that each write changes in the marker and in the registry. It would find the gap of section 5, but reads do not follow the snapshot, so a reader whose snapshot is older than the last write would fail too.
- **A small persistent stamp object, marked at each write, in place of the registry object.** It saves the write of the registry pickle, which is small after the move. It adds an object to each registry.
- **An automatic move**, at the install or at the first write. The card makes the move an explicit step.
- **No guard; move only when no request writes the registry.** Simpler, but a write that races the move is lost.

## Questions for the human

Each question gives Claude's recommendation first.

1. **A new ADR or an edit of ADR 0043.** Recommended: this new ADR 0050, and a pointer in the status of ADR 0043 when it is accepted. Other choice: an amendment section in ADR 0043.
2. **What `install()` does.** Recommended: it lets `move` and `copy_rows` run, and adds the guard of a stock registry (section 1); a moved registry reads and writes in every process. Other choices: no option at all, the move is the only opt-in (the helpers `backend_on` then call nothing); or a moved registry raises in a process without the option.
3. **The key layout.** Recommended: one namespace `zoe.plone.registry:<id>` for each registry, with the keys `f<name>`, `v<name>` and `m` (section 2). Other choice: one namespace with the id as a key prefix.
4. **The pickles.** Recommended: protocol 3, by value, unpickled with `ZODB.broken.find_global` (section 2). Other choice: refuse a value that holds a persistent object.
5. **The cache.** Recommended: the rows of each namespace as bytes in the process, labelled with the serial of the registry; no generation read (section 3). Other choice: a read of the generation at the start of each transaction, as ADR 0043 answer 6 foresaw.
6. **Each write marks the registry object.** Recommended: yes, for plone.protect and so that two writes of a registry conflict and retry (section 6). Other choices: a stamp object in the registry; or last writer wins, and a mark only for plone.protect.
7. **The errors of the check.** Recommended: `Unsupported` at the first read or write of a copy, and `ReadConflictError` when a move back committed after the snapshot (section 5). Other choice: `Unsupported` in both cases.
8. **`copy_rows`.** Recommended: keep the name; it draws a new id for the copy, and refuses a copy that does not hold the id of its source (section 4). Other choices: keep the id of the source for a copy in another store; or no `copy_rows`, only "move back before the copy", and the last part of the acceptance test changes.
9. **`move_back` deletes the rows.** Recommended: yes, so stale rows never read, and an undo of the move back raises `Unsupported` (section 8). Other choice: keep the rows; an undo of the move back then reads the rows as they were at the move back.
10. **Undo of a registry write.** Recommended: state that it changes no record (section 8). Other choice: refuse such an undo; ZODB gives the storage no hook for it, so it needs a change of `zoe.Storage`.
11. **An OFS copy of a site.** Recommended: it raises `Unsupported` until an operator runs `copy_rows`, as the card asks for an in-store copy. Other choice: a subscriber of `IObjectCopiedEvent` calls `copy_rows` for each copied registry; it is a feature that the card does not name.
12. **The window of a move.** Recommended: the guard of section 1 (`readCurrent` of a stock registry in a store with namespaces, while installed) and the `readCurrent` of the trees in `move`. It costs one serial check at the commit of each write transaction in such a store. Other choice: no guard; the move runs when no request writes the registry.
13. **The helpers of the acceptance test.** Recommended: keep them as the planner wrote them; the names of this ADR fit them.
