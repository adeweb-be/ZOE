# Spike: two creations in one folder, the record of the folder and the order of plone.folder

Card ZOE-221, step 1; the success criterion on `ConflictError`s (section 1), F5 and F6 (tiers 1 and 2) and lever 4 of section 9 of the specification. The decision that follows is ADR 0052 (proposed).

**Verdict.** A creation in a folder of plone.app.contenttypes writes six records of the folder. Four of them merge today with tier 1 and the split merge: `_tree`, `_count`, `_mt_index[meta_type]` and the positions `pos`. Two do not. The record of the `Folder` changes only `modification_date`, which tier 2 merges with `max`. The order list is a stock `PersistentList`, with no resolver. A merge of the order list alone lets every creation of the rounds commit, but leaves `pos` wrong for 70 of 81 ids, because each creation of a round gave its id the same position. In every commit of the rounds, the id of the transaction is in a record of `pos` that the commit writes. So a merge that appends the ids of the transaction to the committed order and renumbers those ids in the records of `pos` that the vote stages keeps the order and `pos` in agreement, and writes no record that the transaction did not store beyond the bucket that the split merge of ADR 0049 writes already. On B5 with the split merge, `create-shared` (one folder for every user) on `zoe-native-qi` raises 293 and 189 `ConflictError`s per 1,000 operations at 4 and 16 users, and `create-spread` 0 and 23: every one on a `Folder`.

## Question

Step 1 of the card:

- What does a creation write to the record of the folder, the order list, the `pos` tree, `_tree`, `_count` and `_mt_index`?
- Which conflicts appear, and in which order, once each of them is merged?
- Which merge keeps the order list and `pos` in agreement?

## Method

- **Code.** `card/ZOE-221` at `214091ac`: `main` at `8d0c5e08` (after ZOE-219, ZOE-224 and ZOE-225), with the acceptance test of the planner (`python/zoedb/src/zoe/plone/tests/test_creations_in_one_folder.py`, unchanged). The Plone instances of the scenarios `zoe` and `zoeclient`, built in the worktree from the eggs of `target/plone/eggs/cp313`: CPython 3.13.15, Plone 6.1.5, plone.folder 4.0.2, Products.BTreeFolder2 5.1, plone.dexterity 3.0.8, plone.app.contenttypes 4.0.10, zope.annotation 6.0, BTrees 6.3, persistent 6.5, ZODB 6.2, ZEO 6.2. The file and line numbers below are in these eggs.
- **The acceptance test.** `just plone zoe backends -t test_creations_in_one_folder`, to see it fail.
- **The probe.** A module of tests outside the repository, in the instance of the scenario `zoe` (`target/plone/zoedb/zoe/plone/tests/`, a copy of the package that the build makes). It reuses the class of the acceptance test: the same site, the same copy to a `zoe.Storage` of its own, with a resolution log, and the same rounds of eight creators from one snapshot, ten rounds. It ran:
  - (a) one creation: the records that its commit writes, by the iterator of the store, each named by its place in the folder; and the folder, the order, `pos` and `_mt_index` before and after;
  - (b) the rounds with the policy `{"classes": {}, "split_merges": "on"}`, the policy of `mechanism_off`;
  - (c) the rounds with a tier 2 entry for the `Folder`: `{"tier2": "on", "default": "strict", "fields": {"modification_date": "max"}}`;
  - (d) the rounds with that entry, and the order list replaced, before the rounds, by a subclass of `PersistentList` with a `_p_resolveConflict` that merges appends on both sides (the committed list, then the items that the new list added) and refuses an id on both sides. `zoe.Storage` calls it in the client (tier 3). Nothing changes `pos`;
  - (e) the setting of (d), and (f) 80 creations one after the other, each read back through historical connections: for each commit, the ids that it appended, their positions, the records of `pos` that the commit wrote, the record that holds each new id, and the fields of the `Folder` record that changed, decoded from the records before and after.
- **B5.** `just bench plone run --stack zoe-native-qi --scenario S --rep 0 --steps 4:60,16:60 --port-offset 500 --split-merges on --label spike-ZOE-221` for S = `create-shared` (the add form, every user in the one folder `bench/inbox`) and `create-spread` (each user in folders of its own section), one run each, inside `systemd-inhibit --what=sleep:idle` and `flock` on the lock of the benchmarks, after the 1-minute load average was below 8. The fixture `small-ba01ff255313` (685 objects). `ZOE_BINARY` named a script outside the repository that adds `--resolution-log` to `zoe serve`. The `zoe` binary of `214091ac`, release. Both runs passed their verification.
- **The machine.** Intel Core Ultra 7 155H, 22 logical CPUs, 32 GB, Linux 7.0. The 1-minute load average was 2.2 at the start of the first run and 4.1 at the end of the second. Other agents ran builds and tests.
- The probe, the analysis script, the results and the logs were outside the repository.

## Findings

### 1. What a creation writes to the folder

One creation of a versioned Document in the folder of the test writes 33 records (probe a). Six of them are records of the folder. In the order of their oids, which is the order in which the vote checks them:

| record | class | what the creation does | code |
| --- | --- | --- | --- |
| the folder | `plone.app.contenttypes.content.Folder` | sets `modification_date`; no other field (80 of 80 serial creations, probe f) | `notifyModified` and `setModificationDate`, `plone/dexterity/content.py:430-459` |
| `_tree` | `BTrees.OOBTree.OOBTree` (buckets of 30) | inserts the id, with the new item | `BTreeFolder2Base._setOb`, `Products/BTreeFolder2/BTreeFolder2.py:242-258` |
| `_count` | `BTrees.Length.Length` | adds 1 | same |
| the order | `persistent.list.PersistentList`, state `{'data': [...]}` | appends the id | `DefaultOrdering.notifyAdded`, `plone/folder/default.py:25-30` |
| `pos` | `BTrees.OIBTree.OIBTree` (buckets of 60) | inserts the id with `len(order) - 1` | same |
| `_mt_index[meta_type]` | `BTrees.OIBTree.OIBTree` (buckets of 60) | inserts the id with the value 1 | `_setOb` |

- The order and `pos` are values of the attribute annotations of the folder, an `OOBTree` under `__annotations__` (`zope/annotation/attribute.py:20`, `:85-87`), under the keys `plone.folder.ordered.order` and `plone.folder.ordered.pos` (`plone/folder/default.py:19-20`). `_order(True)` and `_pos(True)` create them at the first creation, with `setdefault` (`:162-190`).
- The meta type of a Dexterity item is `Dexterity Item` (`plone/dexterity/configure.zcml:50`): `_mt_index` holds that key alone, and no key `Document`.
- `_mt_index` itself does not change: the key of the meta type exists. The annotations do not change either.
- A creation inserts into a bucket of `pos` and of `_mt_index[meta_type]`, so BTrees marks the root of each read-current (`BTreeTemplate.c:754`). Up to 60 items the root of `pos` holds its bucket inline, and the creation stores the root.
- Of the 80 serial creations of probe f, 79 wrote one record of `pos` and 1 wrote three: the split of the bucket inline in the root, at the 61st item. Each wrote the record that holds its id.
- `pos` agrees with the order after each serial creation: the id gets `len(order) - 1` right after the append.

### 2. The conflicts, in the order of the merges

Ten rounds of eight creators from one snapshot, with the split merge on (probes b, c and d):

| setting | `ConflictError`s | on | after the rounds |
| --- | ---: | --- | --- |
| (b) the policy of `mechanism_off` | 70 of 70 turns 1 to 7 | the `Folder` record | 11 items: each round keeps its first creation |
| (c) + tier 2 entry for the `Folder` | 70 of 70 | the order list | 11 items |
| (d) + a merge of appends of the order list | 0 | | 81 items, `pos` wrong for 70 |

- In (b) the vote fails on the `Folder`, the stale oid with the lowest oid that no tier merges. In (c) tier 2 merges the `Folder` in 70 of 70 votes, and the vote fails on the order list, the next one.
- In (d) the resolution log shows what merges the rest: tier 1 and the split merge merge `_tree`, `_count`, `_mt_index[meta_type]` and `pos`, with their splits (rule 3 on the buckets of `_tree`, `pos` and `_mt_index[meta_type]`, rule 1 on 5 nodes), and the client resolves the trees of the intid utility (key references in a `NEWOBJ` that tier 1 does not model, as before ZOE-219 for the plain conflicts of their buckets). No other object conflicts.
- In (d) the order holds each of the 81 items once, `objectIds()` gives it, and `len(folder)` and `_mt_index` agree. `pos` does not: 70 of 81 ids have a wrong position, and 10 positions are shared, one for each round. Turn k of a round gave its id the position of turn 0, since all of them read the same order. The first id of each round is right.
- A wrong `pos` loses data later. `notifyRemoved` deletes `order[pos[id]]` and rebuilds `pos` from the order (`plone/folder/default.py:32-44`). With a wrong position, it deletes another id from the order: that item stays in `_tree` but leaves `objectIds()`. `getObjectPosition` gives the wrong index, and `objectIds(spec)` sorts by it (`plone/folder/ordered.py:79-90`).

So the order list must keep its conflict, as finding 5 of `docs/spikes/concurrent-creations.md` says, as long as no merge changes the order and `pos` together.

### 3. Where the ids of the transaction land in `pos`

For each of the 80 commits of the rounds of (e), the probe found the record of `pos` that holds the id of the commit, in the tree right after the commit:

| check | commits |
| --- | ---: |
| the record that holds the id is a record that the commit wrote | 80 of 80 |
| one record holds the ids of the commit | 80 of 80 |
| records of `pos` that the commit wrote: 1 / 2 / 3 | 72 / 7 / 1 |

Up to the 60th item, `pos` holds its bucket inline in the root, and each commit writes the root. At the round that passes 60 items, every creator of the round split that bucket, and the split merge (rule 3 of ADR 0049, with the flag `server_writes` of `zoe.Storage` under a ZODB database) wrote the two buckets of the committed side. After it, each creation inserts into the last bucket, which each commit writes. So the record that holds the id of a transaction is a record that its vote stages: the record that it stored (merged by tier 1 when it is stale), or the bucket Y that rule 3 writes.

### 4. Which merge keeps the order and `pos` in agreement

| candidate | the order | `pos` | records written | |
| --- | --- | --- | --- | --- |
| today: no resolver on the order | right | right | none | every creation but one of a round fails |
| the order alone (probe d) | right | 70 of 81 wrong | the order | loses an item at a later delete |
| the order, and the ids of the transaction renumbered in the records of `pos` that the vote stages | right | right | the order and the records of `pos` that hold the ids of the transaction (1 record in 80 of 80 commits) | |
| the order, and `pos` rebuilt from the merged order | right | right | every record of `pos` | writes buckets that the transaction did not store: a lock of ADR 0049 for each, a client with `server_writes`, a cost in the size of the folder |

The third candidate: the merged order is the committed order c followed by the ids N of the transaction. The merges of tier 1 and of the split merge on the records of `pos` give the committed positions with the inserts of the transaction, `N[i]` at `len(o) + i`, where o is the order that the transaction read: `bucket_merge` keeps each committed key and adds the new keys. Renumbering `N[i]` to `len(c) + i` in the record that holds it gives `{id: index}` of `c + N`, because the committed positions agree with c. The ids of the committed side keep their positions, so no record that only the committed side wrote changes.

Two more facts bear on that merge:

- A record of `pos` that holds an id of the transaction can be a record that the other side did not write: a site whose ids sort far apart, such as `a-...` and `z-...` in one folder of more than 60 items, puts the two ids in two buckets. The record of the transaction is then not stale, and its value must still change. The rounds of the test do not show it, because the ids of a round sort next to each other.
- The rule must find `pos` from the order. Only the annotations of the folder name both. `_mt_index[meta_type]` is an `OIBTree` with the same new keys and the value 1, which is also the position of the first new item when the folder holds one item: a pair found by its content would be a guess.

### 5. B5

One run of each scenario on `zoe-native-qi` with the split merge, at 4 and 16 users:

| scenario | users | operations | ok ops/s | 1st-try ok | failed | write | read | conflicts/1k | on |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| `create-shared` | 4 | 164 | 2.96 | 131 | 1 | 48 | 0 | 292.7 | `Folder` 48 |
| `create-shared` | 16 | 111 | 2.02 | 95 | 0 | 21 | 0 | 189.2 | `Folder` 21 |
| `create-spread` | 4 | 210 | 3.82 | 210 | 0 | 0 | 0 | 0.0 | |
| `create-spread` | 16 | 217 | 3.95 | 212 | 0 | 5 | 0 | 23.0 | `Folder` 5 |

- In `create-shared` every `ConflictError` is on the `Folder` of `bench/inbox`. The vote fails on the first stale oid without a merge, and the `Folder` has the lowest oid of the six, so the runs do not show what the vote would find after it. Probe c says the order list comes next.
- In `create-spread` the 5 `ConflictError`s are on a `Folder` too: two creations in one folder of a section, 11.7 per 1,000 operations over both steps. No other object conflicts in either run: every `ConflictError` of both runs is the case of this card.
- The resolution logs (412 lines for `create-shared`, 673 for `create-spread`, over the whole runs) hold the merges of every tree of the site, not only of the folders: tier 1 with no split (`OOBucket`, `OIBucket`, `OIBTree`, `Length`), rule 3 of the split merge (33 and 128 lines), rule 1 (5 and 19 nodes), and the buckets of the intid utility that the client resolves ("a NEWOBJ that the model does not model", 105 and 261 lines).
- The failed operation of `create-shared` at 4 users is a creation that exhausted its retries on the `Folder`.
- Creations in one folder commit one for each sync of the store, also once they merge: the `Folder` and its order are not classes of tier 1, so the lock hand-off of ADR 0044 never applies to them, and each vote waits until the commit before it is durable. So the rate of `create-shared` has the sync as its bound, with or without the mechanism; its `ConflictError`s and their retries are what the mechanism removes.

### 6. The acceptance test

- It fails today, as the card says: 3 of 4 tests fail, the test of the stock behaviour passes (4 tests, 6.3 s). In each of the 10 rounds, turns 1 to 7 raise `ConflictError` on the `Folder` (70 of 70).
- `test_the_order_holds_each_new_item_once_and_pos_agrees` ends with `_mt_index["Document"]`. The key is `Dexterity Item` (finding 1), so that line raises `KeyError` once the rounds pass. The planner must correct it: a builder never edits an acceptance test.
- With the mechanism of ADR 0052 the record of the `Folder` still needs a tier 2 entry: `mechanism_on` of the test writes `{"classes": {}, ...}`, with no entry. So the helpers change (ADR 0052, section 12).

## Consequences for the card

- The mechanism is a rule of the vote that merges the order list of a folder and renumbers the ids of the transaction in the records of `pos` that the vote stages, under a switch of the merge policy, with the record of the folder merged by tier 2: ADR 0052.
- It needs code in `crates/zoe-core/src/merge/` and `crates/zoe-core/src/commit/`, inside the scope fence, and new invariants in `crates/zoe-core/CLAUDE.md`, outside it. It needs no change of the format, of pack, of the lock manager or of the wire protocol.
- The first item of a meta type in a folder, the first order of a folder, and deletes, moves, sorts and renames keep their conflicts.
- The acceptance test needs the correction of finding 6 and the helpers of ADR 0052.
- Step 4 measures `create-shared` and `create-spread` on `zeo` and `zoe-native-qi` with the mechanism.

## Not measured

- What the vote finds after the `Folder` and the order on B5: the runs fail on the `Folder`, and no build of the rule exists. Step 4 measures it.
- `zeo` in this campaign.
- A site whose ids of one round fall in two buckets of `pos` (finding 4): the rule covers it, the probe does not.
- Folders of more than 15,000 items, where `pos` has a node under its root.
- A quiet machine, and more than one run of each scenario.
