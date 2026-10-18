# 0049. The split merge keeps the bucket of the committed side: rule 3 writes into it, and a declared client invalidates it

Status: proposed. The checkpoint of ZOE-224 (step 2): it changes the wire protocol (a new extension of `register`), the lock manager (a lock that the vote takes after its others, without a wait) and rule 3 of the split merge (ADR 0048). It changes no on-disk format and no pack.

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

When the committed side split A once into A and Y, and the vote may write Y (section 3), rule 3 merges as follows:

- The keys of the range of A merge with the rules of `bucket_merge`, as in ADR 0048: the base is the old A, the committed side is A followed by Y, the new side is the new A followed by X.
- The merged keys below the separator of Y go to A, the others to Y. The next bucket of A stays Y, and the next bucket of Y stays the next bucket of the committed Y.
- The merged node is the committed node: the merge adds no child, removes no child and changes no separator.
- X gets no place in the tree.
- A or Y must not be empty, as in ADR 0048.

For the bucket that a root holds inline, the committed side split it into two new buckets Y1 and Y2 (both halves of a split of an inline bucket are new objects). The merged keys go to Y1 and Y2 at the separator of the committed side, and the merged root is the committed root. The two new buckets of the transaction get no place in the tree.

When the committed side changed A without a split, rule 3 is the rule of ADR 0048: X is a new child with the separator of the transaction, and no bucket leaves the tree. When the vote may not write Y (section 3), rule 3 is the rule of ADR 0048 too: X takes the place of Y.

### 2. What the vote writes

- **A** (or the inline root): the merged record, over the serial of its committed record, as today.
- **Y** (or Y1 and Y2): the merged record, over the serial of its committed record. The transaction did not store Y. It is the first record that a vote writes for an oid that its transaction did not store.
- **The node**: no record when its merged record is its committed record, which is the case unless rule 2 merged another split of the node in the same vote. *(Question 5.)*
- **X** (or the two new buckets of the transaction): no record. X is a new object that no record of the store refers to. *(Question 5.)*

The reply of the vote lists the node, A, X and Y as resolved, with the bare oid of ZEO 6, as for any merge of tier 1. The reply keeps its form: a bare oid that the transaction did not store already means a record that the server wrote, as the oids of an undo (ZEO's `TransactionBuffer` yields such an oid with no data, and the native client ends its current entry, section "Server mode" of the specification). The card expected an extension of the reply. It is not needed.

The commit holds the records of A and Y. So the transaction index lists Y, and every other client gets Y in `invalidateTransaction(tid, oids)`, a stock client too, as for any record of the commit. The client that committed gets no invalidation, as from ZEO, and section 4 says how it learns of Y.

### 3. The lock on Y

The vote takes the exclusive lock of Y after its plan, with a new call of the lock manager that never waits: the call grants the lock when no transaction holds it, no request waits for it and the entry carries no hand-off (ADR 0044); else it fails at once. On a failure the vote does not write Y: rule 3 is the rule of ADR 0048, which writes only records that the transaction stored. For a root that held its bucket inline, the vote takes the locks of Y1 and Y2, or neither.

The plan finds Y in the committed node, which the vote holds exclusively, and takes the lock of Y before rule 3 reads Y. So the rule reads Y under its lock.

Why a lock that does not wait is enough:

- Each writer of Y holds the lock of the node, shared or exclusive, until its commit is durable (ADR 0048, section 3: BTrees writes the node or marks it read-current). The vote of rule 3 holds the node exclusively, since its transaction stored the node. So no commit of Y is in flight, and no other vote can get past the node to a commit of Y.
- In the trees of the spike, Y is newer than its node: a split creates Y under a node that exists, so the oid of Y is above the oid of the node, and every vote takes the lock of the node before the lock of Y. Then no other vote holds the lock of Y or waits for it, and the call finds the lock free.
- When a split of a node moved Y under a newer node, the oid of Y is below the oid of its node, and a writer of Y can hold the lock of Y while it waits for the node. A program that writes Y without the node (ADR 0048, "What the argument assumes") can hold it too. Then the call fails, and rule 3 falls back to the rule of ADR 0048, which is correct without the lock of Y.
- The vote never waits out of oid order, so the order of the locks still excludes a deadlock, and the call never passes a request that waits.
- A lock that is free and carries no hand-off means that every commit of Y is durable (ADR 0044, "Notes of the build"). So the rule reads the last commit of Y, the inserts of the transactions that wrote into Y after S1 included.

Why the lock is needed: when the finish of S2 starts, its exclusive locks pass to the votes that wait for them (ADR 0044). The lock of the node can pass to a vote T' that stored the node. Without a lock on Y, T' would then take the lock of Y freely, read Y from the store before the commit of S2 is durable, find its serial current, and write Y over the record of S1: the merge of S2 would be lost. With the lock, T' waits for Y, and at the finish of S2 the lock of Y passes to T' with the staged record of S2, onto which tier 1 merges, or T' waits until S2 is durable. Y is a bucket, a class of tier 1, so the hand-off of ADR 0044 applies to it.

### 4. Which client may get a record that it did not store: the extension `server_writes`

A ZODB process with a stock `ZEO.ClientStorage` cannot take a record that its transaction did not store:

- `ClientStorage.tpc_vote` puts each bare oid of the reply into `server_resolved`. `TransactionBuffer` yields Y with no data, so `tpc_finish` ends the current entry of Y in the cache of the client (`ZEO/asyncio/client.py`, `tpc_finish_co`). This part is correct.
- `Connection.tpc_vote` turns each oid of the reply into a ghost in the Connection that committed (`ZODB/Connection.py:690-695`). This part is correct.
- The other Connections of the process get only the oids that the committing Connection stored: `MVCCAdapterInstance.tpc_finish` gives `_modified` to `_invalidate_finish`, and `_modified` holds only the oids of `store` and `storeBlob` (`ZODB/mvccadapter.py:101-123`). The server sends no invalidation to the client that committed. So another Connection that holds Y in its object cache keeps the state of Y before the commit, after it moved past the commit: a lookup of a key that S2 merged into Y misses. That is a stale read.

So the server writes Y only for a client that says it invalidates Y in all its Connections. The client says so with a new extension of `register` (section "Extension negotiation" of the specification):

- **`server_writes`**: the connection declares that, when the reply of its `vote` lists an oid that its transaction did not store, the client ends the current entry of that oid in its cache, and gives the oid to every Connection of its process as an invalidation of the tid of the commit, before `lastTransaction()` gives that tid. `get_info` lists the name when the server accepted it.
- `zoe.zeo.ClientStorage` declares it on each connection of its pool. At `tpc_finish`, it calls `invalidate(tid, oids)` of the database with the oids of the reply that the transaction did not store, right before the callback `f(tid)`, while its dispatcher holds the tid. So the other Connections get Y with the other oids of the commit, before `lastTransaction()` moves, as they get the invalidations of the other clients.
- `zoe.Storage` (in-process) does the same in its `tpc_finish`, before the callback. It has no wire; the skin tells the vote that its client takes such records.
- A connection that does not declare `server_writes` (a stock `ZEO.ClientStorage`, and any other client) never gets a record that it did not store: rule 3 is the rule of ADR 0048 for its transactions.

The vote gets one flag from its skin: whether its client takes records that it did not store. A vote of `zoe serve` gets it from the declaration of the connection, and a vote of `zoe.Storage` always has it.

### 5. The switch

No new switch. The rule of section 1 runs where the split merge runs (`split_merges` `on` or `shadow` in the merge policy, ADR 0048) and the client declared `server_writes`. In mode `shadow` the vote writes nothing, as today, and the field `shadow` of the lines gives the outcome of the rule of section 1 for such a client.

### 6. Why no update is lost

- **The merge.** The merged keys of the range of A are the merge of `bucket_merge` of its three states, as in ADR 0048. They go to A and Y, the children of the committed node that hold the range: the node routes each key below the separator of Y to A and each other key to Y. Each key outside the range keeps its bucket. X held only keys of the range. So the tree after the commit is the committed tree with the changes of the transaction.
- **No bucket leaves the tree.** The committed node keeps each of its children. So each write of another transaction under a child of the node stays where a lookup finds it.
- **The read of Y.** The vote reads Y under the exclusive lock of Y, which it takes only when every commit of Y is durable (section 3). So it reads the last commit of Y, the inserts of the transactions that wrote into Y after S1 included.
- **A later writer of Y.** Each later vote that stored Y waits for the lock of Y, until S2 is durable or until the hand-off of S2. Its serial of Y is then stale, so tier 1 merges its insert with the record of S2, refuses with the reason of BTrees, or gives the conflict to the client. A write never goes over the record of S2 unseen.
- **T.** T read the node after S1 and stored Y. The node has no new record, so its read-current check passes; when rule 2 gave the node a new record in the same vote, the node holds every child that T read, and rule 1 passes it. Its write to Y is stale against the record of S2: tier 1 merges the inserts of different keys (for the trees of `intids`, whose keys are key references that the plain tier 1 does not compare, tier 1 gives no answer and the client resolves with `_p_resolveConflict`, as today for a plain conflict on a bucket of `ids`). So T commits with its key in Y, which the tree holds.
- **The clients.** The client of S2 drops Y in all its Connections and in its cache before it publishes the tid (section 4). Every other client gets Y in the invalidation of the commit. No Connection that moved past the commit keeps the state of Y before it.

### 7. A copy, a pack and an undo

- **A copy** through the iterator (`zodbconvert`, `copyTransactionsFrom`, `zoe convert`), `zoe backup` and `zoe restore` copies the records as they are: the commit of S2 holds A and Y, and no record of the node or of X. A restore transaction runs no rule.
- **A pack** needs no change. A merge of rule 3 now leaves no bucket out of the tree, so it leaves no garbage: ADR 0048 left one bucket per merge, two for the bucket of an inline root (148,719 bytes in the three runs of the spike). X has no record. The oid of X stays unused, as an oid that `new_oid` gave to an object that never committed.
- **An undo** runs no rule, as in ADR 0048. An undo of S2 gives back the records of A and Y before S2: the keys of S2 go away and the keys of S1 stay. ADR 0048 gave back the old node, the old A and no X, and put Y back into the tree. When a later commit wrote A or Y (T, for example), the undo meets the conflict of any undo of a record that changed later: tier 1 or the client resolves it, or it fails with `UndoError`, as in ZODB.

### 8. What the build changes, and where

- `crates/zoe-core/src/merge/split.rs`: rule 3 of section 1, the inline root included, and the records it gives (A and Y written, the node and X resolved without a record).
- `crates/zoe-core/src/commit/`: the flag of the client, the lock of Y after the plan with the fall-back to ADR 0048, a resolved oid without a record, a written oid that the transaction did not store, and their lines in the resolution log.
- `crates/zoe-core/src/lock/`: the call that never waits. This is outside the scope fence of the card (question 7). It is not a red zone.
- `crates/zoe-core/CLAUDE.md`: the invariants that change (section "Consequences").
- `crates/zoe-server/src/`: the extension `server_writes` in the negotiation, and the flag of the vote.
- `crates/zoe-client/src/` and `crates/zoe-py/src/`: the declaration, and the oids of the reply that the transaction did not store, for the finish.
- `python/zoedb/src/zoe/zeo.py` and the finish of `zoe.Storage`: `invalidate(tid, oids)` before the callback.
- `docs/spec/technical-specification.md`: "Extension negotiation" (the name), "Invalidations" and the client sections (what a declared client does), section 5 (rule 3).
- No change of `format/`, of `pack/`, of the reply of `vote`, or of `invalidateTransaction`.

### 9. The resolution log and the counters

- A gives its line of rule 3, as today. Y gives a line of tier 1 with the outcome `merged` and `split` 3; its `read` and `committed` are the serial of the record of Y that the merge read. The node and X give a line with the outcome `merged`, `split` 3, no keys and `changed` 0, so `Txn::natively_resolved` lists them and the reply gives them.
- `zoe_structural_merges_total` counts these lines as today. No new counter.
- A fall-back to the rule of ADR 0048 because the lock of Y was taken gives the lines of ADR 0048. The vote writes one `debug` event for it, with the oid of Y.

### 10. The tests and the measure of step 4

- `crates/zoe-core/tests/`: the three transactions of the card in their order, with the client flag: all commit, the tree holds every key and `BTrees.check` passes (through the model of `tests/split_merges.rs`); the chain of the spike (two merges onto Y before T); the inline root; the vote without the flag gives the rule of ADR 0048; the lock of Y held by another transaction gives the fall-back; a vote that waits for Y after S2 gets the hand-off and merges onto the staged record; the switch in mode `shadow`.
- `crates/zoe-core/src/lock/`: the call that never waits refuses a held lock, a lock with a waiter, and an entry with a hand-off.
- `python/zoedb/tests/`: the order of the card against `zoe.Storage` and against `zoe serve` with the native client, with a second Connection of the committing process that holds Y in its object cache before the commit of S2 and reads the key of S2 after it; and against `zoe serve` with a stock `ZEO.ClientStorage`, which declares nothing and keeps the behaviour of ADR 0048.
- The acceptance test of the planner.
- Step 4 of the card: B5 `edit` and `create-spread` at 4 and 16 users, `zeo` against `zoe-native-qi` with the switch on.

## Consequences

- A transaction that inserted into a bucket that a split merge of another transaction had to replace commits. On the logs of the spike, that removes 18 of the 28 `ConflictError`s of `create-spread` on `zoe-native-qi`: 9.6 per 1,000 operations instead of 27.0.
- Rule 3 leaves no bucket out of the tree, and a commit of rule 3 holds two records instead of three (A and Y). The bucket of garbage of ADR 0048 goes.
- A vote can write a record of an oid that its transaction did not store, and can resolve an oid without a record. Invariants of `crates/zoe-core/CLAUDE.md` change: "The rules write only records that the transaction stored"; "a vote takes its locks in ascending oid order" (the lock of Y comes after, and never waits); "a read of the new bucket of the other side without a lock".
- The wire protocol gets the extension `server_writes`. A stock `ZEO.ClientStorage` sees no change: its transactions keep the rule 3 of ADR 0048, and it gets Y in the invalidation of a commit of another client, as any record.
- A store without the switch, or a commit with no conflict, takes the same path as today.

## Alternatives

- **The invalidation of the commit to the stock client that committed.** The server sends the client of S2 `invalidateTransaction(tid, oids)` with every oid of the commit, before the reply of `tpc_finish`. ZEO 6.2 applies it: the cache ends the entries, `ClientStorage.invalidateTransaction` calls `invalidate` of the database for every Connection, and the cache moves its last tid; then `tpc_finish_co` calls `setLastTid` with the same tid, which the cache accepts ("Be a little forgiving", `ZEO/cache.py`). So stock clients would merge too. It departs from the order of ZEO (the client that committed gets an invalidation of its own commit), the committing Connection then drops every object that it wrote, and it rests on the behaviour of each stock version of the client, which no protocol guarantees. Refused for this card: a later card can test it against each supported version of ZEO 6 if B5 on a stock client shows the case.
- **The extension of the reply that the card names.** A new form of the items of the reply of `vote` that says "a record that the server wrote". A bare oid that the transaction did not store already says it, to the stock client and to the native client. What is missing is a promise of the client, which a reply cannot carry, so the declaration is in `register`.
- **The write for every client.** The server writes Y for a stock client too. Its other Connections keep a stale Y (section 4). Refused.
- **Rule 1 merges T into the bucket that replaced Y.** At the vote of T, when its path child Y was replaced by X, the vote merges the write of T to Y into X. It writes a record that T did not store too, with the same lock and the same declaration, and it must follow a chain of replacements (4 of 20 cases in the spike). ADR 0048's bucket of garbage stays, and so does the conflict of any other writer of Y. Section 1 removes the cause, once, in the vote of S2.
- **Rule 3 gives no answer when Y is younger than some bound.** S2 then fails instead of T: the conflict moves, it does not go.
- **Rule 3 keeps Y when the keys of the transaction all fall below the separator of Y.** Then A takes them, Y does not change, and nothing is written outside the transaction. In the spike, 1 of 59 merges had that shape: in a tree whose keys grow, the new keys of both sides are above the separator of the split, in Y.
- **Bigger buckets, keys that do not grow, early locks, a queue for CMFEditions**: refused in ADR 0048 for the cases of rule 3; they do not change here.

## Questions for the human

Each question gives Claude's recommendation first.

1. **The mechanism.** Recommended: rule 3 keeps the bucket of the committed side (section 1), with the lock of section 3 and the extension of section 4. Other choices: rule 1 merges T into the bucket that replaced Y; or accept the case, as ADR 0048 did (question 6 of ADR 0048).
2. **The wire change.** Recommended: a new extension `server_writes` of `register`, and no change of the reply of `vote` or of `invalidateTransaction`. Other choice: a new item form in the reply of `vote`, as the card says, which a stock client would refuse and which still needs the promise of the client.
3. **A stock `ZEO.ClientStorage`.** Recommended: its transactions keep the rule 3 of ADR 0048, so the case stays a conflict when the transaction that merges comes from a stock client; it gets Y in the invalidations of the commits of the other clients. Other choice: the invalidation of the commit to the stock client that committed (section "Alternatives"), in this card or a later one.
4. **The lock of Y.** Recommended: a call that never waits, after the plan, with the fall-back to ADR 0048 when it fails. Other choices: a wait for the lock of Y within the lock timeout (out of oid order: two votes can then wait for each other until the timeout); or no lock, on the argument of the lock of the node alone, which the hand-off of ADR 0044 breaks (section 3).
5. **The node and X.** Recommended: no record of either; the reply lists both as resolved, so each client drops what it stored, the commit holds only A and Y, and a reader of the node after S1 sees no change. Other choice: the node with the bytes of its committed record and X as the transaction stored it. That keeps "a vote writes a record of each oid that its transaction stored", but each merge then leaves X as garbage until a pack, and each transaction that read the node after S1 meets a stale read-current node that rule 1 must pass.
6. **A plain insert into a bucket that the other side split** ("a path child that the other side split", question 6 of ADR 0048, 2 `ConflictError`s in the spike). The machinery of this ADR (a write of a bucket that the transaction did not store, its lock, the declaration) can merge it too: the insert of T into A goes to A or to the new bucket Y of the other side. Recommended: a later card, after step 4 shows whether the gate needs it. Other choice: in this card, as a rule 4.
7. **The scope fence.** The call that never waits is in `crates/zoe-core/src/lock/`, outside the fence of the card, and so is `crates/zoe-py/src/` for the oids of the finish. Recommended: widen the fence to these two paths. Other choice: a card of its own for the lock call, before the build.
8. **A new ADR or an amendment.** Recommended: this new ADR, because it changes the wire protocol and the order of the locks, which ADR 0048 excludes, and it has questions of its own; at its acceptance, the status line of ADR 0048 names it, and section 2 (rule 3) and questions 5 and 6 of ADR 0048 point to it. Other choice: an amendment of ADR 0048, as for ZOE-225.
