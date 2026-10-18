# 0053. The lock of an edit: an edit request takes the intent lock of its object after the traversal and reads from the grant

Status: proposed. The checkpoint of ZOE-212 after the rejection of ADR 0051. It keeps the edit and its version in one transaction, as stock Plone does, and uses the early write-intent locks of ADR 0045. It adds a public Python API to `zoe.zodb` (outside the scope fence of the card), changes section 2 of ADR 0045 (`early-locks on` with no hot class takes the locks that a caller asks for), and adds an opt-in subscriber to `zoe.plone`. It changes no on-disk format, no wire protocol, no code of the core and no code of CMFEditions, ZopeVersionControl or plone.app.versioningbehavior. The acceptance test of the planner cannot pass unchanged (section 6).

## Context

Two saves of one versioned Document from one snapshot both take the next version number and both write the history; the second fails with `ConflictError`, and Zope's retry gives the history of the two saves one after the other (`docs/spikes/concurrent-versioned-saves.md`). No merge at the vote can give that history, and ADR 0051, which moved the version into a transaction of its own, is rejected.

The spike `docs/spikes/versioned-saves-early-locks.md` tried early locks on the history instead:

- **The history classes hot do not work.** Of the 11 records of the history that a save reads, 6 are BTrees of tier 1, which ADR 0045 refuses as hot classes, and the other 5 are not leaf records. With `ShadowHistory` and `ZopeVersionHistory` hot, the second save reads them at the grant and their BTrees at its snapshot, and fails with `KeyError: 1`, which Zope does not retry.
- **A covered subtree is not enough.** When the records that a hot record reaches also load at the grant, two and eight saves commit at the first attempt with the right numbers, but each version misses the committed changes of the saves before it: the Document loads at the traversal, before any record of the history, and the version copies the Document of its snapshot.
- **The lock of the edited object works.** When each save takes the intent lock of its Document at the end of the traversal and then moves the snapshot of its connection to the grant, two and eight concurrent saves commit at the first attempt, and the state equals the sequential reference in commit order, with and without a merge policy.

## Decision

### 1. `zoe.zodb.lock_for_edit(obj)`

A new function of `zoe.zodb`, for a persistent object `obj` that its connection loaded:

1. When the storage of the connection takes no early locks (section 3), or the connection is historical, it does nothing and returns `False`.
2. It takes the intent lock of the oid of `obj` with the owner of the connection (ADR 0045, section 3), with the rules of order and of `LockOrder` of ADR 0045, section 4, and within `lock-timeout`. A timeout raises `ConflictError`, as an intent lock does.
3. When the transaction of the connection holds no change and no savepoint, it moves the snapshot of the connection to the grant: it waits until the storage of the connection holds the commit of the serial that the grant gave (`lastTransaction()` at or past it, within `lock-timeout`), then runs the step of ZODB that begins a transaction: sync, poll the invalidations, move the start of the snapshot past the last commit, and turn the changed objects of the cache into ghosts. It returns `True`.
4. When the transaction holds a change, it keeps the snapshot and returns `False`: the lock still orders the commits, and the vote checks every serial as today.

The lock lasts to the end of the transaction, as every intent lock does. The vote adopts it.

### 2. The subscriber of `zoe.plone`

`zoe.plone` registers a subscriber of `ZPublisher.interfaces.IPubAfterTraversal`. For a request with the method `POST`, `PUT` or `PATCH` whose context (the first of `PARENTS`) has the behaviour `plone.versioning` (`IVersioningSupport`), it calls `lock_for_edit(context)`. It does nothing for other requests and other objects. Readers never take the lock, and edits of different objects never wait for each other.

### 3. Opt-in

- The operator switches it on with the key `early-locks on` of `<zoe>` or of `<zoeclient>` (with `native true`), or its twin `ZOE_EARLY_LOCKS`. The key is read when the storage opens; the database does not hold it, so the profile of `zoe.plone` cannot set it.
- ADR 0045, section 2, says that `early-locks on` with an empty `early-lock-classes` takes no intent lock. This ADR changes that: with `early-locks on` and no hot class, the patch of `zoe.zodb` is installed and the connection binds its owner at `tpc_begin`, but no load takes a lock; only `lock_for_edit` does. A hot class still takes its lock at its load.
- Without `early-locks on` the subscriber does nothing, and the site works as today.

### 4. Why no update is lost

- The locks are advisory (ADR 0045, section 3). Every vote still checks the serial of every record that it writes, so no transaction commits a write that it computed from a state older than the committed one. At worst a save gets the `ConflictError` of today, and Zope retries it.
- The snapshot moves only when the transaction holds no change, so no change made at the old snapshot survives the move. What the traversal read before the move (the security checks of the traversal, for example) is a read at the old snapshot, not a write.
- The edit and its version commit in one transaction, as in stock Plone: a crash keeps both or neither.
- The second edit waits for the commit of the first, then reads the Document and its history after that commit. So its version holds both changes and takes the next number: the history of the two saves one after the other (spike, section 6).
- Two edits of one field give what they give one after the other: the later value wins, without `ConflictError`. A retry of stock Plone gives the same. An edit form submits every field, and z3c.form writes a field whose submitted value differs from the current one; so a form that was loaded before the first commit writes back the old value of a field that the first edit changed. A retry of stock Plone does the same, on the same state.

### 5. What a stock client gets

- A stock `ZEO.ClientStorage`, a `zoe.zeo.ClientStorage` without `early-locks`, or a site without the switch: no lock. The save conflicts as today, and the retry of Zope gives the sequential history.
- `zoe.zeo.ClientStorage` with `native false` refuses `early-locks on` with `ValueError` (ADR 0045, section 8).
- On a server where other clients hold intent locks, the vote of a stock client waits for them as for vote locks, and its serial check refuses a stale write.

### 6. Tests

- **The acceptance test of the planner cannot pass unchanged** with any early lock (spike, finding 1): `open_db` opens `zoe.Storage` with no option, and no environment twin reaches the constructor; the barrier between the save and the commit deadlocks with any lock that the save holds; the test has no request and no traversal; and its two guards expect `ConflictError` where the lock gives the sequential result. A new acceptance test from the planner, approved by the human (question 2), would: open the copy with `early_locks=True`; start each save from one snapshot and let it commit as soon as it is done; run the traversal with the subscriber (`IPubAfterTraversal` with a request of method `PATCH`); check, for two and for eight saves, that each commits at the first attempt and that the state equals the sequential reference in commit order; and check that without `early-locks` the second save raises `ConflictError`.
- **Unit tests of `lock_for_edit`** in `python/zoedb/tests`: the move after a commit of another connection; no move when the transaction holds a change or a savepoint; the lock released at commit, at abort and at the close; a historical connection; a storage without `early-locks`; a `LockOrder` with a hot class of a lower oid; the timeout.
- **Tests of the subscriber** in `zoe.plone`: the methods and the behaviour that select a request; a reader takes no lock; through the native client on `zoe serve`, the move sees the commit of the holder.

### 7. The B5 plan

One `contention-fields` run on `zoe-native-qi` with `early-locks on` in the `<environment>` of the instances, and one without, each inside `systemd-inhibit` and the lock of the bench, with a port offset of its own. Report: the `ConflictError`s at the first attempt and after the retries, the p50 and p95 of the `PATCH`, `zoe_intent_lock_wait_seconds`, the lock timeouts, and in the verification that the last version of each Document holds both fields.

## Consequences

- Concurrent edits of one versioned object commit at the first attempt, one after the other, with the history of the sequential saves, when the operator switches early locks on.
- An edit request of an object waits for the edit of that object before it, up to `lock-timeout`, and holds a thread of Zope while it waits; in native mode it also holds a connection of the pool of the client (ADR 0045, section 3).
- The move reloads the objects that the commits before it changed: 16 records for each versioned save.
- Two edits of one field no longer conflict: the later one wins, as after a retry.
- The merge of tier 2 plays no part in these edits. It still serves the transactions that take no lock.

## Alternatives

- **The history classes hot** (the question of the spike). `KeyError: 1` at the first attempt; ADR 0045 refuses the BTrees, and the other classes are not leaf records (spike, section 3).
- **The history classes hot with a covered subtree.** First attempt, right numbers, but each version misses the changes of the saves before it, and the site takes the last version as up to date (spike, section 4).
- **The Document class hot, with a covered subtree.** The sequential history at the first attempt, but every load of every Document takes an exclusive lock, so readers wait for writers and for each other (spike, section 5).
- **The version of an edit in a transaction of its own** (ADR 0051). Rejected.
- **Keep the conflict.** Zope retries, and the history is the sequential one. The B5 criterion of the card goes back to the human.

## Questions for the human

Each question gives the recommendation first.

1. **The lock of an edit** (sections 1 to 3) instead of the history classes hot. Recommended: yes. The history classes hot raise `KeyError`, and a covered subtree gives versions that miss committed changes. Other choice: keep the conflict.
2. **A new acceptance test** (section 6): the one of the planner cannot run with early locks unchanged. Recommended: yes, the planner writes the one of section 6, with saves that commit when they are done, the subscriber at the traversal, two and eight saves, and the conflict without `early-locks`.
3. **The public API and the scope fence** (section 1): `zoe.zodb.lock_for_edit` is a new public Python API, and `python/zoedb/src/zoe/zodb.py` is outside the scope fence of the card. Recommended: yes, add the function and widen the fence to `zoe/zodb.py`, `zoe/zeo.py` (the wait for the invalidations in native mode) and their tests.
4. **`early-locks on` with no hot class** (section 3), a change of section 2 of ADR 0045. Recommended: yes; this ADR amends ADR 0045. Other choice: a key of its own, such as `edit-locks on`.
5. **Which requests take the lock** (section 2): `POST`, `PUT` and `PATCH` on an object with the versioning behaviour. Recommended: yes; tier 2 merges the other edits. Other choice: every edit of content, which also orders edits that tier 2 merges today.
6. **Two edits of one field** (section 4): the later one wins without `ConflictError`, as after a retry of stock Plone; the guard of the planner that expects the conflict does not hold under the lock. Recommended: accept it; the card's invariant "strict by default" still holds for every merge, and under the lock no merge happens. Other choice: none that keeps the first attempt.
7. **The lock timeout** (section 1): the 10 s of `lock-timeout`, then `ConflictError` and Zope's retry. Recommended: keep it; B5 reports the waits and the timeouts.
8. **The budget** of the card (two sessions) is spent by the two spikes. Recommended: one more budget of two sessions for the build, the new acceptance test and the B5 runs, in this card.
