# 0049. The split merge keeps the bucket of the committed side: rule 3 writes into it, and a declared client invalidates it

Status: accepted. The human approved it with the recommendation of questions 1 to 9 and, for question 10, no answer for a transaction without the flag. The checkpoint of ZOE-224 (step 2): it changes the wire protocol (a new extension of `register`), the lock manager (a lock that the vote takes after its others, without a wait, and a lock that the finish never hands off) and rule 3 of the split merge (ADR 0048). Section 11 amends ADR 0048: without the flag, its rule 3 gives no answer when the other side split the same bucket, so no merge takes a bucket out of the tree, and the text of its section 7 on an undo is corrected. The human approved both together. It changes no on-disk format and no pack.

## Context

ADR 0048 merges the splits of a BTree at the vote. Its rule 3 merges two splits of one bucket A: when the committed side split A into A and a new bucket Y, the merge puts the new bucket X of the transaction in the place of Y, with the separator of Y. Y leaves the tree. ADR 0048 chose this because the transaction stored X and did not store Y: a merge that writes Y needs a lock that the vote takes late and a way to tell the clients of the process that committed, and ADR 0048 wanted neither (its questions 5 and 6).

The spike of ZOE-224 (`docs/spikes/split-merge-replaced-bucket.md`) measures what that costs. On `create-spread` with the split merge on, 18 of the 28 `ConflictError`s of three runs (1,038 operations) are read conflicts "a path child that the other side replaced". Each of the 20 such lines of the whole runs has the order of events of the card:

1. S1 splits the last bucket A of a tree whose keys grow into A and a new bucket Y.
2. S2 read the node before S1 and also split A. Rule 3 puts X in the place of Y. Y leaves the tree.
3. T read the node after S1 and before S2 commits, and inserted its key into Y. Its write to Y is not stale, but the node no longer holds Y. Rule 1 gives no answer, and the vote fails with `ReadConflictError` on the node.

The refusal is right: a pass would lose the insert of T. 4 of the 20 lines of the whole runs follow a chain: a third transaction that read before S1 replaced X in turn. Every merge of rule 3 in the runs replaced a bucket (59 of 59), because the keys grow: every creation splits the last bucket, and two creations that read one state of the node split the same bucket.

A bucket of the committed side leaves the tree only in rule 3: rule 2 adds children, and rule 1 writes nothing. So a rule 3 that keeps Y in the tree removes the case.

## Decision

### 1. Rule 3 keeps the bucket of the committed side

When the committed side split A once into A and Y, and the client of the transaction takes records that it did not store (section 4), rule 3 merges as follows:

- The keys of the range of A merge with the rules of `bucket_merge`, as in ADR 0048: the base is the old A, the committed side is A followed by Y, the new side is the new A followed by X.
- The merged keys below the separator of Y go to A, the others to Y. The next bucket of A stays Y, and the next bucket of Y stays the next bucket of the committed Y.
- The merged node is the committed node: the merge adds no child, removes no child and changes no separator.
- X gets no place in the tree.
- A or Y must not be empty, as in ADR 0048.

For the bucket that a root holds inline, the committed side split it into two new buckets Y1 and Y2 (both halves of a split of an inline bucket are new objects). The merged keys go to Y1 and Y2 at the separator of the committed side, and the merged root is the committed root. The two new buckets of the transaction get no place in the tree.

When the committed side changed A without a split, rule 3 is the rule of ADR 0048: X is a new child with the separator of the transaction, and no bucket leaves the tree.

When the client of the transaction does not take records that it did not store, and the committed side split A, rule 3 gives no answer (section 11): it never puts X in the place of Y, so no merge takes a bucket out of the tree. The same holds for the bucket that a root holds inline and that the committed side split. Rule 3 when the committed side changed A without a split does not change.

### 2. What the vote writes, and the oids without a record

- **A** (or the inline root): the merged record, over the serial of its committed record, as today.
- **Y** (or Y1 and Y2): the merged record, over the serial of its committed record. The transaction did not store Y. It is the first record that a vote writes for an oid that its transaction did not store.
- **The node**: no record when its merged record is its committed record, which is the case unless rule 2 merged another split of the node in the same vote. *(Question 5.)*
- **X** (or the two new buckets of the transaction): no record. X is a new object that no record of the store refers to. *(Question 5.)*

A stored oid that the merge resolves without a record (the node N, X) has these invariants:

- It leaves the staging of the transaction. The batch writes no record of it, and the transaction index does not list it, so no other client gets an invalidation of it.
- Its lock stays exclusive and the transaction holds it until its commit is durable or fails, as any lock of the vote. The finish never hands it off (ADR 0044): there is no staged record to give, and the staged pickle of the client is wrong (its N refers to X). A vote that waits for it gets it at the release, and reads the store.
- The reply of the vote lists it as resolved. Else the Connection that committed keeps the N of its client, which refers to X, and its cache keeps the pickles of N and X.

The reply of the vote lists A, Y, and each stored oid without a record as resolved, with the bare oid of ZEO 6, as for any merge of tier 1. The reply keeps its form: a bare oid that the transaction did not store already means a record that the server wrote, as the oids of an undo (ZEO's `TransactionBuffer` yields such an oid with no data, and the native client ends its current entry, section "Server mode" of the specification). The card expected an extension of the reply. It is not needed.

The commit holds the records of A and Y. So the transaction index lists Y, and each other connection of the store gets Y in `invalidateTransaction(tid, oids)`, a stock client too, as for any record of the commit. The connection that committed gets no invalidation, as from ZEO. That rule holds for each connection, not for each client process: the listener of the pool of the native client is another connection, so it gets the invalidation of each commit of the pool, with every oid of the transaction index. Section 4 says how the process that committed uses it.

### 3. The lock on Y

The vote takes the exclusive lock of Y after its plan, with a new call of the lock manager that never waits: the call grants the lock when no transaction holds it, no request waits for it and the entry carries no hand-off (ADR 0044); else it fails at once. When it fails, rule 3 gives no answer: the vote goes on as without the rule, and in the runs of the spike S2 then fails with `ConflictError` and its client tries again. For a root that held its bucket inline, the vote takes the locks of Y1 and Y2, or gives no answer.

The plan finds Y in the committed node, which the vote holds, and takes the lock of Y before rule 3 reads Y. So the rule reads Y under its lock.

**Why the lock of Y is enough.** The main path rests on the lock of Y alone:

- A lock that is free, with no waiter and no hand-off, means that every commit of Y is durable (ADR 0044, "Notes of the build"). So the rule reads the last commit of Y, the inserts of the transactions that wrote into Y after S1 included.
- From then on, each other writer of Y waits for the lock of Y: a vote that stored Y, an undo and a restore alike, since each takes an exclusive lock on each oid that it writes. It gets the lock at the hand-off of the finish of S2, with the staged record of S2 (Y is a bucket, a class of tier 1, so the hand-off applies), or at the release when S2 is durable. Its serial of Y is then stale. So no write of Y goes over the record of S2 unseen.
- The vote never waits out of oid order, so the order of the locks still excludes a deadlock, and the call never passes a request that waits.

**Why the call usually finds the lock free.** Each writer of Y through the API of BTrees holds the lock of the node, shared or exclusive, until its commit is durable (ADR 0048, section 3). The vote of rule 3 holds the node exclusively, since its transaction stored the node. In the trees of the spike, Y is newer than its node, so its oid is above the oid of the node, and every such writer takes the lock of the node first: none holds or waits for the lock of Y. The call fails when an undo or a restore writes Y (they write Y without the node), when a split of a node moved Y under a newer node (the oid of Y is then below the oid of its node, and a writer of Y can hold the lock of Y while it waits for the node), or when a program writes Y without the node (ADR 0048, "What the argument assumes").

**A vote that the end of a commit in flight wakes.** A vote S2 that got a lock at the hand-off of the finish of S1 waits until the commit of S1 is durable or failed before the rules read the store (ADR 0044). The finish of S1 releases the locks that it did not hand off, Y among them, before it ends its commit, and the end of the commit wakes S2. So S2 finds the new buckets of S1 free, and the call does not fail for them. The release still comes after the commit is durable or failed. The other order, the end of the commit before the release, gave S2 a `ConflictError` that it did not need. That window existed only when every lock that S2 waited for came by a hand-off: the bucket that a root holds inline, where S1 and S2 share only the root, because both halves of each split are new objects. When both sides split a bucket A of a node, both store the node and A. S2 waits in one queue at a time, so the finish of S1 hands it at most one of these locks, and S2 gets the other one at the release of S1, the same release that frees Y. So S2 found Y free in that case with both orders.

**Where the hand-off of the node matters.** When rule 2 gives the node a record in the same vote, the finish of S2 can hand the lock of the node to a vote T' that stored the node and waits for it. T' then goes on to its other locks. Without the lock of Y, T' would take the lock of Y freely, read Y from the store before S2 is durable, find its serial current, and write Y over the record of S1: the merge of S2 would be lost. With the lock, T' waits for Y, as above. When the node has no record (section 2), the finish never hands it off, and T' waits until S2 is durable.

### 4. Which client may get a record that it did not store: the extension `server_writes`

A ZODB process with a stock `ZEO.ClientStorage` cannot take a record that its transaction did not store. The line numbers are those of ZODB 6.2 and ZEO 6.2, the versions of the instances of the spike:

- `ClientStorage.tpc_vote` puts each bare oid of the reply into `server_resolved`. `TransactionBuffer` yields Y with no data (`ZEO/TransactionBuffer.py:94`), so `tpc_finish_co` ends the current entry of Y in the cache of the client (`ZEO/asyncio/client.py:778`). This part is correct.
- `Connection.tpc_vote` turns each oid of the reply into a ghost in the Connection that committed (`ZODB/Connection.py:675-695`). This part is correct.
- The other Connections of the process get only the oids that the committing Connection stored: `MVCCAdapterInstance.tpc_finish` gives `_modified` to `_invalidate_finish`, and `_modified` holds only the oids of `store` and `storeBlob` (`ZODB/mvccadapter.py:201-223`, `_invalidate_finish` at line 91). The server sends no invalidation to the connection that committed. So another Connection that holds Y in its object cache keeps the state of Y before the commit, after it moved past the commit: a lookup of a key that S2 merged into Y misses. That is a stale read.

So the server writes Y only for a client that says it invalidates Y in all its Connections, and the vote gets one flag from its skin: whether its client takes records that it did not store.

**The declaration.** A new extension of `register` (section "Extension negotiation" of the specification):

- **`server_writes`**: the connection declares that, at the finish of a transaction whose `vote` reply listed an oid that the transaction did not store, the client ends the current entry of that oid in its cache, and gives every Connection of its process one invalidation of the tid of the commit with every oid of the commit, before `lastTransaction()` gives that tid. `get_info` lists the name when the server accepted it.

**One invalidation with every oid.** The client calls `invalidate(tid, oids)` of the database once, right before the callback `f(tid)`, with every oid of the commit: the oids that the transaction stored and the oids that the server wrote. Two calls would tear the snapshot: `MVCCAdapterInstance._invalidate` sets `_ltid` to the tid (`ZODB/mvccadapter.py:130-136`), and `poll_invalidations` starts the snapshot after `max(lastTransaction(), _ltid)` (line 143). Another Connection that polls between a call with Y alone and `f(tid)` gets the snapshot `tid + 1` with only Y invalidated: it reads the new Y and the A of its cache from before S2. With one call, a Connection that polls sees all the oids of the commit or none, as for the commits of the other clients. `f(tid)` then gives the stored oids once more, which changes nothing.

**The clients that declare it, and only these:**

- The native mode of `zoe.zeo.ClientStorage` (`_NATIVE_EXTENSIONS`, `crates/zoe-py/src/client.rs`, the pool of `zoe-client`). Its finish takes the oids of its own commit from the listener: the dispatcher of the pool gets the invalidation of the commit of the pool on its listener (`Route::Own` in `crates/zoe-client/src/pool.rs`), with every oid of the transaction index, and hands that list to the finish at its turn. The finish calls `invalidate(tid, oids)` with it before `f(tid)`, while the dispatcher holds the tid. The cache of the pool ends the current entry of each oid of the reply that the transaction did not store, as today. With the cache of the pool off, the finish still calls `invalidate`.
- The in-process path of `zoe.Storage`, when its database is a ZODB `MVCCAdapter` (what `registerDB` gets from `ZODB.DB`). Its finish knows the written oids of the transaction from the core, and calls `invalidate(tid, oids)` with them before the callback. A `zoe.Storage` whose database is something else, a stock `ZEO.StorageServer` for example, does not set the flag. *(Question 9.)*
- Not the asyncio mode of `zoe.zeo.ClientStorage` (`EXTENSIONS`, which `_NATIVE_EXTENSIONS` copies today: the build gives the native mode its own tuple). Not the Rust `Pool` of `zoe-client` by default: `zoe-bench`, `zoe-mcp` and Loupe declare only what they declare today. Not a stock `ZEO.ClientStorage`, which declares nothing.

A vote of `zoe serve` gets the flag from the declaration of its connection. A transaction without the flag never gets a record that it did not store.

### 5. The switch, and shadow mode

No new switch. The rule of section 1 runs where the split merge runs (`split_merges` `on` or `shadow` in the merge policy, ADR 0048) and the vote has the flag of section 4. In mode `shadow` the vote writes nothing, as today. It takes no lock of Y and reads Y without it: the field `shadow` of the lines gives the outcome of the rule of section 1 as if the call had found the lock free. That outcome is a report, so a read of Y without its lock changes no record.

### 6. Why no update is lost

- **The merge.** The merged keys of the range of A are the merge of `bucket_merge` of its three states, as in ADR 0048. They go to A and Y, the children of the committed node that hold the range: the node routes each key below the separator of Y to A and each other key to Y. Each key outside the range keeps its bucket. X held only keys of the range. So the tree after the commit is the committed tree with the changes of the transaction.
- **No bucket leaves the tree.** The committed node keeps each of its children. So each write of another transaction under a child of the node stays where a lookup finds it.
- **The read of Y.** The vote reads Y under its exclusive lock, which it takes only when every commit of Y is durable (section 3). So it reads the last commit of Y.
- **A later writer of Y.** It waits for the lock of Y (section 3). Its serial of Y is then stale, so tier 1 merges, refuses with the reason of BTrees, or gives the conflict to the client. An undo of a transaction that wrote Y meets the record of S2 as a later change of Y.
- **The oids without a record.** The node and X leave the staging, so no batch writes the pickle of the client, which refers to X, and no hand-off gives it to another vote (section 2).
- **T.** T read the node after S1 and stored Y. The node has no new record, so its read-current check passes; when rule 2 gave the node a new record in the same vote, the node holds every child that T read, and rule 1 passes it. Its write to Y is stale against the record of S2: tier 1 merges the inserts of different keys (for the trees of `intids`, whose keys are key references that the plain tier 1 does not compare, tier 1 gives no answer and the client resolves with `_p_resolveConflict`, as today for a plain conflict on a bucket of `ids`). So T commits with its key in Y, which the tree holds.
- **The clients.** The process of S2 drops Y in its cache and gives every oid of the commit to all its Connections in one invalidation, before it publishes the tid (section 4). Every other connection gets Y in the invalidation of the commit. No Connection that moved past the commit keeps the state of Y before it, and no Connection gets a snapshot with a part of the commit.

### 7. A copy, a pack and an undo

- **A copy** through the iterator (`zodbconvert`, `copyTransactionsFrom`, `zoe convert`), `zoe backup` and `zoe restore` copies the records as they are: the commit of S2 holds A and Y, and no record of the node or of X. A restore transaction runs no rule.
- **A pack** needs no change. A merge of rule 3 with the flag leaves no bucket out of the tree, so it leaves no garbage: ADR 0048 left one bucket per merge, two for the bucket of an inline root (148,719 bytes in the three runs of the spike). X has no record. The oid of X stays unused, as an oid that `new_oid` gave to an object that never committed.
- **An undo** runs no rule, as in ADR 0048. An undo of S2 gives back the records of A and Y before S2: the keys of S2 go away and the keys of S1 stay. ADR 0048 gave back the old node, the old A and no X, and put Y back into the tree. An undo of T, which inserted into Y, meets the record of S2 of Y as a later change: tier 1 or the client resolves it, or it fails with `UndoError`, as in ZODB. An undo that runs with S2 holds the lock of Y, so the call of section 3 fails and rule 3 gives no answer.

### 8. What the build changes, and where

- `crates/zoe-core/src/merge/split.rs`: rule 3 of section 1, the inline root included, and the records it gives (A and Y written, the node and X resolved without a record).
- `crates/zoe-core/src/commit/`: the flag of the client; the lock of Y after the plan, with no answer when it fails, with the flag; no answer of rule 3 without the flag when the committed side split; a stored oid that leaves the staging and keeps its lock without a hand-off; a written oid that the transaction did not store; their lines in the resolution log.
- `crates/zoe-core/src/lock/`: the call that never waits, and a lock that the finish never hands off. This is outside the scope fence of the card (question 7). It is not a red zone.
- `crates/zoe-core/CLAUDE.md`: the invariants that change (section "Consequences").
- `crates/zoe-server/src/`: the extension `server_writes` in the negotiation, and the flag of the vote.
- `crates/zoe-client/src/` and `crates/zoe-py/src/`: the declaration of the native mode, and the oid list of the own commit (`Route::Own`) for the finish.
- `python/zoedb/src/zoe/zeo.py` (the tuple of the native mode, and `invalidate(tid, oids)` before the callback) and the finish of `zoe.Storage` (the same, with a ZODB `MVCCAdapter` only).
- `docs/spec/technical-specification.md`: "Extension negotiation" (the name), "Invalidations" and the client sections (what a declared client does), section 5 (rule 3).
- `docs/adr/0048-the-split-merge.md`: its status line points to this ADR; section 2 (rule 3), section 3 (the read of Y), section 7 (the undo) and questions 5 and 6 point to it.
- No change of `format/`, of `pack/`, of the reply of `vote`, or of `invalidateTransaction`.

### 9. The resolution log and the counters

- A gives its line of rule 3, as today. Y gives a line of tier 1 with the outcome `merged` and `split` 3; its `read` and `committed` are the serial of the record of Y that the merge read. The node and X give a line with the outcome `merged`, `split` 3, no keys and `changed` 0, so `Txn::natively_resolved` lists them and the reply gives them.
- `zoe_structural_merges_total` counts these lines as today. No new counter.
- A failed call for the lock of Y gives the line of rule 3 on the node with the outcome `undecided` and the reason "a bucket of the other side that another transaction locks", and the vote goes on as without the rule. The node keeps its line of tier 1, on which the vote fails as without the rule. The line of rule 3 has the mode `on` and `applied` false, as the line of rule 1 for a read conflict, and `zoe_structural_merges_total` counts it as `undecided`. The bucket that a root holds inline gives the same line on the root.

### 10. The tests and the measure of step 4

- `crates/zoe-core/tests/`:
  - the three transactions of the card in their order, with the flag: all commit, the tree holds every key, and the model of `tests/split_merges.rs` checks the tree;
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
- The crash simulation, with the rule of section 1 and the hand-offs on.
- `python/zoedb/tests/`:
  - the order of the card against `zoe.Storage` and against `zoe serve` with the native mode, with a second Connection of the committing process that holds Y in its object cache before the commit of S2 and reads the key of S2 after it;
  - the torn window: a second Connection that polls while the finish of S2 delivers its invalidation sees all the oids of the commit or none (A and Y together);
  - the native mode with its cache off;
  - the asyncio mode of `zoe.zeo.ClientStorage` does not declare `server_writes`, and rule 3 gives no answer for its transactions when the other side split the bucket;
  - `zoe serve` with a stock `ZEO.ClientStorage`, which declares nothing: rule 3 gives no answer for its transactions when the other side split the bucket.
- The acceptance test of the planner.
- Step 4 of the card: B5 `edit` and `create-spread` at 4 and 16 users, `zeo` against `zoe-native-qi` with the switch on.

### 11. The amendment of ADR 0048: no bucket leaves the tree, and an undo

The human approved this section with the rest of this ADR.

**No answer without the flag.** For a transaction without the flag of section 4 (a stock `ZEO.ClientStorage`, the asyncio mode of `zoe.zeo.ClientStorage`, a `zoe.Storage` whose database is not a ZODB `MVCCAdapter`), rule 3 gives no answer when the committed side split the bucket A, or split the bucket that a root holds inline. The vote goes on as without the rule, and the client gets the `ConflictError` of ZODB's resolver. Rule 3 never puts X in the place of Y, so no merge of the split merge takes a bucket out of the tree. When the committed side changed A without a split, rule 3 merges as in ADR 0048: X is a new child of the transaction, and no bucket leaves the tree.

**No read of Y without its lock.** On that path rule 3 sees the split in the committed node and gives no answer before it reads Y, so the vote reads no bucket that its transaction did not store and did not lock. Rule 2 reads the committed node and the buckets that its transaction stored. So the only read of a bucket of the other side is the read of section 3, under the lock of Y. An undo or a restore of Y, which writes Y without the node, can no longer run with a read of Y and be lost.

**A correction of section 7 of ADR 0048.** Its text, "An undo of a transaction whose tree records changed later fails with `UndoError`, as in ZODB", was wrong for a bucket that the rule 3 of ADR 0048 took out of the tree: after S2 replaced Y with X, an undo of T, which wrote only Y, found the record of T current on Y, passed, and had no effect on the tree, since the key of T stayed in X. With this ADR no merge takes a bucket out of the tree, so the case is gone for every client. The text of section 7 of ADR 0048 becomes: "An undo of a transaction that wrote into a bucket never passes with no effect: no merge of rule 3 takes a bucket out of the tree (ADR 0049). When a later commit wrote that bucket, the undo meets that record: tier 1 or the client resolves it, or it fails with `UndoError`, as in ZODB." Records that a store got from the rule 3 of ADR 0048 before this ADR keep their replaced buckets: an undo of a transaction that wrote into one of them still has no effect on the tree.

### 12. Notes of the build

- The finish hands off only the oids that it stages (ADR 0044), and an oid resolved without a record leaves the staging when the vote passes. So the finish never hands it off, with no change of the lock manager but `LockManager::try_exclusive`, the call that never waits.
- A failed call for the lock of Y gives the line of section 9 on the node (`merge::split::LOCKED`, in `commit::split`). No other answer of rules 2 and 3 on a written node gives a line. A transaction without the flag gives no line of its own: rule 3 gives no answer, and the vote goes on as without the rule, with the line of tier 1 for the node. No `debug` event is built: no section of this ADR describes one, and the core logs only WARN events.
- The finish releases the locks that it did not hand off before it ends its commit in flight (section 3, "A vote that the end of a commit in flight wakes"). The release comes after `GroupCommit::finish` returns, so after the commit is durable or failed, and after the settle of the hand-offs that the transaction got. It reads no clock: the log of `lock-warn` comes after the end of the commit. `tests/split_merges.rs` holds the vote that the end of the commit wakes, with the inline root.
- The pool of the native client marks a transaction whose vote listed an oid that it did not store. Its turn then gives the oids of the commit from the invalidation of the listener (`Turn::unstored_commit`), and `zoe.zeo.ClientStorage` calls `invalidate(tid, oids)` of the database with them before `f(tid)`. The oids of an undo take the same path: the Connections get them twice, which changes nothing.
- `zoe.Storage` sets the flag in `registerDB` when its database is a ZODB `MVCCAdapter`. Its `tpc_finish` (in Python) reads every oid of the commit before the finish (`_unstored_commit`) and calls `invalidate(tid, oids)` before the callback.
- Step 4 of the card, one repetition of each cell (B5, `--steps 4:60,16:60`): `create-spread` gives 106 and 107 `ConflictError`s on `zeo` and 0 and 4 on `zoe-native-qi` with the switch on, at 4 and 16 users; `edit` gives 106 and 86 on `zeo` and 0 and 0. No read conflict: the resolution log of the `create-spread` run has no line "a path child that the other side replaced". The 4 conflicts are 3 on a `Folder` and 1 on an `OOBucket` ("Conflicting bucket split" of BTrees). The other two repetitions did not run.

## Consequences

- A transaction that inserted into a bucket that a split merge of another transaction had to replace commits. On the logs of the spike, an estimate (the runs did not have the rule): 18 of the 28 `ConflictError`s of `create-spread` on `zoe-native-qi` go, 9.6 per 1,000 operations instead of 27.0.
- Rule 3 with the flag leaves no bucket out of the tree, and a commit of rule 3 holds two records instead of three (A and Y). The bucket of garbage of ADR 0048 goes.
- A failed call for the lock of Y now gives no answer where ADR 0048 merged: S2 fails with `ConflictError`. The call fails only when another transaction writes Y outside the API of BTrees or holds Y below a newer node.
- A transaction without the flag gets a `ConflictError` where ADR 0048 merged two splits of one bucket: a stock client, the asyncio mode and a `zoe.Storage` without a ZODB `MVCCAdapter` try the transaction again, as ZODB's resolver makes them do. In the spike all the transactions had the native mode.
- A Python reference to X that a program keeps across the commit of S2 (in a local variable, or in an attribute of an object that the commit did not ghost) now refers to an oid without a record: its next load raises `ReadConflictError` in each later transaction (`MVCCAdapterInstance.load` gets no record before the snapshot). A reference from a record of the store is reloaded, so it never names X. BTrees and the code of a Plone 6.1 instance keep no reference to a bucket across a commit.
- A vote can write a record of an oid that its transaction did not store, and can resolve a stored oid without a record. Invariants of `crates/zoe-core/CLAUDE.md` change: "The rules write only records that the transaction stored"; "a vote takes its locks in ascending oid order" (the lock of Y comes after, and never waits); "a read of the new bucket of the other side without a lock" (the only read of Y takes its lock); "the holder hands off each exclusive lock" (not the lock of an oid without a record).
- The wire protocol gets the extension `server_writes`. A stock `ZEO.ClientStorage` gets for its transactions, when the other side split the same bucket, the conflict of ZODB's resolver (section 11), and it gets Y in the invalidation of a commit of another client, as any record.
- A stock `ZEO.StorageServer` over `zoe.Storage` gets no flag (question 9), so the same holds for its clients.
- A store without the switch, or a commit with no conflict, takes the same path as today.

## Alternatives

- **The invalidation of the commit to the stock client that committed.** The server sends the client of S2 `invalidateTransaction(tid, oids)` with every oid of the commit, before the reply of `tpc_finish`. ZEO 6.2 applies it: the cache ends the entries, `ClientStorage.invalidateTransaction` calls `invalidate` of the database for every Connection, and the cache moves its last tid; then `tpc_finish_co` calls `setLastTid` with the same tid, which the cache accepts ("Be a little forgiving", `ZEO/cache.py:464`). So stock clients would merge too. It departs from the order of ZEO (the connection that committed gets an invalidation of its own commit), the committing Connection then drops every object that it wrote, and it rests on the behaviour of each stock version of the client, which no protocol guarantees. Refused for this card: a later card can test it against each supported version of ZEO 6 if B5 on a stock client shows the case.
- **The extension of the reply that the card names.** A new form of the items of the reply of `vote` that says "a record that the server wrote". A bare oid that the transaction did not store already says it, to the stock client and to the native client. What is missing is a promise of the client, which a reply cannot carry, so the declaration is in `register`.
- **The write for every client.** The server writes Y for a stock client too. Its other Connections keep a stale Y (section 4). Refused.
- **Two calls of `invalidate`** (Y alone, then `f(tid)` with the stored oids). It tears the snapshot (section 4). Refused.
- **The fall-back to the rule of ADR 0048 when the call for the lock of Y fails.** It reads Y without its lock while another transaction holds it, an undo for example, and loses that write. Refused.
- **The rule of ADR 0048 with the lock of Y, for a transaction without the flag.** It closes the lost undo that runs with the merge, but it still takes Y out of the tree, so an undo after the merge still passes with no effect. The human chose no answer (question 10).
- **Rule 1 merges T into the bucket that replaced Y.** At the vote of T, when its path child Y was replaced by X, the vote merges the write of T to Y into X. It writes a record that T did not store too, with the same lock and the same declaration, and it must follow a chain of replacements (4 of 20 cases in the spike). ADR 0048's bucket of garbage stays, and so does the conflict of any other writer of Y. Section 1 removes the cause, once, in the vote of S2.
- **Rule 3 gives no answer when Y is younger than some bound.** S2 then fails instead of T: the conflict moves, it does not go.
- **Rule 3 keeps Y when the keys of the transaction all fall below the separator of Y.** Then A takes them, Y does not change, and nothing is written outside the transaction. In the spike, 1 of 59 merges had that shape: in a tree whose keys grow, the new keys of both sides are above the separator of the split, in Y.
- **Bigger buckets, keys that do not grow, early locks, a queue for CMFEditions**: refused in ADR 0048 for the cases of rule 3; they do not change here.

## Questions for the human

Each question gives Claude's recommendation first.

1. **The mechanism.** Recommended: rule 3 keeps the bucket of the committed side (section 1), with the lock of section 3 and the extension of section 4. Other choices: rule 1 merges T into the bucket that replaced Y; or accept the case, as ADR 0048 did (question 6 of ADR 0048).
2. **The wire change.** Recommended: a new extension `server_writes` of `register`, and no change of the reply of `vote` or of `invalidateTransaction`. Other choice: a new item form in the reply of `vote`, as the card says, which a stock client would refuse and which still needs the promise of the client.
3. **A stock `ZEO.ClientStorage`.** Recommended: its transactions get no merge of section 1, so the case stays a conflict when the transaction that merges comes from a stock client (with the decision of question 10, rule 3 gives no answer for it when the other side split the same bucket); it gets Y in the invalidations of the commits of the other clients. Other choice: the invalidation of the commit to the stock client that committed (section "Alternatives"), in this card or a later one.
4. **The lock of Y.** Recommended: a call that never waits, after the plan; when it fails, rule 3 gives no answer and S2 fails with `ConflictError`. Other choices: the fall-back to the rule of ADR 0048, which reads Y while another transaction holds it (section 11); a wait for the lock of Y within the lock timeout (out of oid order: two votes can then wait for each other until the timeout); or no lock, which the hand-off of the node and an undo break (sections 3 and 11).
5. **The node and X.** Recommended: no record of either; they leave the staging, keep their exclusive locks without a hand-off until the commit is durable, and the reply lists both as resolved (section 2). Other choice: the node with the bytes of its committed record and X as the transaction stored it. That keeps "a vote writes a record of each oid that its transaction stored", but each merge then leaves X as garbage until a pack, and each transaction that read the node after S1 meets a stale read-current node that rule 1 must pass.
6. **A plain insert into a bucket that the other side split** ("a path child that the other side split", question 6 of ADR 0048, 2 `ConflictError`s in the spike). The machinery of this ADR (a write of a bucket that the transaction did not store, its lock, the declaration) can merge it too: the insert of T into A goes to A or to the new bucket Y of the other side. Recommended: a later card, after step 4 shows whether the gate needs it. Other choice: in this card, as a rule 4.
7. **The scope fence.** The call that never waits and the lock without a hand-off are in `crates/zoe-core/src/lock/`, outside the fence of the card, and so is `crates/zoe-py/src/` for the oids of the finish. Recommended: widen the fence to these two paths. Other choice: a card of its own for the lock calls, before the build.
8. **A new ADR or an amendment.** Recommended: this new ADR, because it changes the wire protocol and the order of the locks, which ADR 0048 excludes, and it has questions of its own; section 11 amends ADR 0048, and at the acceptance the status line and sections 2, 3 and 7 and questions 5 and 6 of ADR 0048 point to this ADR. Other choice: an amendment of ADR 0048 only, as for ZOE-225.
9. **`zoe.Storage` under another database.** Recommended: `zoe.Storage` sets the flag only when its database is a ZODB `MVCCAdapter`; a stock `ZEO.StorageServer` over `zoe.Storage`, or any other wrapper, keeps the rule 3 of ADR 0048. Other choice: declare a stock `ZEO.StorageServer` over `zoe.Storage` unsupported for the split merge, and set the flag always.
10. **The rule 3 of ADR 0048 for the transactions without the flag** (section 11). Recommended: take the lock of Y with the call that never waits before the read of Y, and give no answer when it fails; and correct section 7 of ADR 0048. Other choices: keep the read without a lock and write the lost undo into ADR 0048 as a known limit; or no answer when the other side split the same bucket, so that no bucket ever leaves the tree. **Decision of the human: no answer for a transaction without the flag** (section 11).
