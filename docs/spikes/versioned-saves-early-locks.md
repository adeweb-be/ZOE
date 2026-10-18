# Spike: concurrent versioned saves under early write-intent locks

Card ZOE-212, after the rejection of ADR 0051; F6, F7 and the B5 criterion of ZOE-205 "the contention on two fields of one Document commits at the first attempt". The first spike of the card is `concurrent-versioned-saves.md`. The decision that follows is ADR 0053 (proposed).

**Verdict.** Early locks on the classes of the history, as ADR 0045 builds them, do not make the second save read the history fresh. Of the 11 records of the history that a save reads, 6 are BTrees, which tier 1 merges, so ADR 0045 refuses them as hot classes; the 5 others refer to these BTrees, so they are not leaf records. With the two roots of the history hot (`ShadowHistory` and `ZopeVersionHistory`), the second save reads the roots at the grant and their BTrees at its snapshot, and fails with `KeyError: 1` in `ShadowHistory.retrieve`. That is not a `ConflictError`, so Zope does not retry it. A prototype that also loads every record that a hot root reaches at the grant (a "covered subtree", a change of the leaf rule) commits two and eight saves at the first attempt with the right version numbers. But each version then holds the Document of the snapshot of its own save: it misses the committed changes of the saves before it. The Document is loaded at the traversal, before the save reads any record of the history, so no lock on the history can come before that read. One mechanism gives the history of the sequential saves at the first attempt: the edit request takes the intent lock of the object that it edits after the traversal, before its first change, and then moves the snapshot of its connection to the grant. A prototype of it gives the sequential reference exactly for two and eight concurrent saves, with and without a merge policy. The acceptance test of the planner cannot run with early locks unchanged (finding 1).

## Question

From the decision of the human that rejected ADR 0051: keep the edit and its version in one transaction, as stock Plone does, and declare the classes of the history records of CMFEditions and ZopeVersionControl in `early-lock-classes` (ADR 0045), so that a second concurrent save waits for the first to commit and then reads the history fresh.

- Which records does `portal_repository.save` read and write, with their classes as ZODB pickles them? Which must load under the lock? Is one loaded before the save, so that the lock comes too late? Which other code loads these classes and would wait?
- With these classes hot, do two and eight concurrent saves of one Document commit at the first attempt, with versions N+1, N+2, ... in commit order? What does each version hold? How does that compare with the history of stock Plone after a retry?
- Lock order, the lock timeout under contention, deadlocks, and what a stock ZEO client or a site without `early-locks` gets.
- Can the profile of `zoe.plone` set this, or must the operator? Does the lock work through `zoe.zeo.ClientStorage` in native mode and through the in-process `zoe.Storage`?

## Method

- **Code.** `card/ZOE-212` at `ec36096f`, the Plone instance of the scenario `zoe`, built in the worktree: CPython 3.13.15, Plone 6.1, Products.CMFEditions 4.0.6, Products.ZopeVersionControl 4.1, plone.app.versioningbehavior 2.0.5, ZODB 6.2. The file and line numbers below are in these eggs.
- **The acceptance test.** `just plone zoe backends -t test_versioned_saves`, once, unchanged: `Ran 4 tests with 2 failures`, as on the card (`ConflictError` on `_versions`, an `OOBTree`).
- **The prototype.** A test module outside the repository (`~/.cache/zoe-build/ZOE-212/spike/test_spike_locks.py`), on the layer and with the helpers of the acceptance test. It runs with the `bin/test` of the scenario, as the recipe does. Each case copies the site to a new `zoe.Storage` with `early_locks=True` and a list of hot classes, then runs N saves, each in a thread with a connection of its own, from one snapshot. Unlike the acceptance test, a save commits as soon as it is done, as a request does. A case runs "ordered" (save k starts when save k−1 holds its locks, and that save holds them 0.3 s before its commit) or "free" (all saves start together). A save sets one field of the Document (`title`, `description`, `rights`, `language`, `subject`, `contributors`, `exclude_from_nav`, `location`) and sends `ObjectModifiedEvent`. The reference runs the same saves one after the other on another copy, in the commit order of the case. The state compared is the one of the acceptance test: the title, description and `version_id` of the Document, and each version with its `version_id`, title and description.
- **The load trace.** A wrapper of `Connection.setstate` records each load of one save, with its class and the frames of CMFEditions and ZopeVersionControl that ask for it; the iterator of the store gives the records that the save writes.
- **The machine.** Linux 7.0, 22 cores. Other worktrees ran at the same time. No result below depends on time, except the waits, which are in a model of 0.3 s per save.

## Findings

### 1. The acceptance test cannot run with early locks unchanged

- `open_db` of the test opens `zoe.Storage(path)` with no option. The environment twins `ZOE_EARLY_LOCKS` and `ZOE_EARLY_LOCK_CLASSES` reach only `<zoe>` and `zoe://` (`zoe/config.py`), not the constructor. So the test cannot take an early lock without a change of the test.
- `concurrent_saves` lets the threads commit only after both saves are done (a barrier between the save and the commit). Any lock that a save takes and holds to its commit stops the other save before the barrier. A run of the same helper on a store with the history classes hot ends after 60 s: one save gets `ConflictError` from the lock timeout (10 s), the other `BrokenBarrierError`.
- The mechanism that passes (section 6) takes its lock at the end of the traversal of a request. The test has no request and no traversal: it calls the save on an object of the root.
- With the lock of section 6, two saves of one field both commit, the second one last, as in the sequential reference. The guard `test_two_saves_of_one_field_raise_conflict_error` expects `ConflictError`, and the guard without a merge policy does too.

The prototype below does not change the module of the test. It reuses its layer, `setUp`, `in_site`, `state` and the copy of the records.

### 2. What one save reads and writes

The trace of one save of the title (cold cache) gives 121 loads. Traversal loads the site, the bucket of its folder, and the Document. The subscriber `create_version_on_save` calls `isUpToDate`, then `save`. The records of the history of the Document, in the order of the loads, with their class as ZODB pickles it (W: the save writes it):

| record | class | loaded by |
| --- | --- | --- |
| the Document (W) | `plone.app.contenttypes.content.Document` | the traversal |
| its `cmf_uid` | `Products.CMFUid.UniqueIdAnnotationTool.UniqueIdAnnotation` | `isUpToDate` → `dereference` |
| `_shadowStorage` | `Products.CMFEditions.ZVCStorageTool.ShadowStorage` (one for the site) | `isUpToDate` → `isRegistered` |
| its `_storage` | `BTrees.OOBTree.OOBTree` (one for the site) | the same |
| the shadow history (W) | `Products.CMFEditions.ZVCStorageTool.ShadowHistory` | `isUpToDate` → `retrieve` |
| `_full` (W) | `BTrees.IOBTree.IOBTree` | the same |
| the `VersionInfo` of the last version | `Products.ZopeVersionControl.Utility.VersionInfo` | the same |
| the repository of ZopeVersionControl | `Products.ZopeVersionControl.ZopeRepository.ZopeRepository` (one for the site) and its `_histories`, an `OOBTree` | `getVersionOfResource` |
| the version history | `Products.ZopeVersionControl.ZopeVersionHistory.ZopeVersionHistory` | the same |
| `_versions` (W) | `BTrees.OOBTree.OOBTree` | `hasVersionId` |
| the last version (W) | `Products.ZopeVersionControl.ZopeVersion.ZopeVersion` | `getVersionOfResource` |
| its wrapper, data and copy | `ZVCAwareWrapper`, `ObjectData`, a `Document` and its `UniqueIdAnnotation` | `copyState` (the copy of the last version, read only) |
| the event log | `Products.ZopeVersionControl.EventLog.EventLog` | `getLogEntries` |
| its `_data` (W) | `BTrees.IOBTree.IOBTree` | the same |
| the last `LogEntry` | `Products.ZopeVersionControl.EventLog.LogEntry` | `_retrieveZVCLogEntry` |
| `_branches` | `BTrees.OOBTree.OOBTree` | `checkinResource` → `isLatestVersion` |
| the branch | `Products.ZopeVersionControl.VersionHistory.BranchInfo` | the same |
| `m_order` (W) | `BTrees.IOBTree.IOBTree` | `latest` |
| `m_date` (W) | `BTrees.IIBTree.IIBTree` | `BranchInfo.append` |

The save writes these 8 records and 8 new ones (the 16 records of the first spike). Between the loads of the history, the save reads the tools and modifiers of CMFEditions, which it does not write.

- **What must load under the lock.** Every record of the history that the save reads: `ShadowHistory` and `_full`, `ZopeVersionHistory`, `_versions`, `_branches`, `BranchInfo`, `m_order`, `m_date`, the last `ZopeVersion`, `EventLog` and `_data`. ZopeVersionControl takes the next version id from `len(m_order) + 1` (`VersionHistory.py:101`), CMFEditions from `_available` of the `ShadowHistory`, and `retrieve` of the last version reads `_full` at the index that `_available` gives.
- **What ADR 0045 allows.** 6 of these 11 records are BTrees of tier 1 (`_full`, `_versions`, `_branches`, `m_order`, `m_date`, `_data`): a hot class of tier 1 fails the open with `ValueError`. The other 5 (`ShadowHistory`, `ZopeVersionHistory`, `BranchInfo`, `EventLog`, `ZopeVersion`) refer to these BTrees, so they are not leaf records, which section 1 of ADR 0045 asks of a hot class. The class name matters: ZODB pickles `ZopeVersionHistory`, a subclass of `VersionHistory`, and a subclass is hot only when the list names it.
- **Loads before the save.** No record of the history loads at the traversal. The first one, the `ShadowHistory`, loads in `isUpToDate`, before `save`, in the same transaction. But the Document loads at the traversal, before the first change, and the version copies it from memory (`ArchivistTool.prepare` clones the object by pickle). So the version holds the Document of the snapshot of its transaction, whatever lock the history takes.
- **A warm cache loads nothing.** A second save on the same connection loads 0 records: the cache holds the Document and the whole history. ADR 0045 turns only the hot objects into ghosts at the end of a transaction. A non-hot record in the cache stays at the state that the connection last saw, and ZODB invalidates it only at the next transaction.
- **Other code that loads these classes and would wait** with them hot: the history viewlet and `@@historyview` (`plone.app.layout`, `ContentHistoryViewlet`: `getHistoryMetadata`, `isUpToDate` for each version), the diff view `@@history` and `versions_history_form` of CMFEditions, the `@history` service of plone.restapi and its serializer with `version=`, the revert, the purge of the history when an object is deleted (`CMFEditions/subscriber.py`) and by `KeepLastNVersionsTool` inside a save, the check-in of plone.app.iterate, the versioning action of plone.app.contentrules, the `${change_*}` substitutions of plone.stringinterp, and the export of plone.exportimport. Each of these readers would take an exclusive lock and hold it to the end of its transaction, so two readers of one history wait for each other too.

### 3. The history classes hot, as ADR 0045 builds them

Two saves (title, description), ordered, hot classes `ShadowHistory` and `ZopeVersionHistory`:

- The second save waits 0.3 s for the lock of the `ShadowHistory`, then reads it at the grant (`nextVersionId` 2, `_available` `[0, 1]`) and its `_full` at its snapshot (no key 1). `ZVCStorageTool.save` (`:227`) asks the last version: `ShadowHistory.retrieve` (`:746`) raises `KeyError: 1`. The request fails with that error. Zope retries only a `ConflictError`, so the user sees an error where stock Plone gives a retry. A `version_id` strategy `max` in the policy changes nothing: the error comes before the vote.
- The same with the Document hot too: `KeyError: 1` on the save that comes second.

This is the case that section 1 of ADR 0045 names: a hot record read at the grant refers to a child that the transaction reads at its snapshot.

### 4. A covered subtree: the records that a hot record reaches load at the grant too

The prototype changes `setstate` once more. When a hot record loads, every record that its pickle refers to is "covered" for the rest of the transaction; a covered record loads its current record, as the hot one does, and refers on to its own children. A covered record that is in the cache with no change is turned into a ghost first. The rule is safe only when every writer of a covered record reaches it through its hot root, and so holds the lock: that holds for the records of one history. Results, hot classes `ShadowHistory` and `ZopeVersionHistory`:

| case | errors | lock calls, longest wait | versions (title, description of each) | equal to the sequential reference |
| --- | --- | --- | --- | --- |
| two saves, ordered, policy as the acceptance test | the second: `ConflictError` on the Document | 6, 0.30 s | — | — |
| two saves, ordered, `version_id` `max` | none, both at the first attempt | 6, 0.30 s | 0 (old, old), 1 (T, old), 2 (old, D) | no: version 2 misses T |
| eight saves, ordered, `max` | none | 18, 0.33 s | 1 (T, old), 2 (old, D), 3 to 8 (old, old) | no |
| eight saves, free, `max` | none | 18, 0.11 s | each version holds only its own change | no |
| two saves of the title, ordered, `max` | the second: `ConflictError` on the Document (`title` is strict) | 6, 0.31 s | — | yes (the first save only) |

- With the policy of the acceptance test, the Document refuses: the first save sets `version_id` 1 and the second, which reads the history at the grant, sets 2, and the field is `strict`. With `max`, the Document merges to the number of the later save, which is the last version.
- The version numbers are right: N+1, N+2, ... in commit order, and the Document holds every change.
- But each version holds the Document of its own snapshot with its own change. In the eight saves, version 8 holds the old title and the old description, and the Document holds T and D. `isUpToDate` compares the modification dates, and the merged Document has the date of the last save, so the site takes the last version as up to date. A revert to it gives back the old title and description. Stock Plone gives the sequential history after a retry (section 7).
- With all 5 non-BTree classes hot (`BranchInfo`, `EventLog` and `ZopeVersion` too), the loads come out of oid order in this site: the last `ZopeVersion` (…bd) loads before `EventLog` (…b6) and `BranchInfo` (…ba). Each such load gives a `LockOrder` and a relock (ADR 0045, section 4). Two ordered saves: 5 relocks, no error. Eight free saves: 14 relocks, and 6 of the 8 saves get `ConflictError`, because a waiter takes a released lock and commits before the relock.

### 5. The Document hot too

With `plone.app.contenttypes.content.Document` hot as well, and the covered subtree, eight free saves all commit at the first attempt, and the state equals the sequential reference in commit order (44 lock calls, the longest wait 0.12 s). The Document then loads under its lock at the traversal, and the rest of the transaction reads the Document and its history at the grant. But the hot class is then the class of every Document: each request that loads a Document (a view, a listing that wakes objects, the indexer of queued indexing, a serializer) takes an exclusive lock and holds it to its end, so readers wait for writers and for each other. ADR 0045 asks of a hot class that its readers nearly always write it; a Document is read far more often than written. A Document also refers to other records (its `cmf_uid`, annotations, blobs in other types), so this case needs the covered subtree too. Two ordered saves of this case wait for each other through the order of the test (the same deadlock as finding 1, after 10 s): any wait between two transactions outside the store deadlocks with a lock.

### 6. The lock of the edited object after the traversal, and the snapshot at the grant

The prototype adds one call at the end of the traversal of each save, before its first change:

1. take the intent lock of the oid of the Document, with the owner of the connection (ADR 0045, section 3), and keep the serial that the grant gives;
2. when the transaction holds no change and no savepoint, run the step of ZODB that begins a transaction (`Connection.newTransaction` of ZODB, not the patched one, which would release the lock): the connection syncs, polls the invalidations, moves its snapshot past the last commit, and turns the objects that other transactions changed into ghosts;
3. go on with the save.

No class is hot (the prototype names one class that no record has, which switches on the patch of ADR 0045). The second save waits for the first to commit, then reads the Document and its whole history as they are after that commit: the save runs as the second save of the sequential reference does.

| case | errors | lock calls, longest wait | objects that the move turned into ghosts | equal to the sequential reference |
| --- | --- | --- | --- | --- |
| two saves, free, policy of the acceptance test | none, both at the first attempt | 2, 0.07 s | 0, then 16 | yes |
| eight saves, free | none | 8, 0.21 s | 0, 16, 24, ..., 64 | yes |
| two saves of the title, free | none: the later save wins | 2, 0.04 s | 0, then 16 | yes |
| eight saves, free, no merge policy | none | 8, 0.25 s | — | yes |

- The versions are N+1, ..., N+8 in commit order, and version k holds the changes of saves 1 to k: the history of the sequential saves.
- The merge policy plays no part: no vote meets a conflict, because each save starts from the commit before it. Two saves of one field give what they give one after the other: the later value wins, without `ConflictError`. Stock Plone gives the same after a retry.
- The 16 objects that a move turns into ghosts are the 16 records of the save before it; the move reloads only those.
- The lock is on the object that the request edits, not on a class: a reader never takes it, and edits of different objects do not wait for each other.
- In the prototype the move works because the in-process store gives the commit of the holder in `lastTransaction()` before the grant. A client of `zoe serve` gets the invalidations of that commit as a message of its own, which can come after the grant: the move must then wait until the last transaction of the client reaches the serial of the grant (not measured).

### 7. Stock Plone after a retry

Without early locks, two saves from one snapshot: the second gets `ConflictError` on `_versions` (an `OOBTree`), and its retry, from a new snapshot, commits. The history then equals the sequential reference: version 2 holds both changes. Section 4 gives a coarser history than this; section 6 gives the same at the first attempt.

### 8. Locks, timeouts, deadlocks and clients

- **Lock order.** With one lock for each edit (section 6), a transaction takes the lock of its object first, at the traversal. A hot class that loads later, with a lower oid, gives a relock (ADR 0045, section 4); under contention on the edited object that relock can end in `ConflictError`, and the retry hint then takes both in oid order. Within the history, the order of the loads is not the order of the oids (section 4), so a list of history classes relocks on each save.
- **The lock timeout.** A save waits for the commit of the saves before it, up to `lock-timeout` (10 s by default), then gets `ConflictError`, and Zope retries it. In the ordered model each save holds the lock 0.3 s, and eight saves take 2.6 s in all. Eight saves that start together and hold the lock for the length of their save wait at most 0.21 s (section 6). In a published request, the hold is the time from the traversal to the end of the commit. A waiting request holds a thread of Zope, and in native mode a connection of the pool of the client while it holds the lock (ADR 0045, section 3: 64).
- **Deadlocks.** The intent locks avoid deadlocks among themselves: oid order, the relock, and the vote that releases the intent locks above an oid before it waits (ADR 0045, section 4). A wait outside the store, between two transactions that hold locks, deadlocks until `lock-timeout`: the barrier of the acceptance test (finding 1) and the order of the prototype (section 5) do that.
- **A stock ZEO client, or a site without `early-locks`.** The client takes no intent lock. Its save reads at its snapshot and its vote fails with `ConflictError` as today; the retry of Zope gives the sequential history (section 7). On a server where other clients hold intent locks, its vote waits for them as for vote locks, and its serial check refuses a stale write.
- **Who sets it.** `early-locks` and `early-lock-classes` are keys of the storage, in `<zoe>` and `<zoeclient>` of `zope.conf` and in their `ZOE_*` twins, read when the storage opens. The database does not hold them, so the profile of `zoe.plone` cannot set them: the operator must. A strategy for `version_id` (section 4) is in the merge policy, which the profile writes. The lock of section 6 needs a call from `zoe.plone` at the traversal, which the profile can register, and the storage key `early-locks on` from the operator.
- **The clients.** The in-process `zoe.Storage` takes the locks: every case above ran on it. `zoe.zeo.ClientStorage` takes them in native mode (ADR 0045, section 3); with `native false`, `early-locks on` fails the open with `ValueError`. This spike did not run through `zoe serve`.

## Consequences for the card

- Early locks on the history classes do not meet the goal: as ADR 0045 builds them they turn the conflict into a `KeyError`, and with a covered subtree they give versions that miss committed changes.
- The lock of the edited object after the traversal, with the snapshot at the grant, gives the sequential history at the first attempt. ADR 0053 proposes it as variant (b), with the re-checks that its review asks for, beside variant (a), which takes the lock only at the retry, before its snapshot (not run in this spike). Both need code in `zoe.zodb`, outside the scope fence of the card, and a subscriber of `zoe.plone`.
- The acceptance test cannot pass unchanged with any early lock (finding 1). The planner must write a new one, with the approval of the human.

## Not measured

- A published request: the prototype calls the lock after `site[DOCUMENT]`, not from a subscriber of the publisher, and has no form, no plone.restapi and no plone.protect.
- `zoe serve` with `zoe.zeo.ClientStorage` in native mode, and the wait for the invalidations of the holder before the move.
- B5 `contention-fields`: the mechanism of section 6 needs code in the Plone instance of the bench, and a run of the history classes hot would only count the `KeyError`s of section 3.
- A change note, the purge of `KeepLastNVersionsTool`, types with blobs, a versioned object in a new folder, mounted databases.
