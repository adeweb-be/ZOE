# 0059. The catalog mirror: a base built from the index trees, a delta per commit, and `catalog_query`

Status: proposed. Card ZOE-283, a spike (finding: `docs/spikes/catalog-mirror.md`; prototype: branch `spike/ZOE-283`). It adds a wire call (`catalog_query`, extension `catalog_query`), a public Python API (`zoe.Storage.catalog_query`, `zoe.zeo.ClientStorage.catalog_query`, the backend `catalog` of `zoe.plone`), a durable namespace and its keys, and a key of `zoe serve` and of `<zoe>`. It changes no on-disk format and no record. It records the decisions Q2 to Q7 of the plan of phase 6 (#597) and gives the design around them.

## Context

F17 asks for ZCatalog queries answered in Rust as of the snapshot tid of the client, with identical rids, order and counts, and the Python path for what the mirror does not cover. The gate of phase 6 asks for a five-index intersection with a sort on 1,000,000 entries under 5 ms at p99 warm (G2) and a mirror ready within 60 s of a cold start (G3). The specification ("Step 2: the mirror") gives a representation, a discovery by class, a maintenance "at `tpc_finish`, before invalidations go out", versions "with structurally shared maps and copy-on-write bitmaps" kept for a window, and "the low hundreds of megabytes" for a catalog of one million objects.

The spike built a prototype in `zoe-catalog` and measured it on `catalog-1m` (56 indexes, 53 columns, 1,000,000 entries). Its numbers are in the finding. The facts that decide this record:

- A cold build from the committed trees reads one tree per index (`_unindex`), `paths`, `uids` and `data`, and needs no other tree of an index.
- The decode of a commit is cheap per record, but not free, and the hub of `zoe serve` must never wait.
- A copy-on-write version per tid copies at least one container of each changed bitmap; for a window of 10,000 tids that is gigabytes. A delta per tid is the changed values only.
- The order of ZCatalog's result depends on which of its four sort paths runs, and the ties of each path follow a rule of their own (finding, "The order of ZCatalog").
- The metadata table is the largest part of the mirror.

## Decision

### 1. Registration (Q3)

Installing the backend `catalog` of `zoe.plone` writes one key for each catalog to the durable namespace `zoe.catalog` (ADR 0043) of the store that holds it: the key is the 8 bytes of the oid of the `Products.ZCatalog.Catalog.Catalog` object (the `_catalog` of the tool), the value a msgpack map `{"path": <physical path of the tool>, "index_path": <physical path of its path index>}`. The catalog of a mount is registered in the store of the mount. A site created later writes its key in the transaction that creates it; a site deleted removes it. A read-only storage writes no key. `uninstall()` removes the keys of the catalogs of the databases that the process has open, in a transaction of its own (the way back of #597, Q3).

The store reads the namespace at its open, and after each commit that writes it: a new key starts a build, a removed key drops the mirror. There is no scan of the store and no build on a first query.

### 2. The build

A mirror is built in the background after the open of the store, so `/readyz` and N7 do not wait for it. The build reads, as of the tid of the open: `paths` (the rows), `uids` (checked against `paths`: a store where they differ gets no mirror), the record of each index with its `_unindex` tree and its `_length`, and `data`. It reads no `_index` tree: each index rebuilds its own sorted keys and postings from `_unindex`. The trees are walked by a pool of threads that load and decode buckets, and the structures of a tree build in a thread of their own while the next tree is walked; the prototype used 22 threads and was ready in 53 s at one million entries from a cold page cache. The build decodes each value into the structure of its index and keeps no tree of decoded values, so that the memory it frees goes back to the system: the prototype, which kept them, left 11.7 GB of resident memory for 1.5 GB of structures. Until the mirror is ready, each query takes the Python path with the reason `not_ready`. `zoe_catalog_mirror_ready` and `zoe_catalog_mirror_build_seconds` report it.

A record that the decoder refuses makes its index unavailable: the queries on it take the Python path with the reason `index_unavailable`. A guessed value is never served.

### 3. The structures of a base

A base is immutable. Its rows are the rids of `paths` in ascending order, so a row number orders as the rid, and every bitmap holds row numbers.

- FieldIndex, DateIndex, UUIDIndex, BooleanIndex: the distinct keys in ascending order (a vector of ints, an arena of strs, or the values), the rows of each key (one row inline, or a roaring bitmap), the slot of the key of each row, and the bitmap of the indexed rows. The slot of a key is its rank, so a sort compares integers.
- KeywordIndex: the same keys and postings, and the slots of each row.
- DateRangeIndex: `since` and `until` of each row, and the rows sorted by each.
- ExtendedPathIndex: the components and the rows of each (component, level), the rows that end at each level, the rows of each parent path, the rows sorted by path, and `_depth`.
- The metadata table: per column, the code of each row (1, 2 or 4 bytes) and the encoded distinct cells. A reply gives the cells of its page, so a brain needs no bucket of `data`.
- The path of each row from `paths`, which a reply gives with the rows.

### 4. Versions and the window

A version at a tid is the base and a patch: the value at that tid of each row that a commit after the base changed, in each tree. Each committed tid adds one delta, its changed rows with their old and new values; for a row of `data`, its changed cells. A query at tid `t` takes the base and the patch of the deltas up to `t`; every clause applies to the base and then to the patched rows, so the patch costs in proportion to its rows. The structure of the specification, a copy-on-write version per tid, is not built (alternatives).

The window keeps the deltas of the last 10,000 tids or of the last 10 minutes, the smaller: `catalog-window-tids` and `catalog-window-seconds` (keys of `zoe serve` and of `<zoe>`). A query whose tid is before the window takes the Python path (`outside_window`). When the patch of the last tid holds more than `catalog-patch-rows` rows (default 20,000), the mirror folds the deltas into a new base in the background and swaps it in; the old base stays until its last query ends, and the deltas keep their old values, so a version before the new base is the new base with the old values of the deltas after `t`.

### 5. The maintenance point

The mirror applies each commit after the hub of `zoe serve` publishes it, in tid order, in a thread of the mirror; in-process, after `tpc_finish` returns. The publication of a commit does not wait for the mirror. A query at a tid that the mirror has not applied yet waits for it, at most `catalog-wait` (default 1 s); past it, the query takes the Python path (`lag`). `zoe_catalog_mirror_lag_tids` reports the distance.

To apply a commit, the mirror looks up each record of the commit in the map of the oids of the trees it mirrors, decodes the records it owns, registers the new buckets that a changed inner node names, and diffs the items of each changed bucket against the rows of its old key range. The records of other objects cost one lookup.

### 6. The memory bound (Q2)

Each store has a bound for its mirrors: `catalog-memory` in the configuration of `zoe serve` and in `<zoe>`, default 2 GiB, which holds one catalog of 1,000,000 entries with the 56 indexes and 53 columns of `xlarge` (1.5 GB) or about two stock catalogs of that size (1.1 GB each). A build that would pass it, or a growth past it, drops the mirror of the store that answered a query least recently; it counts in `zoe_catalog_mirror_drops_total` and its queries take the Python path (`dropped`). A dropped mirror is built again only at the next open of the store, or when an uninstall frees its room. N6 names the bound.

### 7. What the mirror serves, and the reasons of a fallback

Served: the clauses of FieldIndex, DateIndex, UUIDIndex and KeywordIndex (`query`, `range`, `operator`, `not`), BooleanIndex (`query`), DateRangeIndex (`query`) and ExtendedPathIndex (`query`, `level`, `operator`, `depth`, `navtree`, `navtree_start`); a sort on one or several FieldIndex, DateIndex or UUIDIndex, with `sort_order`, `sort_limit`, `b_start` and `b_size`; a query without sort. Each answer gives the rids in the order of ZCatalog, the length of its lazy result and `actual_result_count`.

A query takes the Python path, whole, with a reason that `zoe_catalog_fallbacks_total{reason}` counts:

| Reason | When |
| --- | --- |
| `index` | an index of the query is of another class: ZCTextIndex, GopipIndex, DateRecurringIndex, any add-on index |
| `option` | an option that the plan does not carry, such as `usage` |
| `and_range` | a range with the operator `and` |
| `sort` | a sort on another index, `merge=False`, a `sort_limit` of 0, or several sort indexes on the path of ZCatalog that iterates the index |
| `date_range_order` | a row of the result has no entry in a DateRangeIndex of the query: ZCatalog's answer then depends on the order of its plan, which it learns from timings |
| `index_unavailable` | the decoder refused a record of the index |
| `not_ready`, `dropped`, `stale` | the mirror is building, was dropped, or met a change it cannot follow (a new `_unindex`, a change of the catalog object) until its rebuild |
| `outside_window`, `lag` | the tid of the query is before the window, or the mirror did not reach it in time |
| `overlay` | the overlay of the transaction could not be read |

### 8. The plan, the reply and the overlay on the wire

The extension `catalog_query` adds one call, `catalog_query(catalog, tid, plan, overlay)`. `catalog` and `tid` are 8 bytes. The client builds `plan` with ZCatalog's own `IndexQuery` and the conversion of each index (`_convert`, `_convertDateTime`), so the keys reach the server in the form of the index (Q4):

```text
plan    = {"c": [clause...], "s": [[index, reverse]...], "l": sort_limit | nil,
           "bs": b_start, "bz": b_size | nil, "rows": [column...] | nil, "paths": bool}
clause  = {"k": "v", "i": index, "q": [value...], "and": bool, "r": [min, max] | nil, "not": [value...]}
        | {"k": "b", "i": index, "q": bool | nil}
        | {"k": "d", "i": index, "t": minutes | nil}
        | {"k": "p", "i": index, "q": [[path, level | nil]...], "lv": level, "and": bool,
           "d": depth, "nt": bool, "ns": navtree_start, "ip": [component...]}
value   = nil | bool | int | float | str | [value...] (a tuple)
reply   = {"tid": tid, "r": [rid...], "n": len, "a": actual_result_count,
           "rows": [[cell...]...] | nil, "paths": [path...] | nil}
        | {"tid": tid, "f": reason, "why": text}
overlay = nil | [[oid, pickle]...]
```

A cell is msgpack: nil, bool, int, float, str, bin, an array for a tuple, a map for a dict, and three ext types: `Missing.Value` (ext 1, empty), `DateTime` (ext 2: `_micros` as int64, the naive flag, the name of `_tz`), `Message` (ext 3: the msgpack array of the arguments of its `__reduce__`). The client makes the brains from the rows and the paths with no load.

The overlay (Q6) is the records of the catalog that the transaction changed: the pickles of the objects that the connection registered and that belong to the catalog, and of the new objects that they reach (the buckets of a split, a new treeset). A new object has no oid before the commit, so the client names it with a provisional oid: the high bit set, then a counter of the transaction. The server reads the overlay as it reads a commit (section 5), over the version at `tid`, for this query only; it never enters the deltas. A record of an oid that the mirror does not own costs one lookup, so the client may send every registered object of the catalog's database without a filter. The client keeps the pickles of a transaction from one search to the next while no call of `catalogObject`, `uncatalogObject` or of the schema of the catalog came between them.

### 9. The Python API

- `zoe.Storage.catalog_query(catalog, tid, plan, overlay=None)` and `zoe.zeo.ClientStorage.catalog_query(...)` with the same arguments, the plan and the reply as Python values of section 8. A storage without a mirror of that catalog gives the reply with `f`. The asyncio mode of `zoe.zeo.ClientStorage` and a stock ZEO client have no such call: the backend takes the Python path there.
- `zoe.plone.catalog` is the opt-in backend `catalog` (`ZOE_PLONE_BACKENDS=catalog`, or `zoe.plone:catalog.zcml`), with `install()`, `uninstall()` and `installed()` (Q5). It hooks `Catalog.search`, as `collective.solr` does: it builds the plan, sends it with the overlay when the transaction changed the catalog, and gives a `LazyMap` of brains in the order of the reply; for a refusal, or for a query that it cannot type, it runs the stock search.
- The modes: `on`, the default once installed, and `compare`, which also runs the stock search and logs and counts each difference. `ZOE_PLONE_CATALOG_MODE` gives the mode of a process (Q5). In `compare` mode on a test storage (a `DemoStorage` over ZOE), the backend builds a throwaway mirror in the process from what the Connection sees, with the build of section 2 over the loads of the Connection (Q7).

### 10. Named storages and mounts

Each named storage of `zoe serve` and each store of the process keeps the mirrors of the catalogs that its namespace lists, under its own bound. A mounted database is a store of its own, so its catalog has its own key and its own mirror.

### 11. Metrics

`zoe_catalog_queries_total{path=native|python}`, `zoe_catalog_query_latency_seconds`, `zoe_catalog_mirror_ready`, `zoe_catalog_mirror_build_seconds`, `zoe_catalog_mirror_lag_tids`, `zoe_catalog_fallbacks_total{reason}`, and `zoe_catalog_mirror_bytes`, `zoe_catalog_mirror_drops_total`, `zoe_catalog_overlay_records`.

## Consequences

- The text of "Step 2: the mirror" changes in the build card: discovery by registration, not by class; maintenance after publication; versions as a base and deltas; the memory bound.
- `zoe-catalog` depends on `zoe-pickle` and `roaring`, and reads the store through the read API of `zoe-core`. It stays synchronous.
- A site sees its queries answered by the mirror only after its mirror is built; the build time is the time to native answers after a cold start.
- The fallback reasons are part of the contract: the differential suite (ZOE-292) counts them.

## Alternatives

1. **A copy-on-write version per tid, as the specification says.** Each version copies the container of each bitmap that a commit changes, up to 8 KB: up to about 800 KB for the reindex of one object, so gigabytes for 10,000 versions (an estimate of the finding). The deltas keep the changed values only: 21.6 KB per commit in the measures.
2. **The maintenance before invalidations go out.** It adds the apply of each commit to its publication, in the hub that must never wait: 0.9 to 1.2 ms for an edit, 2.9 to 4.9 ms for a creation, 9.3 to 13.2 ms for a batch of the indexer (p50 to p99, finding).
3. **A build from the `_index` trees.** It reads the IITreeSets of every key, more records than `_unindex`, and still needs `_unindex` for the sort.
4. **A persisted snapshot of the mirror.** A format change; the cold build meets 60 s without it (53.3 s at one million entries).
5. **The client sends only the records that belong to the catalog.** The client would need the set of oids of the catalog; the server already has it.
