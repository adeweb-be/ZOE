# Spike: queued indexing, the catalog conflicts of B5, the queue of CMFCore, and the reads after a save

Card ZOE-218, step 1; the success criterion on `ConflictError`s (section 1) and lever 4 of section 9 of the specification; the gate of phase 4 (#440). The decision that follows is ADR 0047.

**Verdict.** The catalog is the source of the conflicts of saves on `zoe-native`. In the measured steps of B5, 753 of the 793 `ConflictError`s of `edit` and 706 of the 806 of `create-spread` are on objects of the catalog, and most of the others are on new objects that the oid maps do not name; 70 of `create-spread` are on the storage of CMFEditions. A save writes many objects of the catalog: an edit through the form 85 of the 111 objects of its commit, a creation 163 of 198. Every one of these writes comes from one place: the processor `portal-catalog` of the indexing queue of CMFCore, which writes the catalog in the transaction of the save. A processor of `zoe.plone` in its place can send each operation to the durable namespace of ADR 0043 instead. Two readers inside a save, and every page after it, read the catalog: the navigation of each page, the folder views, the folder contents, the searches and plone.restapi. So the indexer needs a short window, and the next request of the user who saved needs a way to wait for it.

## Question

Step 1 of the card:

- Where do the conflicts of the catalog come from in B5 `edit` and `create-spread`: which indexes, which buckets?
- How does the indexing queue of Products.CMFCore process a transaction?
- What does Plone read from the catalog right after a save: the folder contents, the navigation, plone.restapi, the redirect after an edit? That gives the window between a save and its indexing.
- How do a reindex, a move, a delete and a workflow transition queue their operations?
- The baseline ("before"): B5 `edit` and `create-spread` at 4 and 16 users on `zeo` and `zoe-native` from `main`, three repetitions.

## Method

- **Code.** `main` at `812cca0`. The Plone instances of this worktree (`just plone zeo build`, `just plone zoeclient build`, `just plone zeoclient build`), and the small fixture `small-ba01ff255313`, made again by `just bench plone prepare --port-offset 200`.
- **The baseline.** `just bench plone run --stack STACK --scenario SCENARIO --rep N --steps 4:60,16:60 --port-offset 200`, with the default warmup of 60 s and trim of 5 s, for `edit` and `create-spread` on `zeo` and `zoe-native`, three repetitions, the order of the two stacks alternating from one repetition to the next, as `campaign` does. The runs of repetition 0 kept their stores (`--keep-store`). `zoe serve` wrote its resolution log (ADR 0042) into the directory of each run: `ZOE_BINARY` named a script that adds `--resolution-log` to `zoe serve` and runs the binary of this build. The log changes no outcome. The stack keeps its other defaults: tier 1 on, the posting-dict rule off, no early locks, no merge policy. Every run passed its verification.
- **The commits that the runs record.** The first five runs record `812cca0`. The merge of the acceptance test of the planner (`57c3775`, which adds only `python/zoedb/src/zoe/plone/tests/test_queued_indexing.py`) came during the sixth run, and the last five runs record a tree with the files of this spike and of ADR 0047. The instances ran the `zoedb` wheel that `just plone zoeclient build` made from `812cca0`, and the `zoe` binary of `812cca0`; no run loads the files that changed.
- **The machine.** Apple M3 Pro, 6 performance and 6 efficiency cores, 36 GiB, APFS on the internal SSD; no CPU pinning on macOS. CPython 3.13.15 with the GIL in the instances; both servers sync with `fsync(2)`. The machine was not quiet: `just tsan` of `main` and other builds and tests of other worktrees ran during the runs. The table below gives the 1-minute load average during each step, sampled every 15 s with `uptime`: from 4.7 to 31.7 on 12 cores. So the rates of one run do not compare with another run at a different load; the conflict counts change less.
- **The conflicts by structure.** A script inside Zope (`inzope/run.py`) walked the catalog of each store that a run kept, and of the fixture, read-only: the catalog tool and its counter, the metadata, the uids, the paths, each index down to every bucket and tree set, the lexicon, and three tools. It names the structure that holds each oid. The oids of the `ConflictError`s of the probe records and of the lines of the resolution log, in the measured windows, were then counted by structure. An oid that a run created and whose store the run did not keep stays unnamed.
- **What a save writes and reads.** A Zope instance of one thread on a copy of the fixture (FileStorage, no server), with a WSGI filter of the spike in front of Zope. For each request, the filter records the operations that the queue of CMFCore gets and their callers, each processing of the queue and its caller, each search of the catalog and each lookup by UID with their callers, and the oids that the commit stores, named by the walk above. A script drove it through the forms of Plone and plone.restapi as the account `maintainer` (Manager): an edit of the title and the description of a Document, an add of a Document, a publish transition, a rename, a move (cut and paste), a delete, a REST `POST` and `PATCH`, a retract of a section with 156 objects, and the pages after each.
- **The code read**: Products.CMFCore 3.7 (`indexing.py`, `CatalogTool.py`, `CMFCatalogAware.py`, `WorkflowTool.py`), Products.CMFPlone 6.1.5 (`CatalogTool.py`, `browser/navigation.py`), Products.ZCatalog (`UUIDIndex`), BTrees 6.3 (`_base.py`), plone.app.layout 5.0.3, plone.app.uuid 2.2.4, plone.app.discussion 5.2.3, plone.dexterity 3.0.8, plone.restapi 9.15.6, Zope 5.14.2 (`ZPublisher/WSGIPublisher.py`), ZODB 6.2 and ZEO 6.2, in the eggs of `target/plone`.
- The scripts and the filter stay outside the repository; they change no code of the project.

## Findings

### 1. The baseline

The rate is the median of the three repetitions, with its range; `1st-try ok` and the conflicts per 1,000 operations pool the three; `failed ops` is the sum of the three, every one a save whose retries ran out.

| scenario | stack | users | ok ops/s (min–max) | p95 ms | 1st-try ok | conflicts/1k | failed ops |
| --- | --- | ---: | --- | ---: | ---: | ---: | ---: |
| edit | zeo | 4 | 5.7 (3.4–7.1) | 2,337 | 76.9 % | 421.1 | 18 |
| edit | zeo | 16 | 5.4 (4.3–7.1) | 5,124 | 76.1 % | 382.9 | 19 |
| edit | zoe-native | 4 | 7.3 (7.1–7.7) | 1,667 | 79.1 % | 318.9 | 11 |
| edit | zoe-native | 16 | 7.3 (5.8–8.1) | 4,958 | 78.6 % | 331.1 | 12 |
| create-spread | zeo | 4 | 5.6 (5.3–5.6) | 2,257 | 77.3 % | 419.6 | 35 |
| create-spread | zeo | 16 | 5.8 (3.4–5.9) | 6,586 | 77.8 % | 420.1 | 28 |
| create-spread | zoe-native | 4 | 6.6 (6.1–6.6) | 2,037 | 79.4 % | 367.1 | 26 |
| create-spread | zoe-native | 16 | 6.1 (5.8–6.6) | 4,718 | 78.4 % | 376.3 | 23 |

Each run and step, with the load of the machine (mean and highest 1-minute load average during the step):

| scenario | stack | rep | users | ok ops/s | 1st-try ok | conflicts/1k (write + read) | failed | load |
| --- | --- | ---: | ---: | ---: | ---: | --- | ---: | --- |
| edit | zeo | 0 | 4 / 16 | 5.69 / 5.44 | 76.9 / 75.5 % | 421.9 (135 + 0) / 398.7 (122 + 0) | 7 / 7 | 17.8 (24.1) / 23.8 (25.7) |
| edit | zeo | 1 | 4 / 16 | 3.42 / 4.35 | 64.2 / 77.5 % | 601.0 (116 + 0) / 373.0 (91 + 0) | 5 / 5 | 30.5 (32.5) / 31.7 (31.8) |
| edit | zeo | 2 | 4 / 16 | 7.15 / 7.11 | 79.2 / 76.1 % | 333.3 (133 + 0) / 376.9 (150 + 0) | 6 / 7 | 5.3 (5.4) / 5.7 (5.9) |
| edit | zoe-native | 0 | 4 / 16 | 7.29 / 8.15 | 78.8 / 82.1 % | 335.8 (134 + 2) / 293.6 (125 + 8) | 4 / 5 | 9.9 (11.4) / 7.1 (7.9) |
| edit | zoe-native | 1 | 4 / 16 | 7.67 / 5.78 | 79.1 / 72.6 % | 316.9 (133 + 2) / 370.7 (109 + 10) | 4 / 3 | 8.7 (9.7) / 8.7 (15.1) |
| edit | zoe-native | 2 | 4 / 16 | 7.13 / 7.33 | 80.3 / 78.6 % | 303.8 (119 + 1) / 341.5 (131 + 8) | 3 / 4 | 5.4 (5.5) / 5.4 (5.4) |
| create-spread | zeo | 0 | 4 / 16 | 5.56 / 3.36 | 83.6 / 74.5 % | 396.3 (128 + 0) / 484.7 (95 + 0) | 17 / 11 | 6.1 (6.2) / 10.7 (14.1) |
| create-spread | zeo | 1 | 4 / 16 | 5.29 / 5.82 | 76.3 / 77.8 % | 448.2 (134 + 0) / 413.4 (136 + 0) | 8 / 9 | 7.8 (8.3) / 6.8 (7.4) |
| create-spread | zeo | 2 | 4 / 16 | 5.58 / 5.89 | 77.3 / 78.6 % | 416.4 (132 + 0) / 388.6 (129 + 0) | 10 / 8 | 5.0 (5.0) / 5.1 (5.2) |
| create-spread | zoe-native | 0 | 4 / 16 | 6.60 / 5.76 | 78.4 / 76.5 % | 364.9 (83 + 52) / 407.4 (69 + 63) | 7 / 7 | 11.3 (12.9) / 10.4 (12.3) |
| create-spread | zoe-native | 1 | 4 / 16 | 6.09 / 6.09 | 79.4 / 78.4 % | 389.5 (110 + 24) / 373.2 (82 + 46) | 9 / 8 | 14.4 (17.2) / 10.4 (11.1) |
| create-spread | zoe-native | 2 | 4 / 16 | 6.60 / 6.62 | 81.0 / 79.8 % | 348.5 (82 + 48) / 352.2 (76 + 55) | 10 / 8 | 4.8 (5.0) / 4.7 (5.1) |

The conflict classes of the probe records in the two measured steps, and the failed resolutions of the whole runs, warmup included, as `just bench plone report` gives them: in the ZEO server for `zeo`, in the clients for `zoe-native`. Three repetitions each.

| scenario | stack | `ConflictError`s by class | failed resolutions |
| --- | --- | --- | --- |
| edit | zeo | `IOBucket` 735, `OOBTree` 7, `IOBTree` 5 | `IOBucket` changes 824, `OOBucket` changes 10, `OOBTree` internal node 8, `IOBTree` inserts 5 |
| edit | zoe-native | `IOBucket` 649, `IIBTree` 73, read conflicts 31, `OOBucket` 21, `OOBTree` 4, `IITreeSet` 2, `IIBucket` 1, `IOBTree` 1 | `IOBucket` changes 722 (647 in the measured steps), `OOBucket` changes 11 |
| create-spread | zeo | `UniqueIdGeneratorTool` 754 | `UniqueIdGeneratorTool` 849, `OOBucket` changes 10 |
| create-spread | zoe-native | `IOBucket` 289, read conflicts 288, `OOBTree` 124, `IIBTree` 56, `IITreeSet` 11, `IOBTree` 9, `OOBucket` 9, `OIBTree` 2 | `IOBucket` changes 303 (272 in the measured steps), `OOBucket` changes 12 |

`zoe-native` raises fewer conflicts than `zeo` in both scenarios, and in `create-spread` it has none on `portal_uidgenerator` (F29). On `zeo`, each creation that overlaps another one fails on `portal_uidgenerator` first, so its catalog conflicts do not show.

### 2. Where the conflicts come from

The `ConflictError`s of the probe records in the measured windows, by the structure that holds their oid:

| scenario | stack | all | on the catalog | elsewhere | not named |
| --- | --- | ---: | ---: | --- | ---: |
| edit | zeo | 751 | 751 | – | 0 |
| edit | zoe-native | 793 | 753 (723 write, 30 read) | – | 40 (new `IIBTree` postings and buckets) |
| create-spread | zeo | 772 | 0 | `portal_uidgenerator` 772 | 0 |
| create-spread | zoe-native | 806 | 706 (440 write, 266 read) | the storage of CMFEditions 70 (`portal_historiesstorage`: its shadow storage and its version repository) | 30 |

The structures of the catalog, with the most conflicts:

- `edit` on `zoe-native`: the `_wordinfo` buckets (`IOBucket`) of `SearchableText` 331 and `Description` 320; the posting trees (`IIBTree`) of `Description` 36 and `SearchableText` 12, and 27 posting trees that the run created; the buckets and nodes of `sortable_title._index` 37; `Title._wordinfo` 11. On `zeo` the same `_wordinfo` buckets: `SearchableText` 393, `Description` 337.
- `create-spread` on `zoe-native`: `SearchableText._wordinfo`, its buckets 273 and its posting trees 157; the nodes of `cmf_uid._index` 55, `sortable_title._index` 31, `path._index` 25 (and its tree sets 14), `id._index` 19, the uids of the catalog 17, `Title._wordinfo` 16, `cmf_uid._unindex` 14, `UID._index` 13. The 266 read conflicts on the catalog are on the trees above the buckets that the saves change: the posting trees of `SearchableText` 104, the trees of `sortable_title` 23, `path` 29, the uids 17, `id` 15, `UID` 11, `cmf_uid._unindex` 11, `object_provides` 11, and others; 15 more are on the version repository of CMFEditions. The vote of `zoe serve` fails on a stale oid that the transaction marked as read for its current state, as BTrees does for the tree of a bucket that it changes (finding item 26 of `docs/findings/plone-end-to-end.md`).

The resolution log of `zoe serve` gives what tier 1 did in the votes with a conflict, in the measured windows of the three runs of `zoe-native`:

| scenario | lines | merged | no answer | refused |
| --- | ---: | ---: | ---: | ---: |
| edit | 29,214 | 19,930: `Length` 8,877, `IOBucket` 7,185, `IIBTree` 2,598, `OOBucket` 524, `IIBucket` 496, `IITreeSet` 218, `IISet` 32 | 9,178 `IOBucket` | 106: `IIBTree` 74, `OOBucket` 21, `OOBTree` 5, others 6 |
| create-spread | 60,377 | 53,920: `Length` 31,067, `IOBucket` 13,370, `IISet` 3,200, `IIBucket` 3,147, `OOBucket` 1,393, `IIBTree` 1,133, `IITreeSet` 564, `OIBucket` 37 | 6,217: `IOBucket` 5,631, `OIBucket` 586 | 240: `OOBTree` 129, `IIBTree` 57, `IOBucket` 19, `IITreeSet` 12, others 23 |

- Tier 1 merges most of the conflicts: the `Length` counters of the indexes and of the catalog, the buckets of the forward and reverse indexes, the posting trees, the sets.
- "No answer" is a bucket of `_wordinfo` of `Description`, `Title` and `SearchableText` whose values are posting dicts: tier 1 does not compare two dicts without the posting-dict rule, which is off in this stack, so the client resolves. The client then refuses when both saves changed the posting of one word: 647 refusals in `edit`, 272 in `create-spread`. These are the conflicts of item 1.
- The refusals of tier 1 are a delete of the first key of a posting tree (ADR 0042), the internal nodes of `cmf_uid._index`, `sortable_title._index`, `path._index` and of the shadow storage of CMFEditions, and splits of `Title._wordinfo` buckets.

So every structure that conflicts in a save of `edit`, and all but those of CMFEditions in a save of `create-spread`, is an object of the catalog that the save writes.

### 3. What a save writes

The commit of each request of the probed instance, its queued operations, and the objects that it stored: each oid once, and those of the catalog by the structure that holds them.

| change | through | operations queued | objects stored | of the catalog | structures of the catalog |
| --- | --- | ---: | ---: | ---: | ---: |
| edit of the title and the description | the edit form | 1 | 111 | 85 | 13 |
| add of a Document | the add form | 4 | 198 | 163 | 31 |
| publish of a Document | `content_status_modify` | 3 | 36 | 33 | 13 |
| rename of a Document | `object_rename` | 5 | 205 | 158 | 31 |
| move of a Document | `object_cut`, `object_paste` | 4 | 176 | 149 | 31 |
| delete of a Document | `delete_confirmation` | 2 | 166 | 139 | 31 |
| add of a Document | REST `POST` | 4 | 189 | 156 | 31 |
| edit of the title | REST `PATCH` | 1 | 43 | 25 | 11 |
| retract of a section of 156 objects | `content_status_modify` | 159 | 41 | 39 | 15 |

- The edit writes the `_wordinfo`, posting and `Length` objects of `Description` (26) and `SearchableText` (26), then `Title`, `Date`, `modified`, `sortable_title`, the metadata, the counter of the catalog tool and seven other indexes. An add writes 38 sets of `object_provides`, then the trees of `allowedRolesAndUsers`, `path`, the text indexes and most other indexes, the uids, the paths and the `Length` of the catalog.
- The other objects of a commit are the content object, its versions (CMFEditions writes a `ZopeVersion`, a `VersionInfo` and its shadow history at each save), the container of an add, a rename, a move or a delete, and other BTrees outside `portal_catalog` that the walk does not name.
- The retract of a section queues 157 reindexes of `allowedRolesAndUsers` but writes only 39 objects of the catalog: most objects of the section keep their security.
- The `GET` of the edit form stores one object, the Document: the lock of plone.locking, which the locking backend of ADR 0043 moves to the ephemeral namespace.
- The acceptance test of the planner gives the same on the functional layer of plone.app.testing, a smaller site: an edit writes 25 records of the catalog, an add 144, and the second of two saves from one snapshot conflicts on an `OIBTree` of the catalog.

### 4. How the queue of CMFCore processes a transaction

- `CatalogTool.indexObject`, `reindexObject` and `unindexObject` call `getQueue().index`, `.reindex` or `.unindex` (`Products/CMFCore/CatalogTool.py:295-335`), unless `CATALOG_OPTIMIZATION_DISABLED` is set. Plone's `CatalogTool.indexObject` calls `reindexObject` (`Products/CMFPlone/CatalogTool.py:303-309`), so in Plone an index is a reindex with all indexes. `reindexObject` with no index calls `notifyModified()` of the object first, in the transaction of the caller.
- The queue is local to the thread (`IndexQueue(local)`). Its first operation in a transaction joins a data manager (`QueueTM`) and adds a hook before the commit (`indexing.py:137-144`, `:300-305`). An unindex wraps the object in a `PathProxy` that keeps its path, so that the unindex of a moved object removes its old path (`indexing.py:103-127`).
- The processing (`IndexQueue.process`, `indexing.py:212-235`) first coalesces the operations of each object, keyed by the object and its path: an index then an unindex cancel out, the indexes are united (an empty list means all), the metadata flag is kept when any operation asks for it. Then it sorts the unindexes first and calls `index`, `reindex` or `unindex` of each `IIndexQueueProcessor` utility of the site manager. The only such utility of a Plone site is `portal-catalog`, a `PortalCatalogProcessor` registered in the global registry (`Products/CMFCore/content.zcml:44-47`). It calls `catalog._indexObject`, `_reindexObject` or `_unindexObject` (`indexing.py:41-57`), which write the catalog.
- The hook before the commit processes the queue (`indexing.py:316-318`). The search methods process it too, before they search: `searchResults`, `unrestrictedSearchResults`, `search`, `__call__` and `getCounter` of the catalog tool, and the UID lookups of plone.app.uuid. So a search sees the changes of its own transaction. The writes always land in the transaction of the save.
- A savepoint keeps a copy of the queue; an abort clears it. `processQueue()` processes the queue of the thread at once.
- Plone's `catalog_object` and `uncatalog_object` add 1 to the `Length` `_counter` of the catalog tool (`Products/CMFPlone/CatalogTool.py:312-345`), so every save writes it.

### 5. How each change queues its operations

From the probed requests (`edit` first):

- **An edit** (the edit form, a REST `PATCH`): `ObjectModifiedEvent`, then `reindexOnModify` of plone.dexterity: one reindex of the object, all indexes.
- **An add** (the add form, a REST `POST`): four reindexes: `review_state` (the workflow at the creation), the object in full (`_setObject` of BTreeFolder2, through `indexObject`), `cmf_uid` (CMFUid), and the container in full (`reindexOnModify` of the container, for `IContainerModifiedEvent`). The workflow then calls `reindexObjectSecurity`, which searches the catalog (section 7).
- **A workflow transition**: a reindex of all indexes from `editMetadata` of `content_status_modify` (the effective date), a reindex of the workflow variables (`review_state`), and `reindexObjectSecurity`: a reindex of `allowedRolesAndUsers`, without metadata, of each object of the subtree that the catalog lists. For the section of 156 objects: 157 reindexes.
- **A rename**: an unindex of the old path, a reindex of the new path in full, a reindex of `getObjPositionInParent` (plone.folder), a reindex of the container, and the reindex of the modified object. The events reach each object below a moved folder (`dispatchToSublocations`), so each of them gets an unindex of its old path and an index of its new path.
- **A move** (cut and paste): an unindex of the old path, a reindex of the new path in full, and a reindex of both containers.
- **A delete**: an unindex of the object, and of each object below it, and a reindex of the container.
- **A later reindex**: a call of `reindexObject(idxs)` queues a reindex of those indexes, and of the metadata unless it says otherwise.

So the operations that Plone queues are reindexes, with a list of indexes or all of them, and unindexes; an index of CMFCore comes from code outside Plone. Each one names an object at a path, and a move names the old path in its unindex.

### 6. What reads the catalog after a save: the window

The page after each probed save, and the other pages of the probe:

- **Every page of the classic UI** searches the catalog three or four times: the global sections (`topLevelTabs`, then the navigation tree of the viewlet), the navigation portlet (`buildFolderTree`, and three UID lookups for its root), and the events portlet. The title of a new or renamed item in the navigation comes from the catalog.
- **The redirect after an edit or an add** goes to the object (`nextURL` of the edit form is `absolute_url()` of the context); its view renders the object itself. The breadcrumbs read the objects (`PhysicalNavigationBreadcrumbs`), not the catalog.
- **A folder view** lists its items from the catalog (`plone.app.contentlisting`), so a new item is missing and a deleted or moved item stays until its indexing. The folder contents reads its items from the vocabulary `plone.app.vocabularies.Catalog`.
- **plone.restapi**: a `GET` of a folder lists its `items` from the catalog (`serializer/dxcontent.py:181-182`); `@search` searches it; a `GET` of an item serializes the object itself. `POST` and `PATCH` answer from the object.
- **The searches**: `@@search`, the collections, `uuidToObject`, which `resolveuid` links, the related items and the relation widgets use.

So the window shows to the user who saved on the next page: in the navigation, and in the folder views after an add, a rename, a move or a delete. The object itself shows the save at once.

### 7. What reads the catalog inside a save

The probe records the searches that a request makes after it queued an operation:

- **`reindexObjectSecurity`** after the workflow of an add and of each transition (`Products/CMFCore/WorkflowTool.py:602`, `CMFCatalogAware.py:100-131`): it processes the queue, then searches the path of the object for its subtree. For a new object the stock catalog then holds the object, and its reindex of security writes it again. With queued indexing the search finds no new object, and the index of that object computes its security anyway.
- **plone.app.discussion** for a moved object (`comment.py:316-342`): it searches the comments at the old path and calls `uncatalog_object` for each, directly, then reindexes each comment through the queue. The comments at the old path are in the catalog already, so the search finds them; the direct `uncatalog_object` writes the catalog in the save, for an object with comments only.
- No other code of the probed saves calls the catalog API directly. Outside the saves: `clearFindAndRebuild`, `reindexIndex`, the import step of `catalog.xml`, upgrade steps and scripts.

### 8. Facts for an indexer

- **The snapshot.** A connection reads at `lastTransaction()` of its storage, polled at the start of its transaction (`ZODB/mvccadapter.py:143-159`). `ZEO.ClientStorage.sync` does nothing (`ZEO/ClientStorage.py:417-419`): the client has a commit only when its invalidations arrive. A read of a KV namespace goes to the server and gives the last committed value (ADR 0043). So a reader of the queue can see a key whose commit its connection does not see yet. The generation row of the durable namespace is written in the backend write of the key, so it bounds the commits of a scan.
- **Two writers of one bucket key.** The resolver of BTrees refuses a key that both transactions changed, also to the same value (`BTrees/_base.py:521-530`), and two inserts or two deletes of one key (`:560-576`). Two indexers that apply one operation change the same keys (the entry of the document in `_unindex` and `_docwords`, its path in `uids`), so the second vote fails. Tier 1 gives the record of that resolver, so it refuses them too.
- **The UID index** keeps one document for each UID: an insert for another document logs an error and does nothing (`Products/PluginIndexes/UUIDIndex/UUIDIndex.py:87-100`). So the removal of the old path of a moved object must come before the index of its new path. CMFCore sorts the unindexes first for this reason.
- **The publisher** begins the transaction of a request, then notifies `PubStart`, before the traversal (`ZPublisher/WSGIPublisher.py:172-186`). A subscriber of `PubStart` runs before any read of the request, and a new `begin()` there gives the request a new snapshot.
- The test layers of plone.testing do not send `IDatabaseOpenedWithRoot`.

## Consequences for the card

- Each catalog write of a save goes through one utility, `portal-catalog`. A processor of `zoe.plone` in its place takes all of them out of the save, except the direct call of plone.app.discussion for the comments of a moved object.
- The operations name paths. An indexer that resolves each path against the newest state gives the same catalog in any order of the paths, with one exception, the UID index: removals first.
- The indexer must wait for its storage to see the generation of the queue before it reads the objects of a batch.
- Every page reads the catalog for its navigation, so the next page of the user who saved shows the window. The redirect after a save is the first such page: it needs the wait of the same user, or it shows the old navigation and folder lists.
- The two readers inside a save stay correct without the operations of their own transaction.
- The verification of B5 reads the catalog after the run, and the folder views of the scenarios read it during the run: the harness must wait for an empty queue before the verification. The redirect after a save of `edit` and `create-spread` renders the navigation, so the wait of section 6 of ADR 0047 shows in the latency of each save.
- After queued indexing, the conflicts left in these scenarios would be on the storage of CMFEditions in `create-spread` (70 of 806 here) and on the containers of creations. Step 4 measures them, with the gate of #440 through B6.

## Not measured

- The time of the catalog part of a save: the probe did not time it. Step 4 measures the delay of the indexer.
- A quiet machine: the load average of the runs went from 4.7 to 31.7 on 12 cores. More repetitions on a quiet machine, and Linux.
- The posting-dict rule on, early locks, the merge policy: the stack of the baseline has none of them.
- The medium fixture, and a site with comments, multilingual content, content rules, Volto, or add-ons with indexers of their own.
- The folder contents data request of the probe: it answered without a search of the catalog, so its query was not the one of the pattern; the reading of the code stands for it.
- B6 on the stores of these runs: step 4 replays a run that keeps its store.
