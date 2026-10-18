# 0053. The lock of an edit: an edit of versioned content reads its object after the commit of the edit before it

Status: proposed, revised after two adversarial reviews (each SOUND WITH CHANGES; the second recommends variant (a)). The checkpoint of ZOE-212 after the rejection of ADR 0051. The edit and its version stay in one transaction, as in stock Plone. The early write-intent locks of ADR 0045 order the edits of one object. Section 1 gives two variants side by side; the human chooses. Both add public configuration (the key `edit-locks`, its twin `ZOE_EDIT_LOCKS`, the argument `edit_locks=`), a private helper of `zoe.zodb` and an opt-in subscriber of `zoe.plone`. Neither adds a public Python function, or changes the on-disk format, the wire protocol, the core, CMFEditions, ZopeVersionControl or plone.app.versioningbehavior. Both change `python/zoedb/src/zoe/zodb.py`, `storage.py`, `zeo.py` and `config.py`, which are outside the scope fence of the card. Variant (a) amends section 4 of ADR 0045 (one hint for each storage, taken at `begin`). The acceptance test of the planner cannot pass unchanged with either variant (section 5).

## Context

Two saves of one versioned Document from one snapshot both take the next version number and both write the history. The second save fails with `ConflictError`, and Zope's retry gives the history of the two saves one after the other (`docs/spikes/concurrent-versioned-saves.md`). No merge at the vote can give that history. ADR 0051, which moved the version into a transaction of its own, is rejected.

The spike `docs/spikes/versioned-saves-early-locks.md` found:

- The history classes hot, as ADR 0045 builds them, raise `KeyError: 1`. 6 of the 11 history records of a save are tier 1 BTrees, which ADR 0045 refuses as hot classes, and the other 5 are not leaf records.
- With a covered subtree, the saves commit at the first attempt, but each version misses the committed changes of the saves before it. The Document loads at the traversal, before any history record.
- The intent lock of the edited Document after the traversal, then a move of the snapshot of the connection to the grant, gives the sequential history at the first attempt for two and eight saves. This is variant (b) below.

The first review found that a lock on every write method locks readers (`@querystring-search` is a `POST`), and that the move comes after the existence and permission checks of the traversal. The second review checked variant (a) against the sources of `transaction`, ZODB and the core, and found where its hint must be taken and that the retry can lose its lock at its own vote. Sections 1a and 2 hold the changes.

## Decision

### 1. The main decision: two variants

| | (a) the lock at the retry | (b) the move, with re-checks |
| --- | --- | --- |
| first attempt | no lock until `IPubBeforeCommit`, then an intent wait (section 1a, item 7) | the lock after the traversal, then the snapshot moves to the grant |
| on a lost race | `ConflictError` before the vote, and a hint for the retry | none: the edit waits, then reads after the commit before it |
| the retry | takes the lock at its `begin`, before its snapshot, so its traversal and its save read after the commit before it | — |
| traversal decisions (existence, permissions, local roles) | always at the snapshot of the attempt | at the old snapshot, then re-checked after the move |
| `ConflictError`s for each edit of a contended object | one for each lost race; a second retry is possible (section 1a, item 8) | none, unless a re-check fails |
| latency of a lost race | the work of the first attempt, Zope's random delay before a retry (0 to 1 s), and the wait for the lock | the wait for the lock |
| mid-transaction change of the snapshot | none | yes, while the transaction holds no change |
| what can be stale | nothing | what the traversal decided and the re-checks do not cover |
| the goal of the card ("commits at the first attempt") | changes (section 1a, item 9) | holds |

**Recommendation: (a)**, as the second review also recommends. It reads each attempt at one snapshot, as ZODB and Zope expect, so no decision of the traversal can be stale. It reuses the retry of Zope and the hint of ADR 0045. It needs no list of re-checks that an add-on can make incomplete. It changes the goal of the card, so it is the human's call.

#### 1a. Variant (a): the lock at the retry

1. **The decision during the attempt.** At `IPubAfterTraversal`, the subscriber of `zoe.plone` decides whether the request is an edit of the allow-list (section 2.1). If it is, the subscriber keeps on the request the storage of the edited object and its oid. Nothing else happens at the traversal.
2. **Setting the hint.** When the attempt fails with a `ConflictError` that Zope retries, Zope notifies `IPubFailure` with `retry` true, after `tm.abort()` (`ZPublisher.WSGIPublisher.transaction_pubevents`). The subscriber only reads what item 1 kept. It calls the helper of `zoe.zodb` inside `try`/`except`: an error is logged at WARNING and never raised, because an exception in an `IPubFailure` subscriber replaces the `ConflictError` and ends the retries. The helper keeps the oid as an edit hint on the transaction manager of the thread (`transaction.manager.manager`, as for the retry hint of ADR 0045, section 4). It reads the attribute `_txn` of the manager and never calls `manager.get()`, which would create a stray transaction: after the abort, `_txn` is None.
3. **One hint for each storage (M1).** The hint of ADR 0045 (the hot oids of an attempt that ended on the order of its locks) and the edit hint share one entry for each storage: the storage, compared by identity, and the union of the oids. The next `begin` takes all of them in one `lock_intent`, in oid order. This amends section 4 of ADR 0045: the hot oids of a hint are taken at `begin`, not at the first load of a hot object.
4. **Where the hint is taken (H1).** The patched `newTransaction` of `zoe.zodb` takes the hint only when ZODB gets the call from `TransactionManager.begin` or from `registerSynch`:
   - In each pass of the retry loop of `publish_module`, `load_app` opens a connection. `Connection.open` calls `newTransaction(None, False)`; then `registerSynch` makes no call, because the manager has no transaction. Next, `transaction_pubevents` calls `tm.begin()`. It sets `_txn` to the new transaction, then sends `newTransaction(txn)` to the synchronizers.
   - `Connection.afterCompletion` of ZODB 6.2 also calls `newTransaction(txn, False)`, with the completed transaction. In `transaction` 5.0, `abort()` frees the manager (`_free_manager`) before it calls the `afterCompletion` of the synchronizers, so `_txn` is then None. `commit()` calls them before `_free()`, so `_txn` is then still the committed transaction.
   - So the rule has two parts. First, the patched `afterCompletion` sets a flag on the connection for the time of its call of the replaced method, and `newTransaction` takes no hint while the flag is set. Second, `newTransaction` takes the hint only when its argument is not None and is `manager._txn`. A synchronizer never calls `manager.get()`.
   - The `newTransaction` of `begin` releases the locks of the previous transaction, as now. Then it takes the locks of the hint and waits within `lock-timeout`. Only then does it run the `newTransaction` of ZODB, which syncs and moves the snapshot past the last commit (section 2.3 for native mode). So the whole retry, its traversal included, reads after the commit of the holder.
5. **A mounted database (L4).** The connection of a mounted database opens during the traversal, inside the transaction. Its `open` calls `newTransaction(None, False)`, then `registerSynch` sends `newTransaction(self._txn)`, and that call takes the hint of its storage.
6. **The end of the hint (M2, M3).**
   - The `begin` that takes a hint removes it.
   - A lock timeout in that `begin` puts the hint back, then raises the `ConflictError`. Zope retries, and the next `begin` takes the hint again.
   - At `IPubStart` of a request whose `retry_count` is 0, the subscriber drops every edit hint of the thread. `IPubStart` comes after `tm.begin()`, so the subscriber also releases the locks that this `begin` took for a stale hint. The locks are advisory, and the request has read nothing yet.
   - At the end of a retry (`IPubSuccess`, or `IPubFailure` with `retry` false), the subscriber drops the edit hints that no connection took, for example the hint of a mounted database that the retry did not open.
   - A request that Zope does not retry leaves no hint. A request with `x-wsgiorg.throw_errors` (zope.testbrowser with `handleErrors = False`) re-raises before `IPubFailure`, so it leaves no hint either (L1).
7. **An intent wait before the commit of a first attempt.** At `IPubBeforeCommit` of an edit of the allow-list that has no edit lock yet, the subscriber takes the intent lock of the edited object. It waits within `lock-timeout`, and holds no vote lock while it waits. Then it compares the serial that the grant gives with the serial that the transaction read for the object. When the serials are equal, the commit goes on, and the vote adopts the lock. When they differ, it raises `ConflictError`, and items 2 to 6 leave the hint. **Recommended: for.** Without it, a losing first attempt finds the conflict only in its vote. The vote takes its locks in oid order, so it takes the locks of records with lower oids first (on a site without queued indexing, the buckets of the catalog have lower oids than any Document). Then it waits for the intent lock of the retry that holds the Document, for the rest of that request, and every commit that needs those buckets waits behind it. With the wait, a losing first attempt waits before it holds any vote lock, and fails before its vote. The cost is one `lock_intent` round trip for each edit of the allow-list, which is granted at once when nobody holds the object. The wait comes after the work of the request, so it saves no work. Section 2.4 does not apply here: that rule is about the snapshot move.
8. **What it gives, and what it does not guarantee (H2).** The attempt that commits holds the lock of the object from its `begin`, so its whole request reads after the commit of the holder before it. But "at most one retry" is not a guarantee. A vote that holds intent locks and finds one of the locks that it needs busy releases its intent locks above the lowest oid that it needs, waits for its locks in oid order, and takes the intent locks again (`crates/zoe-core/src/commit/mod.rs`, `lock`, the branch on `Refused`). While the retry that holds the Document waits in its vote for a record with a lower oid, another retry can take the lock of the Document in its `begin`. That retry then moves its snapshot before the first one commits, and conflicts on the Document: a second retry. The expectation is that second retries are rare, because they need a lower record to be busy during the vote of a retry. B5 measures them (section 6), and the latency of unrelated edits while one object is contended.
9. **The acceptance criterion** becomes: the edit and its version commit in one transaction; an edit that loses a race retries, and its retry takes the lock of the object at its `begin`; and the history equals the sequential one in commit order, for edits that both save a version (M5). The count of second retries is a number that B5 reports, not a pass condition. This replaces "at the first attempt" in the goal of the card and in the B5 criterion of ZOE-205.
10. **A lock at the traversal of a first attempt is worse** than item 7. The contended edit would wait for the holder, then fail anyway. A first attempt that fails at once without a wait needs a non-blocking `lock_intent`, which is a new argument of the extension `lock_intent` on the wire.

#### 1b. Variant (b): the move, with re-checks

1. After the traversal, an edit of the allow-list takes the intent lock of its object within `lock-timeout`, and keeps the serial that the grant gives.
2. When the transaction holds no change and no savepoint, the helper moves the snapshot. In native mode it first runs the sync of section 2.3. Then it runs the `newTransaction` of ZODB that the patch replaced: sync, poll the invalidations, move the start, turn the changed objects into ghosts. The patched `newTransaction` would release the lock.
3. Then it re-checks what the traversal decided at the old snapshot. First it drops the request cache of `borg.localrole`. Then it checks that the object is still in its parent under its id, with the same oid, and that the user still has the roles of the published view on the object (`getRoles(object, name, published, ...)` and `user.allowed(published, roles)`, as `BaseRequest.traverse` checks). When a re-check fails, it raises `ConflictError`, so the retry of Zope gives the stock answer (`NotFound`, `Unauthorized`).
4. The re-checks cover what the first review found. They do not cover other decisions of the traversal or its hooks: `__before_publishing_traverse__`, the language, virtual hosting, a value that a view computes when it is made. Each of those stays at the old snapshot.

#### 1c. Weighing

- (b) keeps the goal of the card: no `ConflictError` at the first attempt. The spike measured the move: two and eight saves give the sequential history. But (b) reads part of the request at one snapshot and the rest at another, and its correctness rests on a list of re-checks.
- (a) reads each attempt at one snapshot. It costs one `ConflictError`, Zope's delay and the repeated work for each lost race, a possible second retry (item 8), and the change of the goal of the card. Its hint timing depends on the order of calls inside `transaction` and ZODB (item 4), so the first step of its build is a probe at the level of the publisher (section 5). The spike did not run it.

### 2. What applies to both variants

#### 2.1 Which requests lock

An allow-list of the endpoints that save a version. The subscriber decides it during the attempt (section 1a, item 1) and locks only for these:

- the content `PATCH` of plone.restapi (`services/content/update.py`);
- the save button of the edit form of z3c.form: a `POST` to `@@edit` whose form holds `form.buttons.save`, never `form.buttons.cancel`. A submit that fails validation writes nothing: under (a) it never conflicts and leaves no hint; under (b) it holds the lock for the length of its request;
- the revert of `@history` (`PATCH`, `services/history/patch.py`);
- `@workflow` (`POST`, `services/workflow/transition.py`) and the classic `@@content_status_modify` (plone.app.content);
- the content `DELETE` of plone.restapi and `@@delete_confirmation` (plone.app.content), because the delete purges the history.

Never: `@querystring-search`, a creation into a versioned folderish parent (its version is the first one of a new object, and the parent saves no version), `@comments`, `@lock`, `@tus-*`, and any `GET`. The lock is taken only for a type that versions automatically: `portal_repository.isVersionable(obj)` and `supportsPolicy(obj, "at_edit_autoversion")`. Other edits merge in tier 2, as now.

#### 2.2 A key of its own

- **The key.** `edit-locks on` in `<zoe>`, with the twin `ZOE_EDIT_LOCKS`, and in `<zoeclient>`, which has no twins, as `<zeoclient>` has none. `zoe.Storage(path, edit_locks=True)` and `zoe.zeo.ClientStorage(..., edit_locks=True)` take it as an argument. This is public configuration: the specification gets a row in the table of the configuration keys, and the argument in the signature of the opening. `early-locks` and `early-lock-classes` do not change, and section 2 of ADR 0045 stays as it is.
- **The attribute (L3).** `zoe.zodb` reads the boolean `storage.edit_locks`, read-only. On `zoe.Storage` it is true when the storage opened with `edit_locks=True` and is not read-only. On `zoe.zeo.ClientStorage` it is false until the first connection to the server. It becomes true only when the key is on, the client is native, it is not read-only, and the server accepted the extension `lock_intent`. It is computed again at each reconnect.
- **"Takes intent locks" and "is hot".** Today `zoe.zodb` treats a storage as one with early locks only when its set of hot classes is not empty. `early_lock_classes` is None when `early-locks` is off and on a read-only storage, and a frozenset, possibly empty, when it is on. The build separates two questions:
  - "Does the connection take intent locks?": true when the set of hot classes is not empty, or `edit_locks` is true.
  - "Is this class hot?": true only for a class of the set.
- **The paths that change.**
  - `_hot_storage` becomes two functions, one for each question.
  - `tpc_begin` binds the owner of the connection when the connection takes intent locks. Without that, the vote does not adopt the lock of an edit, and waits on it to the timeout.
  - `afterCompletion`, `newTransaction`, `close` and `_drop_hint` run when the connection takes intent locks.
  - `setstate` and `tpc_finish` keep the question of the hot class.
  - `install()` runs when the connection takes intent locks: `storage.py:193` and `zeo.py:1009`.
  - `zeo.py:1055`, the client of ZEO inside, warns for `edit-locks` too.
  - `zeo.py:1119`, a server without the extension `lock_intent`, warns and leaves `edit_locks` false.

#### 2.3 Native mode

`zoe.zeo.ClientStorage.sync()` (`zeo.py:1333`) does nothing while `server_sync` is off, which is the default of ZEO. `lastTransaction()` (`zeo.py:1625`) waits only for a tid that the client is already giving to the DB. So, before a snapshot moves past a grant (the `begin` of the retry in (a), the move in (b)), the helper runs the ping-and-wait sync of the native client (`_native.sync(True)`), whatever `server_sync` says. Then it checks that `lastTransaction()` is at or past the serial that the grant gave for each locked oid. When it is not, it raises `ConflictError`.

#### 2.4 With a change already made

When the transaction already holds a change or a savepoint, the helper takes no lock for a snapshot move: it would wait, then conflict at the vote anyway. The edit goes on as today. The intent wait of section 1a, item 7 is not a snapshot move, and this rule does not apply to it.

#### 2.5 The hold of a lock (M4)

`zoe serve` drops the intent locks of a session that holds them 60 s without a `tpc_begin` (ADR 0045, section 5). The in-process `zoe.Storage` has no such bound: an edit lock lasts until the end of the transaction, an abort, the close of the connection or the drop of its owner. A request that hangs holds the lock until it ends, and the edits that wait for it get `ConflictError` after `lock-timeout`, then retry. **Recommended:** state this in the specification, and add no bound in this card; the slow-holder case of B5 (section 6) shows whether one is needed. A bound in-process is a change of `zoe-py` and the core, outside the fence.

#### 2.6 No public function, and ADR 0045, section 7

The helper is private to `zoe.zodb` (a leading underscore), and only `zoe.plone` calls it. This ADR amends section 7 of ADR 0045, the list of internals of ZODB and `transaction` that the patch reads. It adds the attribute `_txn` of the transaction manager. For (b), it also adds `_registered_objects`, the MVCC adapter instance `_storage` and its `_start`, and the `newTransaction` of ZODB that the install keeps.

### 3. Why no update is lost

- The locks are advisory (ADR 0045, section 3). Every vote still checks the serial of every record that it writes. So no transaction commits a write that it computed from a state older than the committed one. At worst an edit gets the `ConflictError` of today.
- The edit and its version commit in one transaction: a crash keeps both or neither.
- (a): the attempt that commits reads at one snapshot, taken after its grant (or, for a first attempt that waits at `IPubBeforeCommit`, it commits only when the object did not change since its snapshot). (b): the snapshot moves only when the transaction holds no change, and the re-checks of section 1b redo the decisions of the traversal that give a stock answer.
- The history equals the sequential one only for concurrent edits that both save a version (M5). An edit that saves no version, such as a `@workflow` transition that only sets `effective`, merges in tier 2 and retries nothing.
- Two edits of one field give what they give one after the other: the later value wins. A retry of stock Plone gives the same. An edit form submits every field, and z3c.form writes each submitted value that differs from the current one. So a form that was loaded before the first commit writes back the old value of a field that the first edit changed. A retry of stock Plone does the same, on the same state.

### 4. What a stock client gets

- A stock `ZEO.ClientStorage`, a `zoe.zeo.ClientStorage` without `edit-locks`, or a site without the subscriber: no lock. Edits conflict as today, and Zope retries them.
- `zoe.zeo.ClientStorage` with `native false` refuses `edit-locks on` with `ValueError`, as it refuses `early-locks on` (ADR 0045, section 8).
- On a server where other clients hold intent locks, the vote of a stock client waits for them as for vote locks, and its serial check refuses a stale write.

### 5. Tests

**The first step of the build** is a probe at the level of the publisher, before any other code: a retried request through `publish_module` on the functional layer, with a log of each call of `newTransaction` and `afterCompletion` and its arguments, and of the value of `_txn` of the manager. The probe confirms the order of section 1a, items 4 to 6, on the installed versions of `transaction`, ZODB and Zope.

**The acceptance test of the planner cannot pass unchanged** with either variant (spike, finding 1). The planner writes a new one, with the approval of the human. It runs through the publisher of Zope (`plone.testing.zope` with a WSGI client, or zope.testbrowser on the functional layer) with error handling on (L1), on a copy of the site in a `zoe.Storage` with `edit_locks=True`. It checks:

- two and eight concurrent `PATCH`es of plone.restapi on one Document, of different fields, all at once and with staggered arrivals. Each edit commits, and the history equals the sequential one in commit order. Under (a), the test also checks that each retry took the lock of the Document at its `begin`, before its first load, and it reports the second retries. Under (b), each edit commits at the first attempt;
- the same with a background writer of records with lower oids (the catalog on a site without queued indexing);
- two concurrent saves of the `@@edit` form: the same;
- a reader `POST` (`@querystring-search` on the Document) while an edit holds the lock: it takes no lock and does not wait;
- a delete concurrent with an edit: the stock answers (the edit gets `NotFound` after its retry, or commits before the delete);
- a transition that removes Modify, concurrent with an Editor's edit: the stock answers (`Unauthorized` after the retry, or the edit commits first);
- a request that uses up its retries: no edit hint is left on the thread, and the next request of the thread takes no lock;
- without `edit-locks`: the conflict and the retry of today;
- the same run on `zoe serve` with `zoe.zeo.ClientStorage` in native mode.

**Unit tests** in `python/zoedb/tests`:

- the hint: set without `manager.get()`; taken only by the `newTransaction` of `begin` and of `registerSynch`, never by the one of `afterCompletion`, after a commit and after an abort; one entry for each storage, with the hot oids and the edit oids taken in one `lock_intent`; put back on a lock timeout; dropped at the end of a transaction that does not take it;
- (b): the move, and no move with a change or a savepoint;
- the lock released at commit, abort and close; a historical connection; a storage without `edit-locks`; a `LockOrder` with a hot class of a lower oid; the timeout;
- the native sync and the check of section 2.3; `edit-locks on` with an empty set of hot classes adopts the lock at the vote; `edit_locks` on a read-only storage and before the first connection of a client.

**Tests of `zoe.plone`**: the allow-list, endpoint by endpoint; the types that version automatically; an exception in the subscriber of `IPubFailure` is logged and does not replace the `ConflictError`; the drop at `IPubStart` with `retry_count` 0.

### 6. The B5 plan

- `contention-fields` on `zoe-native-qi`, once with `edit-locks on` in the `<zoeclient>` section of the instances and once without, at the constants of the stack (2 instances with 2 threads each) and two steps of users, 4 and 16 (two users on each Document). `bench/` joins the scope fence for the key in the stack.
- A slow-holder case: one user in eight whose edit holds the lock 2 s longer (a content rule of the fixture that sends a mail to a local sink), to measure how long short edits wait and whether they reach `lock-timeout`.
- Each run inside `systemd-inhibit` and the lock of the bench, with a port offset of its own.
- Report: the `ConflictError`s at the first attempt, the second retries, the requests that ran out of retries, the p50 and p95 of the `PATCH`, the latency of unrelated edits during the contention, `zoe_intent_lock_wait_seconds`, the lock timeouts, and, in the verification, that the last version of each Document holds both fields.

## Consequences

- With `edit-locks on`, concurrent edits of one versioned object commit one after the other, with the history of the sequential saves: under (a) after a retry that reads after the commit before it, under (b) at the first attempt.
- An edit that waits for the lock holds a thread of Zope (2 for each instance in B5), and in native mode a connection of the pool of the client (ADR 0045, section 3: 64). A long holder makes short edits wait, where stock Plone lets the short one commit first and retries the long one (the slow-holder case of B5).
- Readers and edits outside the allow-list take no lock.
- `early-locks` and `early-lock-classes` behave as today, except that, under (a), the hot oids of a retry hint are taken at `begin`.

## Alternatives

- **The history classes hot** (the question of the spike): `KeyError: 1` (spike, section 3).
- **The history classes hot with a covered subtree**: versions that miss committed changes (spike, section 4).
- **The Document class hot**: every reader of a Document takes an exclusive lock (spike, section 5).
- **(b) without re-checks**, as the first text of this ADR: an edit after a delete or after a loss of Modify commits (first review).
- **(a) without the intent wait of item 7**: a losing first attempt holds the vote locks of lower records while it waits for the Document (second review).
- **A lock at the traversal of a first attempt in (a)**: the contended edit waits, then fails anyway; a non-blocking lock needs a change of the wire protocol (section 1a, item 10).
- **`early-locks on` with no hot class as the switch**: changes the behaviour for current users.
- **The version of an edit in a transaction of its own** (ADR 0051): rejected.
- **Keep the conflict**: Zope retries, the history is the sequential one, and eight concurrent edits can run out of retries.

## Questions for the human

Each question gives the recommendation first. The second review recommends (a) with the changes of section 1a and section 2.

1. **The variant** (section 1): (a) the lock at the retry, or (b) the move with re-checks. Recommended: (a). Each attempt reads at one snapshot, and nothing depends on a list of re-checks. Other choice: (b), which keeps the first attempt.
2. **The goal of the card under (a)** (section 1a, item 9): an edit that loses a race retries once with the lock taken at its `begin`; the history equals the sequential one for edits that both save a version; second retries are reported, not forbidden. This replaces "at the first attempt" in the card and in the B5 criterion of ZOE-205. Recommended: yes, with (a). Under (b), the goal stays.
3. **The intent wait at `IPubBeforeCommit`** for the first attempts of the allow-list (section 1a, item 7). Recommended: yes. A losing first attempt then waits holding no vote lock, and fails before its vote.
4. **One hint for each storage**, taken at `begin` (section 1a, item 3), an amendment of section 4 of ADR 0045. Recommended: yes.
5. **A new acceptance test** (section 5), through the publisher, with error handling on, from the planner; and the probe of the hint timing as the first step of the build. Recommended: yes.
6. **The allow-list** (section 2.1) and the types that version automatically. Recommended: yes, as listed.
7. **A key of its own**, `edit-locks`, `ZOE_EDIT_LOCKS` and `edit_locks=`, as public configuration in the specification, with the split of "takes intent locks" from "is hot" (section 2.2). Recommended: yes; section 2 of ADR 0045 stays.
8. **The scope fence**: add `python/zoedb/src/zoe/zodb.py`, `storage.py`, `zeo.py`, `config.py`, `component.xml`, their tests, the specification's configuration table, and `bench/`. Recommended: yes. No public function (section 2.6).
9. **The native sync before a snapshot move** (section 2.3): always the ping-and-wait sync, then the check of the granted serial. Recommended: yes.
10. **The hold of a lock in-process** (section 2.5): no bound, stated in the specification; B5 measures the slow holder. Recommended: yes. Other choice: the 60 s bound of `zoe serve` in-process too, in another card.
11. **Two edits of one field** (section 3): the later one wins, as after a retry of stock Plone. Recommended: accept it.
12. **The lock timeout** (section 6): keep `lock-timeout` (10 s), then `ConflictError` and Zope's retry. Recommended: keep it.
13. **The budget** of the card (two sessions) is spent by the two spikes. Recommended: two more sessions in this card for the probe, the build, the new acceptance test and the B5 runs.
