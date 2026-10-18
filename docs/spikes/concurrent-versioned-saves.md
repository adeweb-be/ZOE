# Spike: two concurrent saves of one versioned Document

Card ZOE-212, step 1; F6 (tier 2) and the B5 criterion of ZOE-205 "the contention on two fields of one Document commits at the first attempt". The decision that follows is ADR 0051 (proposed).

**Verdict.** No merge at the vote can give what the two saves give one after the other. Each save of a version writes 16 records. 8 of them are records that both saves change, and both saves make the same change: they take the next version number, from the length of the history in their snapshot. The sequential result needs the second save to take the number after the first, in 13 of its records and in 1 record that only the first save writes. It also needs the copy of the second version to hold the change of the first save, but that copy comes from the snapshot of the second save. A merge picks or joins the changes of two records. It cannot compute a save again on a state that the save did not read. A strategy for `version_id` (`max`, `sum`) or a coupled group does not help: the history refuses first, or right after. A ZOE sequence for the version numbers (F29) does not help either: CMFEditions uses the version number as a position, ZopeVersionControl computes its own numbers, and the copy still holds the old title. One mechanism passes the acceptance test of the planner without a change of CMFEditions or ZopeVersionControl: `zoe.plone` saves the version in a transaction of its own after the commit of the edit. A prototype of it, outside the repository, passes the four tests of the acceptance module. It replaces a subscriber of plone.app.versioningbehavior in the component registry, so it needs an ADR and the approval of the human. When the version transactions run in the other order, the history it gives is coarser than the sequential one.

## Question

Step 1 of the card:

- What do CMFEditions and ZopeVersionControl write when one Document is saved twice at the same time: one save changes the title, the other the description, from one snapshot? Every record, field and BTree key, the version numbers, `version_id` of the Document, `_versions`, the shadow history, the history id handler, and the counters.
- Which conflicts occur, in the order of the vote, and what merge would each one need?
- Can ZOE merge the two saves so that the repository holds what the same saves give one after the other (version 1, then version 2, each with its own id, title and description), without a change of CMFEditions or ZopeVersionControl?
- If a merge must renumber one side, can tier 2 do that byte for byte? What can a `zoe.plone` adapter of CMFEditions do instead, for example with version numbers from a sequence of the store (F29)?

## Method

- **Code.** `card/ZOE-212` at `05f12ba` (`main` at `a5c8ebd` and the acceptance test of the planner, `6093f30`). The Plone instance of the scenario `zoe` (`just plone zoe backends`), built in the worktree: CPython 3.13.15, Plone 6.1, Products.CMFEditions 4.0.6, Products.ZopeVersionControl 4.1, plone.app.versioningbehavior 2.0.5, Products.CMFUid 4.2, BTrees 6.3, Zope 5.14.2. The file and line numbers below are in these eggs.
- **The acceptance test.** `just plone zoe backends -t test_versioned_saves`, once, unchanged.
- **The trace.** A test module of the spike, on the layer and with the helpers of the acceptance test, not committed. It copies the site of the layer to a new `zoe.Storage` three times. It runs save A (the title) on the first copy, save B (the description) on the second copy, and A then B on the third copy. So A and B start from one snapshot, as in the concurrent test, and the third copy is the sequential reference. For each transaction, it reads every record through the iterator of the store and its previous record through `loadBefore`, and decodes both with the unpickler of ZODB. On two copies, the new objects of A and B get the same oids, because the store gives the next oids to each copy in the same order. In a concurrent run they get different oids. A fourth copy runs the concurrent saves of the acceptance test with a resolution log (ADR 0042).
- **The resolver of BTrees.** `_p_resolveConflict` of `IOBucket` and `IIBucket` on the states of the trace, on the uv environment of CPython 3.13 (BTrees 6.3).
- **The prototype.** The same module replaces `create_version_on_save` of plone.app.versioningbehavior in the global registry with a handler that makes the same decision and saves the version in an after-commit hook (section 6). It runs the four tests of the acceptance module with that handler, and one more order of the commits.
- **The machine.** Linux 7.0, 22 cores, 31 GiB. Other worktrees ran builds and tests at the same time: the 1-minute load average was 6 to 9. No result of this spike depends on time.

## Findings

### 1. How the acceptance test fails

`Ran 4 tests with 2 failures`. Both tests of the card fail on the second save, with `ConflictError` on a `BTrees.OOBTree.OOBTree`: `_versions` of the `VersionHistory` of the Document. The two guards pass.

### 2. What one save writes

A save sets the field, then sends `ObjectModifiedEvent`. `create_version_on_save` (`plone/app/versioningbehavior/subscribers.py:10`) asks `portal_repository.isUpToDate`, which compares the modification date of the last version with the one of the Document (`ArchivistTool.py:403-411`). The date changed, so it calls `portal_repository.save` (`CopyModifyMergeRepositoryTool.py:294`). Then:

- `ArchivistTool.prepare` (`ArchivistTool.py:235-289`) takes the version number `len(self.queryHistory(obj))`, the length of the history in the snapshot. It clones the Document by pickle and sets `version_id` of the copy to that number. `PreparedObject` reads the history id of the container through `portal_historyidhandler.register(parent)`. The container has its id from its first version, so this reads and writes nothing.
- `_recursiveSave` (`CopyModifyMergeRepositoryTool.py:451-496`) sets `originator` of the metadata to `"<history id>.<version number>.<location>"`.
- `ZVCStorageTool.save` (`ZVCStorageTool.py:216-237`) and `_applyOrCheckin` (`:369-408`) compute `approxSize`, the size of a protocol-1 pickle of the copy (`getSize`, `:75-124`). They wrap the copy, attach a copy of the `VersionInfo` of the last version with `version_id = str(len(zvc_repo.getVersionIds(obj)))` (`_getVcInfo`, `:469-489`), and call `checkinResource` of ZopeVersionControl.
- `Repository.checkinResource` (`Repository.py:233`) calls `VersionHistory.createVersion` (`VersionHistory.py:101`). It takes the version id `'%d' % (len(branch) + 1)`, a str. It adds that id to `next` of the last version and sets `prev` of the new one. `BranchInfo.append` (`VersionHistory.py:256`) inserts the id into `m_order` under `m_order.minKey() - 1` and sets `m_date[minute] =` that key. `_versions[id]` gets the new `ZopeVersion`. `addLogEntry` inserts a `LogEntry` into `_eventLog._data` under `minKey() - 1` (`EventLog.py:36`). Its `message` is a bytes value that holds two pickles: the comment, and the metadata with `HIGHEST_PROTOCOL` (protocol 5, with a frame).
- `ShadowHistory.save` (`ZVCStorageTool.py:706-731`) takes `nextVersionId`, inserts a deep copy of the shadow data under it into `_full`, appends it to `_available`, adds 1 to `nextVersionId` and the size to `_approxSize`. Its comment says that the write conflict of two saves is wanted.
- `copyVersionIdFromClone` sets `version_id` of the Document to the number.

The trace of save A, from a Document with one version (CMFEditions number 0, ZopeVersionControl id `'1'`), gives 16 records. Save B gives the same 16 oids on its copy:

| record | class | save A | save B | sequential B |
| --- | --- | --- | --- | --- |
| the Document | `Document` | title, `modification_date`, `version_id` 0 → 1 | description, `modification_date`, `version_id` 0 → 1 | `version_id` 1 → 2 |
| the shadow history | `ShadowHistory` | `nextVersionId` 1 → 2, `_available` `[0]` → `[0, 1]`, `_approxSize` 1019 → 2050 | the same, `_approxSize` → 2051 | 2 → 3, `[0, 1, 2]`, 2050 → 3094 |
| `_full` | `IOBTree` (one bucket inline) | insert 1, `originator` `'1.1.0'`, `approxSize` 1031 | insert 1, `'1.1.0'`, 1032 | insert 2, `'1.2.0'`, 1044 |
| `_versions` | `OOBTree` (inline) | insert `'2'` | insert `'2'` | insert `'3'` |
| `m_order` | `IOBTree` (inline) | insert 2147483646 → `'2'` | the same | insert 2147483645 → `'3'` |
| `m_date` | `IIBTree` (inline) | change the minute 2147483647 → 2147483646 | the same | 2147483646 → 2147483645 |
| version 1 | `ZopeVersion` | `next` `()` → `('2',)` | the same | no change |
| `_eventLog._data` | `IOBTree` (inline) | insert 2147483646 | insert 2147483646 | insert 2147483645 |
| version 2 | new `ZopeVersion` | `id` `'2'`, `prev` `'1'` | the same | `id` `'3'`, `prev` `'2'` |
| the wrapper | new `ZVCAwareWrapper` | refers to the copy | the same | the same |
| | new `ObjectData` | refers to the copy | the same | the same |
| | new `VersionInfo` of the wrapper | `version_id` `'1'`, status 0 | the same | `'2'` |
| the copy | new `Document` | `version_id` 1, the new title, the old description | `version_id` 1, the old title, the new description | `version_id` 2, both changes |
| | new `UniqueIdAnnotation` of the copy | `_uid` 1 | the same | the same |
| | new `LogEntry` | `version_id` `'2'`, `message` with `'1.1.0'` | the same | `'3'`, `'1.2.0'`, its own size |
| | new `VersionInfo` of the shadow data | `version_id` `'2'`, status 1 | the same | `'3'` |

The sequential reference also writes `next` `()` → `('3',)` of the version that save A created: a record that only save A writes in the concurrent case.

No other record changes. The catalog is out of the save (queued indexing), and `portal_historyidhandler` and `portal_uidgenerator` are not written: the history id exists since the first version.

### 3. The conflicts, in the order of the vote

The vote checks the oids in ascending order. The Document is the lowest: it was created before its history. The resolution log of the concurrent run gives three lines, then the vote fails:

| oid | record | what the vote does today | what a merge would need |
| --- | --- | --- | --- |
| 1 | the Document | tier 2 merges: `title` and `description` are disjoint, `modification_date` takes `max`, and both sides set `version_id` to 1, so they agree | `version_id` 2: the number of the second save is one more |
| 2 | `ShadowHistory` | no entry in the policy and no `_p_resolveConflict`; the conflict goes to the client and fails | `nextVersionId` 3 (`sum` gives it), `_available` `[0, 1, 2]` (`append` gives `[0, 1, 1]`), `_approxSize` with the size of a copy that holds both changes |
| 3 | `_full` | tier 1 gives no answer ("an opaque node": the values are dicts); the resolver of BTrees refuses two inserts of key 1 | the entry of B under key 2, with `originator` `'1.2.0'` and a new size |
| 4 | `_versions` | tier 1 refuses: "Conflicting inserts" (code 6). The vote ends here | the version of B under `'3'` |
| 5 | `m_order` | (not reached) the resolver of BTrees refuses two inserts of one key, also with the same value | B under 2147483645, with the value `'3'` |
| 6 | `m_date` | (not reached) the resolver refuses two changes of one key, also to the same value | the value 2147483645 |
| 7 | version 1 | (not reached) no entry and no resolver | the write of B drops; version 2 of A gets `next` `('3',)` |
| 8 | `_eventLog._data` | (not reached) two inserts of one key | B under 2147483645 |

The new records of B do not conflict: in a concurrent run they have their own oids. But they hold the number of B and the snapshot of B: version 2 of B (`id`, `prev`), the two `VersionInfo`s, the copy (`version_id` and the old title), and the `LogEntry` (`version_id`, and `originator` and `approxSize` inside the pickle of its `message`).

The resolver of BTrees refuses rows 5 and 6 even when both sides wrote the same value: on `IOBucket`, two inserts of 2147483646 → `'2'` give "Conflicting inserts or deletes"; on `IIBucket`, two changes of one key to one value give "Conflicting changes".

So `version_id` is not the first refusal here. Both saves set it to 1, and tier 2 merges the Document with the value 1, which is wrong for the sequential result. In B5 `contention-fields`, the two saves of a refusal often start from snapshots with different numbers of versions: then they set `version_id` to different values, and tier 2 refuses on the Document first. Both cases end on the history. A strategy for `version_id` moves the refusal from the Document to the history: `max` gives 1 here, and `sum` gives 2 only when each side adds 1 and both versions stay.

### 4. Why no merge at the vote gives the sequential result

The sequential result is not a merge of the two changes. Both saves made the same change: "take the next number". The sequential result needs a different change from B: "take the number after A". To get it, the vote must compute the save of B again on the state after A.

- **The number is in 14 records.** In 13 records of B (8 that both write, 5 new ones) and in 1 record of A (`next` of version 2). The places: `version_id` of the Document and of the copy; `nextVersionId` and `_available`; the keys of 4 BTrees (`_full`, `_versions`, `m_order`, `_eventLog._data`), the values of `m_order` and `m_date`; `id` and `prev` of the new version; `version_id` of two `VersionInfo`s and of the `LogEntry`; `originator` in `_full` and in the `message` of the `LogEntry`. It is an int in some places, a str in others, and an int minus a constant in others. Tier 2 merges one record from its three states, with a strategy for each field. No strategy can take a decision that 14 records share.
- **The copy holds the old title.** The copy of B is a new `Document`, cloned from the snapshot of B. To hold both changes, the vote must apply the changes of A on the Document to another object, with its own oid. That works only for a field that the Document holds inline. A field in a sub-object (a blob, the annotations) is cloned into new objects with new oids, so a change in it cannot follow.
- **The sizes come from Python.** `approxSize` in `_full` and in the `LogEntry`, and `_approxSize`, are the length of a protocol-1 pickle of the copy, made by Python's pickler. With both changes the copy has another size (1044 bytes, not 1032). The core would have to give the length of the pickle that Python makes, for any Document.
- **One record is in A only.** `next` of the version of A must become `('3',)`. The transaction of B did not store that record. ADR 0049 (proposed in ZOE-224, PR #469) lets a vote write a record that its transaction did not store, for the split merge only: a lock that the vote takes without a wait, and clients that declare that they take such records. A write of a `ZopeVersion` would extend it to another case.

**Byte for byte.** For each value alone, a rewrite is possible: zoe-pickle decodes a record and encodes it back to the same bytes (the spike `pickle-round-trip.md`). A one-character str of a record of protocol 3 keeps its length, but `'9'` → `'10'` does not. The `message` of the `LogEntry` holds pickles of protocol 4 and 5 in a bytes value, which tier 2 does not decode; a rewrite must decode them and encode them again with their frame. But there is no reference to compare the result with. The shadow mode of tier 2 compares a merge with a Python reference of each strategy. Here the reference would be the save of CMFEditions itself, run again on another state, with other times and other sizes. So the result cannot be checked byte for byte, and the sizes cannot be computed.

**What it would be.** A "CMFEditions save" merge in the core that knows the records of 9 classes of two products, recomputes a version number, renames keys, edits the copy, edits a pickle inside a bytes value, and writes a record of the other side. That is a new implementation of the save of CMFEditions, at its exact versions, in Rust. It is outside tiers 1 and 2 of the specification, and shadow mode cannot verify it.

### 5. A `zoe.plone` adapter with version numbers from a sequence (F29)

ZOE-219 considered history ids from a sequence. For version numbers it does not work:

- CMFEditions uses the version number as a position. `nextVersionId` is the length of the history (`getLength`); `retrieve(selector)` and `getVersionId` read `_available` by position; `_getZVCAccessInfo` (`ZVCStorageTool.py:447-467`) computes the id of ZopeVersionControl as the number plus 1. A sequence gives numbers with gaps (another history takes values too), so every read of a history would change.
- ZopeVersionControl computes its own numbers: `len(branch) + 1` in `createVersion`, and `minKey() - 1` in `BranchInfo.append` and `EventLog.addEntry`. Numbers from a sequence need a change of ZopeVersionControl too.
- With all numbers different, the two saves would insert different keys, and tier 1 would merge the inserts. But `next` of version 1 (both write it), `prev` of the new versions (both point to version 1), and `m_date` (one key for each minute) still conflict.
- The copy of B still holds the old title. The last version would not hold both changes, so the acceptance test fails on it.

### 6. What passes: the version in a transaction of its own, after the commit of the edit

The only way for the copy of B to hold the change of A is to make the copy after A committed. A retry of the request does that today: Zope runs the save of B again after the `ConflictError`, and the result is the sequential one. The acceptance test asks for both saves to commit at the first attempt.

`zoe.plone` can move the save of the version out of the transaction of the edit:

1. The edit transaction sets the field and makes the decision of the stock subscriber (the same checks, with `isUpToDate`). It does not save the version. It adds an after-commit hook. So it writes the Document only: the field and `modification_date`. Tier 2 merges two such edits of different fields.
2. After a commit that succeeded, the hook opens a new connection of the same database, with its own transaction manager. It finds the object by its physical path, under the request of the thread, with the user wrapped in the user folder of the new connection, and calls `portal_repository.save`. It commits. On a `ConflictError` it aborts and tries again from a new snapshot.
3. The version transaction starts after the edit committed, so its copy holds the edit and every edit that committed before it. It numbers its version after every committed version.

The prototype (outside the repository) replaces `create_version_on_save` in the global registry with such a handler. Results:

- **The four tests of the acceptance module**, called with the prototype on: `Ran 4 tests with 0 failures, 0 errors`. The acceptance module itself is unchanged.
- **The order of the acceptance test** (A commits and its hook ends, then B commits): no error; each version transaction commits at its first attempt; the state equals the sequential reference: the Document holds both changes and `version_id` 2, and the versions are 0 (old, old), 1 (A, old), 2 (A, B).
- **Two saves of the title**: B fails with `ConflictError` on the Document, as the guard asks; its hook does not run. The history holds the version of A.
- **The other order** (A and B commit their edits, then the two version transactions run): no error; the second version transaction conflicts once and commits at its second attempt. The versions are 0 (old, old), 1 (A, B), 2 (A, B). The Document is right, no change is lost, but version 1 holds the change of B too, and two versions hold the same state. The history is coarser than the sequential one.
- The first version of the prototype failed with `Unauthorized` on `save`: the user of the security manager belongs to the user folder of the first connection, and the check of PAS needs the user folder in the acquisition chain of the object. The user must be wrapped in the user folder of the new connection.

The prototype is enough to show that the mechanism passes the acceptance module, but not enough to build. An adversarial review of ADR 0051 found these gaps, and the ADR (sections 2 and 3) closes them:

- **The current transaction.** The prototype keeps the edit as the transaction of the thread during the hook. So `transaction.savepoint()` in `save` goes to the committed edit, and the indexing queue of CMFCore joins `transaction.get()`, the committed edit, which raises `ValueError` (`transaction/_transaction.py:187-193`). The test passed because the Document and its container have a history id already, so the save reindexes nothing. A versioned object without a history id, or with a parent without one, loses its version. The ADR makes the version transaction the current one of the thread for the length of the hook.
- **The user.** The prototype looks the user up again by id in the user folder of the site. That gives `None` for a user of the root `acl_users` and for a special user, and the user at the commit can differ from the user at the event. The ADR keeps the user object of the event and re-wraps it.
- **The site and the database.** The prototype takes the site from the first two parts of the path, and the database of the object. That is wrong for a site in a folder, a child site, and an object in a mounted database. The ADR takes the site from `getSite()` and the root database from the `Application`.
- **The path.** The prototype takes the path at the event. A move later in the transaction loses the version. The ADR reads the path at the commit of the edit.
- **The end.** The prototype closes the connection without an abort after an error that is not a conflict. ZODB refuses to close a joined connection, so the connection leaks. The ADR aborts on every path, then closes.
- **The decision and the retries.** The prototype copies part of the stock decision, makes one version transaction for each modified event, retries only on `ConflictError` and without a wait, and saves again without a re-check. In the other order of the commits, a re-check of `isUpToDate` before the save skips the second version when it has no change note: the versions are then 0 (old, old) and 1 (A, B).

The limits of the mechanism:

- The history equals the sequential one only when the snapshot of each version transaction comes before the commit of the next edit of the object. Else a version can hold the change of a later edit, credited to the user and the change note of another edit (ADR 0051, section 4).
- A crash between the two commits loses the version of that edit and its change note. With the policy `at_edit_autoversion`, the next edit makes a version that holds the change, because `isUpToDate` compares the modification dates; the lost change note is not recovered.
- The request waits for one more commit for each edit of a versioned object, and the thread holds two connections during the hook.
- Two version transactions of one object conflict with each other on the history. These are retries inside the hook, not `ConflictError`s of the request.
- The edit subscriber of plone.app.versioningbehavior does not run. A site with this mechanism runs a copy of its decision. Other callers of `portal_repository.save` (the first version at a creation, revert, plone.app.iterate, scripts) keep saving in their own transaction.

## Consequences for the card

- ZOE does not merge two version saves at the vote. No strategy, coupled group or split merge passes the acceptance test, and `version_id` stays `strict`: `max` would merge the Document with a number that does not match the history.
- The mechanism of section 6 passes the acceptance test without a change of the core, of CMFEditions or of ZopeVersionControl. It replaces a subscriber of plone.app.versioningbehavior. ADR 0051 proposes it, with its questions for the human.
- Without that mechanism, the acceptance test cannot pass. The human then decides: keep the conflict (Zope retries the request, and the result is the sequential one), or change the goal of the card (for example, the B5 criterion on a type without versions, or versions from the history of the store, F36).

## Not measured

- B5 `contention-fields` with the prototype: it changes what a save writes, and the version transactions add commits.
- Edits through the forms and plone.restapi: the trace uses the save of the acceptance test (the field, then `ObjectModifiedEvent`).
- A change note, the first version at a creation, a purge policy that keeps N versions (it purges inside the save), types with blobs or relations, and `zoe.zeo.ClientStorage`.
- The prototype in a published request. In the test, the savepoint that `portal_repository.save` takes went to the committed edit and did no harm, because nothing joined it. A save that reindexes joins the committed edit and fails (section 6, the gaps of the prototype).
- An object or a parent without a history id, a user of the root `acl_users`, a site in a folder, a mount point, and two modified events in one request.
