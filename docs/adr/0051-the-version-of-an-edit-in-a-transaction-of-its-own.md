# 0051. The version of an edit in a transaction of its own: `zoe.plone` saves it after the commit of the edit

Status: rejected, by a decision of the human. The mechanism breaks the atomicity of an edit with its version: a crash, or a version transaction that fails, leaves a committed edit without its version. Two adversarial reviews each found new blocking problems; the second found that the swap of the subscriber never reaches the acceptance test, because the layers of plone.app.testing do not load the ZCML of `zoe.plone` and stack a registry for each layer, that the re-check of `isUpToDate` loses a version when an edit with an earlier `modification_date` commits second, and that the version transaction loses `adopt_roles` and proxy roles. ADR 0053 (proposed) keeps the edit and its version in one transaction; the spike `docs/spikes/versioned-saves-early-locks.md` gives its findings. The text below is the proposal as it was.

The status before the rejection: proposed, revised after an adversarial review. The checkpoint of ZOE-212 (step 2). It adds a behaviour to `zoe.plone`: for a class whose saves tier 2 merges, the version of an edit goes into a transaction of its own after the commit of the edit. It replaces a subscriber of plone.app.versioningbehavior in the component registry. It changes no code of CMFEditions, ZopeVersionControl or plone.app.versioningbehavior, no code of the core, no on-disk format, no wire protocol and no public Python API. It is a new ADR, not an amendment of ADR 0046: the merge policy and tier 2 do not change, and the decision is about when `zoe.plone` saves a version.

The file and line numbers below are in the eggs of the scenario `zoe` (`target/plone/eggs/cp313`): transaction 5.0, ZODB 6.2, Zope 5.14.2, Products.CMFCore 3.7, Products.CMFEditions 4.0.6, Products.CMFUid 4.2, Products.PluggableAuthService 3.0, plone.app.versioningbehavior 2.0.5, plone.protect 5.0.5, plone.transformchain 3.0.2, zope.configuration 7.0, zope.interface 8.2, z3c.zcmlhook 3.0.

## Context

The card asks that two saves of one versioned Document, which change different fields, both commit at the first attempt, and that the repository of CMFEditions then holds what the same saves give one after the other. The acceptance test of the planner (`python/zoedb/src/zoe/plone/tests/test_versioned_saves.py`) checks both, and that two saves of one field still conflict.

The spike of this card (`docs/spikes/concurrent-versioned-saves.md`) gives the facts:

- **What a save writes.** A save of a version writes 16 records. 8 of them both saves change: the Document, the `ShadowHistory` of CMFEditions, its `_full`, and in ZopeVersionControl `_versions`, `m_order`, `m_date`, the last version and the event log. Both saves take the same next version number, from the length of the history in their snapshot. The copy of the Document in the new version comes from the snapshot of its save.
- **How it fails.** Tier 2 merges the Document (both saves set `version_id` to the same value). Then the history refuses: `ShadowHistory` has no merge, the BTrees refuse two inserts of one key, and tier 1 refuses `_versions` with "Conflicting inserts". In B5, where the two snapshots often hold different numbers of versions, tier 2 refuses `version_id` first.
- **No merge at the vote.** The sequential result needs the second save to take the number after the first one, in 13 of its records and in 1 record of the first save, and its copy to hold the change of the first save. That is a new computation of the save, not a merge of two changes. A strategy for `version_id`, a coupled group, a split merge, or version numbers from a sequence (F29) cannot give it (section 4 and 5 of the spike).
- **What passes.** The copy of the second version can hold the change of the first save only when it is made after that save committed. A prototype of `zoe.plone` that saves the version in a transaction of its own after the commit of the edit passes the four tests of the acceptance module, unchanged. The prototype has gaps that the review found; sections 2 and 3 close them.

The card forbids a change of CMFEditions unless an ADR names it and the human approves. This mechanism changes no code of CMFEditions. But it changes when CMFEditions saves the version of an edit, and it replaces a subscriber of another package. So it needs the approval of the human.

## Decision

### 1. No merge of two version saves at the vote

Tiers 1 and 2 keep their rules. The records of CMFEditions and ZopeVersionControl get no entry in the merge policy and no native merge. `version_id` stays `strict` in the derived entries of `zoe.plone` (ADR 0046, section 9): `max` would merge the Document with a number that does not match the history.

### 2. The edit subscriber of plone.app.versioningbehavior

**The swap.** `zoe.plone` registers its own handler for `IVersioningSupport` and `IObjectModifiedEvent`, and removes `plone.app.versioningbehavior.subscribers.create_version_on_save` from the global registry for the same pair. A `zcml:customAction` of z3c.zcmlhook in the ZCML of `zoe.plone` does it, with `order="10"`. zope.configuration runs the actions of a lower order first (`config.py:1836`), and the `<subscriber>` directive of plone.app.versioningbehavior is an action of order 0. So the swap runs after the stock registration of the same configuration, in whatever order the packages load. The swap calls `getGlobalSiteManager().unregisterHandler(create_version_on_save, required=(IVersioningSupport, IObjectModifiedEvent))`. That call gives `False` when the stock subscriber is not registered (`zope/interface/registry.py:503-504`): another package removed it, or the ZCML of plone.app.versioningbehavior did not load before. Then `zoe.plone` registers nothing and logs it at level INFO. An instance, its scripts and the test layers of plone.app.testing see the same handler. *(Question 2.)*

**The guard on the stock function.** `zoe.plone` holds the SHA-256 of the source of `create_version_on_save` (`inspect.getsource`) at the version that it copies, 2.0.5. The swap computes it again. When it differs, or the source is not there, the swap does not happen: the stock subscriber stays, and `zoe.plone` logs an ERROR that names the installed version of plone.app.versioningbehavior. A test of `zoe.plone` checks the hash too, so its suite fails on the upgrade. *(Question 9.)*

**The handler.**

1. It reads the merge policy of the database of the object (`zoe.policy.read`). When the object has no `_p_jar`, or the policy has no entry for its class, or the entry is not in mode `on`, the handler calls the stock subscriber, and nothing else changes. *(Question 5.)*
2. Else it makes the decision of the stock subscriber, a copy of `subscribers.py:10-61`, with the same checks in the same order:
   1. no version for an `IContainerModifiedEvent`;
   2. no version when `context.portal_repository` raises `AttributeError` (an object without an acquisition chain);
   3. no version when `portal_repository.isVersionable(context)` is false;
   4. the change note: `get_change_note(context.REQUEST, None)` of plone.app.versioningbehavior when `getattr(context, "REQUEST", None)` is true, else `None`;
   5. a version is due when there is a change note;
   6. else, when `portal_repository.supportsPolicy(context, "at_edit_autoversion")` is true: a version is due when `base_hasattr(context, "version_id")` is false, or when `isUpToDate(context, context.version_id)` is false, or when it raises `ArchivistUnregisteredError`.

   The copy is a function of its own, `version_due(context, event)`, which gives the decision and the note. It calls the helpers of the stock module (`get_change_note`, `base_hasattr`); it copies only the decision.
3. When a version is due, it does not call `portal_repository.save`. It adds a pending save to the transaction of the edit (below).
4. While a version transaction runs in the thread (section 3), the handler adds no pending save. So a subscriber that the save calls cannot start a loop.

**The pending saves.** One edit transaction holds one mapping of pending saves, in `set_data` and `data` of the transaction (`_transaction.py:512-529`). Its key is the database name and the oid of the object, so two modified events of one object in one transaction give one pending save. A pending save holds:

- the object as the last event gave it, with its acquisition chain;
- the last change note that was not empty;
- the user of the security manager at the last event (`getSecurityManager().getUser()`), as an object, not an id;
- the physical path of `getSite()` at the event, or `None` when there is no site.

The first pending save of a transaction adds two hooks to it: a before-commit hook and an after-commit hook. A subscriber of `zoe.plone` for `IVersioningSupport` and `IObjectMovedEvent` keeps a pending save right when its object moves later in the transaction: OFS sends the event to the moved object and to each object under it, wrapped at the new place (`OFS/subscribers.py:107-115`), and the subscriber stores the object that it gets. On an `IObjectRemovedEvent` it drops the pending save. The before-commit hook reads the physical path of each pending object. So the path is the path at the commit of the edit, not at the event.

So the transaction of the edit writes the Document only: its field and `modification_date`. Tier 2 merges two edits of different fields, and refuses two edits of one field.

### 3. The version transaction

The after-commit hook runs inside `commit()` of the edit, after the commit of its resources and before the edit transaction is freed (`_transaction.py:286-288`), in the same thread, before the response. The request, `zope.globalrequest`, the site and the security manager of the request are still set. One version transaction holds all the pending saves of one edit transaction.

1. **After a commit that failed** (`status` is false), it does nothing.
2. **The database.** It takes the root database from the object: `aq_base(obj.getPhysicalRoot())._p_jar.db()`, the database of the `Application` (`OFS/Application.py:241`). An object in a mounted database has another `_p_jar.db()`, which holds no `Application`.
3. **The connection.** It makes a `transaction.TransactionManager()` of its own, `tm`, and opens `root_db.open(transaction_manager=tm)`. It takes the application from `connection.root()["Application"]` and wraps it in the request of the thread (`app.__of__(RequestContainer(REQUEST=request))`). A traversal into a mounted database opens it through `Connection.get_connection`, which gives it the same transaction manager (`Connection.py:350-356`).
4. **The current transaction.** It makes `tm` the transaction manager of the thread for the length of the hook: `saved = transaction.manager.manager`, then `transaction.manager.manager = tm`, and in `finally`, `transaction.manager.manager = saved`. `transaction.manager` is a `ThreadTransactionManager`, a `threading.local` whose methods call `self.manager` at each call (`_manager.py:221-282`). So, during the hook:
   - `transaction.get()` and `transaction.savepoint()` give the version transaction. `CopyModifyMergeRepositoryTool.save` takes its savepoints in it (`CopyModifyMergeRepositoryTool.py:297`, `:509`).
   - The indexing queue of CMFCore is a `threading.local` of the module (`indexing.py:32`). The commit of the edit processed and cleared it, so its next operation makes a new `QueueTM`, which calls `transaction.get().join(...)` (`indexing.py:300-305`) and joins the version transaction. That happens when `portal_historyidhandler.register` gives a history id to the object or to its parent and reindexes `cmf_uid` (`UniqueIdHandlerTool.py:72-88`). Without the swap, the join goes to the committed edit and raises `ValueError` (`_transaction.py:187-193`), and the version is lost.
   - The queued indexing of `zoe.plone` writes through `zoe.kv.Namespace` of the connection of the catalog, which joins `connection.transaction_manager.get()` (`kv.py:166`). The catalog comes from the new connection, so its operations join the version transaction.
   - An after-commit hook that the save adds goes to the version transaction and runs at its commit. The loop of after-commit hooks of the edit reads the list of the edit transaction itself (`_transaction.py:370`), so it does not run it. The hooks of the edit after this one run with the manager of the thread restored.
   - The connection of the edit stays bound to the manager that it opened with: ZODB binds a connection to `transaction.manager.manager` at `open` (`Connection.py:879-885`). It does not join the version transaction. So every object that the save changes must come from the new connection: a change of an object of the edit connection in the hook joins the committed edit and raises `ValueError`.
   - The swap must be undone before the hook returns: `load_app` of the publisher reads `transaction.manager.manager._txn` after the request (`WSGIPublisher.py:306`), and the edit transaction frees itself from the manager that it holds (`_transaction.py:476-486`).
5. **The site.** It traverses to the path of the site of the pending save, with `unrestrictedTraverse`, and calls `setSite` on it. With no site at the event, it sets none. So a Plone site in a folder, and a child site of a lineage, get the site that the edit had.
6. **The user.** For each pending save, it re-wraps the user of the event in the new connection: when `aq_parent(aq_inner(user))` is a user folder, it takes the physical path of that folder, traverses to it in the new connection, and calls `newSecurityManager(request, aq_base(user).__of__(folder))`. That holds for a user of a Plone site, a user of the root `acl_users`, and the emergency and anonymous users of PAS, which PAS wraps (`PluggableAuthService.py:682-751`, `:894`). A user without a parent (an unwrapped `UnrestrictedUser`, as `SpecialUsers.system`) it uses as it is. It never looks a user up again by id. The check of a permission by PAS needs the user folder in the acquisition chain of the object, so the user must come from the new connection.
7. **Each attempt.** `tm.begin()`, which syncs the connection to the last committed transaction. Then, for each pending save in the order of the events:
   1. It traverses to the path of the pending object and checks that the object it finds has the same oid. Else it logs a WARNING with the path and goes on: the object was removed or replaced.
   2. The re-check. Without a change note, it saves only when the object has no `version_id`, or `isUpToDate(obj, obj.version_id)` is false or raises `ArchivistUnregisteredError`. Else it goes on: a version that another transaction committed holds this state already. With a change note, it always saves, as the stock subscriber does.
   3. It takes a savepoint and calls `portal_repository.save(obj=obj, comment=note)`, as the user of the pending save.
   4. On `FileTooLargeToVersionError`, the save rolled back its own savepoint (`CopyModifyMergeRepositoryTool.py:305-308`). It logs a WARNING with the path, and goes on.
   5. On another error that is not a `TransientError`, it rolls back to its savepoint, logs an ERROR with the path and the traceback, and goes on.

   Then it notes the transaction for the undo log: `note("zoe.plone: save the version of <path>")`, one line for each saved path, and `setUser(user_id, path)` with the user of the first saved pending save and the path of its user folder without the application, as `recordMetaData` of the publisher does (`ZPublisher/utils.py:57-69`). It commits. When no pending save saves, it commits nothing.
8. **The retries.** On a `TransientError` (a `ConflictError`, a `ReadConflictError`) in an attempt or in its commit, it calls `tm.abort()`, waits a random time from 0 to min(1 s, 10 ms × 2^n) after attempt n, and starts a new attempt from step 7, with the re-check. There are at most 10 attempts. *(Question 7.)* Each retry is a line at level INFO. After the last attempt, it logs an ERROR with the paths and gives up: an after-commit hook must not raise, and `_call_hooks` would only log the error (`_transaction.py:371-378`).
9. **The end, on every path.** In `finally`: `tm.abort()`, then `connection.close()`, then the manager of the thread, the site and the security manager of the thread as they were. ZODB refuses to close a connection that is still joined (`Connection.py:278-283`), and the abort unjoins it (`Connection.py:407-421`). So a non-conflict error does not leak the connection: it goes back to the pool.

The version transaction starts after the edit committed. So its copy holds the edit and each edit that committed before it, and it numbers the version after every committed version.

plone.protect checks the writes of a request in its transform, at `IPubBeforeCommit`, before the commit of the edit (`plone/transformchain/zpublisher.py:72`, `plone/protect/auto.py:215-230`). The version transaction runs only after an edit that passed that check committed, so it opens no way around the protection against CSRF.

### 4. What it guarantees, and what it does not

- **The exact case.** When the snapshot of the attempt that commits the version transaction of an edit comes before the commit of the next edit of the object, the history equals the history of the same edits one after the other. The acceptance test runs in that order.
- **Overlapping edits.** Else the copy of a version holds a later edit too. No change of the content is lost, but the history is coarser, and it can credit a change to the wrong user and the wrong change note. An example: edit A by user X, then edit B by user Y, both commit, and the version transaction of B runs first. Its version holds A and B, with the user Y and the note of B. The version transaction of A then finds the object up to date. Without a change note it saves nothing, so the change of A is in a version of Y. With a change note it saves a second version that holds A and B, with X and the note of A. The timestamp of a version is the time of its version transaction, not of its edit. The prototype shows the case without the re-check: the versions (A, B) and (A, B), not (A, old) and (A, B). *(Question 3.)*
- **A lost version.** A crash of the process between the two commits loses the version of that edit and its change note. So does a version transaction that gives up after its retries or on an error. The content of the edit stays. With `at_edit_autoversion`, the next edit of the object makes a version that holds the change, because `isUpToDate` compares the modification dates; that version has the user and the note of the next edit. The lost change note is never recovered. Without `at_edit_autoversion`, the change waits for the next version that a change note asks for. *(Question 4.)*
- **A file too large to version.** When the modifier `AbortVersioningOfLargeFilesAndImages` is on (it is off by default, `StandardModifiers.py:822`), the stock subscriber lets `FileTooLargeToVersionError` out, so the request fails and its edit does not commit. Here the edit commits, the version is not made, and a WARNING names the path. *(Question 11.)*
- **The cost.** The request waits for one more commit for each edit transaction that has a pending save. The thread holds two connections of the root database during the hook, so the pool of the database can need twice the number of threads. ZODB logs a warning over its pool size and a critical line over twice its pool size (`DB.py:152-159`). Each connection has its own cache. *(Question 10.)*
- **Conflicts.** Two version transactions of one object conflict with each other on the history. They retry inside the hook, and the re-check makes most retries end without a save. They are not `ConflictError`s of the request, and B5 counts them apart, from the log lines.

### 5. What does not change

- The code of CMFEditions, ZopeVersionControl and plone.app.versioningbehavior.
- The other callers of `portal_repository.save` save in their own transaction, as today: the first version at a creation (`create_initial_version_after_adding`), revert, plone.app.iterate, scripts, the history service of plone.restapi. *(Question 6.)*
- A site without a merge policy, or whose policy does not merge the class in mode `on`, keeps the stock decision, in the transaction of the edit.
- The core, the merge policy and its derived entries.

### 6. The build

- The code is a module of `zoe.plone` and lines of its ZCML. The scope fence of the card holds it (`python/zoedb/src/zoe/plone/**`); the part `crates/zoe-core/src/merge/**` of the fence stays unused.
- The acceptance test stays as the planner wrote it. *(Question 8.)*
- The build adds tests in `zoe.plone.tests`, on the layer of the acceptance test, with queued indexing on:
  - the edit transaction writes no record of the history;
  - the hook does nothing after a failed commit;
  - a conflict of two version transactions retries, and the second one saves nothing when it has no change note and the object is up to date;
  - the other order of section 4 gives the history that it states;
  - a class without an entry in mode `on` keeps the stock decision;
  - the user is wrapped in the user folder of the new connection; an edit by a user of the root `acl_users` gets its version; an unwrapped special user saves as it is;
  - a versioned object whose parent has no history id, and a versioned object that has none: the reindex of `cmf_uid` joins the version transaction, and the version is there;
  - a non-conflict error in the save: an ERROR line and no version; after the hook the connection is back in the pool (the number of open connections of the database is the same as before), and the manager of the thread, the site and the security manager are as before;
  - two modified events of one object in one request give one version transaction and one version; two versioned objects in one request give one version transaction and two versions;
  - a site in a folder of the application, and an object that moves in the transaction after its modified event: the version is on the object at its new path;
  - `FileTooLargeToVersionError`: the edit commits, a WARNING, no version;
  - the undo log of the version transaction gives the user and the note of section 3;
  - the swap: after the ZCML loads, the handler of `zoe.plone` is registered and the stock subscriber is not; when the stock subscriber is not registered, `zoe.plone` registers nothing; the hash of the stock source matches.
- **The comparison test.** `version_due` and the stock `create_version_on_save` run on the same cases: an `IContainerModifiedEvent`; an object without `portal_repository`; an object that is not versionable; a request with and without a change note; no `REQUEST`; `at_edit_autoversion` on and off; no `version_id`; `isUpToDate` true and false; `ArchivistUnregisteredError`. In each case the stock function runs with the `save` of its `portal_repository` recorded, not done. The test compares the decision and the note.
- A mount point is tested when the scenario has one. The instance of the scenario `zoe` has no Products.ZODBMountPoint today, and the build says so under "Not tested".
- The specification, in a paragraph on `zoe.plone` and versions next to the one on queued indexing, describes the behaviour, its log lines and its limits, with this sentence: "When edits of one versioned object overlap, a version can hold the changes of several edits, credited to the user and the change note of one of them; a version is exact when its snapshot comes before the commit of the next edit of the object. A crash, or a version transaction that gives up, loses the version of an edit and its change note; the content of the edit stays."
- Step 4 of the card measures B5 `contention-fields`. The report gives, apart: the `ConflictError`s of the requests; the version transactions, with their commits, retries, saves that the re-check skips, and give-ups; the commits for each edit; the pool size of the database and the largest number of open connections; and the memory of the connection caches.

## Consequences

- Two edits of different fields of one versioned object commit at the first attempt where tier 2 merges the class. The acceptance test of the card can pass.
- Each edit transaction with a pending save costs one more commit, in the request, and a second connection while it runs.
- The history of an object can be coarser than the sequential one when edits overlap, and can credit a change to the wrong user and change note. A crash, or a version transaction that gives up, loses one version and its change note.
- `zoe.plone` holds a copy of the decision of the stock subscriber, guarded by the hash of its source. A new version of plone.app.versioningbehavior that changes the function gives an ERROR and the stock subscriber, until `zoe.plone` follows it.
- The first version at a creation still saves in the transaction of the creation. Its conflicts are those of ZOE-219 (ADR 0048).

## Alternatives

- **A merge of the two saves in the core.** It would recompute the save of CMFEditions: renumber 14 records of 9 classes, edit the copy, edit a pickle inside a bytes value, recompute sizes that Python's pickler gives, and write a record that the transaction did not store. Shadow mode cannot verify it, because there is no Python reference to compare with (spike, section 4).
- **A strategy for `version_id`** (`max`, `sum`) or a coupled group of the Document with its history. The history refuses after the Document, and a coupled group spans one record only.
- **Version numbers from a sequence of the store (F29).** CMFEditions uses the number as a position, ZopeVersionControl computes its own numbers, and the copy still holds the old title (spike, section 5).
- **The version policy of CMFEditions.** `zoe.plone` could switch off `at_edit_autoversion` for the types and save the version itself. But the policy is site data, which a site without the profile of `zoe.plone` does not hold; the first version at a creation reads the same policy; and the stock subscriber still saves in the edit transaction when a change note is given.
- **A patch of `CopyModifyMergeRepositoryTool.save`.** It would defer every save, also revert and plone.app.iterate, which read the history right after the save in their transaction.
- **A durable queue and a worker,** as for queued indexing (ADR 0047). It closes the crash window, but the version is then not there when the request ends, and the acceptance test reads the history right after the commit.
- **The copy from the state at the commit of the edit.** The version transaction would clone the object from a connection at the tid of the edit, so the history would equal the sequential one in every order. `ArchivistTool.prepare` reads the object that it saves, and sets `version_id` on it, so this needs a change of CMFEditions.
- **A version transaction that is not the current one of the thread.** The prototype kept the edit as the current transaction. Then the savepoint of the save goes to the committed edit, and a reindex during the save raises `ValueError` (section 3, step 4).
- **The user looked up again by id**, as the prototype did. `getUserById` of the user folder of the site gives `None` for a user of the root `acl_users`, a special user and some PAS plugins, and the user at the commit can differ from the user at the event (`adopt_user`, `adopt_roles`).
- **Keep the conflict.** Zope retries the request, and the result is the sequential one. The acceptance test cannot pass, and the B5 criterion of the card goes back to the human.
- **Versions from the history of the store (F36).** It replaces the storage of CMFEditions on sites that opt in, in phase 7, after retention.

## Questions for the human

Each question gives Claude's recommendation first. The adversarial review recommends the same answers for questions 1 to 8.

1. **No merge of two version saves at the vote** (section 1), and `version_id` stays `strict`. Recommended: yes.
2. **The version of an edit in a transaction of its own** (sections 2 and 3): `zoe.plone` replaces the edit subscriber of plone.app.versioningbehavior in the global registry. Recommended: yes, with the six changes of the review: the version transaction is the current one of the thread; the user of the event, re-wrapped; an abort before the close on every path; every stock check, a comparison test and a guard on the stock source; pending saves grouped per object, the re-check, and retries with backoff; the path at the commit, the site from `getSite()`, the root database. Other choices: keep the conflict (the acceptance test cannot pass, and the B5 criterion of the card goes back); or change the goal of the card (the B5 criterion on a type without versions, or wait for F36).
3. **The coarser history when edits overlap** (section 4), with a change credited to the wrong user and change note. Recommended: accept it, and state it in section 4 and in the specification with the sentence of section 6. Other choice: clone from the state at the commit of the edit, which needs a change of CMFEditions.
4. **The lost version** (section 4): a crash between the two commits, or a version transaction that gives up, loses the version and its change note. Recommended: accept it; with `at_edit_autoversion` the next edit makes a version with the change, but the change note is not recovered. Other choice: a durable record of the pending save in the durable namespace (ADR 0043), applied at the start of the process.
5. **When the mechanism is on** (section 2): when the merge policy of the database has an entry in mode `on` for the class of the object. Recommended: yes. Other choices: always when `zoe.plone` loads; or a backend of `ZOE_PLONE_BACKENDS`, which the recipe of the acceptance test does not set today.
6. **Only the edit subscriber** (section 5): the first version at a creation and the other callers of `portal_repository.save` keep their transaction. Recommended: yes.
7. **The retries of the version transaction** (section 3, step 8): at most 10 attempts on any `TransientError`, a random wait from 0 to min(1 s, 10 ms × 2^n), the re-check before each attempt, a line at level INFO for each retry and an ERROR at the end. Recommended: yes, as constants of the module.
8. **The acceptance test** stays as the planner wrote it (section 6). It runs in the order where the history is exact; the build adds a test of the other order. Recommended: yes.
9. **A changed stock function** (section 2): when the hash of the source of `create_version_on_save` differs, `zoe.plone` keeps the stock subscriber and logs an ERROR, and its test fails. Recommended: yes; the site then works as stock Plone, and only the conflict comes back. Other choices: refuse to start the instance; or compare the installed version of plone.app.versioningbehavior, which also fails on a release that does not change the function.
10. **The second connection** (section 4): the thread holds two connections of the root database during the hook. Recommended: accept it; B5 measures it, and the specification tells operators that the pool size of the database must be at least twice the number of threads. Other choice: a pool of its own for the version transactions, a second `DB` on the same storage, which costs one more cache.
11. **A file too large to version** (section 4): the edit commits without a version and with a WARNING, where the stock subscriber fails the request. Recommended: accept it, and state it in the specification. Other choice: keep the stock subscriber on a site where the modifier is on, which gives back the conflict on that site.
