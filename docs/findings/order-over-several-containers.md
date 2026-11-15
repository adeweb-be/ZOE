# Finding: the order over several containers, the navigation and `navtree`

Card ZOE-311, a spike. ADR 0059, its amendment of ZOE-302 (the order clause). The draft that follows from it: the amendment of ZOE-311 at the end of ADR 0059, status Proposed.

**Verdict.** The sort is the cost of these searches, and the filter is not. On a site of the shape of B5 `xlarge-mini`, the global navigation (448 rows in 33 containers) takes 13.9 ms on the Python path: the filter 0.6 ms, the sort 13.3 ms. Of the sort, `getObjectPosition` takes 10.8 ms, 24 µs for each row, because each call looks up the ordering adapter and the annotations again. The positions themselves cost little: one `_pos()` for each container, then a lookup for each row, gives the same keys in 0.84 ms. So the recommendation needs no change of the wire: the mirror answers the filter, and the hook builds the key map of `documentToKeyMap` with one read of the positions for each folder, then calls the stock `sortResults`. That gives the order of the Python path by construction, with every ordering, and took 2.2 ms for the navigation (13.9 ms on the Python path) and 0.6 ms for `navtree` (1.4 ms). An order clause over several containers on the wire costs about the same at this size, and needs a second call, because the client does not know the containers of a depth search before the search.

## The question

ZOE-302 serves a sort on `getObjPositionInParent` over one container. The card asks how the server finds the containers of a result over several containers and their positions, what that costs on the navigation of B5, and what `getObjectPosition` does for an id that is not in the positions.

In the inventory of the Plone suites (finding of ZOE-283), 2,322 of 9,282 calls of `Catalog.search` sort on `getObjPositionInParent`: 1,105 name one container at depth 1 (ZOE-302), 934 have depth 3 (the global navigation), 234 use `navtree` (the navigation portlet), and 49 have another shape.

## What `GopipIndex` does over several containers

`documentToKeyMap` of plone.folder 4.0.2 splits the path of each rid of the result at its last `/`, and finds each parent once with `traverse(getPhysicalRoot(), parent)`. With one parent, it takes the order (ZOE-302). With several, the key of each row is `container.getObjectPosition(id)`:

- A folder of `DefaultOrdering` reads `pos[id]` in its positions, the `OIBTree` under `plone.folder.ordered.pos`. An id that is not in the positions raises `ValueError`, and the whole search raises.
- A container that does not traverse (`None`), or that has no `getObjectPosition` (the Zope application root, an OFS folder), gives 0.
- `UnorderedOrdering` gives `None`. `PartialOrdering` gives the place of the id in its list, `None` for an id outside the list, and raises for an id that the folder does not hold. An add-on ordering gives what it gives.

Each row keeps its key, the rid 0 too, so no row leaves the result. Two more consequences:

- **Ties.** Each container numbers its items from 0, so the keys of a result over several containers repeat, and the order of the ties depends on the path of `sortResults`. `_sort_iterate_resultset` (no limit, or a limit above a quarter of the result) sorts on `(key, rid)`, in reverse for a reverse sort. `_sort_nbest` and `_sort_nbest_reverse` insert with `bisect` in the order of the rids and skip a row equal to the worst once the limit is full: with a limit of 1 and two rows of key 5, `_sort_nbest` keeps the lower rid, and a full reverse sort gives the higher.
- **`None`.** `_sort_iterate_resultset` sorts a `None` key as `(0,)`, before the rows of key 0. `_sort_nbest` and `_sort_nbest_reverse` compare keys with `<=` and `bisect`, and raise `TypeError` for `None` beside another key.

So a sort in the mirror must keep the three rules of ties with integer keys, give the Python path for an id outside the positions, and refuse `None`. The stock `sortResults` does all of this already.

The containers of a result:

- **`navtree`.** `ExtendedPathIndex.search` with `navtree` and depth 1 at level 0 gives the children of each prefix of the path, from the whole path down to `navtree_start`. So the parent of each row is a prefix of the path, and the client knows the containers before the search.
- **A depth of 2 or more.** The global sections viewlet of plone.app.layout 5.0.3 searches the navigation root with `depth` = `navigation_depth`, 3 by default. The containers are the folders between the root and depth − 1 below it that hold a row of the result. The client does not know them before the search.

## Method

A probe test on the layer `ZOE_SITE` of `zoe.plone.testing` (a `zoe.Storage` with its mirror in the process, the backend `catalog` in the mode `on`) built a site of the shape of B5 `xlarge-mini`: 8 units, each with 2 Folders of depth 2 that share 9 leaves, a leaf of 29 Documents, a Folder `news` of 42 News Items and a Collection; 2,537 entries in the catalog. It ran three searches as the site owner, with the queries of Plone 6.1:

- `nav_depth3`: the global sections viewlet (`path` of the site at depth 3, `portal_type` of `displayed_types`, `is_default_page` and `exclude_from_nav` False, sort on `getObjPositionInParent`): 448 rows in 33 containers.
- `navtree_start2`: the navigation portlet on a Document of a leaf (`navtree` 1, `navtree_start` 2): 37 rows in 3 containers.
- `navtree_root`: the portlet on a leaf, `navtree` from the root: 45 rows in 4 containers.

Each time is the median of 9 runs after a warm run, so the ZODB cache holds the objects. `just plone zoe backends -m test_probe_navtree` ran it three times, under the heavy lock and the Plone lock. The numbers below are of the last run; the runs agree within 7 % for the navigation and within 20 % for the `navtree` searches. The decode of the positions in Rust is a separate example over the records that the probe wrote, with `zoe_catalog::read::{Record::decode, node}`, 200 rounds. The probe and the example stay out of the pull request.

## The numbers

Times in ms. "With brains" adds a read of `getPath`, `portal_type`, `exclude_from_nav` and `Title` of each brain, as the viewlet does.

| | `nav_depth3` | `navtree_start2` | `navtree_root` |
| --- | ---: | ---: | ---: |
| rows / containers | 448 / 33 | 37 / 3 | 45 / 4 |
| Python path, the search | 13.9 | 1.41 | 1.65 |
| Python path, with brains | 19.9 | 1.89 | 2.16 |
| Python path, the filter alone | 0.55 | 0.35 | 0.38 |
| `sortResults` on the rids of the filter | 13.2 | 0.98 | 1.20 |
| of it: `paths` of each rid | 0.08 | 0.007 | 0.008 |
| of it: `traverse` of each container | 0.25 | 0.026 | 0.028 |
| of it: `getObjectPosition` of each row | 10.8 | 0.90 | 1.09 |
| the same keys, one `_pos()` for each container | 0.84 | 0.078 | 0.10 |
| the mirror, the filter alone | 0.90 | 0.40 | 0.41 |
| **the split: the filter in the mirror, the sort by the hook** | **2.2** | **0.60** | **0.65** |
| the split, with brains | 6.8 | 0.98 | 1.13 |
| the client finds each container, its order and its positions | 1.58 | 0.15 | 0.20 |
| the server loads the positions of each container | 0.04 | 0.005 | 0.005 |
| the server decodes them (Rust, 34 records, 11,103 bytes) | 0.28 | | |
| one more call of `catalog_query` (a search of one row) | 0.44 | 0.46 | 0.47 |

- The split gives the rids of the Python path, in the same order, in all six variants of each search: both orders, no limit, a limit of 5 (`_sort_nbest` and `_sort_nbest_reverse`) and a limit of 200 (`_sort_iterate_resultset`).
- In every container of the site, the positions agree with the order, and every id of the result is in the positions of its parent. Each container is a folder of `DefaultOrdering`, the site root too (a dexterity `Container` in Plone 6.1). The positions of a folder of up to 60 items are one record, 300 to 1,226 bytes here.
- The decode costs 8 µs for each record of the positions, 0.5 µs for each item.

**The brains of a native reply cost more than the stock brains.** With the rows of the reply, the brains of the 448 rows take 23.7 ms (53 µs each); from `Catalog.__getitem__`, 4.4 to 5.8 ms (10 to 13 µs each). The profile gives the time to `brain` of `zoe.plone.catalog` (the decode of the row of the reply) and to `DateTime.__setstate__`, four for each row: the stock brain reads a record of `data` whose `DateTime` objects the ZODB cache already holds, and the native brain makes them again. This is outside the scope of this card. The split takes the brains of `Catalog.__getitem__`, so it does not pay this cost.

**The scale of B5 `xlarge`.** The global navigation of `xlarge` reaches about 42,000 rows (240 units, about 750 Folders of depth 2, 7,200 leaves, 34,000 News Items and 240 Collections, less what the user may not see) in about 1,200 containers. At the costs for each row and each container above, the Python path would take about 1.3 s for the sort, the split about 0.1 s, and an order clause on the wire about 0.08 s (the client finds 1,200 containers at 48 µs, the server reads them at 10 µs). The brains of `Catalog.__getitem__` would take about 0.5 s whichever path sorts. No B5 run of `xlarge` measured this: the fixture takes 6.5 hours to build.

## The options

1. **The split (recommended).** The mirror answers the filter with every rid; the hook builds the key map as `documentToKeyMap` does, with one `_pos()` for each folder of `DefaultOrdering` (it calls `getObjectPosition` for any other container and for an id outside the positions), and calls the stock `sortResults`. No change of the wire, of the server or of `zoe-catalog`. The order, the ties, the exceptions, `len()`, `actual_result_count` and the batches are those of the Python path by construction, for every ordering. It serves one container too, so it can also take the sorts on `getObjPositionInParent` that the order clause of ZOE-302 refuses (another ordering, another depth). Its cost grows with the rows: about 3 µs for each row in Python. The hook depends on `Catalog.sortResults` and `_sort_limit_arguments` of Products.ZCatalog 7, as it already depends on `instantiate`.
2. **An order clause over several containers on the wire.** The plan names each container with the oids of its order and of its positions; the mirror reads the positions at the tid of the plan or from the overlay, and sorts in Rust with the three rules of ties. For a depth search the client does not know the containers, so the server replies `need` with the parents that the plan lacks, and the client resolves them and calls again (a hint of the last containers of the same path clause saves the second call on most pages). It costs about the same as the split on the probe: 0.90 + 1.58 + 0.32 ms, plus 0.44 ms on a `need`. It adds a key of the plan, a form of the reply, a second call and the read of a tree through the overlay. It saves about 3 µs for each row, which counts only with tens of thousands of rows.
3. **The server finds the containers.** It would read the folders from the Zope root in Rust (`_tree`, `__annotations__`, the positions), at the tid and through the overlay: one call, no work on the client, but the containment of Zope in the server and three to four records of each folder at each query, or a containment index kept with the change sets. The deferred card ZOE-304 asks for such an index. If it builds one, a later amendment can take option 2 without the reply `need`.
4. **The stock `GopipIndex` on the rids of the mirror.** The order of the Python path, but it keeps the 24 µs of `getObjectPosition` for each row: 0.9 + 13.2 ms against 13.9 ms. It saves nothing.
5. **`navtree` only, with its containers in the plan.** Its containers are the prefixes of the path, so it needs no reply `need`. It leaves the 934 searches of the navigation on the Python path.

## Recommendation

Option 1, the split, as the draft amendment of ADR 0059 gives it. It answers the card for both shapes with the order of the Python path, it is the simplest option that the specification allows, and it is as fast as option 2 at the size of the probe. Option 2 or 3 can follow if a B5 run of `xlarge` shows the sort for each row in Python as a cost: the split does not prevent either.

The build (step 3) then touches `python/zoedb/src/zoe/plone/catalog.py` and its tests, ADR 0059, and the documents of the specification that name the sort on `GopipIndex` ("Step 2" and F33). It does not touch `crates/zoe-catalog/**` or `crates/zoe-server/src/protocol/**`.

## The acceptance test

The finding leaves the counting of a split search open (question 1), so it writes no skeleton. The cases of `python/zoedb/src/zoe/plone/tests/test_catalog_gopip.py`, a class of their own, on the layer of ZOE-286 with the backend on, a site of three levels (units, Folders, leaves of Documents, a Folder of News Items in each unit):

- The global navigation (the query of the viewlet, depth 3) and `navtree` queries (from the root, and with `navtree_start` 2 on a Document of a leaf) give the rids, `len()` and `actual_result_count` of the Python path and count as native: in both orders, with `sort_limit` 5 and 200, and with a batch past the middle.
- The same after moves in two containers, each in a transaction of its own (`moveObjectsByDelta` in a leaf, `moveObjectsToTop` in a unit), after a rename and a delete; and inside a transaction after a move, through the transaction overlay.
- At an older snapshot.
- A container whose positions lack an id of the result (written by the test): the search raises the `ValueError` of the Python path.
- A container of `UnorderedOrdering` among others: the order of the Python path, keys `None` included, and the `TypeError` of a `sort_limit`.
- A GopipIndex with another sort index: the Python path with `sort`.
- The same queries through `zoe serve` (`ZOE_SERVE_SITE`).

## Limits

- One shape and one size: `xlarge-mini`, with Documents in the place of the Files. The numbers of `xlarge` above are a projection from the costs of each row and each container; no B5 run measured them.
- The probe ran in the process, on a warm ZODB cache, in one thread. Through `zoe serve` the filter costs one round trip more; the key map reads `paths`, the folders and the positions from the client cache, as the Python path does.
- The cost of option 2 on the server is the load and the decode measured apart, not a built clause.
- The site had no folder of another ordering and no positions that disagree with the order, so the probe compared the orders on folders of `DefaultOrdering` only. The rules for the other cases come from the code of plone.folder 4.0.2 and Products.ZCatalog 7.2.1.

## Open questions for the human

1. **Is the split native?** The acceptance test of the card asks the navigation to "count as native". In the split, the mirror filters and Python sorts. The draft counts it as native and counts it again as `client_sort`. The other way: a path of its own in `zoe_catalog_queries_total`.
2. **The scope of the split.** The draft serves with the split every sort on one GopipIndex alone that the order clause of ZOE-302 does not take: several containers, and also one container of another ordering or at another depth. The narrower way: several containers only, as the card names.
3. **The brains of a native reply.** They cost 53 µs each against 10 to 13 µs from `Catalog.__getitem__` on a warm cache (the `DateTime` cells). A card of its own, outside ZOE-311?
4. **An order clause on the wire (option 2), or a containment index with ZOE-304 (option 3),** only after a B5 run of `xlarge` measures the navigation? The draft says yes, and keeps them as alternatives of the amendment.
