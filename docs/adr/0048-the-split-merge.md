# 0048. The split merge: tier 1 merges the splits of a BTree, and a read-current node lets a split of another child through

Status: accepted. The checkpoint of ZOE-219 (step 2). The human approved it with the recommendation of each of the questions at its end, the widened scope fence of question 4 among them. It adds three rules to tier 1, where ZODB's resolver refuses, and a switch of the merge policy. It changes an invariant of the vote: a stale read-current oid can pass. It needs code in the vote (`crates/zoe-core/src/commit/`). After the build, the human approved the order of key references on the values of the range too (section 2, "The order of the keys"). It changes no on-disk format, no pack and no wire protocol.

ADR 0049 (accepted, ZOE-224) changes rule 3: with a client that declares `server_writes`, rule 3 merges into the bucket Y of the committed side and leaves X out; without it, rule 3 gives no answer when the committed side split the same bucket. So no merge takes a bucket out of the tree. ADR 0049 also corrects section 7 on an undo. The human approved both together. Sections 2, 3 and 7 and questions 5 and 6 below keep their text, and ADR 0049 governs where they differ.

The amendment of ZOE-225 is accepted: the human approved its text with the recommendation of each of its questions. Rule 1 also lets a node through when the other side replaced a child that is not a path child of the transaction (sections 1, 2 and 3), and the vote writes a line of the resolution log for each read-current node that fails it (section 5). Its questions are at the end ("Questions for the human: the amendment of ZOE-225").

## Context

The gate of phase 4 (#440) fails on `create-spread`: after queued indexing (ADR 0047), `zoe-native-qi` raises 27 % of the `ConflictError`s of `zeo` at 4 users and 11 % at 16 users in the runs of the spike; the limit is 10 %. The spike (`docs/spikes/concurrent-creations.md`) finds:

- Every `ConflictError` of `create-spread` but the container (5 of 190) is on four BTrees that every creation of a site shares: `_shadowStorage._storage` and `zvc_repo._histories` of `portal_historiesstorage` (CMFEditions), and `ids` and `refs` of the intid utility. Each is a root whose children are buckets.
- Each conflict is a split. Every revision of the four roots in the runs adds one child and changes nothing else. A creation inserts one key into each tree. The history id and the key reference of `ids` grow, so they go into the last bucket; the keys of the other two trees spread.
- ZODB refuses each case for the structure, not for the keys. `BTree__p_resolveConflict` refuses a node with children (reason 11), `_bucket__p_resolveConflict` refuses a bucket that a side split (reason 0), and `_BTree_set` marks each node of its path read-current, so a split of any other child under it fails the vote with `ReadConflictError`. Two inserts of different keys merge when they are in one bucket.
- The cases: two creations split the same last bucket from one state of the root (96 on the shadow storage, 22 on `ids`); a creation inserts into a bucket while another one split another child (50 read conflicts); two creations split different children (6); a creation inserts without a split into a bucket that another one split (11).
- The acceptance test (`python/zoedb/src/zoe/plone/tests/test_concurrent_creations.py`) runs rounds of eight creations from one snapshot, one folder for each creator. In each round all creators split the same bucket of each tree, or none does, and the first split of each tree is the split of the one bucket that its root holds inline.

ADR 0042 keeps tier 1 byte for byte equal to ZODB's resolver, and names two rules where ZODB refuses (the posting-dict rule and the first-key rule), under one switch, off by default. ADR 0046 puts the switches of tier 1 into the merge policy.

## Decision

### 1. Words

- A **node** is a BTree or a tree set of a family of tier 1 whose state holds children: `(children, firstbucket)`. A **child** is a bucket or a node, with its **separator**, the key before it in the node (none for the first child).
- A **split** of a child A is what `BTree_grow` does: A keeps the keys below a new separator s, a new child X gets the keys at and above s, and the node gets `(s, X)` right after A. A is the **anchor** of the split. A root that holds one bucket inline splits into a node with two new children.
- The **old** record is the record that the transaction read, the **committed** record the current one, and the **new** record the one that the transaction stored. A node **changed only by splits** when its committed record holds every child of its old record, in the same order with the same separators, and every other child of it comes right after its anchor or after another new child of the same anchor.
- A child C of the old record is **replaced** when the committed record holds, in the place of C, a new child with the separator of C, and the child after that new child has the separator of the child after C in the old record, or neither has a child after it. So the new child has the range of C, and C leaves the tree. A merge of rule 3 writes such a record: its new bucket X takes the place of the new bucket Y of the committed side, with the separator of Y. The first child is never replaced: a node keeps its first child and its first bucket. A node **changed only by splits and replacements** holds each child of its old record, or the new child that replaced it, in the same order with the same separators, and every other child of it comes right after its anchor or after another new child of the same anchor. A new child right after a replacement changes the separator after it, so a replaced child is never the anchor of a split.
- The **path children** of a node, for a transaction, are the children of its old record that the transaction stored or marked read-current. BTrees marks each node of the path of a write read-current (`PER_READCURRENT`, `BTreeTemplate.c:754`), so the path children are the children under which the transaction wrote.

### 2. The rules

The rules run in the vote that takes the locks (`vote`, `vote_resolving`), after the check of every serial, on the stale oids of the transaction, as tier 1 does. A node whose records do not decode, a node that changed by anything but splits (for rule 1: by anything but splits and replacements), or a state that the model does not hold, gives no answer: the vote goes on as without the rules.

1. **A read-current node that another child split or replaced.** The transaction marked a node read-current, did not store it, and the node changed only by splits and replacements. When the transaction has at least one path child in that node, no path child is an anchor of a split of the committed side, and no path child is replaced, the node passes the check of the vote. The vote stages nothing for it. A path child that is an anchor is a bucket that the transaction stored and the other side split: rule 3 does not cover that case, and the vote refuses as today (reason 0 on the bucket). A path child that is replaced left the tree with the writes of the transaction under it: rule 1 gives no answer, and the vote fails with `ReadConflict` on the node, as without the rules. The write to that child is not stale, so the check of the node is the only check that stops it. ZOE-224 (#455) takes that case.
2. **Splits of different children.** The transaction stored a node in which it split children, the node changed only by splits on the committed side, and no anchor of the transaction is an anchor of the committed side. The merged node is the committed node, with the new children of the transaction and their separators right after their anchors. Each bucket keeps its one writer; a bucket that the committed side changed without a split and the transaction stored merges with the rules of `bucket_merge`, as tier 1 does today.
3. **A split of one bucket.** The transaction split a bucket A of a node, or the one bucket inline in a root, and on the committed side A is:
   - split once, into A and Y, or
   - changed without a split.
   *ADR 0049 replaces the case where the committed side split A: rule 3 merges into A and Y, the buckets of the committed side, with a client that declares `server_writes`, and gives no answer without it.* The keys of the range of A merge with the rules of `bucket_merge` (`MergeTemplate.c`): the base is the old A, the committed side is A followed by Y (or A alone), and the new side is the new A followed by X, the new bucket of the transaction. The merged keys go to A and X: the keys below the separator go to A, the others to X. The separator is the separator of Y in the committed node when the committed side split, and the separator of the transaction otherwise. The next bucket of A is X; the next bucket of X is the next bucket of Y, or of the committed A. The merged node is the committed node, with X in the place of Y and the separator of Y, or with `(s, X)` right after A. The two halves must not be empty. With two splits of one bucket on the committed side, or two on the side of the transaction, the rule gives no answer.

   For the bucket inline in a root, the range is the whole tree, and both halves of each split are new objects. The merged root holds the two new buckets of the transaction, with the separator of the committed side when it split. When the committed root still holds its bucket inline, the committed side is that bucket, and the separator of the transaction applies. The two new buckets of a committed split both leave the tree.

Rules 2 and 3 do not take replacements: in a node that the transaction stored, a replaced child on the committed side is a change by more than splits, and they give no answer. The rules give no answer for a delete of a key in a range that they merge, for two splits of one child on one side, and for a child node that the transaction split and the other side changed: rule 3 merges only buckets. Each node runs the rules on its own records, at any depth of the tree, so a split of a child node that the other side did not change merges under rule 2. A refusal of `bucket_merge` inside the range (two inserts of one key, a key that both sides changed) refuses, with its reason, as tier 1 does today. A merged bucket can hold more keys than the maximum of its class, as a bucket that ZODB's resolver merges can; the next insert splits it.

**The order of the keys.** Rule 3 compares the keys of the range with the comparisons of tier 1. It also compares key references: two keys of the class `five.intid.keyreference.KeyReferenceToPersistent`, or two of `zope.keyreference.persistent.KeyReferenceToPersistent`, whose state holds an `object` that is a reference to an oid of the same database (a reference that names no database), compare by those oids. That is what `_get_cmp_keys` of zope.keyreference does in conflict resolution (`zope/keyreference/persistent.py:61-95`), where the object of each key is a `PersistentReference`. Two keys of other classes, of two classes, or with a reference that names a database, give no answer. Without this order the keys of `ids` cannot be compared, and its splits keep conflicting. The rules compare the values of the range with the same order: `bucket_merge` compares the value of a key that both sides kept, and the values of `refs` are key references. BTrees compares keys and values with the same `object_compare`, so ZODB's resolver compares them in the same way. Without the order on values, the splits of `refs` keep conflicting (the human's decision on the build).

**A commit in flight.** The rules read the committed records from the store. When the vote holds a lock with a hand-off whose commit is in flight (ADR 0044), the rules do not merge onto the copy: the vote waits until that commit is durable or failed, as for the other cases of the hand-off, and then runs the rules on the store.

### 3. What the merge writes, and why it loses no update

The rules write only records that the transaction stored: the node, the bucket A that it split, and its own new bucket X, a new object. Rule 1 writes nothing. Each merged record is staged as tier 1 stages a merge: over the serial of the committed record, under the locks of the vote, with the size limits; X stays a new object. The reply of the vote lists each of these oids as resolved, as ZEO 6 does for a conflict that its server resolved. The client stored each of them, so its connection turns them into ghosts, its cache drops them, and every other connection of its process gets their invalidation. The other clients get the invalidation of the commit.

In rule 3 the bucket Y of the committed side leaves the tree: no record of the current tree refers to it. It becomes garbage, which a pack with garbage collection removes. The card expected a merge that writes Y instead. Y is not in the transaction: the vote holds no lock on it, ZODB's `MVCCAdapterInstance` invalidates in the other connections of a process only the oids that a connection stored (`ZODB/mvccadapter.py:208-222`), and a ZEO server sends no invalidation to the client that commits. A merge that writes Y needs a lock that the vote takes late and an extension of the protocol. Writing X in the place of Y needs neither.

No update is lost:

- **Rule 1.** The transaction wrote only under its path children. Each path child is still a child of the committed node, and its range there is its range in the old node: a split adds a child right after its anchor, and no path child is an anchor; a replacement takes the place of a child with the same separator and the same separator after it, so it changes the range of no other child, and no path child is replaced. So a lookup in the current tree finds each write of the transaction where the transaction made it, and the tree after the commit is the committed tree with the writes of the transaction: the state that the two transactions give one after the other.
- **Rule 1 and a replaced child.** A replaced child is not a path child, so the transaction wrote nothing under it, and no write of the transaction leaves the tree with it. The bucket before a replaced bucket names it as its next bucket. When the other side wrote that bucket, as rule 3 writes its anchor with the new next bucket, a transaction that stored it has a stale serial: the vote checks the written serials before the read-current oids, and tier 1 refuses a bucket whose next bucket changed (reason 0). When the other side did not write it, the transaction writes the next bucket that the committed tree holds, as a transaction after the other side would. A replaced child on the path must fail: the transaction wrote into a record that the current tree no longer holds, and that write is not stale.
- **Rule 2.** The two sides split disjoint ranges. The merged node holds every child of the committed node, and each new child of the transaction with the range that its split gave it. Each bucket has one writer, or merges with `bucket_merge`. So each key routes to the bucket that holds it, and the tree holds the changes of both sides.
- **Rule 3.** The merged keys of the range of A are the merge of `bucket_merge` of its three states, so each change of either side in that range stays, and each case where `bucket_merge` refuses still refuses. The node routes each key of the range to A or X, which hold it, and every key outside the range keeps its bucket. Y holds no key that A and X do not hold.
- **The bucket that the merge reads.** Rule 3 reads Y, which the vote does not lock. Each write of a bucket of the tree goes through the node: BTrees marks the node read-current or writes it, so the vote of every writer of Y holds the lock of the node, shared or exclusive, until its commit is durable. The vote of rule 3 holds the node exclusively, since the transaction stored it. So the merge reads the last commit of Y. After the commit, a writer of Y fails its check of the node, because Y is no longer a child of it: rule 1 gives no answer for a path child that left.
- **What the argument assumes.** Writes enter the tree at its root, as the API of BTrees does. A program that writes a bucket that it takes from `__getstate__()` marks no node, as ADR 0042 says of the first-key rule. A program that marks a tree read-current itself and writes a key into it loses the check of the other children of that node under rule 1. No code in the eggs of a Plone 6.1 instance calls `readCurrent` but BTrees, and the registry backend of `zoe.plone`: ADR 0050, section 1, says why its calls keep the argument.

The spike checks the rules on real BTrees in a model outside the repository: 36,410 merges in rounds from one snapshot, each followed by `BTrees.check`, `_check` and a lookup of every key of every transaction, with no failure.

### 4. The switch: opt-in, in the merge policy

*Claude's proposal.* The rules depart from ZODB's resolver, as the posting-dict rule does, so a store opts in.

- The merge policy (ADR 0046) gets the switch `split_merges`, with the modes of tier 1: `off`, the default, `shadow` or `on`. `zoe.policy.install` and `validate` take it; the overrides file of a profile of `zoe.plone` can set it, as it sets `posting_dicts`. A store without a policy, or a policy without the switch, keeps today's behaviour.
- No new key of `<zoe>`, of `zoe.Storage` or of `zoe serve` (question 2). The policy is a record of the store, so the switch is the same in both modes, it commits with its transaction, and a copy of the store keeps it.
- `on` merges with the rules. `shadow` runs them, writes their result to the resolution log in the field `shadow`, and the vote goes on as without them: tier 1 refuses as today. ZODB writes no record for these cases, so shadow mode verifies nothing (ADR 0046, section 6). A shadow mode without a `resolution-log` gives the warning of ADR 0046, section 3.
- The rules run only where the tier 1 mode of the class is `on` or `shadow`.

### 5. The resolution log and the counters

Each record that a rule writes gives a line of tier 1 with the outcome `merged` and the new field `split`: the rule, `1`, `2` or `3`. Rule 1 gives one line for the node, with the outcome `passed`. A refusal of `bucket_merge` inside a rule gives `refused` with its reason and `split`. `zoe_structural_merges_total` of `zoe serve` counts them with their outcome. The lines keep every other field of ADR 0042.

**The read conflicts of the vote.** When the vote fails with `ReadConflict` on a stale read-current node and rule 1 gave no answer for that node, the vote writes one line for it, in modes `on` and `shadow`: tier 1, the oid and the class of the node, `read` and `committed`, and `"split":1`.

- In mode `on`, the outcome is `undecided` and its reason is the reason of rule 1, for example "a path child that the other side replaced". `applied` is false: the vote fails as without the rules.
- In mode `shadow`, the outcome is `undecided` with the reason "a read-current oid", as for a node that rule 1 lets through, and the field `shadow` holds `undecided` and the reason of rule 1.
- The vote fails on the first stale read-current oid, so it writes at most one such line. The nodes before it that rule 1 let through keep their lines `passed`, with `applied` false.
- A read-current oid on which the rules did not run gives no line, as today: an oid that is not a node of a family of tier 1, a class whose mode of tier 1 is `off`, a commit in flight that fails the vote at once, or a late wait for a commit in flight.
- `zoe_structural_merges_total` counts the line with the outcome `undecided`. The reasons are texts for the operator: no test fixes them.

### 6. A site on ZEO, and the clients

- The rules are in the store: a site on a ZEO server and a FileStorage keeps the stock behaviour. On such a store the switch is a record that nothing reads.
- A stock `ZEO.ClientStorage`, the native client and `zoe.Storage` need nothing: the store merges at the vote and replies with the oids that it resolved, which the client stored.
- No code of CMFEditions, ZopeVersionControl, zope.intid, five.intid or `zoe.plone` changes, and the rules hold for every BTree of the store, not only the four trees of the spike.

### 7. A copy, a pack and an undo

- **A copy** through the iterator (`zodbconvert`, `copyTransactionsFrom`, `zoe convert`), `zoe backup` and `zoe restore` copy the records as they are: the merged ones, the buckets that left the tree, and the policy with its switch.
- **A pack** needs no change. A bucket that left the tree is not reachable after the commit that merged, so a pack with garbage collection at a later tid removes it; a pack without garbage collection keeps it, as it keeps every record. Each merge of rule 3 leaves at most one such bucket, or two for the split of a bucket inline in a root.
- **An undo** (corrected by ADR 0049, section 11): an undo of a transaction that wrote into a bucket never passes with no effect, because no merge of rule 3 takes a bucket out of the tree. The text below held for the rule 3 of this ADR before ADR 0049. An undo runs no rule: the rules need the vote of a transaction, its stored records and its read-current oids, and an undo has three records of each oid. An undo of a transaction that merged gives back the old node, the old bucket and no X; the bucket Y comes back into the tree with the keys of the committed side. An undo of a transaction whose tree records changed later fails with `UndoError`, as in ZODB.

### 8. What the build changes, and where

- `crates/zoe-core/src/merge/`: the rules, the order of key references, and the switch in `merge::policy`. The order of key references uses the model of tier 2 (`Heap::with_instances`); if `zoe_pickle` needs a change for it, that change is outside the scope fence too.
- `crates/zoe-core/src/commit/` (the vote): the check passes a node under rule 1, and the rules decide for a group of records (a node, a bucket and a new object) and stage them together. This is **outside the scope fence** of the card (question 4). It is not a red zone.
- `crates/zoe-core/CLAUDE.md`: the invariants of the vote and of tier 1 change ("a read-current oid is never resolved"; a merge of one oid).
- `python/zoedb/src/zoe/policy.py` and `zoe/plone/merges.py`: the switch.
- No change of `format/`, of `pack/`, or of the wire protocol.
- The amendment of ZOE-225 changes `merge::split` (rule 1 reads replacements and gives its reason), `commit::split` and the vote (the plan keeps the reason of each stale read-current node without an answer, and the vote writes its entry when it fails on that node), the invariants of the vote in `crates/zoe-core/CLAUDE.md`, and section 5 of the specification. It adds no field to the line, and changes no public Python API, no trait signature, no format and no wire protocol.

### 9. The helpers of the acceptance test

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

The layer gives each test a `DemoStorage` of its own, so the policy goes away with the test.

### 10. The tests and the measure of step 4

- `crates/zoe-core/tests/`: the vote with each rule and each case where it gives no answer: a path child that is an anchor, a removal, a changed separator, a node with no path child, two splits of one bucket on one side, a delete, a refusal of `bucket_merge` inside the range, two levels of nodes, opaque keys, key references of two classes or with a database name, a commit in flight, and the switch in each mode.
- `python/zoedb/tests/`: the model of the spike as a test, against `zoe.Storage` and against `zoe serve` with the native client and a stock `ZEO.ClientStorage`. After each round, `BTrees.check` and a lookup of every key in a new connection of another process, and in another connection of the process that committed, so that a stale cache of a resolved new object shows.
- The acceptance test, green with the helpers of section 9.
- The amendment of ZOE-225: the acceptance test `crates/zoe-core/tests/split_read_conflicts.rs`. A replaced child off the path (a middle child, and a last child when the keys grow) passes in mode `on`; mode `shadow` tells that it would pass; without the switch, or with `off`, the vote keeps the stock behaviour. Four read conflicts in mode `on` write their line: a replaced child on the path, a new child with another separator, a node without a path child, and a path child that the other side split. The unit tests of `merge::split` add a replacement beside a split, two replacements side by side, a new first child, and a replacement followed by a split of it; the last two give no answer.
- **A unit test that changes.** `a_read_current_node_whose_path_child_split_gives_no_answer` in `crates/zoe-core/tests/split_merges.rs` asserts that the vote gives no entry. With the amendment the vote gives one entry for the node, `undecided` with the reason of rule 1 and `split` 1, so the test asserts that entry instead. It is a unit test of ZOE-219, not an acceptance test. `test_in_shadow_mode_the_rounds_raise_conflict_error_and_log_the_rules` in `python/zoedb/tests/test_zoe_split_merges.py` asserts that the field `shadow` of each line of the split merge is `merged` or `passed`. A round of shadow mode whose read conflict gets no answer from rule 1 now gives a line with `undecided` there (question 3 of the amendment).
- Step 4: B5 `edit` and `create-spread` at 4 and 16 users, on `zeo` and on `zoe-native-qi` with the switch on, three repetitions, on a quiet machine; the gate of #440; `just bench native` and B2, because the vote changes. The resolution log counts the merges of each rule, and the bytes of the buckets that left the trees.

## Consequences

- With the switch on, concurrent creations of versioned content commit at the first attempt as long as each creator works in its own folder. On the kept logs of the spike, `create-spread` on `zoe-native-qi` would raise 2.4 `ConflictError`s per 1,000 operations at 4 users and 9.1 at 16 users: 0.6 % and 2.4 % of `zeo`.
- What is left: a split on the committed side against a plain insert of the transaction into the split bucket (11 of 190), and two creations in one folder (5): the `Folder` and the order of plone.folder.
- Every BTree of the store merges its splits, the catalog and `BTreeFolder2` too.
- Each merge of rule 3 leaves a bucket of garbage until the next pack.
- A store without the switch, and a site on ZEO, keep today's behaviour. A commit with no conflict takes the same path as today.
- Tier 1 is no longer byte for byte ZODB's resolver where the switch is on, as with the posting-dict rule. Section 5 of the specification and ADR 0042 say so.
- With the amendment of ZOE-225, a transaction that wrote under another child of a node whose child a merge of rule 3 replaced commits. A transaction that wrote into the replaced child still fails, and the log of a run says why each read-current node failed the vote, without a rebuild from the store.

## Alternatives

- **The merge that writes the bucket of the committed side**, as the card says. It needs a lock that the vote takes after its others, and an invalidation of that bucket in the client that commits, which neither ZODB nor ZEO gives: a new extension of the protocol, and no merge for a stock client. Section 3 writes the bucket of the transaction instead.
- **(b) Keys that do not grow**, from `zoe.plone`. It changes only the history ids: the keys of `zvc_repo._histories` are random already, and the oids order the keys of `ids`. Alone, it leaves 60 per 1,000 at 4 users (15 % of `zeo`): a split of any bucket changes the root that every creation reads. With rules 1 and 2 it passes the gate of B5, but not the acceptance test: the first split of each tree, and every split of `ids`, is a split of one bucket by every creator.
- **(c) Early locks on the four roots.** ADR 0045 refuses a class of tier 1, wants a leaf record, and declares a class hot, not an object. It needs hot objects by oid and a read of a whole tree past the snapshot. Its creators wait for each other from their first insert to their commit. The acceptance test cannot pass with it: each creator changes the site before any of them commits, so the second one waits for a lock that the first holds until the lock timeout.
- **(d) A queue beside the object graph for CMFEditions.** Every read of a history must read the queue too, which patches many methods of CMFEditions and ZopeVersionControl; a copy through the iterator loses the queue and leaves versions out of their trees; it does not cover the intids, so the acceptance test fails on `ids`.
- **Bigger buckets**, a subclass with a larger `max_leaf_size`. It puts a class of `zoe.plone` into the data, tier 1 does not know it, and the first split of each tree still meets every creator.
- **Rule 1 alone, or rules 1 and 2.** They write nothing new for a bucket, but they leave the splits of one bucket, which are most of the conflicts: 79.7 per 1,000 at 4 users.
- **The switch on by default.** The rules merge where ZODB refuses, so a store opts in, as for the posting-dict rule.

## Questions for the human

Each question gives Claude's recommendation first.

1. **The mechanism.** Recommended: the split merge of section 2, with its three rules and the order of key references, in the core. Other choices: the merge that writes the bucket of the committed side (needs an extension of the protocol, and gives nothing to stock clients); keys that do not grow with rules 1 and 2 (passes B5, fails the acceptance test); early locks (cannot pass the acceptance test); a queue for CMFEditions (fails on the intids).
2. **The switch.** Recommended: `split_merges` in the merge policy only, off by default, with the three modes of tier 1. Other choices: also a key of `<zoe>`, of `zoe.Storage` and of `zoe serve`, as `posting-dicts` has; or the rules on by default.
3. **The order of key references.** Recommended: in the rules of this ADR only, under the same switch. Other choices: in every merge of tier 1, so that tier 1 also merges the plain conflicts of the buckets of `ids`, which the client resolves today; or no order, and `ids` keeps its conflicts (the acceptance test then fails, in round 8 in the runs of the planner).
4. **The scope fence.** The rules need the vote, `crates/zoe-core/src/commit/`, and the invariants of `crates/zoe-core/CLAUDE.md`, maybe `crates/zoe-pickle/` for the order of key references. Recommended: widen the fence of the card to these paths. Other choice: a card of its own for the hook in the vote, before this one.
5. **The bucket that leaves the tree.** Recommended: accept the garbage, which pack removes. Other choice: the merge that writes the bucket of the committed side (question 1).
6. **A split against a plain insert into the same bucket** (11 of 190 in the spike) stays a conflict. Recommended: accept it. Other choice: a later card that writes a bucket that the transaction did not store, with its lock and an extension of the protocol.
7. **The changes of the planner to the acceptance test**, which the human decides together with this ADR. Recommended: accept all three.
   - Eight folders, one for each creator, instead of one folder: a creation reindexes its container and sets its `modification_date`, and plone.folder appends to the order of the folder. In one folder the `Folder` conflicts in every round, and so does that order, which must conflict (spike, finding 5). That is not a tree whose keys grow, and `create-spread` has one section for each user.
   - Sixteen rounds of eight from one snapshot, instead of one batch of eight: the trees conflict only when an insert splits a bucket, and the rounds make the buckets fill and split, without a test that reads the buckets.
   - A `zoe.Storage` of its own for each test, instead of the `DemoStorage` of the layer: a `DemoStorage` gives no sequence, so every creation of a round would get the same history id, and it resolves conflicts with ZODB's resolver in Python, not with the tiers of the store.
8. **The helpers of the acceptance test.** Recommended: section 9: `mechanism_on` writes the switch into the site of the layer and gives no keyword argument; `mechanism_off` does nothing.
9. **Two creations in one folder.** Recommended: out of this card; a later card for the container and the order of plone.folder, whose `pos` tree and order list must change together. Other choice: switch tier 2 on for `modification_date` of the folder classes here, which moves the conflict to the order. ZOE-221 took this later card: ADR 0052.
10. **The profile of `zoe.plone`.** Recommended: it does not switch the rules on; a site sets `split_merges` in its overrides file or with `zoe.policy.install`. Other choice: the profile switches them on. Replaced by ZOE-239, a later decision of the human: the profile `zoe.plone:default` switches `split_merges` on, and the sites of a database compose it to the lowest mode that they ask for (ADR 0046, section 9; ADR 0052, section 6).
11. **The stack of step 4.** Recommended: the stack `zoe-native-qi` gets an option of the harness that installs the policy with `split_merges` on in the store of the run, so the gate of #440 compares `zeo` with `zoe-native-qi` with the mechanism, as the card says. Other choice: a new stack name for the mechanism.

## Questions for the human: the amendment of ZOE-225

Each question gives Claude's recommendation first.

1. **What a replacement is.** Recommended: section 1: a new child in the place of a child that is not the first one, with its separator and the separator after it, a bucket or a node. That is what a merge of rule 3 writes, and the argument of section 3 needs no more. Other choices: buckets only; or also a replacement followed by a split of it, whose range together is the old range (no rule writes that today).
2. **Rules 2 and 3 and a replacement.** Recommended: out of this card; they give no answer for a replaced child on the committed side of a node that the transaction stored. Other choice: rule 2 takes replacements too, in this card. A later card can do it if the log of B5 shows the case.
3. **The line in shadow mode, and a Python test.** Recommended: the vote writes the line in mode `shadow` too, as step 1 of the card says, with `undecided` and the reason of rule 1 in the field `shadow`; and the fence of the card widens to `python/zoedb/tests/test_zoe_split_merges.py`, so that its test of shadow mode also takes a line `undecided` with a reason, if a round gives one. Other choice: the line in mode `on` only; the Python test stays as it is.
4. **Which read conflicts give a line.** Recommended: the read-current node on which the vote fails, when rule 1 ran on it: at most one line for each vote. Other choices: a line for each stale read-current node of the plan without an answer, also after the node on which the vote fails; or also a line for a read conflict where the rules did not run (a commit in flight, an oid that is not a node), with a reason that says so.
5. **`applied` on the line of mode `on`.** Recommended: false. The outcome `undecided` does not decide the vote, which fails as without the rules, as for an `undecided` of tier 1 on a written oid. Other choice: true, as for a refusal, because the vote fails on that node.
6. **The unit test of ZOE-219 that changes.** Recommended: `a_read_current_node_whose_path_child_split_gives_no_answer` asserts the one entry of the node (`split` 1, `undecided`, a reason) in place of no entry. Other choice: it asserts only that no entry is `Merged`.
