# Spike: concurrent creations, the conflicts on the trees whose keys grow

Card ZOE-219, step 1; the success criterion on `ConflictError`s (section 1), F5 (tier 1), F7 (early locks) and lever 4 of section 9 of the specification; the gate of phase 4 (#440). The decision that follows is ADR 0048.

**Verdict.** On `zoe-native-qi`, every `ConflictError` of `create-spread` but the container of a creation is on four BTrees that every creation of the site shares: the shadow storage and the version histories of CMFEditions, and the two trees of the intid utility. Each of the four trees has a root and its buckets, and each conflict is a split: every revision of the four roots in the three runs adds one child and changes nothing else. ZODB's resolver refuses each conflict on a node with children, and each bucket that one side split. It does not refuse because of the keys. It sees one record at a time, so it cannot move a key to the bucket of the other split. A merge of the whole node and its split buckets can. That merge writes only the records that the transaction stored, so it needs no new lock, no new invalidation and no change of the wire protocol. On the kept logs it leaves 2.4 `ConflictError`s per 1,000 operations at 4 users and 9.1 at 16 users, against 106.6 and 41.4 today: 0.6 % and 2.4 % of `zeo`. Keys that do not grow, early locks and a queue of CMFEditions each leave more, and none of them passes the acceptance test of the planner.

## Question

Step 1 of the card:

- Map each conflicting structure to its owner and to the code that writes it at a creation: the unnamed `OIBTree`, `OOBucket` and `IOBTree`, the 25 unnamed records, the `Folder` and the read conflicts of the measures of ZOE-218.
- For each, record the key it inserts, why that key grows, and how often a split meets an insert.
- On the log of a kept `create-spread` run on `zoe-native-qi`, measure what each candidate removes: (a) a tier 1 merge of a bucket split against inserts of other keys; (b) keys that do not grow; (c) early write-intent locks on the trees; (d) a queue beside the object graph for the writes of CMFEditions; (e) any other mechanism.

## Method

- **Code.** `card/ZOE-219` at `c000ed2`, the head of `card/ZOE-218` (#444). The `zoe` binary of that commit (`cargo build --release`), and the Plone instances of the scenarios `zeo` and `zoeclient`, built in the worktree from the eggs of `target/plone/eggs/cp313`: CPython 3.13, Plone 6.1.5, Products.CMFEditions 4.0.6, Products.ZopeVersionControl 4.1, Products.CMFUid 4.2, plone.app.versioningbehavior 2.0.5, zope.intid 6.0, five.intid 3.1.0, zope.keyreference 7.0, plone.folder 4.0.2, Products.BTreeFolder2 5.1, BTrees 6.3, ZODB 6.2 and ZEO 6.2. The file and line numbers below are in these eggs. The eggs of `~/.buildout/eggs/cp312` hold other versions (CMFEditions 4.0.3 and 4.0.5, BTrees 5.1 and 6.1), which the instances do not run.
- **The fixture.** `small-ba01ff255313` (685 objects), a copy of the fixture of ZOE-218, which has the same spec.
- **The runs.** `just bench plone run --stack zoe-native-qi --scenario create-spread --rep N --steps 4:60,16:60 --port-offset 300 --keep-store --label spike-ZOE-219` for N = 0, 1, 2, with the default warmup of 60 s and trim of 5 s, under `caffeinate -ims`. `ZOE_BINARY` named a script that adds `--resolution-log` to `zoe serve` (ADR 0042). The log changes no outcome. The stack keeps its defaults: tier 1 on, the posting-dict rule off, no early locks, no merge policy. Every run passed its verification.
- **The machine.** Apple M3 Pro, 12 cores, 36 GiB, macOS 27.0.1. Other worktrees ran builds and tests: the 1-minute load average was 4.8 to 6.2 during the steps.
- **The map of the oids.** A script inside Zope (outside the repository, as in ZOE-218) walked each kept store read-only: the tools of CMFEditions with their two trees, the intid utility with its two trees, the relation catalog, every folder with its `_tree`, `_count`, `_mt_index`, annotations and ordering, every content object, the catalog and the rest of the site. Each node of a tree gets its tree and its role: root, node or bucket. Every oid of a `ConflictError` of the probe records and of the resolution log has a name.
- **The trees.** A second script read every revision of the four trees after the fixture through the iterator of the store: the keys that each transaction adds, the children that each revision of a root adds, and where.
- **The candidates.** Each candidate removes a set of conflicts of the table of finding 1; the reasoning for each set is in finding 6. The split merge of candidate (a) has a model on real BTrees (finding 6), outside the repository.
- **The acceptance test.** The test of the planner (`plan/ZOE-219`, `f6a4b39`), merged unchanged, run with `just plone zoe backends -t test_concurrent_creations`. A script of the same shape on a writable copy of a store gave the objects that eight creations in one folder write.

## Findings

### 1. The runs and the conflicts by structure

| rep | users | operations | ok ops/s | 1st-try ok | write | read | conflicts/1k | failed | load mean (max) |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 0 | 4 | 395 | 7.18 | 88.6 % | 32 | 13 | 113.9 | 0 | 6.2 (7.1) |
| 0 | 16 | 474 | 8.62 | 95.1 % | 16 | 7 | 48.5 | 0 | 5.2 (5.3) |
| 1 | 4 | 405 | 7.36 | 90.9 % | 31 | 8 | 96.3 | 0 | 4.9 (5.0) |
| 1 | 16 | 470 | 8.55 | 96.4 % | 10 | 8 | 38.3 | 0 | 5.3 (5.3) |
| 2 | 4 | 429 | 7.80 | 89.5 % | 38 | 8 | 107.2 | 0 | 5.5 (5.7) |
| 2 | 16 | 482 | 8.76 | 96.5 % | 11 | 6 | 35.3 | 0 | 4.8 (5.2) |

Pooled, the probe records give 131 `ConflictError`s in 1,229 operations at 4 users (106.6 per 1,000, 27.0 % of the 395 of `zeo` in #440) and 59 in 1,426 at 16 users (41.4, 10.9 % of 380). The server counts 130 at 4 users: one request starts in the window and conflicts after it.

By structure, three repetitions pooled:

| structure | owner | conflict | 4 users | 16 users |
| --- | --- | --- | ---: | ---: |
| root of `_shadowStorage._storage` | CMFEditions | write, "Conflicting changes in an internal BTree node" | 76 | 20 |
| bucket of `_shadowStorage._storage` | CMFEditions | write, "Conflicting bucket split" | 2 | 6 |
| root of `zvc_repo._histories` | CMFEditions (ZopeVersionControl) | read | 18 | 15 |
| root of `zvc_repo._histories` | CMFEditions (ZopeVersionControl) | write, internal node | 3 | 0 |
| root of `intids.ids` | zope.intid, five.intid | write, internal node (the client refuses) | 19 | 3 |
| bucket of `intids.ids` | zope.intid, five.intid | write, bucket split (the client refuses) | 1 | 2 |
| root of `intids.refs` | zope.intid | read | 11 | 6 |
| root of `intids.refs` | zope.intid | write, internal node | 1 | 2 |
| the container `Folder` | plone.app.contenttypes | write, `modification_date` | 0 | 5 |
| all | | | 131 | 59 |

So the unnamed structures of ZOE-218 are:

- the `OIBTree` (30) and its `OIBucket` (7): the root and the buckets of `ids` of the intid utility. Tier 1 cannot compare their keys, so it gives no answer ("an opaque node"), and the client's resolver refuses;
- the `IOBTree` (6): the root of `refs` of the intid utility;
- the `OOBucket` (14): the buckets of `_shadowStorage._storage` that the runs created;
- the 25 records without a class and the 66 read conflicts: the roots of `zvc_repo._histories` and of `intids.refs`, which the vote finds stale for the current state that BTrees marks. No read conflict is on another object;
- the `Folder` (4): the container of a creation.

### 2. What a creation writes, and the key of each tree

A creation of a Document through the add form fires `IObjectAddedEvent`. Two subscribers insert into the four trees:

- **The history id.** `create_initial_version_after_adding` of plone.app.versioningbehavior (`plone/app/versioningbehavior/subscribers.py:64`, for `IObjectAddedEvent` in `configure.zcml:42-43`) saves the first version through `portal_repository.save` (`CopyModifyMergeRepositoryTool.py:294`, `_recursiveSave` at `:451`). `ArchivistTool.prepare` asks `portal_historyidhandler.register(obj)` for a history id (`ArchivistTool.py:252-253`). That is the `UniqueIdHandlerTool` of CMFUid, which calls the `IUniqueIdGenerator` utility (`UniqueIdHandlerTool.py:92-100`), `portal_uidgenerator`. On a ZOE stack, `zoe.plone.uidgenerator` gives it `base + value` from a sequence of the store, whose values strictly increase. So the history id grows.
- **The shadow storage.** `ArchivistTool.save` calls `portal_historiesstorage.register` (`ArchivistTool.py:298`, `ZVCStorageTool.py:195`). `_applyOrCheckin` (`ZVCStorageTool.py:369-407`) asks `_getShadowHistory(history_id, autoAdd=True)`, and `ShadowStorage.getHistory` inserts `self._storage[history_id] = ShadowHistory()` (`ZVCStorageTool.py:680`). `_storage` is an `OOBTree` (`:662`). Its key is the history id: it grows.
- **The version histories.** `_applyOrCheckin` calls `zvc_repo.applyVersionControl` (`ZVCStorageTool.py:203`, `Repository.py:155`), which calls `createVersionHistory` (`Repository.py:56-66`): the key is `str(randint(1, 9999999999))` (`:63`), inserted into the `OOBTree` `_histories` (`:65`). The key is random, so it does not grow.
- **The intids.** `addIntIdSubscriber` of five.intid (`five/intid/intid.py:71-88`) registers the key reference of the Document in the intid utility. `IntIds.register` (`zope/intid/__init__.py:147-157`) inserts `refs[uid] = key` (`:155`) and `ids[key] = uid` (`:156`). `ids` is an `OIBTree` and `refs` an `IOBTree` (`family32`, `:75-81`). A key reference sorts by the database name and the oid of its object (`zope/keyreference/persistent.py:61-95`; five.intid's subclass, `five/intid/keyreference.py:60-104`, keeps the comparison). The oid of a new object is the next oid of the store, so the key of `ids` grows. The intid comes from `_generateId` (`zope/intid/__init__.py:127-145`): sequential from a random start in each connection (`_v_nextid`), so the keys of `refs` spread over a few runs, one for each connection.

The container: `BTreeFolder2Base._setOb` (`Products/BTreeFolder2/BTreeFolder2.py:242-258`) writes `_tree`, `_count` and `_mt_index[meta_type]`, and plone.folder's `DefaultOrdering.notifyAdded` (`plone/folder/default.py:25-30`) appends to the `PersistentList` of the order and inserts into the `OIBTree` of the positions. The reindex of the container then sets its `modification_date`, so the record of the `Folder` changes too. Creations in different folders do not share these objects.

### 3. The trees, their splits, and how often a split meets an insert

At the fixture each of the four trees is a root with buckets: 38, 25, 22 and 22 buckets. After a run it holds about 100, 72, 54 and 52. Each root stays a node whose children are buckets: the maximum of a node of these families is 250 or 500 children (`BTrees/_datatypes.py`).

| tree | key | inserts at the last bucket | users | inserts | splits | inserts per split | conflicts | conflicts per split |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `_shadowStorage._storage` (`OOBTree`, buckets of 30) | history id, grows | all | 4 | 1,224 | 79 | 15.5 | 78 | 0.99 |
| | | | 16 | 1,397 | 92 | 15.2 | 26 | 0.28 |
| `zvc_repo._histories` (`OOBTree`, buckets of 30) | random | 3 % | 4 | 1,224 | 61 | 20.1 | 21 | 0.34 |
| | | | 16 | 1,397 | 62 | 22.5 | 15 | 0.24 |
| `intids.ids` (`OIBTree`, buckets of 60) | key reference, by oid, grows | all | 4 | 1,224 | 41 | 29.9 | 20 | 0.49 |
| | | | 16 | 1,397 | 46 | 30.4 | 5 | 0.11 |
| `intids.refs` (`IOBTree`, buckets of 60) | intid, a run for each connection | about a quarter | 4 | 1,224 | 35 | 35.0 | 12 | 0.34 |
| | | | 16 | 1,397 | 43 | 32.5 | 8 | 0.19 |

- Every revision of the four roots after the fixture adds exactly one child and changes no other entry: 189 revisions of the shadow storage, 141 of the version histories, 97 of `ids` and 89 of `refs` in the three runs. No bucket became empty, and no separator changed.
- Every write conflict on a root has exactly one commit of the root between the serial that the transaction read and the committed serial, and that commit added one child. On `_shadowStorage._storage` and `ids` the child comes after the last child: both sides split the bucket at the right end, from one state of the root.
- A key that grows goes into the last bucket, so a split of the last bucket every 15 inserts (30 in `ids`) meets each concurrent creation. On the shadow storage at 4 users, a split meets about one other creation. Tier 1 merges the inserts of concurrent creations into the last bucket without a split, so that bucket passes 30 entries; then every creation that inserts splits it, all of them from one state of the root, and the first to commit refuses the others on the root.
- A random key meets a split in another bucket: the root changed, and the vote fails on the current state that BTrees marked on the root. `zvc_repo._histories` shows what keys that do not grow give: 0.34 conflicts per split at 4 users, against 0.99 for the history ids.
- The 8 bucket-split refusals of the shadow storage and the 3 of `ids` are a split on the committed side against a plain insert of the new side into the same bucket. The vote checks the oids in ascending order, and the root has a lower oid than each bucket, so the new side did not write the root: it did not split.

### 4. What ZODB refuses, and why

In the C code of BTrees 6.3:

- `_BTree_set` marks each node on the path to the bucket as read for its current state (`PER_READCURRENT`, `BTreeTemplate.c:754`; `_set` and `_del` of `_base.py:1051` and `:1123` in Python). The vote then fails with `ReadConflictError` when another transaction changed such a node, whatever the change. Nothing else in the eggs of the instance calls `readCurrent`.
- A bucket splits when an insert gives it more entries than its maximum (`BTreeTemplate.c:796-813`). `BTree_grow` splits it at its midpoint (`bucket_split`, `BucketTemplate.c:590-620`, index `len / 2` at `:596-597`) and inserts into the node the new bucket and its first key as the separator (`BTreeTemplate.c:522`). So every split writes the node.
- `BTree__p_resolveConflict` (`BTreeTemplate.c:1380-1405`) merges only a BTree that holds one bucket inline. `get_bucket_state` (`:1328-1343`) refuses a BTree with children: reason 11, "Conflicting changes in an internal BTree node". So any conflict on a node refuses, also two splits of different buckets.
- `_bucket__p_resolveConflict` (`BucketTemplate.c:1631-1677`) refuses when the next buckets of the three states differ (`:1664-1665`): reason 0, "Conflicting bucket split". So a bucket that either side split refuses.
- `bucket_merge` (`MergeTemplate.c`) merges inserts of different keys in one bucket, and refuses two inserts of one key (reasons 4 and 6) and a key that both sides changed (reason 1). Its comment gives the reason for the structural refusals: the resolver "doesn't have enough info" to fix the node above a bucket.

So the refusals of these trees are not about the keys: two inserts of different keys merge when they are in one bucket. They are about the structure, because the resolver of ZODB merges one record at a time and the vote of ZODB checks one record at a time.

### 5. The acceptance test of the planner

The test makes a Plone site of the functional layer, eight folders and one Document, copies the site to a `zoe.Storage` of its own, and runs 16 rounds. In each round, eight threads create one versioned Document each, in their own folder, from one snapshot, then commit one after the other.

- It fails today, as the card asks. In this worktree: rounds 4, 7, 12 and 15 on the root of `_shadowStorage._storage` ("Conflicting changes in an internal BTree node", seven creators in round 4), round 8 on the root of `ids`, and round 10 on the root of `zvc_repo._histories`, where three creators failed on the current state of the root and three others on a write of it. The other five tests pass.
- From one snapshot, every creator of a round finds the same last bucket, so all of them split it, or none does. The first split of each tree is the split of the one bucket that its root holds inline: every creator splits the same bucket there too, whatever the order of its keys.
- So the test needs a merge of two splits of one bucket, on all four trees, with the comparisons of the key references of `ids`. A mechanism that makes a creator wait for another one before its commit cannot pass it: each creator changes the site before any of them commits.
- Eight creations in one folder, on a fresh site, conflict on other objects: on `_mt_index` of the folder for its first Document ("Conflicting inserts" of the meta type), then on the `Folder` (`modification_date`), and with a merge of that field, on the `PersistentList` of the order of plone.folder. That list must keep its conflict: tier 1 merges the `OIBTree` of the positions of two adds, which both take the same position, and only the refusal of the list keeps the order right. The planner's eight folders keep the test on the trees of the card.

### 6. The candidates on the kept logs

`ConflictError`s left per 1,000 operations, and the share of `zeo` (#440: 395 at 4 users, 380 at 16 users); the gate is 10 %:

| candidate | 4 users | 16 users | the acceptance test |
| --- | --- | --- | --- |
| today | 106.6 (27.0 %) | 41.4 (10.9 %) | fails |
| (a) the split merge, with the order of key references | 2.4 (0.6 %) | 9.1 (2.4 %) | passes, as far as the model shows |
| (a) without the order of key references | 17.9 (4.5 %) | 11.2 (3.0 %) | fails on `ids` |
| (a) only for splits of different children | 79.7 (20.2 %) | 25.2 (6.6 %) | fails |
| (b) history ids that do not grow, alone | 60 (15 %) | 34 (9 %) | fails |
| (b) with the merge of splits of different children | 16.3 (4.1 %) | 7.0 (1.8 %) | fails |
| (c) early locks on the four roots | 0 | 3.5 (0.9 %) | cannot pass |
| (d) a queue for the writes of CMFEditions | 26.0 (6.6 %) | 12.6 (3.3 %) | fails on `ids` |

Each row removes conflicts of the table of finding 1; nothing was built. (a) leaves the bucket-split refusals of finding 3 and the `Folder`. The row of (b) alone gives the shadow storage the conflicts per split of `zvc_repo._histories`. (b) with the merge of splits of different children assumes that all conflicts of the shadow storage then become such splits, as those of `zvc_repo._histories` are. (c) and (d) remove every conflict of the trees that they cover, and nothing else.

**(a) The split merge.** At the vote, tier 1 takes the node and the buckets that the transaction stored together, against the current node and its buckets, when every change of the node on both sides is a split. Three rules (ADR 0048 gives them in full):

1. **A read-current node.** The transaction wrote a bucket under a node that it read, and the other side split another child of that node. The vote lets the stale node through when each child that the transaction wrote keeps its separator and the separator after it. The ranges of its writes did not change, so a lookup finds each of them. The read conflicts of `zvc_repo._histories` and `refs` are all of this kind: 50 of the 190.
2. **Splits of different children.** Both sides split a child of one node, not the same one. The merged node is the current node with the new children of the transaction, each after the child that it split. Each bucket has one writer. The node is the only record that both sides wrote.
3. **Two splits of one bucket**, or a split of a bucket that the other side changed without a split, or a split of the one bucket of a root. The keys of the range of that bucket merge with the rules of `bucket_merge`: base the bucket that the transaction read, committed the bucket and the new bucket of the committed side, new the bucket and the new bucket of the transaction. The merged keys go to the bucket and to the new bucket of the transaction, split at the separator of the current node. The node takes the new bucket of the transaction in place of the new bucket of the committed side. The 96 root conflicts of the shadow storage and the 22 of `ids` are of this kind.

The merge writes only records that the transaction stored: the node, the bucket that it split, and its own new bucket. The card expected a merge that writes the bucket that the committed transaction created. That bucket is not in the transaction: its vote holds no lock on it, and its client would not invalidate it in its other connections. ZODB's `MVCCAdapterInstance` invalidates only the oids that a connection stored (`ZODB/mvccadapter.py:208-222`), and a ZEO server sends no invalidation to the client that commits. So it would need a new lock in the vote and an extension of the protocol. The merge of rule 3 instead leaves the new bucket of the committed side out of the tree: it becomes garbage, which pack removes. Its keys are in the bucket of the transaction.

It loses no update, because:

- the merged keys of the range are the merge of `bucket_merge` of the three states of that range: each change of either side stays, and each case where `bucket_merge` refuses still refuses;
- every key outside the range keeps its bucket, and the node routes each key of the range to the bucket that holds it;
- the bucket that leaves the tree holds no key that the merged buckets do not hold. Each writer of a bucket of the tree goes through the node, and BTrees marks the node read-current or writes it; so the vote of each writer holds the lock of the node until its commit is durable, and the merge, which holds the node, reads the last commit of every bucket under it. A later writer of the bucket that left fails its check of the node: that bucket is no longer a child, and rule 1 refuses.

The model: a script on BTrees 6.3 and a `MappingStorage` builds a tree, opens transactions from one snapshot, lets BTrees insert in each of them, commits the first, and merges each other one with the three rules, by states only, onto the current tree. After each round, `BTrees.check`, `_check` and a lookup of every key of every transaction must pass. With one key in each transaction, as in the acceptance test: 500 trials, 12,097 merges, no failure and no transaction without an answer. With one or two keys in each transaction: 1,000 trials, 24,313 merges, no failure, and 368 transactions without an answer: a split on the committed side against a plain insert of the new side into the split bucket (335), and the same in a root with its bucket inline (33).

The risks:

- It merges where ZODB refuses, so it departs from ADR 0042, which keeps tier 1 byte for byte equal to ZODB's resolver. For these cases ZODB writes no record to compare with. The shadow mode of ADR 0042 cannot verify it.
- Rule 1 changes an invariant of the vote: a stale read-current oid is never resolved today. A program that marks a tree read-current itself and writes a bucket under it would lose the check of the rest of the node. No code in the eggs of the instance does this.
- The keys of `ids` are key references. Tier 1 cannot compare them today. Rule 3 needs their order, which `_get_cmp_keys` gives in conflict resolution: the oid of the object of each reference, when neither reference names a database (`zope/keyreference/persistent.py:61-95`). That is knowledge of a class of another program, as the posting-dict rule is.
- The vote stages records of several oids for one decision, a new object among them, and reads a bucket that it does not lock. The code is in the vote (`crates/zoe-core/src/commit/`), outside the scope fence of the card.
- The merged bucket can hold more entries than the maximum of its class, as a bucket that ZODB's resolver merges can. The next insert splits it.

**(b) Keys that do not grow.** `zoe.plone` can give the history id from a source that scatters it: `base` plus a permutation of the value of the sequence, in a range of its own. That changes only the shadow storage: the keys of `zvc_repo._histories` are random already, and `zoe.plone` cannot choose the oids that order the keys of `ids`. Alone, it moves the shadow storage to the rate of `zvc_repo._histories`, because a split of any bucket still changes the root that every creation reads. With rules 1 and 2 of (a) it passes the gate of B5, but not the acceptance test: the first split of each tree, of the bucket inline in its root, and every split of `ids`, are splits of one bucket by every creator.

**(c) Early locks on the four roots.** A lock before the read of each root, and a read of the whole tree past the snapshot, would make the creators wait for each other. ADR 0045 does not allow it as it is: `early-lock-classes` refuses a class of tier 1 at the open, a hot class must be a leaf record and a tree is not, and a hot class makes every object of that class hot, every `OOBTree` of the site. It needs hot objects by oid and a read past the snapshot of a whole tree. In production the creators then wait for each other from the first insert into a tree to their commit. The acceptance test cannot pass with it: each creator of a round changes the site before any of them commits, so the second creator waits for a lock that the first one holds until after the barrier, until the lock timeout.

**(d) A queue beside the object graph for CMFEditions.** The insert of a new history into the two trees of CMFEditions would go to a durable namespace, and a worker would apply it later, as for queued indexing. Every read of a history (`isRegistered`, `getHistory`, `getVersionHistory`, the history viewlet, `portal_repository.retrieve`) would have to read the queue too, so it needs patches of many methods of CMFEditions and ZopeVersionControl. A copy through the iterator loses the durable rows (ADR 0043), and the copy then holds versions without their entries in the trees. It does not cover the intids, so it fails the acceptance test on `ids`.

**(e) Other mechanisms.**

- **Bigger buckets.** A subclass of `OOBTree` with a larger `max_leaf_size` splits less often, but it puts a class of `zoe.plone` into the data, tier 1 does not know the class, and the first split of each tree still meets every creator.
- **The writes of CMFEditions in a transaction after the commit.** Two such transactions insert into the same trees, and conflict as the saves did.
- **The container of a creation.** Tier 2 can merge `modification_date` of the `Folder` with `max`, and then the order of plone.folder conflicts, as it must (finding 5). It is out of this card: 5 of the 190 conflicts, and the planner's test keeps one folder for each creator.

## Consequences for the card

- The mechanism is the split merge of candidate (a), with the order of key references. It is in the core: no code of CMFEditions, of zope.intid or of `zoe.plone` changes, and a site switches it on through the merge policy. ADR 0048.
- It needs code in the vote, `crates/zoe-core/src/commit/`, outside the scope fence, and a change of the invariants of the vote in `crates/zoe-core/CLAUDE.md`. It needs no change of the format, of pack or of the wire protocol.
- The conflicts that it leaves in `create-spread` are the bucket-split refusals of a split against a plain insert (11 of 190) and the container (5).
- Step 4 measures B5 `edit` and `create-spread` on `zeo` and on `zoe-native-qi` with the merge switched on, and `just bench native` and B2, because the core changes.

## Not measured

- A quiet machine: the load average was 4.8 to 6.2 on 12 cores during the steps. Linux.
- `zeo` in this campaign: the shares use the numbers of `zeo` of #440.
- The candidates themselves: each row of finding 6 removes conflicts of the kept logs, and only the model of (a) runs code; no candidate was built. A merge changes what the transactions after it read, so the numbers of step 4 will differ.
- The anchors of the 3 root conflicts of `zvc_repo._histories` and the 3 of `refs` where both sides split: the transaction that lost does not reach the store. Rules 2 and 3 together cover both cases.
- Trees of three levels: the four trees keep two levels in these runs. The rules of ADR 0048 give no answer for two splits of one child node.
- B6 on the kept stores, the medium fixture, and sites with relations, multilingual content or add-ons that keep their own BTrees.
