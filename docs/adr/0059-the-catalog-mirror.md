# 0059. The catalog mirror: a base built from the index trees, a change set per commit, and `catalog_query`

Status: accepted. Card ZOE-283, a spike (finding: `docs/spikes/catalog-mirror.md`; prototype: branch `spike/ZOE-283`). It adds a wire call (`catalog_query`, extension `catalog_query`), a public Python API (`catalog_query` and `catalog_supported` of `zoe.Storage` and of `zoe.zeo.ClientStorage`, the backend `catalog` of `zoe.plone`), a durable namespace and its keys, and five keys of `zoe serve` and of `<zoe>`. It changes no on-disk format and no record. It records the decisions Q2 to Q7 of the plan of phase 6 (#597) and gives the design around them.

## Context

F17 asks for ZCatalog queries answered in Rust as of the snapshot tid of the client, with identical rids, order and counts, and the Python path for what the mirror does not cover. The gate of phase 6 asks for a five-index intersection with a sort on 1,000,000 entries under 5 ms at p99 warm (G2) and a mirror ready within 60 s of a cold start (G3). The specification ("Step 2: the mirror") gives a representation, a discovery by class, a maintenance "at `tpc_finish`, before invalidations go out", versions "with structurally shared maps and copy-on-write bitmaps" kept for a window, and "the low hundreds of megabytes" for a catalog of one million objects.

The spike built a prototype in `zoe-catalog` and measured it on `catalog-1m` (56 indexes, 53 columns, 1,000,000 entries). Its numbers are in the finding. The facts that decide this record:

- A cold build from the committed trees reads one tree per index (`_unindex`), `paths`, `uids` and `data`. It needs no other tree of an index. It took 53.3 s.
- The decode of a commit is cheap per record, but not free. The invalidation hub of `zoe serve` must never wait.
- A copy-on-write version per tid copies at least one container of each changed bitmap. For a window of 10,000 tids, the finding estimates several gigabytes; the prototype did not build it. A change set per tid holds the changed values only: 21.6 KB per commit.
- The order of a ZCatalog result depends on which of its four sort paths runs. The ties of each path follow a rule of their own (finding, "The order of ZCatalog").
- The structures take 1.52 GB at one million entries, and 1.09 GB for a stock catalog. The metadata table is the largest part.

The evidence has limits (finding, "Limits"). `generic-numbers` misses G2 with 29.0 ms at p99, because the prototype unions the postings of the 3,000 keys of a range one by one. Its fix was not measured. `navtree-depth999` takes 27 ms over 14,707 overridden rows, against 0.17 ms on the base, because the prototype parses the path of each such row at each step. The build peaked at 15.2 GB of resident memory and left 11.7 GB for 1.5 GB of structures. The build of section 2 was not measured. The cold build ran once, with 22 threads. `xlarge` and the B5 runs did not run. The commits are synthetic, and the costs of the wire are estimates: the prototype has no server.

### Terms

- **Registration key**: the key of one catalog in the durable namespace of section 1. It is not the `register` of a ZEO connection.
- **Row**: one entry of the catalog in the mirror, one rid of `paths`. The row number is the rank of the rid. A row is not a record.
- **Base**: the structures of a mirror as of one tid. A base never changes. It is not the base store of a fork.
- **Change set**: what one commit changed in the mirrored trees: the old and the new value of each changed row of each tree. It is not a delta record.
- **Overrides** of a version: the value at its tid of each row that a change set after the base changed.
- **Rebase**: the build of a new base from the base and the change sets. It is not the fold of the log.
- **Mirror window**: how far back the change sets let a query reach. It is not the window of queued indexing.
- **Transaction overlay**: the records that a transaction changed and did not commit, sent with a query. It is not the overlay of `zoe.kv`.
- **Test storage**: a `DemoStorage` over a ZOE storage, at any depth of a stack, as in the test layers of plone.app.testing.

## Decision

### Decisions of the human

1. The specification states the memory of a mirror: about 1.1 GB for a stock catalog of 1,000,000 entries, and 1.5 GB with the indexes of `xlarge`. N6 counts `catalog-memory` as a term of its bound, and the default stays 2 GiB (section 6).
2. The throwaway mirror of the mode `compare` (Q7) runs on a test storage only. A ZOE store uses the real mirror (section 9).
3. `catalog-1m` stands for `xlarge` in this card, as it does for G2 and G3. The probe of ZOE-292 records the inventory of the B5 runs.
4. The durable namespace keeps the name `zoe.catalog` (section 1). The store reads it, and only `zoe.plone` reads the namespaces `zoe.plone.*`.
5. The miss of `generic-numbers` gets no new spike. ZOE-286 builds the rule that the finding calls for (finding, "Queries"), sets its threshold by cost, and tests it, and `just bench catalog` (ZOE-299) proves G2.
6. ZOE-292 adds a replay of the synthetic B5 query log in the mode `compare` on a ZOE store, with commits during the replay. So G1 covers the change sets, the rebase, the mirror window and the wire.

### 1. Registration (Q3)

The backend `catalog` of `zoe.plone` registers the catalog of each Plone site. It finds the sites as queued indexing does (ADR 0047): at the top of the Zope root in the root database, and at or just below each mount point in a mounted database. For each site, it writes one registration key to the durable namespace `zoe.catalog` (ADR 0043) of the store that holds the catalog. The key is the 8 bytes of the oid of the `Products.ZCatalog.Catalog.Catalog` object, the `_catalog` of `portal_catalog`. The value is empty. Each plan gives the path index and its physical path (section 8), so a rename changes no key.

`install()` writes the keys. At each open of a database (`IDatabaseOpenedWithRoot`), an instance with the backend writes the missing keys of the catalogs that it finds and removes the other keys. A site created later writes its key in the transaction that creates it. A site deleted removes its key. A read-only storage writes no key. `uninstall()` gives the stock search back in the process. It removes, in a transaction of its own, the keys of the catalogs of the databases that the process has open. The way back of a site: run `uninstall()` in one instance, then remove the option from every instance (#597, Q3). On a test storage, the keys go to the overlay of `zoe.kv` (ADR 0047, section 9), so the ZOE store under it never sees them.

The store reads the namespace at its open, and after each commit that writes it. A new key starts a build. A removed key drops the mirror. When the oid of a key does not load as a `Catalog`, the catalog gets no mirror (`refused`), and the store logs it once. There is no scan of the store and no build on a first query.

### 2. The build

A mirror builds in the background after the open of the store, so `/readyz` and N7 do not wait for it. The build reads as of the last tid at its start: `paths`, `uids`, the record of each index with its `_unindex` tree and its `_length`, and `data`. It reads no `_index` tree: each index makes its sorted keys and postings from `_unindex`.

- When `uids` is not the inverse of `paths`, or the decoder refuses a record of `paths`, `uids` or `data`, the catalog gets no mirror (`refused`). When the decoder refuses a record of an index, only that index is unavailable (`index_unavailable`). A guessed value is never served.
- The build uses a pool of as many threads as the machine has hardware threads. A process builds one mirror at a time. The pool loads and decodes the nodes of a tree, and the structures of a tree build beside the walk of the next tree.
- The build decodes each value straight into the structure of its index. It keeps its transient values in arenas that it frees whole at its end. So its memory goes back to the system, and the bound of section 6 counts the mirror, not the allocator.
- The commits during a build stay in the store. When the base is ready, the mirror reads the commits after its tid through the transaction index and applies them in tid order. Then the mirror is ready.

Until the mirror is ready, each query takes the Python path (`not_ready`). `zoe_catalog_mirror_ready` and `zoe_catalog_mirror_build_seconds` report the build.

### 3. The structures of a base

The rows of a base are the rids of `paths` in ascending order as signed 32-bit ints, the order of ZCatalog's `IISet`. So a row number orders as the rid, and every bitmap holds row numbers. The rank of a key is its position among the distinct keys of its index, so a sort compares integers.

- FieldIndex, DateIndex, UUIDIndex, BooleanIndex: the distinct keys in ascending order (a vector of ints, an arena of strs, or the values), the rows of each key (one row inline, or a roaring bitmap), the rank of the key of each row, and the bitmap of the indexed rows.
- KeywordIndex: the same keys and postings, and the ranks of each row.
- DateRangeIndex: `since` and `until` of each row, and the rows sorted by each.
- ExtendedPathIndex: the components and the rows of each (component, level), the rows that end at each level, the rows of each parent path, the rows sorted by path, and `_depth`. These structures are not a trie.
- The metadata table: for each column, the code of each row (1, 2 or 4 bytes) and the encoded distinct cells.
- The path of each row, from `paths`.

### 4. Versions and the mirror window

Each commit adds one change set. For a row of `data`, the change set holds only the changed cells. A version at tid `t` is the base and its overrides. A query applies each clause to the base, then to the rows of the overrides, so the overrides cost in proportion to their rows. The copy-on-write version per tid of the specification is not built (alternative 1).

A change set leaves the mirror window when 10,000 newer change sets exist, or when it is older than 10 minutes: `catalog-window-tids` and `catalog-window-seconds`. The mirror drops a change set outside the window when a base holds it. A query at tid `t` is served when each commit after `t` has its change set in the mirror. So a query at the last tid is always served, on an idle store too. Other queries take the Python path (`outside_window`).

When the overrides of the last tid hold more than `catalog-rebase-rows` rows (default 20,000), the mirror rebases in the background. A rebase builds a new structure for each index and each column that the change sets touched, one at a time: 0.2 to 2.0 s for an index at one million entries. The new base shares the other structures with the old base. It replaces the old base when its last structure is ready. The old base stays until its last query ends. The change sets keep their old values, so a version before the new base is the new base with the old values of the change sets after `t`.

### 5. The maintenance point

The mirror applies each commit after its publication, in tid order, in a thread of the mirror. In `zoe serve`, the thread follows the invalidation hub. In the process, `tpc_finish` hands the commit to the thread after its callback. The thread starts at the open of a store whose namespace holds a registration key, never at import. The publication of a commit does not wait for the mirror. A query whose snapshot holds a commit that the mirror has not applied waits for it, at most `catalog-wait` (default 1 s). Past it, the query takes the Python path (`lag`). `zoe_catalog_mirror_lag_tids` reports the distance.

To apply a commit, the mirror looks up each record of the commit in its map of the oids of the mirrored trees. It decodes the records that it owns. It registers the new buckets that a changed inner node names. It compares the items of each changed bucket with the rows of the old key range of the bucket. A record of another object costs one lookup.

Some commits change what the mirror reads: a new root of a mirrored tree (a new `_unindex`, `paths`, `uids` or `data`, as `manage_catalogClear`, `clearFindAndRebuild` and `clearIndex` write), or a change of the `Catalog` object or of the record of an index (`addIndex`, `delIndex`, `addColumn`, `delColumn`). After such a commit, the mirror is stale: the queries at its tid and later take the Python path (`stale`). A stale mirror starts a rebuild in the background: the shadow rebuild of ZOE-303 (Q12), which builds the structures of the changed trees beside the base and swaps them in. Until ZOE-303 merges, the rebuild is a new build (section 2). The stale mirror drops its base before that build starts, so the bound of section 6 holds.

### 6. The memory bound (Q2)

Each store has a bound for its mirrors: `catalog-memory` in the configuration of `zoe serve` and in `<zoe>`, default 2 GiB. The bound counts:

- the bases, the change sets of the window, and the map of the oids of the trees;
- the transaction overlays of the queries that run;
- the transient memory of a build;
- the second copy of the structures that a rebase or a shadow rebuild makes. Both make one structure at a time, so this copy is at most the largest structure: 148 MB for `object_provides` and 102 MB for the column `Description`, at one million entries.

At the bytes of the finding, the default holds one catalog of 1,000,000 entries with the indexes and columns of `xlarge` (1.52 GB), a window of 10,000 change sets (about 216 MB) and the room of a rebase (about 150 MB): about 1.9 GB of 2.15 GB. It does not hold two stock catalogs of that size: 1.09 GB each, 2.19 GB together.

A build, a rebase or a growth that would pass the bound drops the mirror of the store that answered a query least recently. `zoe_catalog_mirror_drops_total` counts the drop, and the queries of that mirror take the Python path (`dropped`). A dropped mirror builds again at the next open of the store, or when an uninstall frees its room.

N6 bounds the resident memory of a server by its configuration. The mirrors add one term to that bound: `catalog-memory` for each store with a registered catalog, beside `cache-size` and the log. The test of N6 (a store of 100 GB under a limit of 2 GB) registers no catalog. A server with one mirror at the default needs a limit 2 GiB higher.

### 7. What the mirror serves, and the reasons of a fallback

Served: the clauses of FieldIndex, DateIndex, UUIDIndex and KeywordIndex (`query`, `range`, `operator`, `not`), BooleanIndex (`query`), DateRangeIndex (`query`) and ExtendedPathIndex (`query`, `level`, `operator`, `depth`, `navtree`, `navtree_start`); a sort on one or several FieldIndex, DateIndex or UUIDIndex, with `sort_order`, `sort_limit`, `b_start` and `b_size`; a query without sort. Each answer gives the rids in the order of ZCatalog, the length of its lazy result and `actual_result_count`.

A query takes the Python path, whole, with one of these reasons:

| Reason | When |
| --- | --- |
| `unsupported` | the storage does not serve `catalog_query`: a stock ZEO client, the asyncio mode of `zoe.zeo.ClientStorage`, a test storage, an older server |
| `not_registered` | the store has no registration key for the catalog |
| `refused` | the build refused the catalog (sections 1 and 2) |
| `index` | an index of the query is of another class: ZCTextIndex, DateRecurringIndex, any add-on index |
| `option` | an option that the plan does not carry, such as `usage` |
| `and_range` | a range with the operator `and` |
| `sort` | a sort on another index (GopipIndex until ZOE-302, ExtendedPathIndex), `merge=False`, a `sort_limit` of 0, or several sort indexes on the path of ZCatalog that iterates the index |
| `transaction` | the transaction changed records before ZOE-291 merged: the interim rule of ZOE-286 (section 8) |
| `too_large` | the plan and the transaction overlay pass the cap of a call (section 8) |
| `plan` | the server does not know a key or a kind of clause of the plan: a client newer than the server (the order clause of ZOE-302, the text clause of ADR 0060) |
| `date_range_order` | a row of the result has no entry in a DateRangeIndex of the query: ZCatalog's answer then depends on the order of its plan, which it learns from timings |
| `index_unavailable` | the decoder refused a record of an index of the query |
| `overlay` | the server could not read a record of the transaction overlay |
| `not_ready`, `dropped`, `stale` | the mirror is building, was dropped, or met a commit that it cannot follow (section 5) |
| `outside_window`, `lag` | a commit after the tid of the query has no change set in the mirror, or the mirror did not reach the tid in time |

The client decides the reasons that it sees in the query: `unsupported`, `index`, `option`, `and_range`, `sort`, `transaction` and `too_large`. The server decides the others. `zoe_catalog_fallbacks_total{reason, side}` counts each one, with `side="client"` or `side="server"`. The client counts in its own process, where `zoe.plone` counts (ZOE-286). The table is a contract: the differential suite (ZOE-292) counts each reason.

### 8. The plan, the reply and the transaction overlay on the wire

The extension `catalog_query` adds one call, `catalog_query(catalog, tid, plan, overlay)`. `catalog` is the 8 bytes of the oid of the `Catalog` object. `tid` is 8 bytes, and it is exclusive, as the tid of `loadBefore`: the answer is the version of the last commit before it. The hook gives the start of the snapshot of its Connection. The call runs on the read pool, outside the transaction of the connection, and takes a turn of its connection, as `kv_get` does. A wait for the mirror (`catalog-wait`) holds its thread of the read pool.

The client builds `plan` with ZCatalog's own `IndexQuery` and the conversion of each index (`_convert`, `_convertDateTime`), so the keys reach the server in the form of the index (Q4):

```text
plan    = {"c": [clause...], "s": [[index, reverse]...], "l": sort_limit | nil,
           "bs": b_start, "bz": b_size | nil, "cols": [column...] | nil, "rn": count}
clause  = {"k": "v", "i": index, "q": [value...], "and": bool,
           "r": [min | nil, max | nil] | nil, "not": [value...]}
        | {"k": "b", "i": index, "q": bool | nil}
        | {"k": "d", "i": index, "t": minutes | nil}
        | {"k": "p", "i": index, "q": [[path, level | nil]...], "lv": level, "and": bool,
           "d": depth, "nt": bool, "ns": navtree_start, "ip": [component...]}
value   = nil | bool | int | float | str | [value...] (a tuple)
reply   = {"tid": tid, "r": rids, "n": len, "a": actual_result_count,
           "rows": [[cell...] | nil...] | nil, "paths": [path...] | nil}
        | {"tid": tid, "f": reason, "why": text}
rids    = bin: each rid as a signed 32-bit big-endian int
overlay = nil | [[oid, pickle]...]
```

- A range clause has an empty `q`. Its `r` gives the lowest and the highest key, both included, and nil for an open end: the `min` and the `max` of the `range` of ZCatalog.
- `r` of a reply holds the rids of the batch in order with `bz`, else every rid of the result, after `l`.
- `rows` and `paths` cover the first `rn` rids of `r`, with the cells of the columns `cols`. The client gives `b_size` as `rn`, or a page of its own when the query has no `b_size`. A `LazyMap` makes a brain past these rows with the stock `Catalog.__getitem__`, which loads the bucket of `data`.
- A cell is msgpack: nil, bool, an int of 64 bits, float, str, bin, an array for a tuple, a map for a dict, and three ext types: `Missing.Value` (ext 1, empty), `DateTime` (ext 2: `_micros` as int64, the naive flag, the name of `_tz`), `Message` (ext 3: the msgpack array of the arguments of its `__reduce__`). A row with a cell of another type (a list, a `datetime`, a `set`, an instance, a larger int) comes as nil, and the client makes its brain with the stock `__getitem__`.
- The reply has no scores. ADR 0060 adds them, with the text clause.
- Amendment (ADR 0060, section 12): a plan holds at most one text clause, `{"k": "t", "i": index, "q": node, "lg": [language...] | nil, "cl": [index...], "st": [staged...], "set": settings | nil}`. The mirror runs the other clauses first and gives the rids of their result to the text index of the catalog. The reply gets `sc`, the integer score of each rid of `r` as a signed 32-bit big-endian int in one bin, when the plan has a text clause and no sort; with a sort, the hits of the text clause are a set in the order of the sort. A failure of the text clause gives the error form `{"tid": tid, "e": error, "why": text}`, with `unavailable`, `packed`, `cleared`, `corrupt` and `too_large`. A text clause of another form makes the plan malformed: `ValueError`.

The transaction overlay (Q6) is the records that the transaction changed in the database of the catalog:

- the pickles of the objects that the connection registered;
- the records in the savepoint storage of the connection that no registered object replaces;
- the new objects that they reach (the buckets of a split, a new treeset). A new object that no savepoint stored has no oid yet, so the client names it with a provisional oid: the high bit set, then a counter of the transaction.

The client sends every such record, with no filter. The server keeps the records of the trees that it mirrors, and a record of another oid costs one lookup (alternative 5). The server reads the overlay as it reads a commit (section 5), over the version at `tid`, for this query only. The overlay never enters the change sets. The client pickles the overlay again for each search (alternative 6).

A call holds at most 50,000 msgpack values and 16 MiB, under the limits of a frame of the server (100,000 values, `max-frame-size`). A larger plan or overlay takes the Python path (`too_large`). Until ZOE-291 merges, the client sends no overlay: a search in a transaction whose connection holds a registered object or a savepoint record takes the Python path (`transaction`).

### 9. The Python API

- `zoe.Storage.catalog_query(catalog, tid, plan, overlay=None)` and `zoe.zeo.ClientStorage.catalog_query(...)` take the arguments of section 8: `catalog` and `tid` as 8 bytes, `plan` as a dict, `overlay` as a list of `(oid, pickle)` pairs of bytes, or `None`. The reply is a dict, with `r` as a list of ints. A plan that does not have the form of section 8 raises `ValueError`; over the wire, the connection stays open. An unknown key or kind of clause gives the reply with `f` (`plan`). The call releases the GIL, as every core call does.
- `catalog_supported()` of both storages tells if the storage serves `catalog_query`: always for `zoe.Storage`, and for `zoe.zeo.ClientStorage` when its server accepted the extension. The hook reads it with `getattr`, as `zoe.kv` reads `kv_supported()`. The asyncio mode of `zoe.zeo.ClientStorage`, a stock ZEO client and a test storage have no such call (`unsupported`).
- `zoe.plone.catalog` is the opt-in backend `catalog` (`ZOE_PLONE_BACKENDS=catalog`, or `zoe.plone:catalog.zcml`), with `install()`, `uninstall()` and `installed()` (Q5). It hooks `Catalog.search`, as `collective.solr` does. It builds the plan, sends it with the transaction overlay, and gives a `LazyMap` of brains in the order of the reply. For a refusal, or for a query that it cannot type, it runs the stock search.
- The modes: `on`, the default once installed, and `compare`. `compare` also runs the stock search, logs and counts each difference, and returns the stock result. `ZOE_PLONE_CATALOG_MODE` gives the mode of a process (Q5).
- On a test storage, in `compare` mode, the backend builds a throwaway mirror for each search (Q7). The build of section 2 reads the test storage at the snapshot of the Connection, then reads the transaction overlay as section 8 does. The mirror answers the one search and is dropped. A private function of `zoe._zoe` builds it: it is no public API. In mode `on`, a test storage has no mirror, and the backend runs the stock search. A ZOE store uses the real mirror in both modes.

### 10. Named storages and mounts

Each named storage of `zoe serve` and each store of the process keeps the mirrors of the catalogs that its namespace lists, under its own bound. A mounted database is a store of its own, so its catalog has its own key and its own mirror.

### 11. Metrics

`zoe_catalog_queries_total{path=native|python}`, `zoe_catalog_query_latency_seconds`, `zoe_catalog_mirror_ready`, `zoe_catalog_mirror_build_seconds`, `zoe_catalog_mirror_lag_tids`, `zoe_catalog_fallbacks_total{reason, side}`, and `zoe_catalog_mirror_bytes`, `zoe_catalog_mirror_drops_total`, `zoe_catalog_overlay_records`.

## Consequences

- The build cards change the specification and the reference. This record does not. The changes:
  - "Step 2: the mirror": the scope (the GopipIndex sort waits for ZOE-302); the representation of section 3, with no trie (the titles of ZOE-288 and ZOE-304 name one); discovery by registration; maintenance after publication; versions as a base, change sets and a rebase, and the rule of the mirror window; the overlay of `catalog_query` and the form of its reply; the memory line, which states about 1.1 GB for a stock catalog of 1,000,000 entries and 1.5 GB with the indexes of `xlarge`.
  - N6 and the paragraph on the memory of a server in "Server mode": `catalog-memory` is a term of the bound, for each store with a registered catalog.
  - The correctness line of "Step 2: the mirror" and "Testing of the catalog": the replay of the synthetic B5 query log in the mode `compare` on a ZOE store, with commits during the replay (ZOE-292).
  - "Extension negotiation", the naming row of the protocol extensions, the method table of "Server mode" and `docs/reference/protocol.md`: `catalog_query`.
  - The table of the Python surface and `docs/reference/python-api.md`: `catalog_query` and `catalog_supported`.
  - The five keys `catalog-memory`, `catalog-window-tids`, `catalog-window-seconds`, `catalog-rebase-rows` and `catalog-wait`, in `<zoe>`, in the flags and the configuration file of `zoe serve`, and their `ZOE_*` twins; `ZOE_PLONE_CATALOG_MODE`; their pages of `docs/reference/`.
  - "Metrics of the catalog" and `docs/reference/metrics.md`: the metrics of section 11.
  - The text of the backends of `zoe.plone`: the backend `catalog`.
  - `crates/zoe-catalog/CLAUDE.md`: the registration, the bound and the transaction overlay.
  - `CONTEXT.md`: the terms of this record. It is outside the scope fence of ZOE-283.
- ZOE-286 takes the interim rule over the registered objects and the savepoint records. ZOE-291 takes the savepoint records and the cap of a call, and no reuse of pickles. ZOE-303 is the way back from `stale`.
- `zoe-catalog` depends on `zoe-pickle` and `roaring`, and reads the store through the read API of `zoe-core`. It stays synchronous. It starts its threads at the open of a store, never at the load of a module.
- A site gets native answers only after its mirror is built. The build time is the time to native answers after a cold start.
- The Plone suites run on test storages, so `compare` mode runs the throwaway mirror there. It covers the build and the read of a transaction overlay. It does not cover the change sets, the rebase, the mirror window or the wire. The replay of ZOE-292 on a ZOE store covers them.

## Alternatives

1. **A copy-on-write version per tid, as the specification says.** Each version copies the container of each bitmap that a commit changes, up to 8 KB: up to about 800 KB for the reindex of one object, so an estimated several gigabytes for 10,000 versions. The change sets keep the changed values only: 21.6 KB per commit in the measures.
2. **The maintenance before invalidations go out.** It adds the apply of each commit to its publication, in the hub that must never wait: 0.9 to 1.2 ms for an edit, 2.9 to 4.9 ms for a creation, 9.3 to 13.2 ms for a batch of the indexer (p50 to p99, finding).
3. **A build from the `_index` trees.** It reads the IITreeSets of every key, more records than `_unindex`, and still needs `_unindex` for the sort.
4. **A persisted snapshot of the mirror.** A format change; the cold build meets 60 s without it (53.3 s at one million entries).
5. **The client sends only the records of the catalog.** The client would need the set of the oids of the catalog; the server already has it.
6. **The client keeps the pickles of a transaction between two searches**, while no call of `catalogObject`, `uncatalogObject` or of the schema came between them. A savepoint moves the registered objects to the savepoint storage. `manage_catalogClear`, `clearIndex` and a direct call of the `index_object` of an index change the catalog without these calls. A resend costs 0.4 to 5.7 ms of pickling (finding).
7. **A rebase that builds a whole new base.** It holds two copies of the base at once: about 3 GB for the catalog of `xlarge`, over the default bound.
