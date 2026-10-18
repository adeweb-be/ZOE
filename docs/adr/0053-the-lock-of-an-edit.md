# 0053. The lock of an edit: an edit of versioned content reads its object after the commit of the edit before it

Status: proposed, revised after an adversarial review (SOUND WITH CHANGES). The checkpoint of ZOE-212 after the rejection of ADR 0051. The edit and its version stay in one transaction, as in stock Plone, and the early write-intent locks of ADR 0045 order the edits of one object. Section 1 gives two variants side by side; the human chooses. Both add a key of the storage (`edit-locks`), a private helper of `zoe.zodb`, and an opt-in subscriber of `zoe.plone`. Neither adds a public Python API or changes the on-disk format, the wire protocol, the core, CMFEditions, ZopeVersionControl or plone.app.versioningbehavior. Both change `python/zoedb/src/zoe/zodb.py` and `zeo.py`, outside the scope fence of the card. The acceptance test of the planner cannot pass unchanged with either (section 5).

## Context

Two saves of one versioned Document from one snapshot both take the next version number and both write the history. The second fails with `ConflictError`, and Zope's retry gives the history of the two saves one after the other (`docs/spikes/concurrent-versioned-saves.md`). No merge at the vote can give that history. ADR 0051, which moved the version into a transaction of its own, is rejected.

The spike `docs/spikes/versioned-saves-early-locks.md` found:

- The history classes hot, as ADR 0045 builds them, raise `KeyError: 1`: 6 of the 11 history records of a save are tier 1 BTrees, which ADR 0045 refuses, and the other 5 are not leaf records.
- With a covered subtree, the saves commit at the first attempt, but each version misses the committed changes of the saves before it: the Document loads at the traversal, before any history record.
- The intent lock of the edited Document after the traversal, then a move of the snapshot of the connection to the grant, gives the sequential history at the first attempt for two and eight saves.

The review of this ADR found two blocking problems in that design and six more:

- **H1.** Not every `POST`, `PUT` or `PATCH` on versioned content saves a version. plone.restapi serves `@querystring-search` as a `POST` on any content, and the listing blocks of Volto use it, so anonymous views of a page with a listing would wait on the lock of its edits. `@comments`, `@lock`, the `PATCH` chunks of `@tus-upload`, the Cancel button and a page with a validation error would lock too.
- **H2.** The move comes after the checks of the traversal (`BaseRequest.traverse` checks existence and permissions before `IPubAfterTraversal`). An Editor's `PATCH` that waits while a transition takes Modify away would commit; stock Plone gives `Unauthorized` after its retry. An edit that waits behind a delete would write the removed Document and a version into a purged history; stock Plone gives `NotFound`. The request cache of `borg.localrole` would keep the old local roles.
- **M1 to M6, L1 to L3** are in section 2.

## Decision

### 1. The main decision: two variants

| | (a) the lock at the retry | (b) the move, with re-checks |
| --- | --- | --- |
| first attempt | no lock; runs as today | the lock after the traversal, then the snapshot moves to the grant |
| on a lost race | `ConflictError` at the vote, as today, and a hint for the retry | none: the edit waits, then reads after the commit before it |
| the retry | takes the lock before its snapshot, so its traversal and its save read after the commit before it | — |
| traversal decisions (existence, permissions, local roles) | always at the snapshot of the save | at the old snapshot, then re-checked after the move |
| `ConflictError`s for each edit of a contended object | at most one | none, unless a re-check fails |
| latency of a lost race | the work of the first attempt, Zope's random delay before a retry (0 to 1 s), and the wait for the lock | the wait for the lock |
| mid-transaction change of the snapshot | none | yes, while the transaction holds no change |
| what can be stale | nothing | what the traversal decided and the re-checks do not cover |
| the goal of the card ("commits at the first attempt") | changes to "at most one retry" | holds |

**Recommendation: (a).** It reads everything at the snapshot of the attempt that commits, as ZODB and Zope expect, so no decision of the traversal can be stale. It uses the rule of ADR 0045 that already exists (the hint takes the locks before the reads of the retry). It needs no re-check whose list can be incomplete. It changes the goal of the card, so it is the human's call.

#### 1a. Variant (a): the lock at the retry

1. **First attempt.** An edit takes no lock. Two edits of one object conflict at the vote, as today.
2. **The hint.** When an attempt of an edit of the allow-list (section 2.1) fails with a `ConflictError` that Zope retries, the subscriber of `IPubFailure` of `zoe.plone` (its `retry` flag is true) leaves an edit hint: the oid of the edited object. The private helper of `zoe.zodb` keeps it on the transaction manager of the connection of the object. Under Zope that is `transaction.manager.manager`, the manager of the thread, as for the retry hint of ADR 0045, section 4. The hint holds: the storage, compared by identity; the set of oids; and the failed transaction, which never takes its own hint. The retry of Zope runs on the same thread (`publish_module`: `delay_retry`, then `request.retry()`, then the next pass of its loop), and no other transaction runs on the thread in between.
3. **The retry takes the lock before its snapshot.** In each pass, `load_app` opens a connection (`Connection.open` runs `newTransaction(None, False)`), then `transaction_pubevents` calls `tm.begin()`, which sends `newTransaction(txn)` to the connection. The patched `newTransaction` of `zoe.zodb` (`LockingConnection`) ends the locks of the previous transaction, as now. Then, when the transaction argument is not None and the manager holds an edit hint for the storage, it takes the intent locks of the hint in one `lock_intent`, in oid order, and waits within `lock-timeout`. Only then does it run the `newTransaction` of ZODB, which syncs and moves the snapshot past the last commit. So the whole retry, its traversal included, reads after the commit of the edit that held the lock. A connection that opens later, inside the transaction, as the connection of a mounted database does during the traversal, takes the hint in the `newTransaction` of its `open`, because the transaction of the manager is then the retry and not the failed one.
4. **The end of the hint.** The retry takes the hint and drops it. A transaction that does not take it drops it at its end (`afterCompletion`, as `_drop_hint` does now). `IPubFailure` with `retry` false leaves no hint. So no hint reaches another request of the thread.
5. **What it gives.** The retry holds the lock of the object from before its snapshot to its end. Every other edit of the allow-list on that object either waits for that lock (its own retry) or conflicts at the vote and retries with a hint (its first attempt). A first attempt that votes while a retry holds the lock waits for the lock, as for a vote lock, then fails its serial check. So the retry never conflicts on the object, and each edit commits after at most one retry. Zope's three retries never run out on this object, where today eight concurrent edits can use all three. A retry can still conflict on another object, as today.
6. **The acceptance criterion** becomes: the edit and its version commit in one transaction; each concurrent edit of the object commits after at most one retry; the retry does not conflict on the object; and the history equals the sequential one in commit order. This replaces "at the first attempt" in the goal of the card and in the B5 criterion of ZOE-205.
7. **A lock at the first attempt is worse.** If the first attempt took the lock at the end of the traversal and raised `ConflictError` when the grant gave a newer serial, a contended edit would wait for the holder and then fail anyway. A first attempt that fails at once without a wait needs a non-blocking `lock_intent`, a new argument of the extension `lock_intent` on the wire. (a) needs neither.

#### 1b. Variant (b): the move, with re-checks

1. After the traversal, an edit of the allow-list takes the intent lock of its object within `lock-timeout`, and keeps the serial that the grant gives.
2. When the transaction holds no change and no savepoint, the helper moves the snapshot: in native mode it first runs the ping-and-wait sync of section 2.3; then it runs the `newTransaction` of ZODB that the patch replaced (sync, poll the invalidations, move the start, turn the changed objects into ghosts). The patched one would release the lock.
3. Then it re-checks what the traversal decided at the old snapshot: the object is still in its parent under its id, with the same oid; the user still has the roles of the published view on the object (`getRoles(object, name, published, ...)` and `user.allowed(published, roles)`, as `BaseRequest.traverse` checks); and the request cache of `borg.localrole` is dropped first. When a re-check fails, it raises `ConflictError`, so the retry of Zope gives the stock answer (`NotFound`, `Unauthorized`).
4. The re-checks cover what the review found. They do not cover other decisions that the traversal or its hooks make (`__before_publishing_traverse__`, the language, virtual hosting, a value that a view computes when it is made). Each of those stays at the old snapshot.

#### 1c. Weighing

- (b) keeps the goal of the card: no `ConflictError` at the first attempt. The spike measured the move: two and eight saves give the sequential history. But (b) reads part of the request at one snapshot and the rest at another, and its correctness rests on a list of re-checks that the next add-on can make incomplete.
- (a) reads each attempt at one snapshot. Its cost is one `ConflictError`, Zope's delay and the repeated work for each lost race, and the change of the goal of the card. It reuses the retry and the hint of ADR 0045. The spike did not run it.

### 2. What applies to both variants

#### 2.1 Which requests lock (H1, M5)

An allow-list of the endpoints that save a version. The subscriber locks only for these:

- the content `PATCH` of plone.restapi (`services/content/update.py`);
- the save button of the edit form of z3c.form: a `POST` to `@@edit` whose form holds `form.buttons.save`, not `form.buttons.cancel`. Under (a) a submit that fails validation writes nothing, so it never conflicts and never leaves a hint. Under (b) it takes the lock for the length of its request, because the subscriber cannot know the outcome of the validation;
- the revert of `@history` (`PATCH`, `services/history/patch.py`);
- `@workflow` (`POST`, `services/workflow/transition.py`) and the classic `@@content_status_modify` (plone.app.content);
- the content `DELETE` of plone.restapi and `@@delete_confirmation` (plone.app.content), because the delete purges the history.

Never: `@querystring-search`, a creation into a versioned folderish parent (its version is the first one of a new object; the parent does not version), `@comments`, `@lock`, `@tus-*`, and any `GET`. The lock is taken only for a type that versions automatically: `portal_repository.isVersionable(obj)` and `supportsPolicy(obj, "at_edit_autoversion")`. Other edits merge in tier 2, as now.

#### 2.2 A key of its own (M1, M3)

- **The key.** `edit-locks on` in `<zoe>` and in `<zoeclient>`, with the twin `ZOE_EDIT_LOCKS` for `<zoe>`. `<zoeclient>` has no twins, as `<zeoclient>` has none, so the key goes in the section. `early-locks` and `early-lock-classes` do not change: a site that uses them today sees no change. Section 2 of ADR 0045 stays as it is.
- **Early locks on, and the hot classes.** Today `zoe.zodb` treats a storage as one with early locks only when its set of hot classes is not empty. `storage.early_lock_classes` is None when `early-locks` is off and on a read-only storage, and a frozenset, possibly empty, when it is on. The build separates two questions:
  - "Does the connection take intent locks?": true when the set of hot classes is not empty, or `edit-locks` is on.
  - "Is this class hot?": true only for a class of the set.
- **The paths that change.** `_hot_storage` becomes two functions, one for each question. `tpc_begin` binds the owner of the connection when the connection takes intent locks; without that, the vote does not adopt the lock of an edit and waits on it to the timeout. `afterCompletion`, `newTransaction`, `close` and `_drop_hint` run when the connection takes intent locks. `setstate` and `tpc_finish` keep the question of the hot class. `install()` runs when the connection takes intent locks: `storage.py:193` and `zeo.py:1009`. `zeo.py:1055`, the client of ZEO inside, warns for `edit-locks` too. `zeo.py:1119`, a server without the extension `lock_intent`, warns and turns edit locks off. `zeo.py:1119` reads `_early_lock_classes`, and `zeo.py:1130` gives `early_lock_classes`; a read-only client keeps None, and takes no edit lock.

#### 2.3 Native mode (M2, L2)

`zoe.zeo.ClientStorage.sync()` (`zeo.py:1333`) does nothing while `server_sync` is off, the default of ZEO, and `lastTransaction()` (`zeo.py:1625`) waits only for a tid that the client is already giving to the DB. So, before a snapshot moves past a grant (the `newTransaction` of the retry in (a), the move in (b)), the helper runs the ping-and-wait sync of the native client (`_native.sync(True)`) whatever `server_sync` says. After the sync, it asserts that `lastTransaction()` is at or past the serial that the grant gave for each locked oid. A failed assertion raises `ConflictError`.

#### 2.4 With a change already made (L1)

When the transaction already holds a change or a savepoint, the helper takes no lock: it would wait, then conflict at the vote anyway. The edit goes on as today.

#### 2.5 No public API (L3), and ADR 0045, section 7 (L2)

The helper is private to `zoe.zodb` (a leading underscore), and only `zoe.plone` calls it. This ADR amends section 7 of ADR 0045, the list of internals of ZODB that the patch reads. It adds `transaction_manager` (already read by the hints), and for (b) `_registered_objects`, the MVCC adapter instance `_storage` and its `_start`, and the `newTransaction` of ZODB that the install keeps.

### 3. Why no update is lost

- The locks are advisory (ADR 0045, section 3). Every vote still checks the serial of every record that it writes, so no transaction commits a write that it computed from a state older than the committed one. At worst an edit gets the `ConflictError` of today.
- The edit and its version commit in one transaction: a crash keeps both or neither.
- (a): the attempt that commits reads at one snapshot, taken after the grant. (b): the snapshot moves only when the transaction holds no change, and the re-checks of section 1b redo the decisions of the traversal that matter for a stock answer.
- Two edits of one field give what they give one after the other: the later value wins. A retry of stock Plone gives the same. An edit form submits every field, and z3c.form writes each submitted value that differs from the current one; so a form loaded before the first commit writes back the old value of a field that the first edit changed. A retry of stock Plone does the same, on the same state.

### 4. What a stock client gets

- A stock `ZEO.ClientStorage`, a `zoe.zeo.ClientStorage` without `edit-locks`, or a site without the subscriber: no lock. Edits conflict as today, and Zope retries them.
- `zoe.zeo.ClientStorage` with `native false` refuses `edit-locks on` with `ValueError`, as it refuses `early-locks on` (ADR 0045, section 8).
- On a server where other clients hold intent locks, the vote of a stock client waits for them as for vote locks, and its serial check refuses a stale write.

### 5. Tests

**The acceptance test of the planner cannot pass unchanged** with either variant (spike, finding 1). The planner writes a new one, with the approval of the human. It runs through the publisher of Zope (`plone.testing.zope` with a WSGI client, or `zope.testbrowser` on the functional layer), on a copy of the site in a `zoe.Storage` with `edit_locks=True`, and checks:

- two and eight concurrent `PATCH`es of plone.restapi on one Document, of different fields: each commits ((a): after at most one retry, and no retry conflicts on the Document; (b): at the first attempt), and the history equals the sequential one in commit order;
- two concurrent saves of the `@@edit` form: the same;
- a reader `POST` (`@querystring-search` on the Document) while an edit holds the lock: it takes no lock and does not wait;
- a delete concurrent with an edit: the stock answers (the edit gets `NotFound` after its retry, or commits before the delete);
- a transition that removes Modify concurrent with an Editor's edit: the stock answers (`Unauthorized` after the retry, or the edit commits first);
- without `edit-locks`: the conflict and the retry of today;
- the same run on `zoe serve` with `zoe.zeo.ClientStorage` in native mode.

**Unit tests** in `python/zoedb/tests`: the hint (kept on the manager of the thread, taken by the next transaction before its snapshot, never by the failed one, dropped at the end of a transaction that does not take it); (b) the move and its refusal with a change or a savepoint; the lock released at commit, abort and close; a historical connection; a storage without `edit-locks`; a `LockOrder` with a hot class of a lower oid; the timeout; the native sync and the assertion of section 2.3; `edit-locks on` with an empty set of hot classes adopts the lock at the vote. **Tests of `zoe.plone`**: the allow-list, endpoint by endpoint, and the types that version automatically.

### 6. The B5 plan (M4, M6)

- `contention-fields` on `zoe-native-qi`, once with `edit-locks on` in the `<zoeclient>` section of the instances and once without, at the constants of the stack (2 instances with 2 threads each) and two steps of users, 4 and 16 (two users on each Document). `bench/` joins the scope fence for the key in the stack.
- A slow-holder case: one user in eight whose edit holds the lock 2 s longer (a content rule of the fixture that sends a mail to a local sink), to measure how long short edits wait and whether they reach `lock-timeout`.
- Each run inside `systemd-inhibit` and the lock of the bench, with a port offset of its own.
- Report: the `ConflictError`s at the first attempt and after the retries, the requests that ran out of retries, the p50 and p95 of the `PATCH`, `zoe_intent_lock_wait_seconds`, the lock timeouts, and, in the verification, that the last version of each Document holds both fields.

## Consequences

- With `edit-locks on`, concurrent edits of one versioned object commit one after the other, with the history of the sequential saves: (a) after at most one retry, (b) at the first attempt.
- An edit that waits for the lock holds a thread of Zope (2 for each instance in B5), and in native mode a connection of the pool of the client (ADR 0045, section 3: 64). A long holder makes short edits wait, where stock Plone lets the short one commit first and retries the long one (the slow-holder case of B5).
- Readers and edits outside the allow-list take no lock.
- `early-locks` and `early-lock-classes` behave as today.

## Alternatives

- **The history classes hot** (the question of the spike): `KeyError: 1` (spike, section 3).
- **The history classes hot with a covered subtree**: versions that miss committed changes (spike, section 4).
- **The Document class hot**: every reader of a Document takes an exclusive lock (spike, section 5).
- **(b) without re-checks**, as the first text of this ADR: an edit after a delete or after a loss of Modify commits (review, H2).
- **A lock at the first attempt in (a)**: the contended edit waits, then fails anyway; a non-blocking lock needs a change of the wire protocol (section 1a, item 7).
- **`early-locks on` with no hot class as the switch**: changes behaviour for current users (review, M3).
- **The version of an edit in a transaction of its own** (ADR 0051): rejected.
- **Keep the conflict**: Zope retries, the history is the sequential one, and eight concurrent edits can run out of retries.

## Questions for the human

Each question gives the recommendation first.

1. **The variant** (section 1): (a) the lock at the retry, or (b) the move with re-checks. Recommended: (a). Every decision of the request reads at one snapshot, nothing depends on a list of re-checks, and each edit commits after at most one retry. Other choice: (b), which keeps the first attempt.
2. **The goal of the card under (a)** (section 1a, item 6): "at most one retry, and no conflict of the retry on the object" instead of "at the first attempt", in the card and in the B5 criterion of ZOE-205. Recommended: yes, with (a). Under (b), the goal stays.
3. **A new acceptance test** (section 5), through the publisher, from the planner. Recommended: yes.
4. **The allow-list** (section 2.1) and the types that version automatically. Recommended: yes, as listed.
5. **A key of its own, `edit-locks`** (section 2.2), with the split of "takes intent locks" from "is hot". Recommended: yes; ADR 0045, section 2 stays.
6. **The scope fence** (sections 2.2, 2.3 and 6): add `python/zoedb/src/zoe/zodb.py`, `zeo.py`, `storage.py`, `config.py`, `component.xml`, their tests, and `bench/`. Recommended: yes. No public API (section 2.5).
7. **The native sync before a snapshot move** (section 2.3): always the ping-and-wait sync, then the assertion of the granted serial. Recommended: yes.
8. **Two edits of one field** (section 3): the later one wins, as after a retry of stock Plone. Recommended: accept it.
9. **The lock timeout** (section 6): keep `lock-timeout` (10 s), then `ConflictError` and Zope's retry; the slow-holder case of B5 measures it. Recommended: keep it.
10. **The budget** of the card (two sessions) is spent by the two spikes. Recommended: two more sessions in this card for the build, the new acceptance test and the B5 runs.
