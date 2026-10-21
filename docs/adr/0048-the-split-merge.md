# 0048. The split merge: tier 1 merges the splits of a BTree, a read-current node lets a split of another child through, and rule 3 keeps the bucket of the committed side

Status: accepted. It adds three rules to tier 1, where ZODB's resolver refuses, and a switch of the merge policy. It changes invariants of the vote: a stale read-current oid can pass, a vote can write a record of an oid that its transaction did not store, and a vote takes one lock after its others, without a wait. It adds the extension `server_writes` to the wire protocol. It changes no on-disk format and no pack.

## Context

### Concurrent creations

The gate of phase 4 (#440) fails on `create-spread`: after queued indexing (ADR 0047), `zoe-native-qi` raises 27 % of the `ConflictError`s of `zeo` at 4 users and 11 % at 16 users in the runs of the spike; the limit is 10 %. The spike (`docs/spikes/concurrent-creations.md`) finds:

- Every `ConflictError` of `create-spread` but the container (5 of 190) is on four BTrees that every creation of a site shares: `_shadowStorage._storage` and `zvc_repo._histories` of `portal_historiesstorage` (CMFEditions), and `ids` and `refs` of the intid utility. Each is a root whose children are buckets.
- Each conflict is a split. Every revision of the four roots in the runs adds one child and changes nothing else. A creation inserts one key into each tree. The history id and the key reference of `ids` grow, so they go into the last bucket; the keys of the other two trees spread.
- ZODB refuses each case for the structure, not for the keys. `BTree__p_resolveConflict` refuses a node with children (reason 11), `_bucket__p_resolveConflict` refuses a bucket that a side split (reason 0), and `_BTree_set` marks each node of its path read-current, so a split of any other child under it fails the vote with `ReadConflictError`. Two inserts of different keys merge when they are in one bucket.
- The cases: two creations split the same last bucket from one state of the root (96 on the shadow storage, 22 on `ids`); a creation inserts into a bucket while another one split another child (50 read conflicts); two creations split different children (6); a creation inserts without a split into a bucket that another one split (11).
- The acceptance test (`python/zoedb/src/zoe/plone/tests/test_concurrent_creations.py`) runs rounds of eight creations from one snapshot, one folder for each creator. In each round all creators split the same bucket of each tree, or none does, and the first split of each tree is the split of the one bucket that its root holds inline.

ADR 0042 keeps tier 1 byte for byte equal to ZODB's resolver, and names two rules where ZODB refuses (the posting-dict rule and the first-key rule), under one switch, off by default. ADR 0046 puts the switches of tier 1 into the merge policy.

### The bucket that leaves the tree

A rule 3 that merges two splits of one bucket A by putting the new bucket X of the transaction in the place of the new bucket Y of the committed side, with the separator of Y, takes Y out of the tree. The spike `docs/spikes/split-merge-replaced-bucket.md` measures what that costs. On `create-spread` with such a split merge on, 18 of the 28 `ConflictError`s of three runs (1,038 operations) are read conflicts "a path child that the other side replaced". Each of the 20 such lines of the whole runs has this order of events:

1. S1 splits the last bucket A of a tree whose keys grow into A and a new bucket Y.
2. S2 read the node before S1 and also split A. Rule 3 puts X in the place of Y. Y leaves the tree.
3. T read the node after S1 and before S2 commits, and inserted its key into Y. Its write to Y is not stale, but the node no longer holds Y. Rule 1 gives no answer, and the vote fails with `ReadConflictError` on the node.

The refusal is right: a pass would lose the insert of T. 4 of the 20 lines of the whole runs follow a chain: a third transaction that read before S1 replaced X in turn. Every merge of rule 3 in the runs replaced a bucket (59 of 59), because the keys grow: every creation splits the last bucket, and two creations that read one state of the node split the same bucket.

A bucket of the committed side leaves the tree only in rule 3: rule 2 adds children, and rule 1 writes nothing. So a rule 3 that keeps Y in the tree removes the case. But Y is not in the transaction: the vote holds no lock on it, ZODB's `MVCCAdapterInstance` invalidates in the other connections of a process only the oids that a connection stored (`ZODB/mvccadapter.py:208-222`), and a ZEO server sends no invalidation to the client that commits. A merge that writes Y needs a lock that the vote takes late and a promise of the client (part B).

## Decision

### A. The split merge

#### A1. Words

- A **node** is a BTree or a tree set of a family of tier 1 whose state holds children: `(children, firstbucket)`. A **child** is a bucket or a node, with its **separator**, the key before it in the node (none for the first child).
- A **split** of a child A is what `BTree_grow` does: A keeps the keys below a new separator s, a new child X gets the keys at and above s, and the node gets `(s, X)` right after A. A is the **anchor** of the split. A root that holds one bucket inline splits into a node with two new children.
- The **old** record is the record that the transaction read, the **committed** record the current one, and the **new** record the one that the transaction stored. A node **changed only by splits** when its committed record holds every child of its old record, in the same order with the same separators, and every other child of it comes right after its anchor or after another new child of the same anchor.
- A child C of the old record is **replaced** when the committed record holds, in the place of C, a new child with the separator of C, and the child after that new child has the separator of the child after C in the old record, or neither has a child after it. So the new child has the range of C, and C leaves the tree. A merge that puts a new bucket in the place of a bucket of the other side writes such a record. No rule of this ADR writes one (section B1), but a store can hold such records (section B11). A replacement is a bucket or a node, and the first child is never replaced: a node keeps its first child and its first bucket. A node **changed only by splits and replacements** holds each child of its old record, or the new child that replaced it, in the same order with the same separators, and every other child of it comes right after its anchor or after another new child of the same anchor. A new child right after a replacement changes the separator after it, so a replaced child is never the anchor of a split.
- The **path children** of a node, for a transaction, are the children of its old record that the transaction stored or marked read-current. BTrees marks each node of the path of a write read-current (`PER_READCURRENT`, `BTreeTemplate.c:754`), so the path children are the children under which the transaction wrote.

#### A2. The rules

The rules run in the vote that takes the locks (`vote`, `vote_resolving`), after the check of every serial, on the stale oids of the transaction, as tier 1 does. A node whose records do not decode, a node that changed by anything but splits (for rule 1: by anything but splits and replacements), or a state that the model does not hold, gives no answer: the vote goes on as without the rules.

1. **A read-current node that another child split or replaced.** The transaction marked a node read-current, did not store it, and the node changed only by splits and replacements. When the transaction has at least one path child in that node, no path child is an anchor of a split of the committed side, and no path child is replaced, the node passes the check of the vote. The vote stages nothing for it. A path child that is an anchor is a bucket that the transaction stored and the other side split: rule 3 does not cover that case, and the vote refuses (reason 0 on the bucket). A path child that is replaced left the tree with the writes of the transaction under it: rule 1 gives no answer, and the vote fails with `ReadConflict` on the node, as without the rules. The write to that child is not stale, so the check of the node is the only check that stops it.
2. **Splits of different children.** The transaction stored a node in which it split children, the node changed only by splits on the committed side, and no anchor of the transaction is an anchor of the committed side. The merged node is the committed node, with the new children of the transaction and their separators right after their anchors. Each bucket keeps its one writer; a bucket that the committed side changed without a split and the transaction stored merges with the rules of `bucket_merge`, as tier 1 does.
3. **A split of one bucket.** The transaction split a bucket A of a node into A and its new bucket X, or split the one bucket inline in a root.
   - **The committed side changed A without a split.** The keys of the range of A merge with the rules of `bucket_merge` (`MergeTemplate.c`): the base is the old A, the committed side is the committed A, and the new side is the new A followed by X. The merged keys below the separator of the transaction go to A, the others to X. The next bucket of A is X; the next bucket of X is the next bucket of the committed A. The merged node is the committed node, with `(s, X)` right after A. For the bucket inline in a root, when the committed root still holds its bucket inline, the committed side is that bucket: the merged root holds the two new buckets of the transaction, with the separator of the transaction.
   - **The committed side split A once, into A and Y** (or split the bucket inline in a root into Y1 and Y2). With a client that takes records that it did not store (section B4), rule 3 merges into the buckets of the committed side (section B1). Without it, rule 3 gives no answer (section B11).
   - The two halves must not be empty. With two splits of one bucket on the committed side, or two on the side of the transaction, the rule gives no answer.

Rules 2 and 3 do not take replacements: in a node that the transaction stored, a replaced child on the committed side is a change by more than splits, and they give no answer. The rules give no answer for a delete of a key in a range that they merge, for two splits of one child on one side, and for a child node that the transaction split and the other side changed: rule 3 merges only buckets. Each node runs the rules on its own records, at any depth of the tree, so a split of a child node that the other side did not change merges under rule 2. A refusal of `bucket_merge` inside the range (two inserts of one key, a key that both sides changed) refuses, with its reason, as tier 1 does. A merged bucket can hold more keys than the maximum of its class, as a bucket that ZODB's resolver merges can; the next insert splits it.

**The order of the keys.** Rule 3 compares the keys of the range with the comparisons of tier 1. It also compares key references: two keys of the class `five.intid.keyreference.KeyReferenceToPersistent`, or two of `zope.keyreference.persistent.KeyReferenceToPersistent`, whose state holds an `object` that is a reference to an oid of the same database (a reference that names no database), compare by those oids. That is what `_get_cmp_keys` of zope.keyreference does in conflict resolution (`zope/keyreference/persistent.py:61-95`), where the object of each key is a `PersistentReference`. Two keys of other classes, of two classes, or with a reference that names a database, give no answer. Without this order the keys of `ids` cannot be compared, and its splits keep conflicting. The rules compare the values of the range with the same order: `bucket_merge` compares the value of a key that both sides kept, and the values of `refs` are key references. BTrees compares keys and values with the same `object_compare`, so ZODB's resolver compares them in the same way. Without the order on values, the splits of `refs` keep conflicting. The order applies only in the rules of this ADR, under their switch.

**A commit in flight.** The rules read the committed records from the store. When the vote holds a lock with a hand-off whose commit is in flight (ADR 0044), the rules do not merge onto the copy: the vote waits until that commit is durable or failed, as for the other cases of the hand-off, and then runs the rules on the store.

#### A3. What the merge writes, and why it loses no update

Rules 1 and 2, and rule 3 when the committed side changed A without a split, write only records that the transaction stored: the node, the bucket A that it split, and its own new bucket X, a new object. Rule 1 writes nothing. Section B2 gives what rule 3 writes when the committed side split A. Each merged record is staged as tier 1 stages a merge: over the serial of the committed record, under the locks of the vote, with the size limits; X stays a new object. The reply of the vote lists each of these oids as resolved, as ZEO 6 does for a conflict that its server resolved. The client stored each of them, so its connection turns them into ghosts, its cache drops them, and every other connection of its process gets their invalidation. The other clients get the invalidation of the commit.

No update is lost:

- **Rule 1.** The transaction wrote only under its path children. Each path child is still a child of the committed node, and its range there is its range in the old node: a split adds a child right after its anchor, and no path child is an anchor; a replacement takes the place of a child with the same separator and the same separator after it, so it changes the range of no other child, and no path child is replaced. So a lookup in the current tree finds each write of the transaction where the transaction made it, and the tree after the commit is the committed tree with the writes of the transaction: the state that the two transactions give one after the other.
- **Rule 1 and a replaced child.** A replaced child is not a path child, so the transaction wrote nothing under it, and no write of the transaction leaves the tree with it. The bucket before a replaced bucket names it as its next bucket. When the other side wrote that bucket, a transaction that stored it has a stale serial: the vote checks the written serials before the read-current oids, and tier 1 refuses a bucket whose next bucket changed (reason 0). When the other side did not write it, the transaction writes the next bucket that the committed tree holds, as a transaction after the other side would. A replaced child on the path must fail: the transaction wrote into a record that the current tree no longer holds, and that write is not stale.
- **Rule 2.** The two sides split disjoint ranges. The merged node holds every child of the committed node, and each new child of the transaction with the range that its split gave it. Each bucket has one writer, or merges with `bucket_merge`. So each key routes to the bucket that holds it, and the tree holds the changes of both sides.
- **Rule 3, without a split on the committed side.** The merged keys of the range of A are the merge of `bucket_merge` of its three states, so each change of either side in that range stays, and each case where `bucket_merge` refuses still refuses. The node routes each key of the range to A or X, which hold it, and every key outside the range keeps its bucket. Section B6 gives the argument when the committed side split A.
- **The node.** Each write of a bucket of the tree goes through the node: BTrees marks the node read-current or writes it, so the vote of every writer of a bucket holds the lock of the node, shared or exclusive, until its commit is durable. The vote of rule 3 holds the node exclusively, since the transaction stored it. Rule 3 reads a bucket of the other side only under its own lock (section B3).
- **What the argument assumes.** Writes enter the tree at its root, as the API of BTrees does. A program that writes a bucket that it takes from `__getstate__()` marks no node, as ADR 0042 says of the first-key rule. A program that marks a tree read-current itself and writes a key into it loses the check of the other children of that node under rule 1. No code in the eggs of a Plone 6.1 instance calls `readCurrent` but BTrees, and the registry backend of `zoe.plone`: ADR 0043, section B1, says why its calls keep the argument.

The spike checks the rules on real BTrees in a model outside the repository: 36,410 merges in rounds from one snapshot, each followed by `BTrees.check`, `_check` and a lookup of every key of every transaction, with no failure.

#### A4. The switch: opt-in, in the merge policy

The rules depart from ZODB's resolver, as the posting-dict rule does, so a store opts in.

- The merge policy (ADR 0046) has the switch `split_merges`, with the modes of tier 1: `off`, the default, `shadow` or `on`. `zoe.policy.install` and `validate` take it; the overrides file of a profile of `zoe.plone` can set it, as it sets `posting_dicts`. A store without a policy, or a policy without the switch, keeps the stock behaviour.
- The profile `zoe.plone:default` switches `split_merges` on, and the sites of a database compose it to the lowest mode that they ask for (ADR 0046, section 9; ADR 0052, section 6).
- No key of `<zoe>`, of `zoe.Storage` or of `zoe serve`. The policy is a record of the store, so the switch is the same in both modes, it commits with its transaction, and a copy of the store keeps it.
- `on` merges with the rules. `shadow` runs them, writes their result to the resolution log in the field `shadow`, and the vote goes on as without them: tier 1 refuses as it does without the switch. ZODB writes no record for these cases, so shadow mode verifies nothing (ADR 0046, section 6). A shadow mode without a `resolution-log` gives the warning of ADR 0046, section 3.
- The rules run only where the tier 1 mode of the class is `on` or `shadow`.

#### A5. The resolution log and the counters

Each record that a rule writes gives a line of tier 1 with the outcome `merged` and the field `split`: the rule, `1`, `2` or `3`. Rule 1 gives one line for the node, with the outcome `passed`. A refusal of `bucket_merge` inside a rule gives `refused` with its reason and `split`. `zoe_structural_merges_total` of `zoe serve` counts them with their outcome. The lines keep every other field of ADR 0042.

**The read conflicts of the vote.** When the vote fails with `ReadConflict` on a stale read-current node and rule 1 gave no answer for that node, the vote writes one line for it, in modes `on` and `shadow`: tier 1, the oid and the class of the node, `read` and `committed`, and `"split":1`.

- In mode `on`, the outcome is `undecided` and its reason is the reason of rule 1, for example "a path child that the other side replaced". `applied` is false: the vote fails as without the rules, as for an `undecided` of tier 1 on a written oid.
- In mode `shadow`, the outcome is `undecided` with the reason "a read-current oid", as for a node that rule 1 lets through, and the field `shadow` holds `undecided` and the reason of rule 1.
- The vote fails on the first stale read-current oid, so it writes at most one such line. The nodes before it that rule 1 let through keep their lines `passed`, with `applied` false.
- A read-current oid on which the rules did not run gives no line: an oid that is not a node of a family of tier 1, a class whose mode of tier 1 is `off`, a commit in flight that fails the vote at once, or a late wait for a commit in flight.
- `zoe_structural_merges_total` counts the line with the outcome `undecided`. The reasons are texts for the operator: no test fixes them.

#### A6. A site on ZEO, and the clients

- The rules are in the store: a site on a ZEO server and a FileStorage keeps the stock behaviour. On such a store the switch is a record that nothing reads.
- A stock `ZEO.ClientStorage`, the native client and `zoe.Storage` need nothing for rules 1 and 2, and for rule 3 when the committed side did not split A: the store merges at the vote and replies with the oids that it resolved, which the client stored. When the committed side split A, rule 3 needs the flag of section B4.
- No code of CMFEditions, ZopeVersionControl, zope.intid, five.intid or `zoe.plone` changes, and the rules hold for every BTree of the store, not only the four trees of the spike.

#### A7. A copy, a pack and an undo

- **A copy** through the iterator (`zodbconvert`, `copyTransactionsFrom`, `zoe convert`), `zoe backup` and `zoe restore` copy the records as they are, and the policy with its switch. A restore transaction runs no rule.
- **A pack** needs no change. No merge of the rules takes a bucket out of the tree (sections B1 and B11), so the rules leave no garbage.
- **An undo** runs no rule: the rules need the vote of a transaction, its stored records and its read-current oids, and an undo has three records of each oid. An undo of a transaction that wrote into a bucket never passes with no effect: no merge of rule 3 takes a bucket out of the tree. When a later commit wrote that bucket, the undo meets that record: tier 1 or the client resolves it, or it fails with `UndoError`, as in ZODB. Section B7 gives the undo of a merge of section B1.

#### A8. What the build changes, and where

- `crates/zoe-core/src/merge/`: the rules, the order of key references, and the switch in `merge::policy`. The order of key references uses the model of tier 2 (`Heap::with_instances`).
- `crates/zoe-core/src/commit/` (the vote): the check passes a node under rule 1, and the rules decide for a group of records (a node, a bucket and a new object) and stage them together. It is not a red zone.
- `merge::split` reads replacements in rule 1 and gives its reason; `commit::split` and the vote keep the reason of each stale read-current node without an answer, and the vote writes its line when it fails on that node.
- `crates/zoe-core/CLAUDE.md`: the invariants of the vote and of tier 1 ("a read-current oid is never resolved"; a merge of one oid).
- `python/zoedb/src/zoe/policy.py` and `zoe/plone/merges.py`: the switch.
- Section 5 of the specification.
- No change of `format/`, of `pack/`, or of the wire protocol for part A.

#### A9. The helpers of the acceptance test

`mechanism_on(test)` writes the switch into the site of the layer, which the test then copies to its store, and gives no keyword argument. `mechanism_off(test)` does nothing: without a policy the store keeps the stock behaviour.

```python
def mechanism_on(test):
    import zoe.policy

    zoe.policy.install(test.layer["portal"]._p_jar, {"classes": {}, "split_merges": "on"})
    transaction.commit()
    return {}


def mechanism_off(test):
    return {}
```

The layer gives each test a `DemoStorage` of its own, so the policy goes away with the test. The acceptance test has this shape:

- Eight folders, one for each creator, not one folder: a creation reindexes its container and sets its `modification_date`, and plone.folder appends to the order of the folder. In one folder the `Folder` conflicts in every round, and so does that order, which must conflict (spike, finding 5). That is not a tree whose keys grow, and `create-spread` has one section for each user.
- Sixteen rounds of eight from one snapshot, not one batch of eight: the trees conflict only when an insert splits a bucket, and the rounds make the buckets fill and split, without a test that reads the buckets.
- A `zoe.Storage` of its own for each test, not the `DemoStorage` of the layer: a `DemoStorage` gives no sequence, so every creation of a round would get the same history id, and it resolves conflicts with ZODB's resolver in Python, not with the tiers of the store.

#### A10. The tests and the measure

- `crates/zoe-core/tests/`: the vote with each rule and each case where it gives no answer: a path child that is an anchor, a removal, a changed separator, a node with no path child, two splits of one bucket on one side, a delete, a refusal of `bucket_merge` inside the range, two levels of nodes, opaque keys, key references of two classes or with a database name, a commit in flight, and the switch in each mode.
- `crates/zoe-core/tests/split_read_conflicts.rs`: a replaced child off the path (a middle child, and a last child when the keys grow) passes in mode `on`; mode `shadow` tells that it would pass; without the switch, or with `off`, the vote keeps the stock behaviour. Four read conflicts in mode `on` write their line: a replaced child on the path, a new child with another separator, a node without a path child, and a path child that the other side split. The unit tests of `merge::split` add a replacement beside a split, two replacements side by side, a new first child, and a replacement followed by a split of it; the last two give no answer.
- `a_read_current_node_whose_path_child_split_gives_no_answer` in `crates/zoe-core/tests/split_merges.rs` asserts the one entry of the node: `undecided`, with the reason of rule 1 and `split` 1.
- `python/zoedb/tests/`: the model of the spike as a test, against `zoe.Storage` and against `zoe serve` with the native client and a stock `ZEO.ClientStorage`. After each round, `BTrees.check` and a lookup of every key in a new connection of another process, and in another connection of the process that committed, so that a stale cache of a resolved new object shows. `test_in_shadow_mode_the_rounds_raise_conflict_error_and_log_the_rules` in `python/zoedb/tests/test_zoe_split_merges.py` takes `merged`, `passed` or `undecided` with a reason in the field `shadow` of each line of the split merge.
- The acceptance test, green with the helpers of section A9.
- The measure: B5 `edit` and `create-spread` at 4 and 16 users, on `zeo` and on `zoe-native-qi` with the switch on, three repetitions, on a quiet machine; the gate of #440; `just bench native` and B2, because the vote changes. The stack `zoe-native-qi` takes the option `--split-merges` of the harness, which installs the policy with `split_merges` on in the store of the run. The resolution log counts the merges of each rule.

### B. Rule 3 keeps the bucket of the committed side

#### B1. The merge into the buckets of the committed side

When the committed side split A once into A and Y, and the client of the transaction takes records that it did not store (section B4), rule 3 merges as follows:

- The keys of the range of A merge with the rules of `bucket_merge`: the base is the old A, the committed side is A followed by Y, the new side is the new A followed by X.
- The merged keys below the separator of Y go to A, the others to Y. The next bucket of A stays Y, and the next bucket of Y stays the next bucket of the committed Y.
- The merged node is the committed node: the merge adds no child, removes no child and changes no separator.
- X gets no place in the tree.
- A or Y must not be empty.

For the bucket that a root holds inline, the committed side split it into two new buckets Y1 and Y2 (both halves of a split of an inline bucket are new objects). The merged keys go to Y1 and Y2 at the separator of the committed side, and the merged root is the committed root. The two new buckets of the transaction get no place in the tree.

When the committed side changed A without a split, rule 3 is the rule of section A2: X is a new child with the separator of the transaction, and no bucket leaves the tree.

#### B2. What the vote writes, and the oids without a record

- **A** (or the inline root): the merged record, over the serial of its committed record.
- **Y** (or Y1 and Y2): the merged record, over the serial of its committed record. The transaction did not store Y. A vote writes a record for an oid that its transaction did not store only here.
- **The node**: no record when its merged record is its committed record, which is the case unless rule 2 merged another split of the node in the same vote.
- **X** (or the two new buckets of the transaction): no record. X is a new object that no record of the store refers to.

A stored oid that the merge resolves without a record (the node N, X) has these invariants:

- It leaves the staging of the transaction. The batch writes no record of it, and the transaction index does not list it, so no other client gets an invalidation of it.
- Its lock stays exclusive and the transaction holds it until its commit is durable or fails, as any lock of the vote. The finish never hands it off (ADR 0044): the finish hands off only the oids that it stages, and this oid left the staging when the vote passed. There is no staged record to give, and the staged pickle of the client is wrong (its N refers to X). So the lock manager needs no change for it but `LockManager::try_exclusive`, the call that never waits (section B3). A vote that waits for it gets it at the release, and reads the store.
- The reply of the vote lists it as resolved. Else the Connection that committed keeps the N of its client, which refers to X, and its cache keeps the pickles of N and X.

The reply of the vote lists A, Y, and each stored oid without a record as resolved, with the bare oid of ZEO 6, as for any merge of tier 1. The reply keeps its form: a bare oid that the transaction did not store already means a record that the server wrote, as the oids of an undo (ZEO's `TransactionBuffer` yields such an oid with no data, and the native client ends its current entry, section "Server mode" of the specification).

The commit holds the records of A and Y. So the transaction index lists Y, and each other connection of the store gets Y in `invalidateTransaction(tid, oids)`, a stock client too, as for any record of the commit. The connection that committed gets no invalidation, as from ZEO. That rule holds for each connection, not for each client process: the listener of the pool of the native client is another connection, so it gets the invalidation of each commit of the pool, with every oid of the transaction index. Section B4 says how the process that committed uses it.

#### B3. The lock on Y

The vote takes the exclusive lock of Y after its plan, with a call of the lock manager that never waits (`LockManager::try_exclusive`): the call grants the lock when no transaction holds it, no request waits for it and the entry carries no hand-off (ADR 0044); else it fails at once. When it fails, rule 3 gives no answer: the vote goes on as without the rule, and in the runs of the spike S2 then fails with `ConflictError` and its client tries again. For a root that held its bucket inline, the vote takes the locks of Y1 and Y2, or gives no answer.

The plan finds Y in the committed node, which the vote holds, and takes the lock of Y before rule 3 reads Y. So the rule reads Y under its lock.

**Why the lock of Y is enough.** The main path rests on the lock of Y alone:

- A lock that is free, with no waiter and no hand-off, means that every commit of Y is durable (ADR 0044, section A4). So the rule reads the last commit of Y, the inserts of the transactions that wrote into Y after S1 included.
- From then on, each other writer of Y waits for the lock of Y: a vote that stored Y, an undo and a restore alike, since each takes an exclusive lock on each oid that it writes. It gets the lock at the hand-off of the finish of S2, with the staged record of S2 (Y is a bucket, a class of tier 1, so the hand-off applies), or at the release when S2 is durable. Its serial of Y is then stale. So no write of Y goes over the record of S2 unseen.
- The vote never waits out of oid order, so the order of the locks still excludes a deadlock, and the call never passes a request that waits.

**Why the call usually finds the lock free.** Each writer of Y through the API of BTrees holds the lock of the node, shared or exclusive, until its commit is durable (section A3, "The node"). The vote of rule 3 holds the node exclusively, since its transaction stored the node. In the trees of the spike, Y is newer than its node, so its oid is above the oid of the node, and every such writer takes the lock of the node first: none holds or waits for the lock of Y. The call fails when an undo or a restore writes Y (they write Y without the node), when a split of a node moved Y under a newer node (the oid of Y is then below the oid of its node, and a writer of Y can hold the lock of Y while it waits for the node), or when a program writes Y without the node (section A3, "What the argument assumes").

**A vote that the end of a commit in flight wakes.** A vote S2 that got a lock at the hand-off of the finish of S1 waits until the commit of S1 is durable or failed before the rules read the store (ADR 0044). The finish of S1 releases the locks that it did not hand off, Y among them, before it ends its commit in flight, and the end of the commit wakes S2. So S2 finds the new buckets of S1 free, and the call does not fail for them. The release comes after `GroupCommit::finish` returns, so after the commit is durable or failed, and after the settle of the hand-offs that the transaction got. It reads no clock: the log of `lock-warn` comes after the end of the commit. The other order, the end of the commit before the release, gives S2 a `ConflictError` that it does not need. That window exists only when every lock that S2 waited for came by a hand-off: the bucket that a root holds inline, where S1 and S2 share only the root, because both halves of each split are new objects. When both sides split a bucket A of a node, both store the node and A. S2 waits in one queue at a time, so the finish of S1 hands it at most one of these locks, and S2 gets the other one at the release of S1, the same release that frees Y. So S2 finds Y free in that case with both orders. `tests/split_merges.rs` holds the vote that the end of the commit wakes, with the inline root.

**Where the hand-off of the node matters.** When rule 2 gives the node a record in the same vote, the finish of S2 can hand the lock of the node to a vote T' that stored the node and waits for it. T' then goes on to its other locks. Without the lock of Y, T' would take the lock of Y freely, read Y from the store before S2 is durable, find its serial current, and write Y over the record of S1: the merge of S2 would be lost. With the lock, T' waits for Y, as above. When the node has no record (section B2), the finish never hands it off, and T' waits until S2 is durable.

#### B4. Which client may get a record that it did not store: the extension `server_writes`

A ZODB process with a stock `ZEO.ClientStorage` cannot take a record that its transaction did not store. The line numbers are those of ZODB 6.2 and ZEO 6.2, the versions of the instances of the spike:

- `ClientStorage.tpc_vote` puts each bare oid of the reply into `server_resolved`. `TransactionBuffer` yields Y with no data (`ZEO/TransactionBuffer.py:94`), so `tpc_finish_co` ends the current entry of Y in the cache of the client (`ZEO/asyncio/client.py:778`). This part is correct.
- `Connection.tpc_vote` turns each oid of the reply into a ghost in the Connection that committed (`ZODB/Connection.py:675-695`). This part is correct.
- The other Connections of the process get only the oids that the committing Connection stored: `MVCCAdapterInstance.tpc_finish` gives `_modified` to `_invalidate_finish`, and `_modified` holds only the oids of `store` and `storeBlob` (`ZODB/mvccadapter.py:201-223`, `_invalidate_finish` at line 91). The server sends no invalidation to the connection that committed. So another Connection that holds Y in its object cache keeps the state of Y before the commit, after it moved past the commit: a lookup of a key that S2 merged into Y misses. That is a stale read.

So the server writes Y only for a client that says it invalidates Y in all its Connections, and the vote gets one flag from its skin: whether its client takes records that it did not store.

**The declaration.** An extension of `register` (section "Extension negotiation" of the specification):

- **`server_writes`**: the connection declares that, at the finish of a transaction whose `vote` reply listed an oid that the transaction did not store, the client ends the current entry of that oid in its cache, and gives every Connection of its process one invalidation of the tid of the commit with every oid of the commit, before `lastTransaction()` gives that tid. `get_info` lists the name when the server accepted it.

**One invalidation with every oid.** The client calls `invalidate(tid, oids)` of the database once, right before the callback `f(tid)`, with every oid of the commit: the oids that the transaction stored and the oids that the server wrote. Two calls would tear the snapshot: `MVCCAdapterInstance._invalidate` sets `_ltid` to the tid (`ZODB/mvccadapter.py:130-136`), and `poll_invalidations` starts the snapshot after `max(lastTransaction(), _ltid)` (line 143). Another Connection that polls between a call with Y alone and `f(tid)` gets the snapshot `tid + 1` with only Y invalidated: it reads the new Y and the A of its cache from before S2. With one call, a Connection that polls sees all the oids of the commit or none, as for the commits of the other clients. `f(tid)` then gives the stored oids once more, which changes nothing.

**The clients that declare it, and only these:**

- The native mode of `zoe.zeo.ClientStorage` (`_NATIVE_EXTENSIONS`, `crates/zoe-py/src/client.rs`, the pool of `zoe-client`). Its finish takes the oids of its own commit from the listener: the dispatcher of the pool gets the invalidation of the commit of the pool on its listener (`Route::Own` in `crates/zoe-client/src/pool.rs`), with every oid of the transaction index, and hands that list to the finish at its turn. The pool marks a transaction whose vote listed an oid that it did not store; its turn then gives the oids of the commit from the invalidation of the listener (`Turn::unstored_commit`). The finish calls `invalidate(tid, oids)` with them before `f(tid)`, while the dispatcher holds the tid. The oids of an undo take the same path: the Connections get them twice, which changes nothing. The cache of the pool ends the current entry of each oid of the reply that the transaction did not store. It drops the current entry of each oid that the transaction stored and that the commit does not hold, N and X: the entry of N at its old start stays current on the server, and its next load enters the cache. With the cache of the pool off, the finish still calls `invalidate`.
- The in-process path of `zoe.Storage`, when its database is a ZODB `MVCCAdapter` (what `registerDB` gets from `ZODB.DB`). `zoe.Storage` sets the flag in `registerDB` then. Its `tpc_finish` (in Python) reads every oid of the commit before the finish (`_unstored_commit`) and calls `invalidate(tid, oids)` with them before the callback. A `zoe.Storage` whose database is something else, a stock `ZEO.StorageServer` for example, does not set the flag.
- Not the asyncio mode of `zoe.zeo.ClientStorage` (`EXTENSIONS`; the native mode has its own tuple). Not the Rust `Pool` of `zoe-client` by default: `zoe-bench`, `zoe-mcp` and Loupe do not declare it. Not a stock `ZEO.ClientStorage`, which declares nothing.

A vote of `zoe serve` gets the flag from the declaration of its connection. A transaction without the flag never gets a record that it did not store.

#### B5. The switch, and shadow mode

No switch of its own. The rule of section B1 runs where the split merge runs (`split_merges` `on` or `shadow` in the merge policy, section A4) and the vote has the flag of section B4. In mode `shadow` the vote writes nothing. It takes no lock of Y and reads Y without it: the field `shadow` of the lines gives the outcome of the rule of section B1 as if the call had found the lock free. That outcome is a report, so a read of Y without its lock changes no record.

#### B6. Why no update is lost

- **The merge.** The merged keys of the range of A are the merge of `bucket_merge` of its three states. They go to A and Y, the children of the committed node that hold the range: the node routes each key below the separator of Y to A and each other key to Y. Each key outside the range keeps its bucket. X held only keys of the range. So the tree after the commit is the committed tree with the changes of the transaction.
- **No bucket leaves the tree.** The committed node keeps each of its children. So each write of another transaction under a child of the node stays where a lookup finds it.
- **The read of Y.** The vote reads Y under its exclusive lock, which it takes only when every commit of Y is durable (section B3). So it reads the last commit of Y.
- **A later writer of Y.** It waits for the lock of Y (section B3). Its serial of Y is then stale, so tier 1 merges, refuses with the reason of BTrees, or gives the conflict to the client. An undo of a transaction that wrote Y meets the record of S2 as a later change of Y.
- **The oids without a record.** The node and X leave the staging, so no batch writes the pickle of the client, which refers to X, and no hand-off gives it to another vote (section B2).
- **T.** T read the node after S1 and stored Y. The node has no new record, so its read-current check passes; when rule 2 gave the node a new record in the same vote, the node holds every child that T read, and rule 1 passes it. Its write to Y is stale against the record of S2: tier 1 merges the inserts of different keys (for the trees of `intids`, whose keys are key references that the plain tier 1 does not compare, tier 1 gives no answer and the client resolves with `_p_resolveConflict`, as for a plain conflict on a bucket of `ids`). So T commits with its key in Y, which the tree holds.
- **The clients.** The process of S2 drops Y in its cache and gives every oid of the commit to all its Connections in one invalidation, before it publishes the tid (section B4). Every other connection gets Y in the invalidation of the commit. No Connection that moved past the commit keeps the state of Y before it, and no Connection gets a snapshot with a part of the commit.

#### B7. A copy, a pack and an undo

- **A copy** through the iterator (`zodbconvert`, `copyTransactionsFrom`, `zoe convert`), `zoe backup` and `zoe restore` copies the records as they are: the commit of S2 holds A and Y, and no record of the node or of X. A restore transaction runs no rule.
- **A pack** needs no change. The merge of section B1 leaves no bucket out of the tree, so it leaves no garbage. X has no record. The oid of X stays unused, as an oid that `new_oid` gave to an object that never committed.
- **An undo** runs no rule. An undo of S2 gives back the records of A and Y before S2: the keys of S2 go away and the keys of S1 stay. An undo of T, which inserted into Y, meets the record of S2 of Y as a later change: tier 1 or the client resolves it, or it fails with `UndoError`, as in ZODB. An undo that runs with S2 holds the lock of Y, so the call of section B3 fails and rule 3 gives no answer.

#### B8. What the build changes, and where

- `crates/zoe-core/src/merge/split.rs`: rule 3 of section B1, the inline root included, and the records it gives (A and Y written, the node and X resolved without a record).
- `crates/zoe-core/src/commit/`: the flag of the client; the lock of Y after the plan, with no answer when it fails, with the flag; no answer of rule 3 without the flag when the committed side split; a stored oid that leaves the staging and keeps its lock without a hand-off; a written oid that the transaction did not store; their lines in the resolution log.
- `crates/zoe-core/src/lock/`: the call that never waits, and a lock that the finish never hands off. It is not a red zone.
- `crates/zoe-core/CLAUDE.md`: the invariants that change (see Consequences).
- `crates/zoe-server/src/`: the extension `server_writes` in the negotiation, and the flag of the vote.
- `crates/zoe-client/src/` and `crates/zoe-py/src/`: the declaration of the native mode, and the oid list of the own commit (`Route::Own`) for the finish.
- `python/zoedb/src/zoe/zeo.py` (the tuple of the native mode, and `invalidate(tid, oids)` before the callback) and the finish of `zoe.Storage` (the same, with a ZODB `MVCCAdapter` only).
- `docs/spec/technical-specification.md`: "Extension negotiation" (the name), "Invalidations" and the client sections (what a declared client does), section 5 (rule 3).
- No change of `format/`, of `pack/`, of the reply of `vote`, or of `invalidateTransaction`.

#### B9. The resolution log and the counters

- A gives its line of rule 3. Y gives a line of tier 1 with the outcome `merged` and `split` 3; its `read` and `committed` are the serial of the record of Y that the merge read. The node and X give a line with the outcome `merged`, `split` 3, no keys and `changed` 0, so `Txn::natively_resolved` lists them and the reply gives them.
- `zoe_structural_merges_total` counts these lines. No counter of its own.
- A failed call for the lock of Y gives the line of rule 3 on the node (`merge::split::LOCKED`, in `commit::split`) with the outcome `undecided` and the reason "a bucket of the other side that another transaction locks", and the vote goes on as without the rule. The node keeps its line of tier 1, on which the vote fails as without the rule. The line of rule 3 has the mode `on` and `applied` false, as the line of rule 1 for a read conflict, and `zoe_structural_merges_total` counts it as `undecided`. The bucket that a root holds inline gives the same line on the root.
- No other answer of rules 2 and 3 on a written node gives a line. A transaction without the flag gives no line of its own: rule 3 gives no answer, and the vote goes on as without the rule, with the line of tier 1 for the node. The core logs only WARN events, so the rule has no `debug` event.

#### B10. The tests and the measure

- `crates/zoe-core/tests/`:
  - the three transactions S1, S2 and T in their order, with the flag: all commit, the tree holds every key, and the model of `tests/split_merges.rs` checks the tree;
  - a chain of three merges of rule 3 onto one Y, then T: all commit, and `BTrees.check` passes on the tree;
  - the inline root;
  - the vote without the flag: rule 3 gives no answer when the committed side split the bucket (and for an inline root that the committed side split), and still merges when the committed side changed the bucket without a split;
  - the lock of Y held by another transaction: rule 3 gives no answer;
  - an undo of T after S2 never passes with no effect, with the flag and without it, through `zoe.Storage` and through `zoe serve`;
  - an undo of T that runs with S2: the undo holds Y, rule 3 gives no answer, and the undo takes effect; an undo of T after S2 meets the record of S2;
  - no hand-off of a node without a record: a vote T' that stored the node and waits for it gets the lock at the release, after S2 is durable, and reads the store;
  - rules 2 and 3 in one vote (the node gets a record) with a vote T' that stored the node and Y: T' gets the node at the hand-off and waits for Y, then merges onto the staged Y of S2 or waits until S2 is durable;
  - the switch in mode `shadow`: no lock of Y, nothing written, the outcome in `shadow`.
- `crates/zoe-core/src/lock/`: the call that never waits refuses a held lock, a lock with a waiter, and an entry with a hand-off; a lock that the finish never hands off.
- The crash simulation `crates/zoe-server/tests/simulated_split_merges.rs` runs split writers (`Simulation::split_merges` of `zoe_server::sim`): a merge policy with `split_merges` `on`, one `IIBTree` with buckets of at most 4 keys, and clients of `zoe-client` that declare `server_writes` and insert growing keys. Two of them that read one state of the last bucket both split it, so the vote of the second writes Y and resolves N and X without a record. Syncs take simulated time, so finishes hand off locks, and the server crashes with a power loss at each phase of a commit. After each recovery and after the run, the driver walks the tree (the separators, the key order and the next-bucket chain, the checks of `BTrees.check` on a tree of one node), and checks that the tree holds every acknowledged key, that no bucket with a record is outside the tree (so no record of X exists), that each transaction of the store reads (a commit that a crash interrupted and that landed included), and that the transaction index of each commit whose vote resolved oids lists Y and neither N nor X when the vote wrote Y. It does not read the resolution log.
  - The finish of each such commit must hand off no more locks than the records of its commit, a general bound: the observer of the core gives a count for each finish, not the oids, so this check fires only when a finish hands off more locks than its commit has records. A vote that wrote Y has at least the records of A and Y, so a hand-off of N alone does not reach that bound. So the simulation also asserts that the finish of a vote that wrote Y hands off no lock: in a tree of one node it hands off nothing (below), so a hand-off there is of N or X, or breaks that premise.
  - 1,000 seeds pass: 79,581 acknowledged votes wrote Y and dropped 159,162 oids; finishes handed off 47,731 locks, 47,099 of them in acknowledged commits and none in the finish of a vote that wrote Y; 3,071 finishes after a vote that wrote Y and 13,046 votes of a split got no reply, because of a crash (before the vote or its reply, or during the finish) or a lost connection.
  - In a tree of one node, no vote of rule 3 merges onto a commit in flight, and its finish hands off nothing: every other writer of A or Y takes the lock of N first, and the vote holds N until its commit is durable. So the simulation does not cover a hand-off to or from a vote of rule 3, a split of a node, or the root that held its bucket inline (Y1 and Y2).
- `python/zoedb/tests/`:
  - the order S1, S2, T against `zoe.Storage` and against `zoe serve` with the native mode, with a second Connection of the committing process that holds Y in its object cache before the commit of S2 and reads the key of S2 after it;
  - the torn window: a second Connection that polls while the finish of S2 delivers its invalidation sees all the oids of the commit or none (A and Y together);
  - the native mode with its cache off;
  - the asyncio mode of `zoe.zeo.ClientStorage` does not declare `server_writes`, and rule 3 gives no answer for its transactions when the other side split the bucket;
  - `zoe serve` with a stock `ZEO.ClientStorage`, which declares nothing: rule 3 gives no answer for its transactions when the other side split the bucket.
- The measure, one repetition of each cell (B5, `--steps 4:60,16:60`), `zeo` against `zoe-native-qi` with the switch on: `create-spread` gives 106 and 107 `ConflictError`s on `zeo` and 0 and 4 on `zoe-native-qi`, at 4 and 16 users; `edit` gives 106 and 86 on `zeo` and 0 and 0. No read conflict: the resolution log of the `create-spread` run has no line "a path child that the other side replaced". The 4 conflicts are 3 on a `Folder` and 1 on an `OOBucket` ("Conflicting bucket split" of BTrees). The other two repetitions did not run.

#### B11. No answer without the flag, and a replaced bucket in a store

**No answer without the flag.** For a transaction without the flag of section B4 (a stock `ZEO.ClientStorage`, the asyncio mode of `zoe.zeo.ClientStorage`, a `zoe.Storage` whose database is not a ZODB `MVCCAdapter`, a stock `ZEO.StorageServer` over `zoe.Storage`), rule 3 gives no answer when the committed side split the bucket A, or split the bucket that a root holds inline. The vote goes on as without the rule, and the client gets the `ConflictError` of ZODB's resolver. Rule 3 never puts X in the place of Y, so no merge of the split merge takes a bucket out of the tree. When the committed side changed A without a split, rule 3 merges as section A2 says.

**No read of Y without its lock.** On that path rule 3 sees the split in the committed node and gives no answer before it reads Y, so the vote reads no bucket that its transaction did not store and did not lock. Rule 2 reads the committed node and the buckets that its transaction stored. So the only read of a bucket of the other side is the read of section B3, under the lock of Y. An undo or a restore of Y, which writes Y without the node, cannot run with a read of Y and be lost.

**A replaced bucket in a store.** A store can hold records of a rule 3 that put X in the place of Y, from a build without part B. Their replaced buckets stay out of the tree: an undo of a transaction that wrote into one of them has no effect on the tree, and rule 1 lets a node with such a replacement through (section A2).

## Consequences

- With the switch on, concurrent creations of versioned content commit at the first attempt as long as each creator works in its own folder. On the kept logs of the spike, `create-spread` on `zoe-native-qi` with rules 1 and 2 and a rule 3 that replaces Y would raise 2.4 `ConflictError`s per 1,000 operations at 4 users and 9.1 at 16 users: 0.6 % and 2.4 % of `zeo`. The rule of section B1 removes 18 of the 28 `ConflictError`s of three such runs: an estimate of 9.6 per 1,000 operations instead of 27.0.
- What is left: a split on the committed side against a plain insert of the transaction into the split bucket (11 of 190 in the first spike, 2 in the second), and two creations in one folder (5): the `Folder` and the order of plone.folder, which ADR 0052 merges.
- Every BTree of the store merges its splits, the catalog and `BTreeFolder2` too.
- A store without the switch, and a site on ZEO, keep the stock behaviour. A commit with no conflict takes the same path as without the switch.
- Tier 1 is not byte for byte ZODB's resolver where the switch is on, as with the posting-dict rule. Section 5 of the specification and ADR 0042 say so.
- A transaction that wrote under another child of a node whose child was replaced commits. A transaction that wrote into the replaced child fails, and the log of a run says why each read-current node failed the vote, without a rebuild from the store.
- Rule 3 with the flag leaves no bucket out of the tree, and its commit holds two records (A and Y). It leaves no garbage for pack.
- A failed call for the lock of Y gives no answer: S2 fails with `ConflictError`. The call fails only when another transaction writes Y outside the API of BTrees or holds Y below a newer node.
- A transaction without the flag gets a `ConflictError` when both sides split one bucket: a stock client, the asyncio mode and a `zoe.Storage` without a ZODB `MVCCAdapter` try the transaction again, as ZODB's resolver makes them do. In the spike all the transactions had the native mode.
- A Python reference to X that a program keeps across the commit of S2 (in a local variable, or in an attribute of an object that the commit did not ghost) refers to an oid without a record: its next load raises `ReadConflictError` in each later transaction (`MVCCAdapterInstance.load` gets no record before the snapshot). A reference from a record of the store is reloaded, so it never names X. BTrees and the code of a Plone 6.1 instance keep no reference to a bucket across a commit.
- A vote can write a record of an oid that its transaction did not store, and can resolve a stored oid without a record. The invariants of `crates/zoe-core/CLAUDE.md` say so: the rules write records that the transaction did not store (Y); a vote takes its locks in ascending oid order, except the lock of Y, which comes after and never waits; the only read of the new bucket of the other side takes its lock; the holder hands off each exclusive lock but the lock of an oid without a record.
- The wire protocol has the extension `server_writes`. A stock `ZEO.ClientStorage` gets for its transactions, when the other side split the same bucket, the conflict of ZODB's resolver (section B11), and it gets Y in the invalidation of a commit of another client, as any record.
- A stock `ZEO.StorageServer` over `zoe.Storage` gets no flag, so the same holds for its clients.

## Alternatives

- **A rule 3 that puts X in the place of Y**, with no lock and no extension. Y leaves the tree: a transaction that inserted into Y after the split fails with `ReadConflictError` (18 of the 28 `ConflictError`s of `create-spread` in the second spike), each merge leaves a bucket of garbage until a pack (one per merge, two for the bucket of an inline root: 148,719 bytes in the three runs of that spike), an undo of a transaction that wrote into Y passes with no effect, and an undo of a transaction whose tree records changed later, which would fail with `UndoError` in ZODB, can pass.
- **The same rule with the lock of Y, for a transaction without the flag.** It closes the lost undo that runs with the merge, but it still takes Y out of the tree, so an undo after the merge still passes with no effect.
- **The same rule without a lock of Y, with the lost undo as a known limit.** An undo or a restore of Y that runs with the merge is lost.
- **The fall-back to a rule that replaces Y when the call for the lock of Y fails.** It reads Y without its lock while another transaction holds it, an undo for example, and loses that write.
- **A wait for the lock of Y within the lock timeout.** It waits out of oid order: two votes can then wait for each other until the timeout.
- **No lock of Y.** The hand-off of the node and an undo break it (sections B3 and B11).
- **The node with the bytes of its committed record, and X as the transaction stored it.** It keeps "a vote writes a record of each oid that its transaction stored", but each merge then leaves X as garbage until a pack, and each transaction that read the node after S1 meets a stale read-current node that rule 1 must pass.
- **The invalidation of the commit to the stock client that committed.** The server sends the client of S2 `invalidateTransaction(tid, oids)` with every oid of the commit, before the reply of `tpc_finish`. ZEO 6.2 applies it: the cache ends the entries, `ClientStorage.invalidateTransaction` calls `invalidate` of the database for every Connection, and the cache moves its last tid; then `tpc_finish_co` calls `setLastTid` with the same tid, which the cache accepts ("Be a little forgiving", `ZEO/cache.py:464`). So stock clients would merge too. It departs from the order of ZEO (the connection that committed gets an invalidation of its own commit), the committing Connection then drops every object that it wrote, and it rests on the behaviour of each stock version of the client, which no protocol guarantees. A later card can test it against each supported version of ZEO 6 if B5 on a stock client shows the case.
- **An extension of the reply of `vote`**, a new form of its items that says "a record that the server wrote". A bare oid that the transaction did not store already says it, to the stock client and to the native client, and a stock client would refuse a new form. What is missing is a promise of the client, which a reply cannot carry, so the declaration is in `register`.
- **The write for every client.** The server writes Y for a stock client too. Its other Connections keep a stale Y (section B4).
- **Two calls of `invalidate`** (Y alone, then `f(tid)` with the stored oids). It tears the snapshot (section B4).
- **The flag always on for `zoe.Storage`**, with a stock `ZEO.StorageServer` over `zoe.Storage` unsupported for the split merge. The flag is a promise of the database, and a `ZEO.StorageServer` does not keep it.
- **Rule 1 merges T into the bucket that replaced Y.** At the vote of T, when its path child Y was replaced by X, the vote merges the write of T to Y into X. It writes a record that T did not store too, with the same lock and the same declaration, and it must follow a chain of replacements (4 of 20 cases in the spike). The bucket of garbage stays, and so does the conflict of any other writer of Y. Section B1 removes the cause, once, in the vote of S2.
- **Rule 3 gives no answer when Y is younger than some bound.** S2 then fails instead of T: the conflict moves, it does not go.
- **Rule 3 keeps Y only when the keys of the transaction all fall below the separator of Y.** Then A takes them, Y does not change, and nothing is written outside the transaction. In the spike, 1 of 59 merges had that shape: in a tree whose keys grow, the new keys of both sides are above the separator of the split, in Y.
- **A plain insert into a bucket that the other side split, as a rule 4.** The machinery of part B can merge it: the insert of T into A goes to A or to the new bucket Y of the other side. A later card takes it if the gate needs it (2 `ConflictError`s in the second spike).
- **(b) Keys that do not grow**, from `zoe.plone`. It changes only the history ids: the keys of `zvc_repo._histories` are random already, and the oids order the keys of `ids`. Alone, it leaves 60 per 1,000 at 4 users (15 % of `zeo`): a split of any bucket changes the root that every creation reads. With rules 1 and 2 it passes the gate of B5, but not the acceptance test: the first split of each tree, and every split of `ids`, is a split of one bucket by every creator.
- **(c) Early locks on the four roots.** ADR 0044 (part B) refuses a class of tier 1, wants a leaf record, and declares a class hot, not an object. It needs hot objects by oid and a read of a whole tree past the snapshot. Its creators wait for each other from their first insert to their commit. The acceptance test cannot pass with it: each creator changes the site before any of them commits, so the second one waits for a lock that the first holds until the lock timeout.
- **(d) A queue beside the object graph for CMFEditions.** Every read of a history must read the queue too, which patches many methods of CMFEditions and ZopeVersionControl; a copy through the iterator loses the queue and leaves versions out of their trees; it does not cover the intids, so the acceptance test fails on `ids`.
- **Bigger buckets**, a subclass with a larger `max_leaf_size`. It puts a class of `zoe.plone` into the data, tier 1 does not know it, and the first split of each tree still meets every creator.
- **Rule 1 alone, or rules 1 and 2.** They write nothing new for a bucket, but they leave the splits of one bucket, which are most of the conflicts: 79.7 per 1,000 at 4 users.
- **The switch on by default in the core.** The rules merge where ZODB refuses, so a store opts in, as for the posting-dict rule.
- **A key of `<zoe>`, of `zoe.Storage` and of `zoe serve` for the switch**, as `posting-dicts` has. The policy is a record of the store, which both modes read and a copy keeps.
- **The order of key references in every merge of tier 1**, so that tier 1 also merges the plain conflicts of the buckets of `ids`. It changes tier 1 outside the switch. **No order of key references:** `ids` keeps its conflicts, and the acceptance test fails.
- **A replacement of buckets only, or also a replacement followed by a split of it.** A replacement of a node follows the same argument, and no rule writes a replacement followed by a split.
- **Rules 2 and 3 that take replacements.** A later card can do it if the log of B5 shows the case.
- **The line of a read conflict in mode `on` only.** Shadow mode would then not tell why a node fails.
- **A line for each stale read-current node of the plan without an answer, or also a line where the rules did not run.** The vote fails on the first one, and a line where the rules did not run has no reason of rule 1 to give.
- **`applied` true on the line of a read conflict in mode `on`.** The outcome `undecided` does not decide the vote, which fails as without the rules.

## Merged ADRs

- ADR 0049, the split merge keeps the bucket of the committed side: Decision, part B. Its section N is section BN here, for N from 1 to 11; its section 11 also changed rule 3 in section A2 and the undo in section A7. Its section 12 is in sections B2, B3, B4, B9 and B10. Its context is in Context, "The bucket that leaves the tree", and its alternatives are in Alternatives.
