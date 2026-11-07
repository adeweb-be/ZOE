# Finding: the catalog mirror, built from the index trees of a catalog of one million entries

Card ZOE-283; requirements F17, N6 and N7, the sections "Step 2: the mirror, `zoe-catalog`", "Metrics of the catalog" and "Testing of the catalog" of the specification, and the decisions Q2 to Q7 of the plan of phase 6 (#597). The prototype and the scripts are on the branch `spike/ZOE-283` at [`71ce00c4`](https://github.com/adeweb-be/ZOE/tree/71ce00c40dc45339e37b774c0a521b613553ea42), not for merge. The design that this finding proposes is ADR 0059 (proposed).

**The numbers, in short.**

- **Cold build (G3): 53.3 s**, against 60 s, from a cold page cache, 22 threads, metadata table included: 1,367,593 records (1.87 GB of pickles) read and decoded, one tree after the other, with the structures of each tree built in a thread beside the next walk. In a new process with the store in the page cache: 29.2 s. No persisted snapshot is needed.
- **Memory: 1,521 bytes per entry, 1.52 GB at one million entries** (56 indexes and 53 columns of `xlarge`): 422 MB for 32 value indexes (13 bytes per entry each), 276 MB for 10 KeywordIndexes (`object_provides` alone 148 MB), 518 MB for the metadata table, 128 MB for the map of tree oids. A stock catalog (25 mirrored indexes, 32 columns): 1.09 GB. Not the low hundreds of megabytes of the specification. The process RSS was 11.7 GB: the prototype keeps the transient values of the build in the allocator; a server build must not.
- **Queries (G2): five of the six five-index intersections under 5 ms at p99 warm** (0.06 to 1.84 ms); `generic-numbers` 29.0 ms, because the prototype unions the postings of a range of 3,000 keys one by one (not measured again after the fix that it calls for).
- **Maintenance per commit: 0.88 ms (p50) / 1.20 ms (p99) for an edit, 2.9 / 4.9 ms for a creation, 9.3 / 13.2 ms for a batch of 4 objects of the indexer.** ADR 0059 applies each commit after its publication, so the hub never waits for it.
- **The overlay of Q6:** a transaction that edits one entry sends 22 records (8.0 KB, 3 new objects with provisional oids), pickled in 0.4 ms in Python and read into a version in 0.20 ms; with a creation and a removal, 112 records (104.6 KB), 5.7 ms and 2.2 ms. The answers equal ZCatalog's in the transaction.
- **Versions:** one change set per tid, 21.6 KB each in these commits, about 216 MB for a window of 10,000 tids; a copy-on-write version per tid would cost gigabytes (an estimate).
- **Check of step 4: no mismatch.** On `catalog-1m`, 45 queries equal ZCatalog's rids, order, `len()` and `actual_result_count`, ties included, and 10 queries inside two transactions over the overlay; on a testbed of three seeds, 3,152 more over every sort path and option of the scope, overlays and a commit.
- **Inventory (Q6):** in the Plone suites, 9,282 calls of `Catalog.search`; the prototype serves 66.8 %; 25.0 % sort on `getObjPositionInParent` (the order index, ZOE-302), 6.6 % use DateRecurringIndex, 1.5 % ZCTextIndex. In the part of the suites recorded with the fixed probe, 18.2 % of the searches ran in a transaction that had changed the catalog. The B5 runs were not run (see "Limits").
- **The order of ZCatalog (Q7):** four sort paths with four rules for ties; `_sort_nbest` keeps neither the lowest nor the highest rids at its threshold, and the prototype follows its rule exactly.

**The design that follows (ADR 0059, proposed):** registration of each catalog in the durable namespace `zoe.catalog`; a build in the background after the open; a base with one change set per tid and a rebase; the apply after publication with a bounded wait of a query at a newer tid; a memory bound of 2 GiB per store; typed plans built with `IndexQuery`; the transaction overlay as the changed records of the transaction, savepoints included, with provisional oids; the Python API and the backend `catalog` of `zoe.plone`.

## Question

Can a mirror that Rust builds from the committed index trees of a catalog meet the targets of the specification (a five-index intersection with a sort under 5 ms at p99 warm, a mirror ready within 60 s of a cold start, the low hundreds of megabytes for one million objects), and how must it be built: its build, its memory, its maintenance, its versions, the overlay of a transaction, the queries that Plone sends, the order of ZCatalog, and the form of a plan on the wire.

## Method

**The prototype** (`crates/zoe-catalog` on the branch; about 4,900 lines with its driver). It reads a store through the read API of `zoe-core` and decodes records with zoe-pickle (`decode_record_with`, a heap of `Heap::with_values`, `btrees`). It has no Python and no thread at the load of a module.

- **The base**: the indexes, the metadata table and the paths of one catalog at one tid, built from the trees and never changed (ADR 0059, section 3). The rows are the rids of `paths` in ascending order; every bitmap is a `roaring` bitmap of rows.
- **The build** walks each tree with a pool of threads: a thread loads a node, decodes it, queues the children of an inner node and hands the items of a bucket to the builder of the index. It reads `paths`, `uids` (checked to be the inverse of `paths`), the `_unindex` and `_length` of each index, and `data`, one tree after the other.
- **The versions**: one change set per committed tid with the old and the new value of each changed row of each tree; a version is the base and the overrides of the change sets up to its tid (ADR 0059, "Terms").
- **The apply of a commit**: the map of the oids of the mirrored trees, the decode of the records that it holds, the new buckets that a changed inner node names, and the diff of the items of each changed bucket against the rows of its old key range.
- **The overlay of a transaction**: the same read, over the version at the tid of the query, kept for the query alone.
- **The query** of a typed plan over a version: the clauses of each index as ZCatalog applies them, then the sort on the path of `sortResults` that ZCatalog takes (question 7).

**The driver** (`examples/mirror.rs`): `bench` opens a store read-only, builds the mirror with the pages of the store dropped from the page cache (`posix_fadvise(DONTNEED)` on every file of the store), measures the memory, drops the mirror and builds it again warm, answers the plans with their check, reads the overlays, then applies synthetic commits of the shapes of B5 and measures the versions; `test` runs the testbed below.

**The check of step 4** has two parts.

- On `catalog-1m`: `check.py` runs in the interpreter of the zoe scenario, opens the ZOE store through `zoe.Storage(read_only=True)`, and for each query builds the typed plan as the client will (ZCatalog's own `IndexQuery` and the conversion of each index), and records the answer of ZCatalog (`Catalog.searchResults`: the rids in order, `len()`, `actual_result_count`). Then two transactions change the catalog without a commit (an edit of 4 indexes of one entry; the same with a new entry and a removed one); the records that the connection registered, and the new objects that they reach with a provisional oid, form the overlay; the answers of ZCatalog in that transaction are the reference.
- **The testbed** (`testbed.py`): a catalog of 3,000 and 6,000 entries with the 7 index classes of the scope, 14 indexes and 8 columns, in a `MappingStorage`, no Zope; random values with many ties, paths 7 deep, missing attributes; 600 random queries over every option of the scope, every sort path, batches, `not` and ranges; four overlays (1 to 200 changed entries, with new and removed entries, so with new buckets); and one commit with 300 queries after it. Three seeds.

**The inventory of question 6**: a recording wrapper of `Catalog.search` (`probe_sitecustomize.py`, outside the repository) wrote one line per call in `just plone zoe suites`. The B5 runs of `browse`, `catalog` and `mixed` were not run (see "Limits").

**Machine**: the reference machine (Intel Core Ultra 7 155H, 6 performance cores of 2 threads and 10 efficiency cores, 30 GB, NVMe), Linux 7.0, rustc 1.99 in release. Two other spikes ran on the machine at other times; the measures held the lock of heavy jobs.


## The cold build (question 1)

From a cold page cache (the 11.2 GB of the store dropped with `posix_fadvise`), 22 threads, with the metadata table:

| Measure | Value |
| --- | --- |
| Time to ready | **53.3 s** (target 60 s) |
| Records read and decoded | 1,367,593 (1.87 GB of pickles): the buckets and inner nodes of 53 trees (`paths`, `uids`, `data` and the `_unindex` of the 50 mirrored indexes), 16,377 to 33,599 records for each tree and 1 for each of the 3 empty indexes, and 56 records of the catalog, the indexes and their `_length` |
| The walks, tree after tree | 43.9 s; `data` 8.6 s (772 MB of pickles), `paths` 3.7 s, an `_unindex` 0.5 to 2.6 s |
| The structures, beside the walks | 56.7 s of build threads in all; `object_provides` 20.4 s (a KeywordIndex of about 30 interfaces per entry: 30 million pairs to sort), `data` 4.7 s, `path` 4.0 s, a KeywordIndex 1.6 to 2.2 s, a value index 0.2 to 1.2 s |
| Peak RSS of the process | 15.2 GB (see "Memory") |
| A build in a new process, with the store in the page cache | 29.2 s |
| A build again in the same process, right after a drop of the mirror | 58.5 s (the allocator still holds the memory of the first build) |

About 24 s of the cold build wait on the disk; the rest is the walk of each tree in turn and the sort of the keyword pairs of `object_provides`, which ends after the last walk. Both have room: walks of several trees at once, a read of the pages in key order, and a decoder for buckets alone in place of the generic tree of zoe-pickle (ZOE-281 measured 21 to 24 µs a record with it). No persisted snapshot of the mirror is needed for G3.

The build runs after the open of the store, so N7 holds: the open of the store took 0.01 s; the mirror is ready 53 s later, and until then each query takes the Python path (`not_ready`).

## Memory (question 2)

The bytes of the structures of the base at 1,000,000 entries, counted from their capacities:

| Part | Indexes or columns | MB | Bytes per entry |
| --- | ---: | ---: | ---: |
| FieldIndex, DateIndex, UUIDIndex | 32 | 422.0 | 13.2 per index |
| KeywordIndex | 10 | 275.9 | 27.6 per index; `object_provides` 147.9 |
| BooleanIndex | 6 | 26.0 | 4.3 per index |
| DateRangeIndex | 1 | 33.2 | 33.2 |
| ExtendedPathIndex | 1 | 65.5 | 65.5 |
| The rows and the paths of `paths` | | 52.9 | 52.9 |
| The metadata table | 53 columns | 517.6 | 9.8 per column; `Description` 102.1 MB, `UID` 45.0 MB |
| The map of the oids of the trees, for the maintenance | | 128.4 | 128.4 |
| **All** | | **1,521** | **1,521** |

The stock catalog alone (its 25 mirrored indexes and 32 columns, without the 25 `bench_*` indexes and 21 columns of `xlarge`) takes 965 MB, and 1,093 MB with the map of oids: 581 MB of indexes, rows and paths, 384 MB of table. So the low hundreds of megabytes of the specification do not hold as built: a stock catalog of one million entries takes about 1.1 GB, half of it the metadata table, and 1.5 GB with the indexes of `xlarge`. The largest parts are the table, the keys of the unique indexes (`UID`, `sortable_title`, the dates), and `object_provides`.

The resident memory of the process is far above these bytes: 10.9 GB after the build, 11.7 GB with the mirror, 15.2 GB at its peak. The build holds the decoded values of a tree (a `Val` per value, a boxed tuple per row of `data`) while its structures build, and glibc keeps the memory that 22 threads freed in their arenas. A drop of the mirror freed it in 2.6 s, and the RSS stayed at 10.0 GB. A mirror in `zoe serve` must build without that: values decoded straight into the structures of the index, and an allocator or arenas that give the memory back, or the bound of N6 counts the allocator, not the mirror.

**The bound (Q2).** At these bytes, a default of 2 GiB per store holds the mirror of one catalog of 1,000,000 entries with the indexes of `xlarge`, its window and the room of a rebase of one index. It does not hold two stock catalogs of that size (2 × 1,093 MB is more than 2 GiB). A drop is a free of its structures (2.6 s at one million entries, in a thread of the mirror). A build again costs a build: 53 to 59 s at one million entries.

**The versions in the window.** 21.6 KB per change set in these synthetic commits (the old and new value of each changed row of each tree; a row of `data` is all its cells): 216 MB for 10,000 tids of the same shapes.

## Queries (question 3)

In-process, warm, one thread, 200 runs of each, the rids and the counts only (a reply with the rows and the paths of a page of 10 to 100 rids costs 0.0 to 0.4 ms more):

| Query of the fixture | Hits | p50 | p99 | ZCatalog, cold |
| --- | ---: | ---: | ---: | ---: |
| `listing` (5 clauses, sort on `effective` descending, `sort_limit` 20) | 481,046 | 0.47 ms | **0.60 ms** | 11.4 s |
| `drafts` (sort on `modified`, 50) | 18 | 0.05 ms | **0.06 ms** | 19.9 s |
| `subject` (sort on `effective`, 10) | 4,939 | 0.83 ms | **1.58 ms** | 0.4 s |
| `generic-words` (sort on `sortable_title`, 30) | 4,044 | 1.69 ms | **1.84 ms** | 47.8 s |
| `generic-numbers` (sort on `bench_date_01`, 20) | 3,058 | 26.6 ms | **29.0 ms** | 10.8 s |
| `folders` (sort on `sortable_title`, 100) | 32,760 | 1.06 ms | **1.19 ms** | 2.4 s |

Five of the six five-index intersections are under 5 ms at p99 (G2). `generic-numbers` is not: its clause `bench_field_01` is a range over 3,000 int keys, and the prototype unions the postings of the 3,000 keys one by one. A test of the key of each row of the result set of the other clauses (about 30,000 rows here), which the prototype already does when that set is smaller than the keys of the range, brings it to the cost of the other queries; it was not measured again.

The other queries of the check, for scale: a sort of the whole result without a limit costs 30 ms for 481,046 rows (`listing-nolimit`) and 19 ms for 708,279 on the path that iterates the index (`state-iterate-index`); a sort on several indexes 10 to 12 ms (it ranks every row); a range over many keys 19 ms (`range-int`, as above); path, `navtree`, UID, boolean, `not` and keyword queries 0.001 to 0.5 ms; `unknown-key` (a sort on `id` of 960 rows) 3 ms.

## Maintenance (question 4)

`catalog-1m` holds no history, so the driver makes commits of the shapes of B5 from the buckets of the store (see "Limits"), 50 of each, applied in tid order, warm:

| Shape | Rows | Mirrored records | Changed values | Apply p50 | p99 |
| --- | ---: | ---: | ---: | ---: | ---: |
| an edit (`modified`, `sortable_title`, `Date`, `data`) | 1 | 4 | 4 | 0.88 ms | 1.20 ms |
| a reindex of one object (every mirrored tree) | 1 | 48 | 48 | 2.24 ms | 2.97 ms |
| a creation (a new rid in every mirrored tree) | 1 | 49 | 49 | 2.93 ms | 4.88 ms |
| a batch of the indexer (4 objects reindexed) | 4 | 193 | 193 | 9.35 ms | 13.2 ms |

B5 writes 25 to 163 catalog records for a save without queued indexing and 97 to 213 per operation of the indexer (ZOE-281); the mirror decodes only the records of the trees that it mirrors (the `_unindex` of each index, `data` and `paths`) and looks the others up in a map. So a maintenance "before invalidations go out" would add about 1 to 5 ms to the publication of a save and 10 to 13 ms to a batch of the indexer, in the hub of `zoe serve`, whose mutex must queue frames only, and for each commit of the store. A mirror that applies after the publication adds nothing there; a query at a tid that the mirror has not applied waits for at most the apply of the commits before it: the same 1 to 13 ms, only for a query that comes in that window. ADR 0059 takes the second.

## The overlay of a transaction (question 5)

| Measure | An edit of 4 indexes of one entry | The same, a new entry and a removed one |
| --- | --- | --- |
| Records that the connection registered | 19 | 99 |
| New objects reached (provisional oids) | 3 | 13 |
| Records and bytes sent | 22, 8.0 KB | 112, 104.6 KB |
| Pickling them in the client (Python 3.13) | 0.4 ms | 5.7 ms |
| Reading them into a version in the mirror | 0.20 ms | 2.18 ms |
| Changed rows of the overlay | 4 | 23 |
| Answers equal to ZCatalog in the transaction | 5 of 5 | 5 of 5 |

On the synthetic commits, an overlay read and the `listing` query over it cost 1.3 ms (an edit) to 12.3 ms (a batch of 4) at p50. Over the wire the same bytes go in the call: 8 to 105 KB, a few tens of microseconds on a local socket beside the round trip of 20 µs or more. The testbed read overlays of 1, 3, 20 and 200 changed entries (113 to 5,078 records, 78 KB to 2.1 MB) with no mismatch in 160 queries per seed.

**Naming the new objects.** A new object (the bucket of a split, a new treeset of a key, a new `IITreeSet` of `_index_parents`) has no oid before the commit. The client pickles it with a provisional oid, the high bit set and a counter of the transaction, and the persistent ids of the records that point to it carry that oid. The mirror reads a changed inner node, registers its new children for the read, and reads their records as buckets of the tree.

**Keeping the pickles between two searches.** ZODB registers an object once per transaction, at its first change, so a later change of a registered object leaves no trace in the connection. The client therefore cannot reuse the pickles of the previous search by looking at the registered objects. It can when it knows that the catalog did not change since: the backend wraps `Catalog.catalogObject`, `uncatalogObject` and the calls that change the schema, and keeps the pickles while none of them ran. That rule misses a savepoint, which moves the registered objects to the savepoint storage of the connection, and the calls that change the catalog without `catalogObject` or a change of the schema (`manage_catalogClear`, `clearIndex`). At 0.4 to 5.7 ms of pickling and 0.2 to 2.2 ms of reading, a resend for each search is affordable, so ADR 0059 resends.

**Share.** In the part of the suites that the probe recorded with its fix, 18.2 % of the searches ran in a transaction that had changed the catalog, with p50 119 records and 19 KB to send (question 6).

## Versions (question 9)

The prototype keeps one change set per tid, not one copy-on-write version per tid:

| Measure | Value |
| --- | --- |
| Bytes of a change set of these commits | 21.6 KB on average (the old and new values; a row of `data` counts all its 53 cells) |
| The window of 10,000 tids | about 216 MB of these change sets |
| The overrides of a version, from the change sets | 0.7 to 2.1 ms after 200 commits (14,707 changed rows) |
| A query over these overrides | `listing` 0.61 ms (0.47 at the base); most queries within 1 ms of their time at the base; `navtree-depth999` 27 ms, against 0.17 ms, because the prototype parses the path of each overridden row at each step |
| A rebase of one index into a new base | 0.2 s (`review_state`) to 2.0 s (`path`) |

A change set that keeps the changed cells of a row of `data`, not the whole row, divides the window by about three. A version per tid with copy-on-write bitmaps copies, for each changed key, at least the container of the bitmap that holds the row (up to 8 KB): a reindex of one object changes 48 trees, about 100 keys, so up to about 800 KB per version and gigabytes for a window of 10,000 tids. That is an estimate from the sizes of the containers; the prototype did not build it.

## The check (step 4)

| Check | Queries | Equal | Mismatch | Python path |
| --- | ---: | ---: | ---: | ---: |
| `catalog-1m`, the six queries of the fixture and 39 more (every sort path, batches past the middle, path, `navtree`, `level`, `not`, `and`, ranges, UID, a DateRangeIndex alone) | 46 | 45 | 0 | 1 (a DateRecurringIndex) |
| `catalog-1m`, two transactions that change the catalog, through the overlay | 10 | 10 | 0 | 0 |
| The testbed, three seeds, at the commit | 1,800 | 1,784 | 0 | 16 (ZCatalog itself raises) |
| The testbed, overlays of 1 to 200 changed entries | 480 | 474 | 0 | 6 (ZCatalog raises) |
| The testbed, after one commit applied to the mirror | 900 | 894 | 0 | 6 (ZCatalog raises) |

Every answer that the mirror gave equals ZCatalog's: the rids, their order, `len()` and `actual_result_count`, ties included. The first runs of the testbed found two faults of the prototype that the final runs no longer have: the client took the sort from the query after `make_query`, which sorts the list of `sort_on`; and two new keys of the overrides between the same two keys of the base had one rank.

## The queries that Plone sends (question 6)

The calls of `Catalog.search` in `just plone zoe suites` (the suites of `Products.CMFPlone`, `plone.app.contenttypes` and `plone.restapi`: 2,975 tests, all green): 9,282 calls. The B5 runs of `browse`, `catalog` and `mixed` on `small` and `xlarge-mini` were **not run**: the session ended while their fixture builds and runs waited for the lock of heavy jobs, which other spikes held for most of the session. The suites hold the shapes of the pages of Plone and of the REST API that B5 drives; the shares of a site with users differ.

| Index type | Calls with it | Share |
| --- | ---: | ---: |
| ExtendedPathIndex | 8,274 | 89.1 % |
| KeywordIndex | 3,616 | 39.0 % |
| FieldIndex | 2,725 | 29.4 % |
| BooleanIndex | 2,195 | 23.6 % |
| DateRangeIndex | 980 | 10.6 % |
| DateRecurringIndex | 615 | 6.6 % |
| ZCTextIndex | 137 | 1.5 % |
| UUIDIndex | 109 | 1.2 % |
| DateIndex | 3 | 0.03 % |

Options (one row per index of a call; a share is of the calls):

| Index type | Options | Share |
| --- | --- | ---: |
| ExtendedPathIndex | `query` | 61.3 % |
| ExtendedPathIndex | `query`, `depth` 1 | 12.1 % |
| ExtendedPathIndex | `query`, `depth` 3 | 10.1 % |
| ExtendedPathIndex | `query`, `depth` 0 | 2.6 % |
| ExtendedPathIndex | `query`, `navtree` 1, `navtree_start` 2 | 2.0 % |
| ExtendedPathIndex | `depth` 2, 4, 5, 999; `navtree` with `depth` 0, 1, 99, 999; `level` 1 | 1.0 % |
| KeywordIndex | `query` | 45.7 % |
| KeywordIndex | `query`, `operator` `and` | 0.01 % |
| BooleanIndex | `query` | 44.3 % |
| FieldIndex | `query` | 31.3 % |
| FieldIndex | `query`, `range` `min:max` | 0.01 % |
| DateRangeIndex | `query` | 10.6 % |
| DateRecurringIndex | `query`, `range` `min` (one `min:max`) | 6.6 % |
| UUIDIndex | `query` (one `not`) | 1.2 % |
| DateIndex | `query`, `range` `min` | 0.03 % |

No call used `usage`, a `not` of a FieldIndex or a KeywordIndex, or a `level` below 0.

| Sort | Share |
| --- | ---: |
| none | 67.9 % |
| one index | 32.1 % |
| several indexes (`portal_type` then `sortable_title` or `getId`) | 0.03 % |
| `getObjPositionInParent` (GopipIndex), ascending, no limit | 25.0 % |
| `start` (DateRecurringIndex), ascending, `sort_limit` 5 or none | 6.6 % |
| `sortable_title`, `id`, `getId` (FieldIndex), mostly ascending | 0.3 % |
| `Date`, `effective`, `created` (DateIndex), descending | 0.2 % |
| `path` (ExtendedPathIndex) | 0.05 % |

`sort_limit` is in 5.4 % of the calls, `b_start` and `b_size` in 0.9 %. Non-index keys of the queries: `sort_on`, `sort_order`, `sort_limit`, `b_start`, `b_size`, `Language`, `show_inactive`, `metadata_fields`.

**What the scope of F17 serves.** The prototype serves 66.8 % of the calls. The rest take the Python path: a sort on `getObjPositionInParent` (25.0 %, the order index of ZOE-302), a DateRecurringIndex (6.6 %, the `start` and `end` of events: not in the scope of F17), a ZCTextIndex (1.5 %, the text index of ZOE-284), a sort on the path index (0.05 %). With the order index, the share is 91.8 %; with DateRecurringIndex too, 98.4 %.

**In a transaction that changed the catalog.** The first run of the probe missed these (it walked the catalog through its acquisition wrapper, which refuses `__getstate__`). The run with the fix was stopped at the end of the session after 2,189 calls, the layers of `Products.CMFPlone` (its shares of sorts differ from the whole: 64 % sort on `getObjPositionInParent`): 398 of them (18.2 %) ran in a transaction that had changed the catalog, and the prototype serves 316 of those. Such a search sends p50 119 registered records of the catalog (p90 146), 19 KB (p90 22 KB, max 45 KB), and 6 new objects (p90 24), pickled in 0.69 ms (p90 0.84 ms). The integration tests create content and search in one transaction; on a site, a save that searches is rarer (a form posts, then redirects).

## The order of ZCatalog (question 7)

From `Products/ZCatalog/Catalog.py` of Products.ZCatalog 7.2.1, the version of Plone 6.1.5. `searchResults` takes `sort_on`, `sort_order` and `sort_limit` from the query as given, then `search` runs `make_query`, which sorts every list of the query, and gives `b_start` and `b_size` to the sort. With `b_size`, the limit is `b_start + b_size`; with `sort_limit` and no `b_size`, `b_size` is the limit. A limit at or above the length of the result set becomes that length.

**Without `sort_on`.** The result set is the intersection of the sets of the indexes, an `IISet` in ascending order of the rid as a signed 32-bit int. A batch is a slice of it. `len()` is the length of the slice (the length of the set when there is no `b_size`), and `actual_result_count` is the length of the set. An empty set, or a query that no index uses, gives an empty `LazyCat`: an empty query finds nothing.

**With `sort_on`.** `sortResults` picks one of four paths, by the length `rlen` of the result set, the limit, and `len(sort_index)`, which for every UnIndex is its `_length`: the number of distinct keys, not of documents.

| Path | When | Order | Ties | Rows without a sort key |
| --- | --- | --- | --- | --- |
| `_sort_iterate_index` | no limit, and `rlen > len(sort_index) × (rlen / 100 + 1)` | by key, descending with `reverse` | ascending rid, also in reverse | left out; `actual_result_count` stays `rlen`, `len()` is the rows with a key |
| `_sort_iterate_resultset` | no limit, or `4 × limit > rlen`, or several sort indexes | `sorted` of (key, rid) | ascending rid; descending rid in reverse, for one index | left out and taken off `actual_result_count` |
| `_sort_nbest` | a limit, `4 × limit ≤ rlen`, one index, reverse | descending key | descending rid | as above |
| `_sort_nbest_reverse` | the same, not reverse | ascending key | ascending rid | as above |

The two N-best paths insert each row of the result set, in ascending rid, into a sorted list of `limit` rows. `_sort_nbest_reverse` keeps the `limit` rows of the lowest keys, and of a key at the threshold the rows of the lowest rids: the same rows as a sort by (key, rid). `_sort_nbest` does not keep the rows of the highest rids at its threshold. Let `T` be the key of the `limit`-th row from the top: every row above `T` stays, and of the rows at `T`, the ones that stay are the rows of the highest rids among the first `limit` rows, in ascending rid, whose key is at or above `T`. A sort by (key, rid) gives other rows at the threshold. The prototype follows that rule, and a simulation of `_sort_nbest` against it agrees on 20,000 random cases.

**Several sort indexes.** `_getSortIndex` returns a list, and the sort always takes `_sort_iterate_resultset` when there is a limit or a large result, else `_sort_iterate_index`. A row without a key in any sort index leaves the result. `multisort` sorts the rows by their whole key in the order of the first index, a stable sort; when the orders differ, it sorts again by the keys that have an order. `sort_order` as a list gives one order per index, and the indexes past its end take part in the first sort only. So ties keep ascending rid. In `_sort_iterate_index` with several indexes, a row without a key in a second index gets a shorter key, and when the orders differ the second sort raises `IndexError`.

**A batch past the middle.** With `b_size` and a `b_start` past half of `rlen`, `sortResults` reverses the order, takes the batch from the other end, and reverses the batch. A `b_start` at or past `rlen` gives an empty result with `actual_result_count` `rlen`.

**`actual_result_count`.** The length of the result set, less the rows without a sort key on every path but `_sort_iterate_index`. `len()` of the result is the length of the batch, or on `_sort_iterate_index` the number of rows with a key.

**The order of the indexes.** `search` applies the indexes in the order of its plan: from the timings that it measured for the same keys of query (`CatalogPlan`, a map of the process), else by name; every index of the scope is an `ILimitedResultIndex`. The sets intersect in any order with one exception: a DateRangeIndex with a result set removes the rows that do not match, so a row of the set that the index does not hold stays; as the first index it gives only the rows that it holds. Its answer then depends on timings. The prototype takes the Python path when a row of the result has no entry in the DateRangeIndex of the query.

## The plan, the reply and the overlay (question 8)

The prototype takes the plan that `check.py` builds as the client will: for each index of the catalog whose query names are in the query after `make_query`, ZCatalog's own `IndexQuery` with the `query_options`, `operators` and `useOperator` of the index, and the conversion of the index applied to the keys (`_convert` of an UnIndex, which turns a date of a DateIndex into its minutes; `_convertDateTime` of a DateRangeIndex; the truth of the first key of a BooleanIndex, which reads only that one). The sort comes from the query before `make_query` (which sorts the lists of a query, `sort_on` among them) with the same rules as `searchResults`: one order applies to every sort index, a list gives one per index, an index past its end has none. A query that holds an index of a class out of the scope gets no plan: the client takes the Python path without a call.

| Clause | Carries |
| --- | --- |
| value (FieldIndex, DateIndex, UUIDIndex, KeywordIndex) | the converted keys, `operator`, the `min` and `max` of `range`, the converted keys of `not` |
| boolean | the truth of the first key, or none |
| date range | the term in minutes |
| path (ExtendedPathIndex) | each path with its level, `level`, `operator`, `depth`, `navtree`, `navtree_start`, and the physical path of the index, which `search` compares with the first components of a path of level 0 |

The keys are none, bool, int, float, str or a tuple of them: the conversion leaves no `DateTime` in a plan. The reply gives the rids of the batch in order, `len()` and `actual_result_count`, and, when asked, the path and the cells of each rid of the batch, so the client makes its brains without a bucket. ADR 0059 gives the msgpack form of the plan, of the reply, of a refusal and of the overlay, and three ext types for the cells (`Missing.Value`, `DateTime`, `Message`).

## A split of ZOE-286 to ZOE-291 (step 6)

The numbers ask for one change of content, not of count:

- **ZOE-286** takes the versions (the base, the change sets, the overrides, the rebase), the apply of a commit and the four sort paths of ZCatalog with their ties, beside FieldIndex: a FieldIndex query cannot be exact without them, and the testbed is its acceptance harness. Its budget stays 3 if the testbed and `mirror test` move into it from the prototype.
- **ZOE-291**, the overlay, reads the records of a transaction with the code of the apply of ZOE-286 over the version of the query: the server side shrinks to the call and its limits; the client side (the pickles of the registered objects and of the savepoint records, with provisional oids, and the cap of a call) stays. Budget 2 → 1.
- **ZOE-289**, the metadata table, carries the memory question: the table is the largest part of the mirror (see "Memory"). It should start after the human sets the default of the memory bound (ADR 0059, section 6).
- ZOE-287, ZOE-288 and ZOE-290 stay as written.

## Limits

- **Not run: the B5 runs of question 6** (`browse`, `catalog` and `mixed` on `small` and `xlarge-mini`). Their fixtures had to be built first, and each step needs the lock of heavy jobs, which other spikes held for most of the session; the session ended before. The inventory rests on the Plone suites.
- **Partly run: the share of searches in a transaction that changed the catalog.** The first run of the probe did not see the changed records; the run with the fix stopped at the end of the session after 2,189 of about 9,300 calls.
- **`xlarge` was not measured.** The B5 fixture `xlarge` is not built on the machine, and its build takes about 6.5 hours. `catalog-1m` has the catalog of `xlarge` (56 indexes, 53 columns) at four times its entries, and the human counted it for G2 and G3; every cost of the build, of the memory and of a commit grows with the entries and the records, so the numbers of `xlarge` are those of `catalog-1m` at about a quarter. The commit shapes come from B5 on `small` (ZOE-281) and are synthetic on `catalog-1m`.
- **The commits are synthetic.** `catalog-1m` holds no history, so the driver makes each commit: it rewrites the buckets of the chosen rids in the chosen trees (a changed value, or a new rid) with zoe-pickle, as ZODB's resolver writes them. A real save also writes the `_index` trees, the treesets and the text indexes, which the mirror looks up and leaves: one map lookup each.
- **The rebase is measured per index**, not as a background job beside queries; the window of 10,000 tids is an estimate from the bytes of the change sets of the synthetic commits.
- **One run of each measure.** The cold build ran once, with 22 threads; a run with fewer threads was not run. The queries ran 200 times each.
- **The wire is estimated**, from the bytes of the plan and of the overlay; the prototype has no server.
- **The testbed is not Plone.** Its catalog has the index classes of the scope with stub objects; the check on `catalog-1m` and the Plone suites of the differential suite (ZOE-292) cover the rest.
