# zoe-catalog

The catalog mirror: the ZCatalog indexes of each registered catalog of a store in memory, answering queries as of a tid (ADR 0059). Specification: "Catalog acceleration and search", "Step 2: the mirror".

Built: FieldIndex, KeywordIndex, DateIndex, DateRangeIndex, BooleanIndex, UUIDIndex and ExtendedPathIndex with their clauses, and the sorts on FieldIndex, DateIndex and UUIDIndex; the registration, the build, the base and its change sets, the rebase, the mirror window, the memory bound and the counts; the plan and the reply of section 8 as a tree of msgpack values (`Wire`). The metadata table, the wire of `zoe serve` and the transaction overlay come later.

## Layout

- `read.rs`: `Reads`, what the mirror reads of a store (`zoe_core::Store` gives it), the decode of a record and the walk of a tree over several threads.
- `base.rs`, `build.rs`: the rows and the structures of a base, and their build.
- `change.rs`: the change sets and the read of the records of a commit.
- `mirror.rs`: the versions of one mirror, the window and the rebase.
- `query.rs`: the clauses of each index type and the four sort paths of ZCatalog.
- `plan.rs`: the codec of a plan and of a reply.
- `mirrors.rs`: the mirrors of a store, their thread, their builds and the bound.
- `reason.rs`, `counts.rs`: the reasons of a fallback and the counts.
- `testing.rs`, with the feature `testing`: the records of a catalog as Products.ZCatalog writes them, and `commit`.
- `tests/data/zcatalog.py` writes `tests/data/zcatalog.json`: records and answers of Products.ZCatalog on FieldIndexes, for `tests/mirror.rs`. `tests/data/indexes.py` writes `tests/data/indexes.json` for the other index types, for `tests/indexes.rs`. `tests/data/path.py` writes `tests/data/path.json` for ExtendedPathIndex, for `tests/path.rs`. Run each with `PYTHONHASHSEED=0`.

## Invariants

- A query is answered as of the snapshot tid of the client, exclusive as the tid of `loadBefore`. A result never names an object that the Connection cannot load.
- A query with an index or an option that the mirror does not serve takes the Python path, whole, with its reason. So does a snapshot outside the mirror window, and a catalog that is not registered, not ready, refused, dropped or stale. The names of the reasons are a contract (`Reason::name`). The store also gives `sort`, a reason of the client in ADR 0059, section 7, for several sort indexes on the path of ZCatalog that iterates the first sort index: only the size of the result decides that path. A range bound that the keys of the index do not order gives `option`.
- Rids, order, `len()` and `actual_result_count` equal those of ZCatalog, ties included: each of the four sort paths keeps its own rule of ties, and a batch past the middle takes the other end.
- Security stays in Plone: `allowedRolesAndUsers` is a KeywordIndex like any other.
- Each index reads the record of the index object, its `_unindex` tree and its `_length`, and no other tree: no `_index`, no `_index_value` of a BooleanIndex, no `_since` or `_always` of a DateRangeIndex, no `_index_parents` or `_index_items` of an ExtendedPathIndex. An ExtendedPathIndex reads `_depth` and `multi_valued` from its record. FieldIndex, DateIndex, UUIDIndex and BooleanIndex keep the structure of a FieldIndex; a KeywordIndex keeps the keys of each row; a DateRangeIndex keeps `since` and `until` of each row and the rows sorted by each. A change set keeps the value of a row of a KeywordIndex as its distinct keys in ascending order, and a row without a key as a row without a value.
- A UUIDIndex whose `_length` in the version differs from its rows with a UUID gives `index_unavailable` to each clause and sort on it. `_length` counts the keys of `_index`, which the mirror does not read. A UUID at two rows, or a UUID that left `_index` while a row keeps it (`removeForwardIndexEntry` deletes the key whichever row holds it), makes ZCatalog find other rows than `_unindex` gives. The count follows the `_length` and the rows of each change set.
- A key of `not` that the keys of the index do not order gives `option`: ZCatalog raises.
- A path clause follows `ExtendedPathIndex.search` step by step, with the `None` of BTrees: `intersection` with no set gives the other set whole, so a level at which no row ends adds the whole set of the path. A search that reads `_index_items` (the level 0 with a depth of 0, a `navtree` of depth 0, and a `navtree` of depth 1 with `multi_valued`) gives `index_unavailable`: `_index_items` keeps one rid for each path, loses it when any row of the path leaves, and keeps the key of a path of one component, so no tree that the mirror reads gives it. A commit that raises `_depth` writes the index record, so the mirror goes stale and builds again.
- A rebase gives a new row only to a rid that the old base has no row for, whatever the order of the new rids.
- A clause on a DateRangeIndex applies after the other clauses. A row of the result without an entry in a DateRangeIndex of the query gives `date_range_order`, as does a date outside the range of the index with another clause: the answer of ZCatalog then depends on the order of its plan. The only clause of a query comes first in that plan.
- The mirror builds only registered catalogs: the keys of the durable namespace `zoe.catalog`, read at the start and after each commit that changes its generation. No scan of the store, no build on a first query.
- A base never changes. A change set holds only the changed values. A rebase keeps the number of each row, so the structures that it does not build again stay shared.
- The mirrors of a store stay within `catalog-memory`, the memory of a build and of a rebase included. Past it, the mirror that answered a query least recently drops, and the drop is counted.
- The publication of a commit never waits for the mirror: `Mirrors::committed` only records the tid. The mirror never makes a commit fail. A mirror that fails (a panic, a refused record) gives the Python path to its catalog, logs once through `take_events`, and never gives a wrong result.
- A commit that writes another pickle for the catalog object or for a mirrored index object, or deletes a mirrored object, makes the mirror stale: it drops its base and builds again.
- The crate is synchronous: no tokio, no Python inside. It starts its threads in `Mirrors::start`, never at the load of a module. A build takes a share of the store for each read, so it never keeps the store open; `stop` waits for the thread and the builds.
- One build at a time in a process. A build decodes the keys straight into arenas, freed whole when the structure is built.
- No mutex of the mirror is held while a query computes: a query takes the base and the change sets that it needs, then releases the lock.
