# 0052. The order of a folder of plone.folder merges at the vote: the order list and its positions change together

Status: accepted. The checkpoint of ZOE-221 (step 2), revised after the adversarial review of its pull request. The human approved it with the recommendation of each question, except question 3: the `zoe.plone` profile switches `folder_orders` on. It adds a rule to the vote: the order merge. It merges the order list of a folder of plone.folder and the positions beside it, where ZODB has no resolver, under a new switch of the merge policy, off by default. It puts knowledge of plone.folder into the core. It lets the vote rewrite a record that the transaction stored and that is not stale. It changes no on-disk format, no pack and no wire protocol. It answers question 9 of ADR 0048, which kept two creations in one folder out of ZOE-219.

## Context

ADR 0048 and ADR 0049 merge the splits of BTrees, so concurrent creations of versioned content commit at the first attempt when each creator works in its own folder. Two creations in one folder still conflict. The spike of this card (`docs/spikes/folder-order.md`) finds:

- A creation of a Document in a folder of plone.app.contenttypes writes six records of the folder: the record of the `Folder`, `_tree`, `_count`, `_mt_index[meta_type]`, the order list and the positions. On the record of the `Folder` it changes only `modification_date` (80 of 80 serial creations). The order list is a `persistent.list.PersistentList` and the positions an `BTrees.OIBTree.OIBTree`, both in the attribute annotations of the folder, under the keys `plone.folder.ordered.order` and `plone.folder.ordered.pos` (`plone/folder/default.py:19-20`, `:162-190`).
- `DefaultOrdering.notifyAdded` appends the id to the order and gives it the position `len(order) - 1` in `pos` (`plone/folder/default.py:25-30`). So two creations from one snapshot give their two ids the same position.
- In rounds of eight creations in one folder from one snapshot, with the split merge on: today, every creation but the first of a round fails on the `Folder` (70 of 70). With a tier 2 entry that merges `modification_date` with `max`, every one fails on the order list (70 of 70). With a merge of the order list alone (appends on both sides, a resolver of a probe), all 80 creations commit, and `pos` is wrong for 70 of the 81 ids: each round gives its eight ids one position.
- A wrong `pos` loses data later: `notifyRemoved` deletes `order[pos[id]]` (`plone/folder/default.py:32-44`), so a delete of an id with a wrong position removes another id from the order, and `objectIds()` no longer lists it.
- In each of the 80 commits of the rounds, the id of the transaction is in a record of `pos` that the commit writes: a record that the transaction stored, or the bucket of the other side that rule 3 of ADR 0049 writes.
- `_tree`, `_count` and `_mt_index[meta_type]` merge today, with tier 1 and the split merge.

ZODB's `PersistentList` has no `_p_resolveConflict`, so ZODB refuses every conflict on it. ADR 0046 (section 11, point 8) warns against an entry for a stock class: the policy cannot tell one object of the class from another, and a merge of every `PersistentList` of a site would merge lists whose items depend on each other.

## Decision

### 1. Words

- The **folder** F is an object whose record holds the attribute `__annotations__`, a reference to its **annotations** A, a `BTrees.OOBTree.OOBTree`.
- The **order** L is the `persistent.list.PersistentList` that A holds under `plone.folder.ordered.order`. Its state is `{'data': [id, ...]}`.
- The **positions** P are the `BTrees.OIBTree.OIBTree` that A holds under `plone.folder.ordered.pos`: the root of the tree, its child nodes and its buckets. plone.folder keeps `pos == {id: index}` of the order.
- o, c and n are the old, the committed and the new states of L, as in ADR 0048: the record that the transaction read, the current record and the record that the transaction stored.
- N is the list of the ids that the transaction appended, `n[len(o):]`, and C the list of the ids that the committed side appended, `c[len(o):]`.

### 2. The order merge

The rule runs in the vote that takes the locks (`vote`, `vote_resolving`), as the split merge does. The first conflict of the vote makes its plan, with the plan of the split merge. A vote without a conflict makes no plan and reads nothing new.

1. **The order.** The rule runs on each stale written oid L whose staged record has the class `persistent.list.PersistentList`. The class pickles of the three records of L (old, committed, new) must all name `persistent.list.PersistentList`, else the rule gives no answer. It never runs on a `PersistentList` that is not the order of a folder.
2. **The folder.** The rule finds the folder among the records that the transaction stored (stale or not) whose class is not a class of tier 1: a record whose state is a dict with the key `__annotations__`, whose value is a reference to an oid A of the same database. The rule decodes these records with the heap of tier 2, within the step budget, until one names L. A must be an `OOBTree` that holds its bucket inline, with `plone.folder.ordered.order` a reference to L and `plone.folder.ordered.pos` a reference to an oid P. The rule reads the current record of A. When the transaction stored A too, the staged record of A must name the same L and the same P as the current record. When no stored record names L that way, the rule gives no answer.
3. **The list.** The three states of L decode as dicts whose only key is `data`, a list of strs. o must be a prefix of c and of n, and N must not be empty. The merged state is `{'data': c + N}`, with the class pickle of the new record.
4. **The positions that the transaction wrote.** The records of P are its root and the children of each of its nodes, in the old, the committed and the new state of each node. The rule reads only the nodes of P that the transaction stored or marked read-current, which the vote holds locked. Let S be the set of the buckets of P that the transaction stored, the new buckets of its splits among them, and the root of P when its old or its new state holds a bucket inline. The rule checks one equation over S, as maps of key to value:

   **the union of the new states of S equals the union of the old states of S, plus exactly the items `{N[i]: len(o) + i}`, which the union of the old states does not hold.**

   The old state of a stored bucket is its record at the serial that the transaction read. A new bucket of the transaction has no old state. The old state of a root counts only when it holds its bucket inline: that bucket is its old state. A key that a split of the transaction moves from a bucket to its new bucket is in both unions with one value, so it counts as unchanged.

   The rule also checks each node of P that the transaction stored: its new state must hold each child of its old state, unless the transaction stored that child. A node that drops a child that the transaction did not store gives no answer: the keys of that child would leave the tree unseen by the equation.

   Two walks through the check:

   - **A rename on create, in a folder of more than 60 items.** `plone.api.content.create` without an id creates the item under a temporary id, then renames it in the same transaction (`plone/api/content.py:124-125`). The rename runs `notifyRemoved`, whose `pos.clear()` and rebuild store a new root and new buckets X1 and X2, and drop the old buckets B1 and B2 without a write; `notifyAdded` then appends the new id. Seen key by key against the old tree, the change is one append. But the old states of S are empty (X1 and X2 are new, and the old root holds children, not a bucket inline), while the new states hold every id of the folder: the equation fails. The new root also drops B1 and B2, which the transaction did not store: the node check fails too. The rule gives no answer, and the vote fails on L as without the rule. A key check alone would have passed, and a commit of the other side that inserted into B1 would have lost its position.
   - **The same rename in a folder of at most 60 items.** P holds its bucket inline. S is the root alone: its old state is the old bucket, its new state is the old bucket plus `{N[0]: len(o)}`. The equation holds, no node drops a child, and the rule merges. The result is right: tier 1 merges the inline root of both sides, and the rule renumbers N[0].
5. **The renumbering.** After the vote checked every written and read-current serial, and after tier 1 and the split merge gave the records that the vote stages for the oids of P, the rule changes the value of each id N[i] to `len(c) + i` in those records. Those records are: the merged record of tier 1 or of the split merge for a stale record of P, the record that the transaction stored for a record of P that is not stale, and the bucket Y of the other side that rule 3 of ADR 0049 writes. Each id of N must be in exactly one of them. A record that the split merge resolves without a record (the new bucket X of ADR 0049) is not among them: its keys are in A or in Y.
6. **What the vote stages.** L over the serial of its committed record. Each record of P that holds an id of N, with the new values: over the serial of the committed record for a stale record or for Y, as the merges stage today, and over the serial that the transaction read for a record that is not stale. The size limits apply to each record; a record over a limit gives no answer. The reply of the vote lists L and each record of P that the rule changed as resolved, as for any merge.
7. **The self-check.** Before the vote stages anything for the rule, the rule reads back the records of step 6: each id N[i] is in exactly one of them, with the value `len(c) + i`; each other item of these records is the item that tier 1 or the split merge gave; and the merged list holds each id once. A check that fails gives no answer. In mode `shadow` the rule runs the same renumbering and check on the records of the positions that the transaction stored, since the vote fails on the order before it merges the other records, and its line says what the check gave (section 9): no client resolution verifies the rule.
8. **Before tier 2.** The rule takes L before tier 2. When the rule finds that L is the order of a folder (step 2), tier 2 never merges L, whatever the policy says and whatever the outcome of the rule: a merge of the order alone corrupts `pos` (spike, finding 2). `zoe.policy.validate` also refuses a policy with a tier 2 entry for `persistent.list.PersistentList` whose `tier2` is not `off`, while `folder_orders` is not `off` (question 12). A `PersistentList` that is not the order of a folder keeps its entry of tier 2, if the policy gives one.

The rule writes no record that the transaction did not store, beyond the bucket Y that the split merge writes already. It takes no new lock: L, the stored records of P and F are locked exclusively, the nodes of P on the path of the transaction are locked shared, and Y keeps the lock of ADR 0049. It never writes A or F: tier 2 writes F.

**The outcomes.** A refusal fails the vote with `ConflictError` on L at once. No answer leaves the conflict on L as without the rule: `Conflict` for `vote`, and the client of `vote_resolving` finds no `_p_resolveConflict` on `PersistentList` and raises `ConflictError`. So a case without an answer keeps the stock behaviour, and the vote stages nothing of P for the rule.

**Notes for the build.**

- The plan covers L as the plan of the split merge covers its oids: the check of the written serials skips L, and the vote stages its merged record.
- The equation of step 4 reads the staged new bucket X of the transaction before the vote drops the oids that the split merge resolves without a record: X holds keys of N until then.
- L leaves the list of conflicts that `vote_resolving` gives back to the client, as any merged oid.
- A record of P that the vote stages again goes through the staging (`staged.put`), which drops an encoded payload of the oid (`Encoded`, the invariant of the staging), so the finish encodes the new pickle.

### 3. Which changes merge

Only appends on both sides merge: o is a prefix of c and of n.

- **A refusal.** An id of N that c holds: both sides added the same id. `_tree` refuses two inserts of one key already, and the rule refuses them too, so the order never holds an id twice.
- **No answer.**
  - A delete on either side: `notifyRemoved` deletes an item of the order and rebuilds `pos` from scratch (`plone/folder/default.py:32-44`), so o is not a prefix, and the transaction changed other keys of P.
  - A move (`moveObjectsByDelta`), a sort (`orderObjects`) and a rename of an existing item (`manage_renameObject`, a delete, an add and a move) on either side: o is not a prefix.
  - A rename on create on the committed side, in a folder of more than 60 items: its rebuild gives the root of P new children, a change by more than splits. The transaction marked that root read-current, rule 1 of ADR 0048 gives no answer, and the vote fails with `ReadConflict` on the root.
  - A transaction that appended nothing (N is empty), an item that is not a str, an id twice in N or in C, an id of N or C that o holds.
  - Changes of P by the transaction that break the equation of section 2, step 4 (a value of another key, a delete, another key, a rebuild of `pos` into new buckets), or a stored node of P that drops a child that the transaction did not store. A rename on create in a folder of more than 60 items is such a case; in a folder of at most 60 items it merges (section 2, step 4).
  - A record of P that tier 1 or the split merge merged onto a commit in flight of a hand-off (ADR 0044): the rule reads L, A and P from the store.
  - No stored record that names L through its annotations (another ordering, a list that is not an order), annotations that are not an `OOBTree` with its bucket inline, a staged A that names another L or P than the current A, a record of L whose class is not `persistent.list.PersistentList`, a record that does not decode, a state that the model does not hold, a merge past the step budget of `merge::budget`, a self-check that fails.
- **The first order of a folder.** The first creation in a folder creates L and P and inserts them into A (`setdefault`, `plone/folder/default.py:162-190`). Two first creations from one snapshot both insert the same keys into A, and tier 1 refuses A. The rule does not take this case.
- **The first item of a meta type in a folder** (point 7 of the planner). Two first creations of a meta type both insert the key of the meta type into `_mt_index`, each with an `OIBTree` of its own, and tier 1 refuses two inserts of one key. This card does not take it (question 6). It happens once for each meta type and each folder.

### 4. The order of the merged items

The merged order is the committed order, then the ids of the transaction in the order in which the transaction appended them. Each id of the committed side keeps its position, so the rule changes no record that only the committed side wrote. A round of creations from one snapshot gets the order of its commits. The acceptance test asks only that each round comes after the rounds before it.

### 5. The record of the folder

The rule does not merge F. A creation changes only `modification_date` of F, and tier 2 merges it with an entry of the class of the folder (ADR 0046):

```python
{"plone.app.contenttypes.content.Folder": {"tier2": "on", "default": "strict", "fields": {"modification_date": "max"}}}
```

- The profile of `zoe.plone` derives such an entry for the class of each Dexterity type, in shadow mode (ADR 0046, section 9). A site switches the entries of its container classes on in its overrides file, or with `zoe.policy.install`. The switch of section 6 does not do it: the entry is the policy of a class, and tier 2 already owns it.
- The classes: every class of an `IOrderableFolder` whose ordering is `DefaultOrdering` of plone.folder: `plone.app.contenttypes.content.Folder`, the other Dexterity containers, add-on folder types and the Plone site root (`Products.CMFPlone.Portal.PloneSite`, a Dexterity container in Plone 6). The rule reads the annotations, not the class, so it needs no list of classes; each class needs its tier 2 entry. A folder with another ordering (`unordered`, `partial` or a custom adapter) keeps the stock behaviour: the orderings `unordered` and `partial` are out of this card.

### 6. The switch

- The merge policy gets the switch `folder_orders`, with the modes of tier 1: `off`, the default of the core, `shadow` or `on`. The profile `zoe.plone:default` switches it on: its overrides file `zoe-merge-policy.json` holds `{"folder_orders": "on"}`, so the import step of the profile writes the switch into the policy of the site, and the overrides of a policy package of the site can set it back to `off` (a decision of the human, question 3). The profile does not switch `split_merges` on (ADR 0048, question 10), and its derived entries of tier 2 stay in shadow mode (ADR 0046, section 9): a site switches the entries of its container classes on (section 5) and `split_merges` on, in its overrides. Until then the switch changes nothing for the site: the vote fails on the record of the folder before it reaches the order. `zoe.policy.SWITCHES` holds it, so `zoe.policy.install`, `validate` and the overrides file of a profile of `zoe.plone` take it, as `split_merges`. No key of `<zoe>`, of `zoe.Storage` or of `zoe serve`: the policy is a record of the store, so the switch is the same in both modes and a copy of the store keeps it.
- The rule runs on the records of P only where the mode of tier 1 of their class is not `off`, and in the mode of both (the lower one), as the split merge does. The vote finds the orders whenever the switch is not `off`, whatever the mode of tier 1, so tier 2 never merges one (section 2, step 8).
- The database keeps the latest mode of `folder_orders` that each of its sites asked for, by the path of the site (`root['zoe.plone.folder_orders']`), and the policy takes the lowest of them (`off`, then `shadow`, then `on`; `zoe.plone.merges.lowest_mode`). So an `off` of one site lasts when another site of the database imports the profile, with a warning in the log, and a site that raises its own mode raises the policy when no other site asks for less. Every other switch keeps the rule of ADR 0046: the new value wins.
- A mode for the whole database goes through the overrides of a site. `update_policy` gives the policy the lowest mode of the sites, so an `off` that `zoe.policy.install` writes lasts only until the next `update_policy` of a site that asks for more: an import of a profile, or an edit of a Dexterity type. That update replaces the lower mode of the policy with a warning that names `folder_orders` and the replaced mode. When the policy holds no `folder_orders`, the update logs no such warning: the first import of the profile is not a change for the operator.
- `update_policy` drops the mode of a site that is gone: the walk of its path from the Zope root stops at an object of the database of the updated site that holds no item of the next id. So the mode of a removed site goes at the next update of another site, and a renamed or moved site gives its mode under its new path at its own next update; between the two, the database does not count it. A walk that stops in another database (below a mount point), meets an object that is not a container, or fails keeps the path, so a site in a mount point keeps its mode when a site of another database updates. The limit: a database that is the root database of one Zope instance and is mounted in another one keeps the modes of the sites of both, and an update in the first instance drops the modes that the second one wrote below its mount point, until their sites update again. An operator clears a mode by hand with `del root['zoe.plone.folder_orders'][path]`.
- The switch does not need `split_merges`: the rule renumbers the records that the vote stages, whatever merged them. A folder whose `pos` or `_tree` splits needs `split_merges` too, else its creations conflict where both sides split the same bucket. A site switches both on.
- In mode `shadow`, the rule runs, writes its outcome in the field `shadow` of its lines, and the vote goes on as without it. `PersistentList` has no `_p_resolveConflict`, so the client raises `ConflictError` and no client resolution verifies the rule (ADR 0046, section 6). The self-check of section 2, step 7 is the only check of shadow mode.
- **The companions.** The intended setup is a site with queued indexing (ADR 0047) and `split_merges` on. Queued indexing indexes the new items after the commit, from the committed order (section 7, "Values derived from a position"). The split merge merges the splits of `pos`, `_tree` and `_mt_index[meta_type]`. The documentation of the switch names both.
- A core of an older build reads a policy with `folder_orders` as no policy, with a warning, as for any unknown key (ADR 0046, section 12, point 21). A site updates its servers before it writes the switch.

### 7. Why no update is lost

Let c and P_c be the committed order and the committed positions. plone.folder keeps `P_c == {id: index in c}`: each transaction that writes P through `DefaultOrdering` writes L in the same transaction and keeps the agreement, and the committed state is the state of the commits one after the other.

- **The order.** The merged order `c + N` holds every item of the committed order at its position, then each id of the transaction once (the refusal of section 3). It is the order that the transaction would give if it ran after the committed side: `notifyAdded` appends to the order that it finds.
- **The positions.** The merges of tier 1 and of the split merge on the records of P give `P_c` with the inserts of the transaction: `bucket_merge` keeps each key of the committed side, the keys of N are new keys, and the split merge routes each key of the range to the bucket that holds it (ADR 0048, section 3; ADR 0049, section 6). The rule then gives each N[i] the value `len(c) + i`. So the merged positions are `{id: index in c + N}`: they agree with the merged order. The rule changes values only, never a key, a separator or a child, so every lookup of the tree routes as before.
- **The records that the rule changes** hold only values of ids of N, which only the transaction inserted. So no change of another transaction is overwritten. A record of P that is not stale holds the last commit of that record: its serial is the one that the transaction read, under its exclusive lock.
- **`_tree`, `_count`, `_mt_index` and F.** `_tree` and `_mt_index[meta_type]` merge inserts of different keys with tier 1 and the split merge; two inserts of one key refuse. `_count` is a `Length`: tier 1 adds the two changes. `_mt_index` itself does not change. F merges `modification_date` with `max` in tier 2; every other field of F is `strict`.
- **One vote.** The vote stages all these records together, or fails and stages none. So the folder never holds an id in `_tree` that its order lacks, or the reverse.
- **The read of L.** L is not a class of tier 1, so its lock is never handed off (ADR 0044): when the vote holds the lock of L, the last commit of L is durable. The build makes this an invariant of the vote: the lock of the order of a folder is never handed off, also when a policy names its class. Each writer of P through plone.folder writes L in the same transaction, so the committed records of P belong to the commits of L that the vote reads. The plan of the order has no wait of its own. With `split_merges` on, it relies on the wait of the plan of the split merge, which waits for the commits in flight of its hand-offs before it reads. That wait does not cover every record of P: a handed record of P whose oid is lower than the first stale oid can merge in tier 1 before the plan of the split merge waits. Without `split_merges`, no plan waits. In both cases the guards of `commit/order.rs` keep the result right: the rule gives no answer for a record of P merged onto a commit in flight, or whose commit in flight has not ended.
- **The read of A.** The vote does not lock A. The rule reads only the two references of A, to find L and P. A writer that replaces the order list (the deprecated `_set_order`) writes A and a new list, not L. Then the current A names another list and the rule gives no answer; or that writer commits after the vote and the stock path would lose the append of the transaction in the same way, since that writer does not write L. The rule loses nothing that the stock path keeps.
- **A reader.** A connection reads L and P at one snapshot. The commit holds L and every record of P that the rule changed, under one tid, so a reader sees both before the commit or both after it. Each other connection gets L and these records in the invalidation of the commit: the transaction stored them. The connection that committed gets them as resolved oids in the reply of the vote, so it turns them into ghosts. Y follows ADR 0049.
- **Values derived from a position.** The rule changes the position of each id of N after the transaction ran. A value that the transaction derived from that position, in another record, would keep the old position. The catalog of Plone 6.1 holds none: its index `getObjPositionInParent` is a `GopipIndex` of plone.folder (`Products/CMFPlone/profiles/default/catalog.xml:138-142`, `plone/folder/nogopip.py:63-125`), which stores no value and computes the positions from the order of the folder when it sorts, and no metadata column holds the position. A site that replaces that index with an index that stores values, or a subscriber that writes a position in the transaction of a creation, would keep the old position. With queued indexing, the indexer reads the committed order after the commit. The argument assumes that no other record of the transaction holds a value derived from the position of an id of N.
- **What the argument assumes.** Writes of the order go through `DefaultOrdering`, which writes L and P together. A program that writes `pos` without the order, or changes the order without `pos`, breaks the agreement of plone.folder whatever the store does.

### 8. A site on ZEO, a stock client, a client with `server_writes`

- **A site on ZEO, a FileStorage, a ZOE store without the switch**: the stock behaviour. From one snapshot, the first creation in a folder commits. Each other creation raises `ConflictError` on the `Folder` (without a tier 2 entry) or on the order list (with one), and the publisher of Zope retries it from a new snapshot. On a ZEO server or a FileStorage the switch is a record that nothing reads.
- **A stock `ZEO.ClientStorage`, the asyncio mode of `zoe.zeo.ClientStorage`, the native client, `zoe.Storage`**: nothing to declare. The rule writes only records that the transaction stored, and the reply lists them as resolved. A stock client gets the merge of the server core as in-process.
- **`server_writes`** (ADR 0049) matters only to rule 3 of the split merge. Without the flag, rule 3 gives no answer where both sides split the same bucket of `pos`, `_tree` or `_mt_index[meta_type]`, so a creation in one folder still conflicts at those splits: about once every 15 creations for `_tree`, whose last bucket splits at 31 keys into two halves (`docs/spikes/concurrent-creations.md`, finding 3). With the flag, the rule renumbers the ids of the transaction in Y too.

### 9. The resolution log and the counters

- L gives a line of tier 1 with the class `persistent.list.PersistentList`, the outcome `merged`, `refused` (reason "an id that both sides added", a new `Reason`) or `undecided` (with the reason of section 3), `keys` the ids of N, `changed` their number, and the new field `order`: `"list"`.
- A record of P that the rule changed adds `"order":"pos"` to its line of tier 1 or of the split merge. A record of P that is not stale gets a line of its own: tier 1, its class, `merged`, `read` equal to `committed`, `keys` the ids that the rule renumbered there, and `"order":"pos"`.
- In mode `shadow`, the line of L has the outcome `undecided` with the reason "an order in shadow mode", and `shadow` holds the outcome of the rule: `merged` when the self-check of section 2, step 7 passed, else `undecided` with the reason "a self-check that failed". The vote goes on as without the rule.
- A stale `PersistentList` for which the rule found no folder gives no line: the rule did not run on it.
- `zoe_structural_merges_total{storage, tier, mode, outcome}` of `zoe serve` counts these lines with `tier="1"`, with no new label and no new counter. The field `order` of the log tells the lines of the rule apart. The reasons are texts for the operator: no test fixes them.

### 10. A copy, a pack and an undo

- **A copy** through the iterator (`zodbconvert`, `copyTransactionsFrom`, `zoe convert`), `zoe backup` and `zoe restore` copies the records as they are, the policy with its switch among them. A restore transaction runs no rule.
- **A pack** needs no change. The rule takes no record out of a tree and leaves no garbage.
- **An undo** runs no rule, as for the split merge: an undo has three records of each oid and no vote of its own transaction. An undo of a creation T whose order no later commit changed gives back L and the records of P before T, which agree: each record that T wrote has the record of the committed side before it. An undo of T after a later commit of L meets three records of a `PersistentList`: no tier merges them, the client finds no resolver, and the undo fails with `UndoError`, as in ZODB.

### 11. What the build changes, and where (the scope fence)

- `crates/zoe-core/src/merge/`: a new module `merge::order` (the folder, the list, the changes of P, the renumbering, by states), the switch in `merge::policy`, the field `order` and the new `Reason` in `merge::Resolution`.
- `crates/zoe-core/src/commit/`: the plan of the rule beside the plan of the split merge, L covered in the check of the written oids, the renumbering after the checks, a stored record that is not stale staged again, the lines. `commit/group.rs` builds a `Resolution` and takes the new field.
- `crates/zoe-pickle/`: no change expected. The rule decodes lists of strs and the states of `OIBTree` and `OOBTree`, which the models of tiers 1 and 2 hold, and pickles them as tier 1 does. A change there stays inside the fence.
- `crates/zoe-core/tests/`: section 13.
- `python/zoedb/src/zoe/policy.py`: `SWITCHES`, the refusal of section 2, step 8 in `validate`, and the documentation of the switch and of its companions. `zoe/plone/merges.py` takes it through `SWITCHES`; its documentation names it.
- `bench/`: the option of section 14.
- `docs/spec/technical-specification.md`: section 5 (the rule, the switch, the field `order`), the row F6 and the table of the tests; `docs/adr/0048-the-split-merge.md`: question 9 points to this ADR.
- `crates/zoe-core/CLAUDE.md`: new invariants (the order merge, its equation and its node check; a stored record that is not stale that the vote stages again; a `PersistentList` merges only as the order of a folder, and tier 2 never merges an order; the lock of the order of a folder is never handed off). This file is **outside the scope fence** of the card (question 10).
- No change of `format/`, of `pack/`, of `lock/`, of the wire protocol, of the reply of `vote` or of `invalidateTransaction`.

### 12. The helpers of the acceptance test

The switch keeps the name of the test, `folder_orders`. The record of the folder needs the tier 2 entry of section 5, so both helpers write it, and only `mechanism_on` writes the switch:

```python
def mechanism_on(test):
    import zoe.policy

    folder = {"tier2": "on", "default": "strict", "fields": {"modification_date": "max"}}
    policy = {
        "classes": {"plone.app.contenttypes.content.Folder": folder},
        "split_merges": "on",
    }
    if SWITCH in zoe.policy.SWITCHES:
        policy[SWITCH] = "on"
    zoe.policy.install(test.layer["portal"]._p_jar, policy)
    transaction.commit()
    return {}


def mechanism_off(test):
    import zoe.policy

    folder = {"tier2": "on", "default": "strict", "fields": {"modification_date": "max"}}
    zoe.policy.install(
        test.layer["portal"]._p_jar,
        {"classes": {"plone.app.contenttypes.content.Folder": folder}, "split_merges": "on"},
    )
    transaction.commit()
    return {}
```

So the test without the switch shows the conflict that the switch removes, on the order list (question 4). Only the two helpers change, and their docstrings say what they write.

**The assertion on `_mt_index`.** `test_the_order_holds_each_new_item_once_and_pos_agrees` reads `_mt_index["Document"]`. The key of `_mt_index` is the `meta_type` of the item, and a Dexterity item has the `meta_type` `Dexterity Item` (`plone/dexterity/configure.zcml:50`). The probe of the spike finds only that key. So the last assertion of the test raises `KeyError` once the rounds pass. The planner corrects it (`_mt_index[folder[FIRST].meta_type]`, or `"Dexterity Item"`) before the build, and the docstrings of the module that name `_mt_index["Document"]` with it: the builder never edits the acceptance test (question 5).

### 13. The tests

- `crates/zoe-core/tests/folder_orders.rs`, the vote, with records built as plone.folder writes them (F with `__annotations__`, A inline, L, P):
  - appends on both sides, one and several commits on the committed side, one and two ids of the transaction: L is `c + N`, P agrees, the reply lists L and the records of P;
  - P with its bucket inline, P with buckets where the ids of the two sides are in one bucket, and in two buckets, so that the record of the transaction is not stale and is staged again;
  - P with a node under its root (three levels), so that the rule reads a child node on the path of the transaction;
  - P split by both sides, with `server_writes` (the ids of the transaction in Y) and without (rule 3 gives no answer, so the vote fails on the bucket); P split by the transaction alone, so that the rule renumbers its new bucket X;
  - the rename on create of section 2, step 4: in a P of buckets, the rebuild into new buckets gives no answer, by the equation and by the node check, each on its own; in a P with its bucket inline, it merges;
  - the refusal of one id on both sides;
  - each other case without an answer of section 3: a delete, a move, a sort and a rename on either side, an empty N, an item that is not a str, a record of L of another class (old, committed or new), no stored record that names L, a staged A that names another L or P, a record of P merged onto a commit in flight (a gate holds the finish of a transaction that writes the positions without the order, and hands the lock of `pos` off to the creation), the mode of tier 1 of `OIBucket` `off`;
  - two votes at once on one folder: the second waits for the lock of L until the first is durable, then merges;
  - a policy with a tier 2 entry for `persistent.list.PersistentList`: the rule takes L, and tier 2 never merges an order, in modes `on` and `shadow` of the switch; a `PersistentList` that is not an order keeps its entry;
  - the switch `off`, `shadow` and `on`, and a policy without it; the self-check in mode `shadow`;
  - the lines of section 9;
  - an undo of a creation that the rule merged.
- Unit tests of `merge::order`: the decode of the list, the class checks, the prefix, the equation and the node check, the renumbering, the self-check, the budget.
- `python/zoedb/tests/`: a model without Plone: an object with attribute annotations, its `PersistentList` and its `OIBTree` under the keys of plone.folder, and the code of `notifyAdded` and `notifyRemoved`, in rounds from one snapshot past a split of `pos`, against `zoe.Storage`, and against `zoe serve` with the native client and with a stock `ZEO.ClientStorage`. After each round, in another connection of the process that committed and in another process: `pos == {id: index}` of the order, each id once, `BTrees.check` on `pos`. The stock client commits each round but those of a split of one bucket. The model also runs:
  - the rename on create (a delete of a temporary id, then an append, in one transaction) against an append of the other side, in a folder of more than 60 items (a `ConflictError`, and the order and `pos` agree after the retry) and of at most 60 items (both commit);
  - a new bucket X that the rule renumbers, in-process, with the native client and with a stock `ZEO.ClientStorage`: the other side inserts an id into the first bucket of `pos`; the transaction appends two ids and splits the full last bucket into it and X, which holds its ids. No other transaction wrote X, the last bucket or the root, so the rule renumbers X and stages it again, and the reply lists X as resolved: `Connection.tpc_vote` of the Connection that committed turns X, an object that the transaction created, into a ghost. That Connection then reads every key of `pos` after the commit, with the right positions, and so does another process.
- `python/zoedb/src/zoe/plone/tests/`: tests of the builder (not acceptance tests) on a site of the functional layer copied to a `zoe.Storage`, with the mechanism on:
  - with queued indexing: rounds of creations in one folder, then `drain`; a search of the catalog sorted on `getObjPositionInParent` gives the order of the folder, and the index stores no position;
  - with the stock indexing: the creations of a round commit, or raise `ConflictError` and commit at their retry, and the order, `pos` and the sorted search of the catalog agree;
  - the import of the profile `zoe.plone:default` writes `folder_orders` `on` into the policy, and an override of `off` wins.
- The acceptance test of the planner, with the helpers of section 12. It covers one folder, one process, `zoe.Storage` and the inline and split states of `pos` in rounds from one snapshot. It never reaches a record of P that is not stale (the ids of a round sort next to each other), two votes at once, a stock client, or a node under the root of P: the tests above carry them.
- `zoe.policy.validate` takes `folder_orders`, and refuses a tier 2 entry for `persistent.list.PersistentList` while the switch is not `off`; the overrides of `zoe.plone.merges` take the switch.

### 14. Step 4: the measure

- B5 `create-shared` (the one folder `bench/inbox`) and `create-spread`, `--steps 4:60,16:60`, on `zeo` and on `zoe-native-qi`, one repetition of each cell, on a quiet machine.
- `zoe-native-qi` with the mechanism: the harness gets the option `--folder-orders MODE`, as `--split-merges` (question 11 of ADR 0048). It writes the switch `folder_orders` and the tier 2 entry of section 5 for `plone.app.contenttypes.content.Folder` into the store of the run, keeps the other classes and switches of the policy, and `result.json` records the policy. A run takes `--split-merges on --folder-orders on`.
- The resolution log counts the lines with `order` and the conflicts that are left, by class.
- The baseline: the runs of finding 5 of the spike, on `zoe-native-qi` with `--split-merges on` alone.

### 15. Notes of the build

- Step 4 ran one repetition of each cell (B5, `--steps 4:60,16:60`), at `2b337bfa`. `create-shared` (one folder) gives 106 and 42 `ConflictError`s on `zeo` at 4 and 16 users (779.4 and 368.4 per 1,000 operations), and 0 and 1 on `zoe-native-qi` with `--split-merges on --folder-orders on` (0.0 and 8.8 per 1,000). The one is a read conflict, "a path child that the other side split" on an `OIBTree`: the case of question 6 of ADR 0048. The order merge merged 152 orders, each with its line `pos`. `create-spread` gives 107 and 93 on `zeo` and 0 and 0 on `zoe-native-qi`. Every `ConflictError` of `zeo` is on `UniqueIdGeneratorTool`. The mean batch of a sync of `zoe serve` is 1.00 commit, as section "Consequences" says of creations in one folder.
- The catalog of Plone 6.1 stores no position (section 7).
- The build of step 4 held a guard that this ADR does not name: no answer for a transaction that wrote no key of the queue of queued indexing. The review removed it. The runs of `zoe-native-qi` queue their indexing, so the guard did not change them.
- In mode `shadow`, the self-check reads the records of the positions that the transaction stored (section 2, step 7).

## Answers to the points of the planner

| Point | Answer | Section |
| --- | --- | --- |
| 1. The record of the folder | Only `modification_date` changes at a creation (80 of 80). Tier 2 merges it with `max`, through an entry of the class in the policy of the site; the derived entries of `zoe.plone` stay in shadow mode until a site switches them on. | 5 |
| 2. Which classes | Every folder whose annotations hold the order and the positions of `DefaultOrdering`, whatever its class; each container class needs its tier 2 entry. `unordered`, `partial` and custom orderings keep the stock behaviour. | 5 |
| 3. Where the merge runs | A rule of the core that finds the pair through the annotations of the folder: no stock class is marked, no data changes. | 2, Alternatives |
| 4. The order and `pos` together | One vote stages L and each record of P that holds an id of the transaction, renumbered, when one equation over the stored buckets of P holds and no stored node drops a child. The rule writes no record that the transaction did not store beyond Y of ADR 0049, and needs no new lock call. | 2, 7 |
| 5. Which changes merge | Appends on both sides only; the same id on both sides refuses; deletes, moves, sorts and renames give no answer. | 3 |
| 6. The order of the merged items | The committed order, then the ids of the transaction. | 4 |
| 7. The first item of a meta type | Out of this card. | 3, question 6 |
| 8. No lost update | The argument covers L, P, `_tree`, `_count`, `_mt_index`, F and an old reader. | 7 |
| 9. The switch | `folder_orders` in the merge policy, `off`, `shadow`, `on`, off by default in the core and on in the profile of `zoe.plone`, independent of `split_merges`; shadow mode has only its self-check. | 6 |
| 10. ZEO, FileStorage, clients | The stock behaviour and a retry; no client declares anything; `server_writes` matters only to rule 3 of the split merge. | 8 |
| 11. The log and the metric | A line of tier 1 for L with `"order":"list"`, `"order":"pos"` on the records of P; `zoe_structural_merges_total` counts them. | 9 |
| 12. Copy, pack, undo | As is; no garbage; an undo runs no rule. | 10 |
| 13. The scope fence | The code fits `merge/**` and `commit/**`; `zoe-pickle` likely unchanged; `crates/zoe-core/CLAUDE.md` is outside the fence; `docs/spec` changes. | 11 |
| 14. Step 4 | `create-shared` and `create-spread` on both stacks, with `--folder-orders on`. | 14 |

## Consequences

- With the switch, the split merge and a tier 2 entry for the class of a folder, concurrent creations in one folder commit at the first attempt, and the order of the folder and `pos` agree.
- The core knows plone.folder: the two keys of its annotations and the shape of its order. A change of these in a later plone.folder makes the rule give no answer, so the stock conflict comes back; nothing is merged wrong.
- A vote can stage again a record that its transaction stored and that is not stale: a bucket of `pos` that the other side did not write. The reply lists it as resolved. A new invariant of the vote.
- A `PersistentList` merges only as the order of a folder that the annotations of a stored record name, and tier 2 never merges an order. Every other `PersistentList` keeps the stock behaviour, or its entry of tier 2.
- The profile of `zoe.plone` switches the rule on, but alone it merges no order: its derived entries stay in shadow mode, so the record of a folder conflicts before its order, and `split_merges` is off. A site gets the merge once its overrides switch on the entries of its container classes, and `split_merges` where the positions of a folder split.
- Creations in one folder commit one after the other, one for each sync of the store. The `Folder` and its order are not classes of tier 1, so their locks are never handed off (ADR 0044), and the vote of each creation waits until the commit before it is durable. The rule removes the `ConflictError`s and their retries, not that wait. B5 `create-shared` reads with that in mind: its rate in one folder has the sync as its bound.
- A value that the transaction derived from the position of a new item, in another record, keeps the old position. The catalog of Plone 6.1 holds no such value.
- Deletes, moves, sorts and renames in a folder still conflict with a concurrent change of the same order, as with ZODB.
- A store without the switch, a site on ZEO, and a commit with no conflict take the same path as today.
- Section 5 of the specification and the row F6 change.

## Alternatives

- **A merge of the order list alone**, as a tier 2 entry with `append` on `data` for `persistent.list.PersistentList`, or a resolver. It marks a stock class (ADR 0046, section 11, point 8), and `pos` stays wrong: the probe of the spike measures 70 wrong positions of 81, and a later delete removes the wrong id from the order. Refused.
- **A rebuild of `pos` from the merged order.** It writes every record of `pos`, most of which the transaction did not store: a lock of ADR 0049 for each bucket, a client with `server_writes`, and a cost in the size of the folder. The renumbering writes only the records that hold the ids of the transaction (one record in 80 of 80 commits of the spike). Refused.
- **An ordering of `zoe.plone`** (an adapter registered for `IOrderableFolder`) whose data the tiers merge: for example an order list of a class of `zoe.plone` with `append`, and positions computed from the list, not stored. It changes the data of each folder, needs a migration and a way back, makes `getObjectPosition` linear in the size of the folder or adds a cache of each connection, and a site on ZEO with that data gets a class that plone.folder does not know. Refused for this card.
- **Positions that need no renumbering**, such as values from a sequence of the store: `getObjectPosition` and `moveObjectToPosition` of plone.folder give and take indexes, so it changes the meaning of `pos`. Refused.
- **The pair recognised by its content**, without the annotations: a `PersistentList` and an `OIBTree` whose inserts are the appended ids with their positions. `_mt_index[meta_type]` is an `OIBTree` with the same keys and the value 1, which is also a position when the folder holds one item. That is a guess. Refused.
- **The switch on the record of the folder**: the rule also merges `modification_date` of F, with no entry. It duplicates tier 2 and would merge F with no policy of its class. Refused.
- **The switch on by default.** The rule merges where ZODB refuses, so a store opts in, as for the split merge.

## Questions for the human

Each question gives Claude's recommendation first. The adversarial review of the pull request found the design sound with changes; the text above holds its changes, and each recommendation below is also the recommendation of the review.

1. **The mechanism.** Recommended: the order merge of section 2, in the core, with the pair found through the annotations of a record that the transaction stored, and with the equation and the node check of step 4. Other choices: an ordering of `zoe.plone` with data that the tiers merge (a change of the data of each folder, a migration and a way back); a rebuild of `pos` (writes every bucket of `pos`, with a lock and `server_writes` for each).
2. **Knowledge of plone.folder in the core.** The rule names the keys `plone.folder.ordered.order` and `plone.folder.ordered.pos`, the classes `persistent.list.PersistentList`, `BTrees.OOBTree.OOBTree` and `BTrees.OIBTree.OIBTree` and the attribute `__annotations__`, as the split merge names the classes of key references. Recommended: accept, under the switch, with exact class checks on the three records of L, on A and on P.
3. **The switch.** Recommended: `folder_orders` in the merge policy only, off by default, with the three modes of tier 1, independent of `split_merges`; the profile of `zoe.plone` does not switch it on; the documentation names queued indexing (ADR 0047) and `split_merges` as its companions (section 6). Other choices: the switch implies `split_merges`; or a key of `<zoe>`, `zoe.Storage` and `zoe serve` too. **Decision of the human:** `folder_orders` is off by default in the core, and the profile `zoe.plone:default` switches it on.
4. **The record of the folder and the helpers.** Recommended: F merges through the tier 2 entry of its class, which a site switches on; both helpers write the entry of `plone.app.contenttypes.content.Folder`, and only `mechanism_on` writes the switch (section 12). Other choices: `mechanism_off` keeps its text, so the test without the switch fails on the `Folder`; or the switch also merges `modification_date` of F (refused in "Alternatives").
5. **The assertion on `_mt_index["Document"]`** of the acceptance test raises `KeyError`, because the key is the meta type `Dexterity Item`. Recommended: the planner corrects the assertion, and the docstrings that name it, before the build. Other choice: the builder takes the correction with the helpers, with the approval of the human.
6. **The first item of a meta type, and the first order of a folder.** Recommended: out of this card; each happens once for each folder (and each meta type), and the test avoids both. A later card can merge two new `OIBTree`s under one key of `_mt_index` with the machinery of ADR 0049, if B5 shows it. Other choice: in this card.
7. **Deletes, moves, sorts and renames, the rename on create in a folder of more than 60 items among them.** Recommended: no answer, the stock conflict; the B5 scenarios do none. Other choice: a later card for a delete against an append, which needs a rebuild of `pos`.
8. **A record that is not stale, staged again.** Recommended: accept the new invariant of the vote (section 2, step 6), with the test of a new bucket X that the rule renumbers, through a stock `ZEO.ClientStorage` and the Connection that committed (section 13). Other choice: no answer when an id of the transaction is in a record of `pos` that is not stale (two creations whose ids fall in different buckets then conflict).
9. **A new ADR or an amendment.** Recommended: this new ADR. It adds a rule on a class that is not a BTree, a switch, knowledge of plone.folder and new invariants of the vote, and answers question 9 of ADR 0048 without a change of its rules or of those of ADR 0049. Other choice: an amendment of ADR 0048.
10. **The scope fence.** The invariants change in `crates/zoe-core/CLAUDE.md`, outside the fence: the order merge, a stored record staged again, tier 2 never merges an order, and the lock of the order of a folder is never handed off. Recommended: widen the fence to that file. Other choice: a card of the documenter after the merge.
11. **The option of the harness.** Recommended: `--folder-orders MODE` writes the switch and the tier 2 entry of the `Folder` class into the store of a run, and `result.json` records the full policy (section 14). Other choices: a general option that writes a policy file; or a new stack name.
12. **An order and tier 2.** A tier 2 entry for `persistent.list.PersistentList` would merge an order without renumbering `pos`. Recommended: both guards: the rule takes L before tier 2, and tier 2 never merges an order of a folder; and `zoe.policy.validate` refuses a tier 2 entry for `persistent.list.PersistentList` whose `tier2` is not `off` while `folder_orders` is not `off` (section 2, step 8). Other choices: only the guard of the core; or only the refusal of `validate`, which a policy written by other means passes.
13. **Values derived from a position.** The rule changes the position of each new id after the transaction ran, so a value derived from it in another record of the transaction keeps the old position (section 7). Recommended: accept, with queued indexing as the intended setup and the test of the catalog after `drain` (section 13). Other choice: no answer when the transaction also wrote a record of the catalog, which would need knowledge of the catalog in the rule.
