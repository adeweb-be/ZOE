# 0048. The split merge: tier 1 merges the splits of a BTree, and rule 3 keeps the bucket of the committed side

Status: accepted. It adds three rules to tier 1 where ZODB's resolver refuses, a switch of the merge policy, and the extension `server_writes` of the wire protocol. In the vote, a stale read-current oid can pass, a vote can write an oid that its transaction did not store, and it takes one lock after its others, without a wait. It changes no on-disk format and no pack.

## Context

### Concurrent creations

The gate of phase 4 (#440) fails on `create-spread`: with queued indexing (ADR 0047), `zoe-native-qi` raises 27 % of the `ConflictError`s of `zeo` at 4 users and 11 % at 16 users, over the limit of 10 %. In the spike `docs/spikes/concurrent-creations.md`, all but 5 of 190 conflicts are splits of four BTrees that every creation shares: `_shadowStorage._storage` and `zvc_repo._histories` of CMFEditions, and `ids` and `refs` of the intid utility. ZODB's resolver refuses a node with children and a bucket that a side split. BTrees marks each node of the path of a write read-current, so a split of another child fails the vote with `ReadConflictError`. Cases: two splits of one last bucket (118), an insert beside a split of another child (50), splits of different children (6).

### The bucket that leaves the tree

A rule 3 that puts the new bucket X of the transaction in the place of the new bucket Y of the committed side takes Y out of the tree. In the spike `docs/spikes/split-merge-replaced-bucket.md`, this causes 18 of 28 `ConflictError`s. S1 splits the last bucket A into A and Y. S2 read the node before S1, splits A and puts X in the place of Y. T read the node after S1 and inserted into Y, so T must fail. A rule 3 that keeps Y removes the case. But the transaction did not store Y, so its vote holds no lock on Y, and the other Connections of its process get no invalidation of Y (part B).

## Decision

### A. The split merge

#### A1. Words

- A **node** is a BTree or a tree set of a family of tier 1 whose state holds children. The **separator** of a child is the key before it.
- A **split** of a child A, its **anchor**, puts a new child right after A with the keys of A at and above a new separator. A root with one bucket inline splits into two new children.
- The **old**, **committed** and **new** records are the one that the transaction read, the current one and the one that it stored.
- A node **changed only by splits** holds every child of its old record with the same separators, and each other child comes right after its anchor or a new child of that anchor.
- A child, not the first one, is **replaced** when a new child takes its place with its separator and the next separator. It leaves the tree. No rule here writes a replacement (section B1), but a store can hold one (section B11).
- The **path children** of a node are the children of its old record that the transaction stored or marked read-current.

#### A2. The rules

The rules run where tier 1 runs, after the check of every serial, on each node by itself at any depth. Any other case gives no answer, and the vote goes on as without them (specification, "Storage core", "Conflict resolution dispatch", item 2).

1. **A read-current node that another child split or replaced.** A stale node that the transaction marked read-current and did not store passes when the other side changed it only by splits and replacements, and split or replaced none of its path children, of which it holds at least one. The vote writes nothing for it. A replaced path child gives no answer: the write under it is not stale, so only the check of the node stops it.
2. **Splits of different children.** In a node that the transaction stored and that both sides changed only by splits of different anchors, the merged node is the committed node with the new children of the transaction right after their anchors.
3. **A split of one bucket.** The transaction split a bucket A into A and X, or the bucket inline in a root. The keys of the range of A merge with `bucket_merge`, and neither half may be empty.
   - When the other side changed A without a split, the keys below the separator of the transaction go to A, and the others go to X, a new child right after A. An inline root gets the two new buckets of the transaction.
   - When the other side split A once, into A and Y, rule 3 merges into the buckets of the committed side when the transaction has the flag of section B4 (section B1). Without the flag, it gives no answer (section B11).

Rules 2 and 3 take no replacement, and rule 3 merges only buckets. A refusal of `bucket_merge` refuses with its reason. A merged bucket can exceed the maximum size of its class until the next insert splits it.

**The order of the keys.** Two key references of one class (`five.intid.keyreference.KeyReferenceToPersistent` or `zope.keyreference.persistent.KeyReferenceToPersistent`) whose `object` is an oid of the same database compare by that oid, as `_get_cmp_keys` does. The order applies to keys and values, and only in these rules.

**A commit in flight.** When the vote holds a lock with a hand-off whose commit is in flight (ADR 0044), the rules wait until that commit is durable or failed, then read the store, not the copy.

#### A3. What the merge writes, and why it loses no update

Rules 2 and 3, when the other side did not split A, write only the node, A and the new object X, which the transaction stored (section B2 for the other case). Tier 1 stages each merged record, and the reply of the vote lists it as resolved. Each rule keeps the range of each child under which a side wrote, so a lookup finds every write (section B6). The argument rests on two points:

- **The node.** At each write of a bucket, BTrees marks the node read-current or writes it. So each writer of a bucket holds the lock of the node until its commit is durable. The vote of rule 3 holds it exclusively.
- **What the argument assumes.** Writes enter the tree at its root. A program that marks a tree read-current itself and writes a key into it loses the check of rule 1. In a Plone 6.1 instance, only BTrees and the registry backend of `zoe.plone` call `readCurrent` (ADR 0043, section B1).

The spike checks 36,410 merges on real BTrees with `BTrees.check` and a lookup of every key: no failure.

#### A4. The switch: opt-in, in the merge policy

- The merge policy (ADR 0046) has the switch `split_merges`: `off` (the default), `shadow` or `on`. Only the policy sets it, so it is the same in both modes and a copy keeps it (specification, "Storage core", "Conflict resolution dispatch").
- The profile `zoe.plone:default` sets it to `on`, and the sites of a database compose it to their lowest mode (ADR 0046, section 9; ADR 0052, section 6).
- The rules run only where the tier 1 mode of the class is `on` or `shadow`. ZODB writes no record for these cases, so shadow mode verifies nothing (ADR 0046, section 6).

#### A5. The resolution log and the counters

The lines of the rules, and the line of a node on which the vote fails with `ReadConflict` without an answer of rule 1: specification, "Storage core", "Conflict resolution dispatch". That line is `undecided`. In mode `on`, it has the reason of rule 1 and `applied` false. In mode `shadow`, it has the reason "a read-current oid", and `shadow` holds the reason of rule 1. An oid on which the rules did not run gives no line.

#### A6. A site on ZEO, and the clients

A site on a ZEO server and a FileStorage keeps the stock behaviour. No code of CMFEditions, ZopeVersionControl, zope.intid, five.intid or `zoe.plone` changes.

#### A7. A copy, a pack and an undo

A copy, `zoe backup` and `zoe restore` copy the records and the policy as they are, and run no rule. A pack needs no change: no merge takes a bucket out of the tree. An undo runs no rule. An undo of a transaction that wrote into a bucket never passes with no effect, because no merge takes a bucket out of the tree: when a later commit wrote that bucket, the undo meets that record, and tier 1 or the client resolves it, or it fails with `UndoError`, as in ZODB (section B7).

#### A8. What the build changes, and where

The rules are in `merge::split` and `commit::split`. Part A changes no `format/`, no `pack/` and no wire protocol.

#### A9. The helpers of the acceptance test

In `python/zoedb/src/zoe/plone/tests/test_concurrent_creations.py`, `mechanism_on` installs a policy with `split_merges` `on`, and `mechanism_off` does nothing. Rounds of eight creations start from one snapshot, one folder for each creator.

#### A10. The tests and the measure

Tests: `crates/zoe-core/tests/split_merges.rs`, `crates/zoe-core/tests/split_read_conflicts.rs` and `python/zoedb/tests/test_zoe_split_merges.py`. Measure: B5 with `--split-merges on` against the gate of #440 (specification, "ConflictError strategy", "Lever 4: structural, in Plone").

### B. Rule 3 keeps the bucket of the committed side

#### B1. The merge into the buckets of the committed side

When the other side split A once into A and Y, and the transaction has the flag of section B4, the keys of the range merge with `bucket_merge`. The base is the old A, the committed side is A and Y, and the new side is the new A and X. The keys below the separator of Y go to A, the others to Y. The node, its separators and the next buckets stay those of the committed side, and X gets no place in the tree. For a root whose inline bucket the other side split into Y1 and Y2, the keys go to Y1 and Y2, and the root keeps its committed record.

#### B2. What the vote writes, and the oids without a record

- The vote writes A and Y (or Y1 and Y2) over the serials of their committed records. Only here does a vote write an oid that its transaction did not store.
- The node, when its merged record is its committed record, and X (or the two new buckets of the transaction) get no record. Such an oid leaves the staging: no batch writes it, and the transaction index does not list it. It keeps its exclusive lock until the commit ends. The finish never hands it off (ADR 0044), because a finish hands off only staged oids.
- The reply of the vote lists A, Y and each oid without a record as bare oids, so the Connection that committed drops its pickles of the node and of X. A bare oid that the transaction did not store already means a record that the server wrote, as for an undo.
- Each other connection gets Y in `invalidateTransaction(tid, oids)`. The connection that committed gets none, as from ZEO.

#### B3. The lock on Y

After its plan, the vote takes the lock of Y (or Y1 and Y2) with `LockManager::try_exclusive`. The call never waits. It grants a lock that no transaction holds, with no waiter and no hand-off. Else it fails, and rule 3 gives no answer. The rule reads Y only under this lock.

Such a lock means that every commit of Y is durable (ADR 0044, section A4), so the rule reads the last commit of Y. Each later writer of Y, undo and restore included, waits for the lock and then has a stale serial. The call adds no wait, so no deadlock. It usually finds the lock free, because each writer of Y through BTrees takes the lock of the node first (section A3, "The node"). It fails when an undo or a restore writes Y, when Y is below a newer node, or when a program writes Y without the node (section A3, "What the argument assumes").

A finish releases the locks that it did not hand off before it wakes the votes that wait for its commit (specification, "Storage core", "The lock hand-off (ADR 0044)"). So these votes find its new buckets free. When a finish hands off a node that rule 2 wrote, the lock of Y makes the new owner wait for Y.

#### B4. Which client may get a record that it did not store: the extension `server_writes`

A stock `ZEO.ClientStorage` ghosts Y only in the Connection that committed: `MVCCAdapterInstance` invalidates in the other Connections only the oids that a Connection stored. So the vote writes Y only for a client that declares `server_writes` in `register` (specification, "Server mode", "Extension negotiation"). At the finish, such a client calls `invalidate(tid, oids)` of the database once, right before `f(tid)`, with every oid of the commit. Two calls tear the snapshot: the first sets `_ltid`, so a Connection that polls between them sees a part of the commit.

- The native mode of `zoe.zeo.ClientStorage` declares it (`_NATIVE_EXTENSIONS`). It takes the oids of its commit from the invalidation of its listener (`Route::Own`, `Turn::unstored_commit`), also with the cache of the pool off (specification, "Server mode", "The native client (F28)").
- `zoe.Storage` sets the flag in `registerDB` when its database is a ZODB `MVCCAdapter`, and reads the oids of the commit with `_unstored_commit`.
- The asyncio mode, the Rust `Pool` of `zoe-client` by default, a stock `ZEO.ClientStorage`, and `zoe.Storage` under another database do not.

A vote of `zoe serve` gets the flag from its connection. A transaction without the flag never gets a record that it did not store.

#### B5. The switch, and shadow mode

Section B1 runs under `split_merges`, when the vote has the flag. In mode `shadow` the vote writes nothing, takes no lock of Y and reads Y without it. The field `shadow` gives the outcome as if the lock were free.

#### B6. Why no update is lost

The merged keys of the range are the merge of `bucket_merge` of its three states, and the split merge routes each key of the range to the bucket that holds it, A or Y. Each other key keeps its bucket, and the committed node keeps every child, so the writes of other transactions stay where a lookup finds them. A later writer of Y, such as T, meets the record of S2 as a stale serial (section B3). The clients drop the old Y (section B4).

#### B7. A copy, a pack and an undo

A copy keeps the records: the commit of S2 holds A and Y, and no record of the node or of X. A pack needs no change, and the oid of X stays unused. An undo runs no rule. An undo of S2 gives back A and Y before S2. An undo of T meets the record of S2 as a later change: tier 1 or the client resolves it, or it fails with `UndoError`. An undo that runs with S2 holds the lock of Y, so rule 3 gives no answer.

#### B8. What the build changes, and where

Section B1 is in `merge::split`, the lock of Y in `lock/`. Part B changes no `format/`, no `pack/`, no reply of `vote` and no `invalidateTransaction`.

#### B9. The resolution log and the counters

- A and Y give lines `merged` with `split` 3. The `read` and `committed` of Y are the serial that the merge read. The node and X give a line `merged`, `split` 3, no keys and `changed` 0, so `Txn::natively_resolved` lists them.
- A failed call for the lock of Y gives a line of rule 3 on the node or the inline root (`merge::split::LOCKED`): `undecided`, the reason "a bucket of the other side that another transaction locks", mode `on`, `applied` false, beside the line of tier 1 of the node.
- `zoe_structural_merges_total` counts these lines. No other answer of rules 2 and 3 on a written node gives a line. The rule has no `debug` event.

#### B10. The tests and the measure

- Tests: `crates/zoe-core/tests/split_merges.rs`, the unit tests of `lock/` and `python/zoedb/tests/test_zoe_split_merge_keeps_the_bucket.py`.
- The crash simulation `crates/zoe-server/tests/simulated_split_merges.rs` passes 1,000 seeds (specification, "Performance and testing harness", "C. Durability and crash consistency", item 6). The finish of a commit whose vote resolved oids hands off no more locks than the records of its commit, a general bound. The finish of a vote that wrote Y hands off no lock: in a tree of one node, every other writer of A or Y waits for the lock of the node, which the vote holds until its commit is durable. It does not cover a hand-off to or from a vote of rule 3, a split of a node, or an inline root.
- The measure (B5, one repetition, at 4 and 16 users): `create-spread` gives 106 and 107 `ConflictError`s on `zeo`, 0 and 4 on `zoe-native-qi`; `edit` gives 106 and 86, and 0 and 0. No read conflict "a path child that the other side replaced" is left.

#### B11. No answer without the flag, and a replaced bucket in a store

Without the flag, rule 3 gives no answer when the other side split A or the inline bucket, and the client gets the `ConflictError` of ZODB's resolver. Rule 3 gives that answer before it reads Y, so the only read of a bucket of the other side is under the lock of Y (section B3). A store can hold records that put X in the place of Y: their buckets stay out of the tree, an undo of a transaction that wrote into one of them has no effect on the tree, and rule 1 lets their node through (section A2).

## Consequences

- With the switch on, concurrent creations of versioned content commit at the first attempt when each creator works in its own folder. Every BTree merges its splits, the catalog too.
- What is left: a plain insert into a bucket that the other side split, and two creations in one folder (ADR 0052).
- With the switch, tier 1 is not byte for byte ZODB's resolver (ADR 0042).
- A Python reference to X kept across the commit of S2 names an oid without a record: its next load raises `ReadConflictError`. BTrees and Plone 6.1 keep none.
- The new invariants of the vote are in `crates/zoe-core/CLAUDE.md`.

## Alternatives

- **A rule 3 that puts X in the place of Y**, with or without a lock of Y, or as a fall-back when `try_exclusive` fails. Y leaves the tree: later inserts into Y fail, garbage stays until a pack, an undo can pass with no effect, and a read of Y without its lock loses a write.
- **A wait for the lock of Y.** It waits out of oid order: a deadlock until the timeout.
- **No lock of Y.** The hand-off of the node and an undo lose a write (sections B3 and B11).
- **The node with its committed bytes, and X as stored.** X becomes garbage, and readers of the node after S1 meet a stale node.
- **An invalidation of its own commit to a stock client.** It departs from ZEO, and no protocol guarantees it.
- **A new item form in the reply of `vote`.** A bare oid already says it, and a stock client refuses a new form.
- **The write of Y for every client, or two calls of `invalidate`.** A stale Y or a torn snapshot (section B4).
- **The flag always on for `zoe.Storage`.** A `ZEO.StorageServer` over it does not keep the promise.
- **Rule 1 merges T into the bucket that replaced Y.** It follows chains of replacements, and the garbage stays.
- **No answer when Y is young.** The conflict moves from T to S2.
- **Keep Y only when the new keys fall below its separator.** 1 of 59 merges has that shape.
- **A rule 4 for a plain insert into a bucket that the other side split.** 2 `ConflictError`s: the gate does not need it.
- **Keys that do not grow, from `zoe.plone`.** Alone, 15 % of `zeo`, and the first split of each tree still conflicts.
- **Early locks on the four roots (ADR 0044, part B).** The creators wait for each other until the lock timeout.
- **A queue beside the object graph for CMFEditions.** Many patches, a copy loses it, and it misses the intids.
- **Bigger buckets.** A class of `zoe.plone` in the data, and the first split still meets every creator.
- **Rule 1 alone, or rules 1 and 2.** The splits of one bucket stay: 79.7 per 1,000 at 4 users.
- **The switch on by default, or as a key of `<zoe>` and `zoe serve`.** A store opts in, and a policy record is the same in both modes and in a copy.
- **The order of key references in all of tier 1.** It changes tier 1 outside the switch. **No such order:** `ids` keeps its conflicts.
- **Replacements of buckets only, or in rules 2 and 3 too.** Nodes follow the same argument, and no measure needs the rest.
- **Other lines of a read conflict**: in mode `on` only (shadow mode cannot tell why a node fails), one for each stale node or where the rules did not run (no reason of rule 1), or with `applied` true (`undecided` does not decide the vote).

## Merged ADRs

- ADR 0049, the split merge keeps the bucket of the committed side: Decision, part B. Its section N is section BN here, for N from 1 to 11; its section 11 is also in rule 3 of section A2 and in section A7. Its section 12 is in sections B2, B3, B4, B9 and B10. Its context is in "The bucket that leaves the tree", its alternatives in Alternatives.
