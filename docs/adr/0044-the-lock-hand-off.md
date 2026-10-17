# 0044. The lock hand-off: a merge at the vote takes the lock of a commit that is in a commit batch

Status: accepted. A checkpoint of ZOE-204 (step 2): it changes the invariant of group commit in section 5 of the specification. The human approved it as written, with the answers of its draft to the questions below.

## Context

F7 and N5 ask that the writers of a hot object stop going one by one. B5 shows why they do (item 2 of the ranked table of `docs/findings/plone-end-to-end.md`): each Plone save writes the same `BTrees.Length` counters and the same buckets of the catalog, so the saves conflict on them, and `zoe_fsync_batch_size` stays at 1.

How a commit works today (`crates/zoe-core/src/lock/mod.rs`, `crates/zoe-core/src/commit/`):

- The vote takes an exclusive lock on each written oid and a shared lock on each read-current oid, in ascending oid order. A request waits behind each earlier request on its oid. The waits of one vote last at most `lock-timeout`.
- After it holds every lock, the vote compares each read serial with the current serial. Tier 1 (ADR 0042, ZOE-203) merges a stale oid of a class that it knows; a refusal fails the vote with `ConflictError`; no answer lists the conflict for the client.
- `tpc_finish` gives the transaction to group commit (`commit/group.rs`). The finishes that wait form the next batch, in arrival order. The leader of a batch gives each member its tid in batch order, writes every member in one backend write transaction under the commit sequencer, syncs once, and wakes the members. A failed write or sync halts the store: each member of the batch and each later finish fail with `Halted`.
- A transaction releases its locks after its finish returns, so after its commit is durable.

So a writer B that conflicts with a writer A on a hot `Length` waits for the lock until A is durable, then merges against A's record and joins a later batch. Each commit on the hot oid waits for one whole sync of the commit before it: the commits on that oid go one per batch.

Two facts make an earlier merge safe:

- Once A's finish has started, A cannot abort. Its staged records are final: they are the records that its batch writes. A's commit then becomes durable, or the store halts.
- Group commit writes in arrival order, in one backend write transaction per batch, and batches never overlap. A finish that starts after A's finish is in A's batch after A, or in a later batch. So it is never durable before A.

## Decision

### 1. When the hand-off applies

A vote B waits for the exclusive lock of an oid X that a transaction A holds. When A's finish starts (A gives its member to group commit), the lock manager hands the lock of X to the request at the head of the queue of X, when that request is the exclusive request of a vote. The hand-off gives B the staged record of X of A as the committed state of X. B then goes on with its other locks, in oid order.

A shared request (read-current) at the head of the queue does not take a hand-off: it waits until A releases, as today, because its check needs the committed serial. A restore is never a holder of a hand-off: its finish can fail alone (a tid that fails its check), so its records are not final when the finish starts.

### 2. What the vote does with a handed-off oid

At the check of the serials, the committed record of X is the staged record of A, not the current record of the store.

- Tier 1 merges: B stages the merged record over the commit of A. The reply of the vote gives X as resolved (the bare oid of ZEO 6), as for any merge of tier 1.
- Tier 1 refuses: the vote fails with `ConflictError` at once, with the reason of BTrees, as for any refusal.
- Tier 1 gives no answer, or does not run (class `off`, or `shadow`): B keeps the lock of X and waits until the commit of A is durable, then goes on as today, with the tid of A as the committed serial. The client can then load the committed record to resolve the conflict. This wait counts in `lock-timeout`.

A second vote of B (after the client resolved other conflicts) does not check X again: B holds X, and nothing committed X between.

### 3. Order and durability

B's finish starts after its vote, so after A's finish: B is in A's batch after A, or in a later batch. Its tid is after the tid of A. Its client gets the acknowledgement when B's batch is durable, as today. A failed write or sync of A's batch halts the store, so B fails with `Halted` too. So:

- An acknowledged commit is durable (N1).
- A commit is never durable before the commit that it merged onto.
- Readers see A after A's batch publishes, and B after B's batch publishes: no reader sees a state that is not durable. Invalidations go out after the sync of their batch, as today.

The commits of a hot oid can now share a batch: A, then B merged onto A, then C merged onto B, each one taking the lock when the finish before it starts. Their finishes wait in the same queue, so the next leader writes them together, with one sync.

### 4. The lock manager

- A finish that starts marks the exclusive locks of its transaction as committing, and hands each one that has an exclusive vote at the head of its queue to that vote, under the mutex of the lock table, as a release grants a lock today.
- The holder makes the staged record of each handed-off oid available to the new owner. The new owner reads a copy, never the spill file of the holder, which goes away when the transaction of the holder ends.
- The release after the finish releases only the locks that the transaction still owns. A handed-off lock belongs to its new owner.
- Locks stay in oid order: a hand-off only grants a lock sooner than a release would, so it adds no cycle. A wait still ends at `lock-timeout` or at a `LockCancel`.

### 5. Both modes, and the clients

The hand-off is in `zoe-core`, so `zoe.Storage` and `zoe serve` both have it. It needs no change of the wire protocol: a stock `ZEO.ClientStorage` sees a vote that resolved an oid, as from a ZEO 6 server that resolved a conflict. A commit that writes no hot object takes the same path as today: no waiter, no hand-off.

### 6. Measurement

A counter `zoe_lock_handoffs_total` counts the hand-offs, and `zoe_fsync_batch_size` shows the commits per sync. The test is the acceptance test of the card: 64 writers on one `Length` through `zoe serve`, with a mean `zoe_fsync_batch_size` above 1. The kill loop under the simulator, with hot `Length` writers and the hand-off on, gives 0 losses and no stale read.

## Consequences

- Section 5 of the specification changes: "With per-OID locks already held, batching changes nothing about isolation" becomes: a lock passes from a commit whose finish started to a vote that merges onto it, and the merge commits after it.
- The vote has a new wait: for the durability of a holder, when the conflict on a handed-off oid goes to the client. It is the wait of today, moved after the lock.
- A long chain of merges on one oid fills one batch. The batch grows with the number of writers, and its write takes longer.
- No on-disk format change, no wire change, no new key: the hand-off follows from tier 1 being on.

## Alternatives

- **The hand-off at the vote of the holder.** A has voted but can still abort: a merge onto its state would hold a change that never commits. Refused.
- **The hand-off when the batch of the holder is written** (its tid known). Then B goes to the next batch, and each batch holds one commit of the hot oid: the commits of the hot oid stay one per sync. Refused.
- **Merge in the batch, by the leader.** The leader would run tier 1 on the members that conflict. It takes Rust merges into the write path under the sequencer, and a refusal would fail a member after its vote passed. Refused.
- **No lock at all for the classes of tier 1.** The commits would merge at the finish. Same objection.

## The answers of the human

1. The hand-off starts when the finish of the holder starts, so the commits on a hot oid can share one batch.
2. A conflict on a handed-off oid that goes to the client waits for the durability of the holder, in `lock-timeout`.
3. No limit on the commits of one batch: group commit takes every finish that waits, as before.
4. The counter is `zoe_lock_handoffs_total`.

## Notes of the build

- The holder hands off only an oid whose staged record is of a class that tier 1 merges in mode `on`. For another oid, the new owner could only wait for the durability of the holder, which is what the queue gives without a hand-off, so the holder keeps the lock and copies nothing.
- The hand-off comes after the member of the holder is in the queue of group commit, so a finish that starts after the hand-off is later in the commit order.
- A new owner that releases the lock before the holder is durable (a failed vote, an abort) leaves the hand-off in the entry: the next exclusive request takes the lock with it, and a shared request waits until the holder is durable. A new owner whose own commit is durable ends the hand-off that it got before it releases the lock, because the store then holds the last commit of the oid, also when the finish of the holder did not end the hand-off yet. So no vote reads the oid from the store while the store does not hold its last commit, and no vote merges onto a record that a later commit replaced.
- A finish that waits for the end of a commit batch waits on a waiter of the store clock, as a lock wait does, so the simulator runs other threads during the sync of a batch.
- The merged record of the new owner gets the tid of the holder as its previous serial when its batch writes it. A member whose holder got no tid fails with `Protocol`; the holder then failed alone only when no tid is left, and the member gets no tid either.
- A refusal at once gives no `serials` when the holder has no tid yet, as a lock timeout does. A line of the resolution log of a merge onto a commit in flight gives the user and the description of the holder, and its tid, or 0 when its batch did not give it one yet.
