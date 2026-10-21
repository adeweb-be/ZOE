# 0047. Queued indexing: a save writes its catalog operations to a durable namespace, and one indexer applies them

Status: accepted. It adds an opt-in backend and a public Python API, `zoe.plone.indexing`, to `zoe.plone`. It changes no on-disk format, no wire protocol and no code of the core.

## Context

Without this backend, `zoe-native` raises 46 % of the `ConflictError`s of `zeo` on the B5 log; the gate of phase 4 allows 10 %. The facts of the spike (`docs/spikes/queued-indexing.md`):

- The processor `portal-catalog` of the indexing queue of CMFCore writes the catalog in the transaction of the save: 85 catalog objects for an edit of the B5 fixture, 163 for a creation.
- 753 of the 793 `ConflictError`s of `edit` and 706 of the 806 of `create-spread` are on catalog buckets: the text indexes, `sortable_title`, `path`, `UID`. No merge of one record resolves them: the word count of an index lives in another object (ADR 0042).

The durable namespace of ADR 0043 commits or aborts with the transaction and takes no lock.

## Decision

The specification holds the mechanics of each part: "Server mode", "`zoe.plone` and queued indexing". Each section below keeps the rules that the specification does not state.

### 1. The option

`indexing` is an opt-in backend of `zoe.plone`, as `locking` is: `ZOE_PLONE_BACKENDS=indexing` (or `locking,indexing`) or `zoe.plone:indexing.zcml`, which call `install()`. `install()` registers the processor of `zoe.plone` as the utility `portal-catalog` of `IPortalCatalogQueueProcessor`, keeps the stock one and starts no thread. A second call of `install()` or `uninstall()` does nothing. Every instance of a site needs the option: an instance without it writes the catalog in the save, which converges (section 3) but conflicts with the indexer.

### 2. The queue processor

The processor writes one key for each operation in `zoe.plone.indexing:<oid of the catalog, 16 hex digits>`, and no catalog object.

- The key: the time in microseconds (8 bytes, big-endian), 8 random bytes that `begin()` draws once for each processing, the operation number (4 bytes, big-endian). No two writes share a key.
- The value: JSON `{"op": "index" | "reindex" | "unindex", "path": ..., "idxs": [...], "metadata": bool}`. An empty `idxs` means all indexes. An unindex has the path that `PathProxy` kept. A reader of JSON runs no code.
- A retry of Zope writes new keys. The keys count toward `max-transaction-size`: a folder transition writes 159 keys of about 150 bytes.
- A search in the save calls `processQueue()`, which sends the operations to the namespace (section 5).

### 3. The indexer

- `Indexer` uses its own connection and transaction manager, the system user, a `Testing.makerequest` request and the site as local site. It closes the connection when `run()` ends.
- Coalescing keeps one entry for each path: the union of the indexes, or all indexes when one operation is an index, an unindex or a reindex of all. A path resolves to an object that has `reindexObject`.
- The wait for the generation fails as a `TransientError` after 10 s. On a `DemoStorage` the indexer does not wait.
- A `TransientError`: abort, pause 0.1, 0.2, 0.4, then 0.8 s; after 5 tries `run()` raises it, and the keys stay.
- Another `Exception` on a path: abort, add 1 to `failures` of its keys in a transaction of its own, and apply the batch without them. A key with 5 failures moves to `zoe.plone.indexing.failed:<oid>` with the error, and an ERROR event.
- A `BaseException` that is not an `Exception`: abort and raise at once.
- **Convergence.** The catalog equals a reindex from scratch when each change of an indexed value queued an operation, as the stock catalog assumes.
- **The security subtree.** `reindexObjectSecurity` in a save misses the objects still in the queue. The indexer adds the catalog subtree of each security reindex, so a new document in a folder made private loses `Anonymous`.
- Two indexers on one key write the same buckets, so one vote fails and retries.
- The batch size 100, the scan every 0.2 s, the lease of 30 s and the wait of 2 s are module constants, not keys of `zope.conf`.

### 4. The thread, and one indexer for each site

- A thread takes a free lease: it writes its token in a commit with no record, reads the key again, and goes on only when it holds its token. It reads the lease before each batch, so two holders overlap by one batch at most, which costs conflicts, not a wrong catalog. After a crash, another process takes the lease within 30 s.
- A thread reads only the mount points of its own database (`listMountPaths()` of the `dbtab`), so a search does not grow with the number of databases.
- The thread finds the catalog as an attribute of the site, not as a local utility.

### 5. What stays synchronous

- Direct calls of the catalog API write the catalog in their own transaction: `catalog_object`, `uncatalog_object`, `reindexIndex`, `refreshCatalog`, `clearFindAndRebuild`, `manage_catalogRebuild`, the import of `catalog.xml`, upgrade steps, the plone.app.discussion handler of a move. A conflict with a batch makes the indexer retry.
- A read in a save does not see the operations of that save. `reindexObjectSecurity` of a new object finds no subtree; the index of the object computes its security anyway.
- Nothing else is synchronous. A synchronous index, such as `path` or `UID`, brings the conflicts back.

### 6. The window, and the next request of the same user

The window of a save runs from its commit to the commit of its batch. Meanwhile the catalog shows the site before the save, and a page that reads the object shows the save. The cookie `zoe.indexing` makes the next request of the same browser wait for the batch, at most 2 s. It holds no user data and no secret, and its path is the site. A client without cookies and every other user read within the window.

**The bound.** In B5 `edit` and `create-spread` at 16 users on `zoe-native-qi`, the delay of a key (section 11) is at most 1 s at p95 and 5 s for every key, and the queue is empty at most 10 s after the last save.

### 7. plone.protect

A `GET` whose only write is a key passes the CSRF check: an operation only makes the catalog equal to the object. A reindex of all indexes calls `notifyModified()`, which writes the object, so the check sees it. The indexer runs outside a request.

### 8. A site without namespaces

A storage without KV namespaces keeps the stock processor, and the thread skips its site. The option on a ZEO site changes nothing and refuses nothing.

### 9. Test layers on a DemoStorage

On a `DemoStorage` over a ZOE storage, the keys go to the overlay of `zoe.kv`. No thread runs in a test layer, so a test calls `Indexer(portal).run()`; a crash test raises a `BaseException` from `after_batch`. Plone's own suites search the catalog in the transaction of a change, so they run without the option: `just plone zoe backends` keeps `ZOE_PLONE_BACKENDS=locking`.

### 10. The way back, copies and undo

To switch off, empty each queue, then remove the option from every instance. An instance without the option runs no indexer, so a key left in the queue stays out of its catalog. The specification gives the rules of copies and undo.

### 11. The status

`@@zoe-indexing` gives the state of the queue as JSON. The delay of a key runs from the time in the key to the commit of its batch. The B5 harness waits on it for an empty queue and records the delays.

## Consequences

- Two saves of different objects do not conflict on the catalog. A save still writes the content object, its versions, the container of a creation or a delete, the intid and relation catalogs, and the keys.
- The catalog work moves to one indexer for each site, at the same cost. An edit commits 85 fewer records of its 111.
- An add-on indexer that raises does not abort the save.
- The catalog lags the content by the window.
- Two sites in one Zope root have two queues and two leases.
- The writes-on-GET report (F32) counts neither the keys nor the commits of the indexer.
- The acceptance tests: `python/zoedb/src/zoe/plone/tests/test_queued_indexing.py` and `test_queued_indexing_mounted.py`.

## Alternatives

- **More merges in tier 1.** No exact word count (ADR 0042).
- **A queue in the object graph** (an `IOTreeSet` of oids). The conflicts move to the queue.
- **Index in an after-commit hook.** Same conflicts, and the response waits.
- **Keys from a store sequence.** A server call for each processing; no sequence on a `DemoStorage`.
- **One key for each transaction.** A large move gives one large value.
- **The UID as the key.** The catalog is keyed by path; a delete leaves no object.
- **A synchronous mode for searches in a save.** Most saves search, so they write the catalog again.
- **An indexer in its own process.** Its own deployment and the eggs of the site.
- **No wait for the user who saved.** That user sees the window.
- **A wait in every request of a logged-in user.** Requests that saved nothing pay it.
- **Raise on a site without namespaces.** The option then stops a ZEO site.
- **Retry a failed operation forever.** It stays in each batch.
- **Drop an operation after one failure.** A passing error loses a change.
- **A bound set from the measured numbers.** It accepts any result.
- **The constants as keys of `zope.conf`.** Four more public keys.
- **Queue the objects of an undone transaction.** Code for a rare act that a reindex covers.
- **No status view.** The log gives no queue size to wait on.
- **One operation for a security subtree in `reindexObjectSecurity`.** It changes CMFCore.
- **Each thread searches every mount point.** The work grows with the databases.
- **One thread searches every database.** Each mounted site depends on it.
- **No search by the thread of a mounted database.** A restart leaves its keys until a save.
- **Solr or another search engine.** Another service.
- **The catalog projection of phase 10.** It needs the mirror of phase 6.
