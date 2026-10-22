# 0043. KV namespaces: an ephemeral namespace with a TTL, a durable namespace, and the backends of zoe.plone on them

Status: accepted. Part A changes the on-disk format (a red zone), the wire protocol and the public Python API. Part B adds an opt-in backend to `zoe.plone` and a public Python API (`zoe.plone.registry`), and changes no format, no protocol and no code of the core.

## Context

### The namespaces

F31 asks for KV namespaces: an ephemeral flavour with a TTL, no history and no pack, for sessions, CSRF keys and locks, and a durable flavour for `plone.app.registry`. Outside the object graph, their writes conflict with no content write, and an expired key goes without a pack. They need new key spaces, so they are a format change.

What Plone 6.1.5 does:

- **Sessions.** Plone 6.1 keeps nothing in a Zope session. Zope 5 has no `/temp_folder`, so `session_data_manager` has no container. The login (`plone.session`) is a signed cookie.
- **Locks.** When an edit form begins, plone.locking puts a WebDAV `LockItem` of 10 minutes on the content object. In B5 `edit`, these lock and unlock writes are half of all transactions (`docs/findings/plone-end-to-end.md`, item 27).
- **CSRF keys.** The `KeyManager` of plone.keyring, a persistent utility of the site and `_key_manager` of the Zope root, rotates its keys only at a login. No GET writes them.

A sequence (ADR 0024) is outside the two-phase commit. A namespace write must commit or abort with its transaction.

### The registry

`portal_registry` keeps its records in two `OOBTree`s, `_fields` and `_values`, in `_records`, which is not persistent. Each field is a persistent object, so a record of its own. An upgrade step of plone.app.upgrade changes a field in place. The field of an add-on that is gone loads as a broken object, whose pickle raises. A registry holds 1.7 MB. No GET of B5 writes it.

One namespace for every registry, as views of the two trees, fails in three ways:

1. Two sites of one Zope root overwrite each other, and the uninstall of one empties the other.
2. A copy into another store (`copyTransactionsFrom`, zodbconvert, a zexp import, `zoe convert` and back) reads as empty and takes writes.
3. Before its first write, a registry keeps all its records in its own pickle: 346 KB, and 208 KB more for each write on FileStorage.

## Decision

### A. The namespaces

#### A1. Two flavours, three key spaces

Both flavours keep one row per key and no history, and a crash loses no acknowledged write. Pack never reads them. An ephemeral key has a TTL; a durable key stays until a delete. Dumps carry only the durable flavour (section A7).

The key spaces `kv_ephemeral` (tag 7), `kv_expiry` and `kv_durable` (tag 8): specification, "Storage core", "Data model". Their invariants: `crates/zoe-core/CLAUDE.md`. The reasons:

- The keys are bytes: redb decodes a `&str` key with `unwrap`, so a flipped bit could make a lookup panic.
- UTF-8 byte order is the code point order of a Python `str`, so a scan of str keys gives the order of an `OOBTree`.
- `CorruptKv` never names the key: a key can be a session id.
- An older build ignores the namespaces, so the format name stays `ZOE format 2`.

#### A2. Writes join the two-phase commit

The staging, the finish, the namespace-only commit and the reads: specification, "Storage core", "KV namespaces (F31, ADR 0043)".

- **The last writer wins**, as Products.Transience resolves sessions. A lock or a registry value is set, not merged.
- A namespace-only commit holds no oid lock and shares the sync of its batch.
- A delete of an absent key does nothing.
- A TTL is at most 2^32 − 1 seconds, the maximum timeout of a WebDAV lock. Each write sets it again. No call refreshes it alone.
- Reads run on any thread and on a read-only store. A client cache of a durable namespace stays valid while its generation stays the same. The generation is the zero tid until a commit writes the namespace.

#### A3. Expiry without a pack

The sweep step and when it runs: specification, "Storage core", the row `kv_expiry` and "KV namespaces (F31, ADR 0043)". A write and a delete leave the old index key, so a write reads no row in the commit path. The sweep has no option. A read-only or halted store runs no step. `close` of `zoe.Storage` waits for its sweep thread without the GIL. redb has one writer, so a commit waits for at most one step. A crash can lose a step: it then gives back only expired rows, which reads hide and the next step removes.

#### A4. The protocol extension `kv`

The client declares `kv` in the third argument of `register`, and `get_info` gives it back under `extensions` (F13), as for `sequences`. The five calls: specification, "Server mode", "KV namespaces over the protocol (F31, ADR 0043)". A scan limit below 1 is a `ValueError`.

#### A5. The Python API

The methods of `zoe.Storage` and `zoe.zeo.ClientStorage`, and `zoe.kv.Namespace`: specification, "In-process mode", "Python surface", the row "KV namespaces (F31)". ZODB gives no hook for writes beside the objects of a connection, so `zoe.kv` gives one. `zoe.kv._supported` is private.

#### A6. `zoe verify` and the bit-flip harness

`zoe verify` reads every namespace row and checks the range and the order of the keys of each scan. Its report: specification, "Loupe: desktop inspector and editor", "`zoe verify`". Its findings: in `checksums`, a frame that fails its checksum, a name that runs past the key, is empty or is not UTF-8, and an ephemeral payload under 8 bytes; then an ephemeral row without its index key, a durable namespace without a generation row, and a generation after the last tid. A finding names the namespace and the xxh3-64 hash of the key (of the row, when the row gives no key), never the key. An index key without its row, or with another expiry, is no finding: writes leave such keys.

The gate is `every_flipped_bit_of_a_namespace_frame_raises_corrupt_kv` of `crates/zoe-core/tests/kv_namespaces.rs` (specification, "Performance and testing harness", "Bit-flip injection"). `kv_expiry` has no frame, so it is outside the gate. A step removes only a row that passes its checksum, so a flip there never removes a live key.

#### A7. The other tools

- **Pack** removes the transaction rows of namespace-only commits before T: specification, "Storage core", "Pack".
- **The iterator**, zodbconvert and `copyTransactionsFrom` give these commits without records, and leave the rows out.
- **`zoe convert`** leaves both flavours out. To a Data.fs, its report counts the durable rows, and it prints a warning. It refuses a moved registry (section B9).
- **`zoe backup` and `zoe restore`** (ADR 0030) keep every durable row: specification, "Loupe: desktop inspector and editor", "`zoe backup` and `zoe restore`: dumps (N15)". Every durable write has a tid, so a backup with nothing new misses no change.
- **The change feed** (F23, later) gives no key and no value; it can name the durable namespaces of a commit. **Replication** (later) carries the writes with their commit, and a follower runs no step until its promotion. **Undo** (F11) gives back no key.

#### A8. The backends of `zoe.plone`, and the writes-on-GET report

- **The locking backend.** `zoe.plone:locking.zcml`, or `ZOE_PLONE_BACKENDS=locking`, installs `zoe.plone.locking` (its mechanics: `crates/zoe-py/CLAUDE.md`). It acts only on an object that provides `ITTWLockable` and whose storage has namespaces; every other object keeps the stock behaviour. Each savepoint and a hook before the commit stage the row again, for a `LockItem` that changed in place (`refresh`). One row of the ephemeral namespace `zoe.plone.locks`, under the oid of an object, holds all its locks, with the TTL of its longest valid lock. So the later commit wins for all the locks of one object. A row with no lock is deleted. A lock that is no longer valid reads as absent, and `killinvalids` deletes it, as in OFS. The backend carries no lock over its install or its uninstall: a lock lives for minutes.
- **The install.** The backends install when the ZCML loads (`zcml:customAction` of z3c.zcmlhook), as plone.protect patches Zope.
- **The test layers.** plone.app.testing turns plone.autoinclude off, so the layer of `zoe.plone.testing` installs the backends that `ZOE_PLONE_BACKENDS` names.
- **The session data manager.** Not built: Plone 6.1 has no session container (see Context). It waits for a site that needs sessions.
- **The CSRF keys** stay in plone.keyring: they change only at a login.
- **The writes-on-GET report** (`zoe_writes_on_get_total`): specification, "Server mode", "Health, metrics, shutdown". It counts records, not namespace writes.

### B. The registry backend

The rows, check, cache and steps of `zoe.plone.registry`: specification, "Server mode", "`zoe.plone` and the registry (F31, ADR 0043)" ("the registry paragraph" below).

#### B1. The option and the guard

- A second `install()` or `uninstall()` does nothing. Neither moves a registry: the move is an explicit step (section B5).
- The data decides, not the process (the registry paragraph). So an instance that misses the option still serves the site.
- **The guard** runs wherever `zoe.plone` loads. It skips a registry with no connection or no oid. A root that keeps its keys in its own state is written by each write of a record, and a resolution could merge such a write into the trees that the move left. So for such a root, the guard also calls `readCurrent` on the registry object. A change of a field in place reads no node, so only the guard makes it conflict.
- **The argument of ADR 0048 holds.** ADR 0048 assumes that a program that marks a node read-current writes no key under it. Else rule 1 loses the check of the other children of that node.
  - The guard marks only the two roots, adds no path child and writes no key. So for a transaction that writes no key under a root, rule 1 gives no answer, and the vote still checks. The registry object is not a tree.
  - The move marks every node and bucket of the trees, so every child is a path child, and rule 1 gives no answer for a split or a replacement of the other side. Rules 2 and 3 take only a node in which the transaction split a child, so they give no answer for a root of the move.
  - A root that the move empties changed by more than splits and replacements, so no rule lets an earlier write through. An unchanged root passes rule 1, so the move empties the roots and does not just write them again.
- **The early return:** the registry paragraph, "The guard". The mark of the guard counts, not only the objects read current: BTrees reads a root current at each set and delete, also one that changes nothing.
- **The cost**, on every ZOE site with `zoe.plone`: a transaction that reads the registry and writes an object checks the serials of the two roots at its vote, and retries after a move or a split of a root.

#### B2. What moves, and the rows

- The id comes from `secrets`. Two registries never share a namespace, in one store or in two.
- A record name starts with a letter (`_validkey`) and holds at most 1,023 bytes. A longer name raises `ValueError` at its write.
- `MovedRecords` is a subclass of `_Records`. Its pickled state (the unwrapped registry, the id, the counter) holds about a hundred bytes. `keys(min, max)` of its views includes `max`, as in an `OOBTree`. Its class name is in the pickles of the store, so it stays importable.

#### B3. The pickles

A record holds a field of plone.registry and a value that can be a tuple, a set, a frozenset, a `Decimal`, a `datetime` or bytes. JSON (ADR 0047) holds none of them without a codec of our own. ZODB keeps the same objects as pickles in the same store, so the rows add no new trust. A persistent object that is not a field raises `TypeError` at the write: it is a bug of its writer. A read unpickles with `ZODB.broken.find_global`, so a broken field reads as a broken object while its class is gone, and as the field when the class comes back.

#### B4. A read, and the cache

- Each method of the views first runs the check (section B6), a write too.
- The cache keeps the rows of a namespace as bytes, with a label: the counter of their marker. A view whose registry has a counter at or below the label takes the cached rows; else it scans. A transaction takes the rows once, at its first read, and keeps them to its end, with its own writes over them.
- The cache is per storage (through a weak reference) and per namespace. A scan that sees the generation change scans again. After 3 tries, it raises `ConflictError`, and Zope retries the request. A scan fills the cache only when its label is above the cached label. The namespace read least recently goes first.
- Each write changes the counter of the registry object, so ZODB invalidates it in each connection. While nobody writes it, a request makes no storage call for it.
- A connection at a past time (`db.open(at=...)`) reads the current rows too. A read after the end of a transaction, as in an after-commit hook, joins nothing. An in-place change of a value is not written, as with the stock trees.

#### B5. `move`, `move_back`, `copy_rows` and `reset`

Without `install()`, `move`, `copy_rows` and `reset` raise `RuntimeError`.

- **`move`** leaves a moved registry as it is. It calls `readCurrent` on each object of the trees that it reads and on each persistent field that it pickles. So a write after its snapshot, a change of a field in place too, makes it fail with `ConflictError`. It empties each root with children (`clear`, which writes no bucket).
- **`move_back`** of a registry that fails the check raises `Unsupported`. The generation row stays.
- **`copy_rows`** raises `ValueError` unless `source` passes the check and `copy`, with the id of `source`, fails it. It reads `source` with an atomic scan and calls `readCurrent` on it. It deletes the old rows of `copy` only under a marker with the oid of `copy`.
- **`reset`** of a registry that passes the check raises `ValueError`. The records of Plone itself come back from `profile-Products.CMFPlone:dependencies`. To find a ZEO client, it also looks under `ZODB.blob.BlobStorage`. Another wrapper can hide the client, so reset only a storage opened directly. No argument forces a reset.
- **Two stores in one process.** A script opens both stores, the source through its `zoe serve` when an instance holds it, calls `copy_rows` and commits the copy.

#### B6. The check: a registry never reads as empty

A view checks before it reads or writes a row. It raises `Unsupported` when the registry has no connection, its storage has no namespaces, its namespace has no marker or a marker of another layout version, the marker holds another oid, or the counter of the marker is below the counter of the registry. So a copy without its rows raises at its first read and its first write, and no write reaches the namespace of another registry. A counter of the marker above the counter of the registry passes: a later commit wrote the rows. The message names the registry and the way out: `copy_rows`, `move_back` in the source before a copy, or `reset`. Only `zoe backup` and `zoe restore` give a copy that passes.

**The overlay of a `DemoStorage`** gets the rows after the records. In that moment, another thread can load the new registry and fail the check. Tests run each request in the test thread, so they do not meet it.

#### B7. A write, plone.protect and conflicts

- The first write of a transaction reads the stored marker with `kv_get`, not from the cache.
- A field changed in place is pickled before the commit and before each savepoint (`flush_before_savepoint`). A row can differ from a new pickle of its object (another writer, another protocol, a shared str). The data manager then pickles a new decode of the kept bytes, and equal pickles mean no change. So a read never writes a row.
- **plone.protect** sees the registry object. So a POST that writes a moved registry needs the token, as with the stock trees.
- **Conflicts.** Two writes of a moved registry both write the registry object, so one retries on the newer rows: the last-writer rule never applies to them. With the stock trees, two writes in one bucket merge. Writes of the registry are rare. Before a move, read the writes-on-GET report and the conflict log.
- **The window of a move.** A write that began before a move fails on the guard, and its retry writes the rows. A write that commits during a move makes the move fail. A write that began before a move back conflicts on the counter of the registry object.

#### B8. Two sites in one database

Each registry has its own id, namespace, marker and cache entry. So a write or a `move_back` of one registry changes no row of another. `uninstall()` moves nothing and deletes nothing.

- **An OFS paste of a site** raises `Unsupported` when its handlers read the registry of the copy, and copies nothing. Move the registry back, copy the site, then move both registries.
- **A deleted site** keeps its rows: pack never reads them. Move the registry back before the delete.

#### B9. Pack, undo, backup and the other tools

- **Pack** never reads the rows (section A1).
- **Undo** gives back no key (section A7):
  1. **A write of a moved registry.** The undo gives back the old counter, and the rows keep the higher one, so no record changes. The next write takes its counter from the stored marker.
  2. **A stock write before the move.** It changes a bucket that the registry no longer reaches.
  3. **The move.** It gives back the stock trees when no write followed and no pack removed them. The rows stay, unread.
  4. **The undo of an undo of a move.** It gives back the rows of the move, and loses a stock write between the two undos. Move again instead.
  5. **A move back.** It gives back a registry without rows: **each request raises `Unsupported`** until the undo of that undo. Never undo a move back.
- **`zoe backup` and `zoe restore`** keep the rows and the oids (ADR 0030, and section A7 of this ADR).
- **`zoe convert`** to a Data.fs refuses a moved registry, unless `--force`: specification, "Loupe: desktop inspector and editor", "Converters, `zoe convert` and a Loupe screen". zodbconvert gets no such check.
- **`zoe verify`** checks the rows as every durable row (section A6).

#### B10. A site without the backend, on ZEO, or leaving ZOE

- A registry that never moves keeps the stock behaviour, except for the guard (section B1).
- On a storage without namespaces, the option refuses only the move. A stock `ZEO.ClientStorage` on `zoe serve` reads a moved registry as `Unsupported`.
- **The way back**, and what needs it first: the registry paragraph, "The way back". A downgrade of `zoedb` without it unpickles `_records` as a broken object.
- **Leaving ZOE** without it gives a copy that raises on each request. While the source exists, `copy_rows` repairs a ZOE copy. A copy outside ZOE needs a move back in the source and a new copy. With no source left, use `reset` (section B5).

#### B11. The test layers

- **Integration layers.** The acceptance test moves the registry in the transaction of the test, which the layer aborts. The check finds the staged marker.
- **Functional layers.** The commits go to the `DemoStorage` of the test and its overlay. Each `DemoStorage` is a storage of its own for the cache.
- **`test_exportimport`** uses a `Registry()` in no database: the guard skips it.
- plone.app.testing loads no ZCML of `zoe.plone`, so `zoe.plone.testing.install()` installs the guard on each ZOE layer, also in `just plone zoe suites`. The tests of the registry backend install the backend themselves.

## Consequences

- With the locking backend, the GET of an edit form writes no object and conflicts with no content write. plone.protect does not see a namespace write. A backend whose writes need its check also writes an object, as the registry does (section B7).
- A namespace-only commit costs a tid, a transaction row and a share of a sync.
- Two editors can both take the lock of one edit form. The later write wins.
- `Backend` and `WriteTxn` have required methods for the namespaces.
- A write of a moved registry writes about a hundred bytes and the rows that change, not 208 KB of buckets. Two writes of it always conflict.

## Alternatives

- **ZODB objects with merges:** two writes of one key still conflict, and a lock still writes the content object.
- **A separate store file, an in-memory cache, or Redis:** no atomic commit with the records.
- **The `sequences` key space:** `&str` keys, outside the two-phase commit.
- **History for the durable flavour:** reads become range scans, and pack must read the namespace.
- **A version check for each key:** two writers of one key conflict.
- **Ephemeral-only commits without a sync or a tid:** a crash loses acknowledged writes, and a ZEO client that saw a lost tid refuses the server.
- **A sweep cursor without `kv_expiry`:** each step reads live rows.
- **One namespace for every registry:** see Context.
- **The oid as the namespace name:** two stores built alike share oids.
- **The id as a key prefix in one namespace:** one shared generation row and scan range.
- **JSON values, or any persistent object by value:** section B3.
- **Move only the values:** the fields are most of the bytes.
- **A cache on the generation:** one storage call for each transaction.
- **No write counter:** a copy into a restored store reads older rows.
- **A stamp object in place of the registry object:** one more object, to save a small write.
- **`Unsupported` for a move back after the snapshot:** the retry succeeds.
- **No option, or a moved registry that raises without it:** the data decides.
- **An automatic move:** the move is an explicit step.
- **The id of the source for a copy in another store:** a new id never shares rows.
- **No `copy_rows`, or no `reset`:** a new move back and copy, or trees written by hand.
- **Keep the rows at the move back:** stale rows could read.
- **Refuse the undos of section B9:** ZODB gives the storage no hook.
- **`copy_rows` from a subscriber of `IObjectCopiedEvent`:** the specification does not name it.
- **The guard on the registry object:** each reader conflicts with the subscribers of the resources and the theme.
- **The guard only with the option, or no guard:** a write that races the move is lost.
- **Accept the loss of a change of a field in place:** an upgrade step makes one.
- **Write a new pickle of a row at its first read:** a read then writes.
- **A warning of `zoe convert` as the only signal:** the Data.fs holds a registry that raises.

## Merged ADRs

- ADR 0050, the registry backend: Decision, part B. Its section N is section BN here, for N from 1 to 11. Its context is in Context, "The registry", and its alternatives are in Alternatives.
