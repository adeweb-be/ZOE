# 0051. The version of an edit in a transaction of its own: `zoe.plone` saves it after the commit of the edit

Status: proposed. The checkpoint of ZOE-212 (step 2). It adds a behaviour to `zoe.plone`: for a class whose saves tier 2 merges, the version of an edit goes into a transaction of its own after the commit of the edit. It replaces a subscriber of plone.app.versioningbehavior in the component registry. It changes no code of CMFEditions, ZopeVersionControl or plone.app.versioningbehavior, no code of the core, no on-disk format, no wire protocol and no public Python API. It is a new ADR, not an amendment of ADR 0046: the merge policy and tier 2 do not change, and the decision is about when `zoe.plone` saves a version.

## Context

The card asks that two saves of one versioned Document, which change different fields, both commit at the first attempt, and that the repository of CMFEditions then holds what the same saves give one after the other. The acceptance test of the planner (`python/zoedb/src/zoe/plone/tests/test_versioned_saves.py`) checks both, and that two saves of one field still conflict.

The spike of this card (`docs/spikes/concurrent-versioned-saves.md`) gives the facts:

- **What a save writes.** A save of a version writes 16 records. 8 of them both saves change: the Document, the `ShadowHistory` of CMFEditions, its `_full`, and in ZopeVersionControl `_versions`, `m_order`, `m_date`, the last version and the event log. Both saves take the same next version number, from the length of the history in their snapshot. The copy of the Document in the new version comes from the snapshot of its save.
- **How it fails.** Tier 2 merges the Document (both saves set `version_id` to the same value). Then the history refuses: `ShadowHistory` has no merge, the BTrees refuse two inserts of one key, and tier 1 refuses `_versions` with "Conflicting inserts". In B5, where the two snapshots often hold different numbers of versions, tier 2 refuses `version_id` first.
- **No merge at the vote.** The sequential result needs the second save to take the number after the first one, in 13 of its records and in 1 record of the first save, and its copy to hold the change of the first save. That is a new computation of the save, not a merge of two changes. A strategy for `version_id`, a coupled group, a split merge, or version numbers from a sequence (F29) cannot give it (section 4 and 5 of the spike).
- **What passes.** The copy of the second version can hold the change of the first save only when it is made after that save committed. A prototype of `zoe.plone` that saves the version in a transaction of its own after the commit of the edit passes the four tests of the acceptance module, unchanged.

The card forbids a change of CMFEditions unless an ADR names it and the human approves. This mechanism changes no code of CMFEditions. But it changes when CMFEditions saves the version of an edit, and it replaces a subscriber of another package. So it needs the approval of the human.

## Decision

### 1. No merge of two version saves at the vote

Tiers 1 and 2 keep their rules. The records of CMFEditions and ZopeVersionControl get no entry in the merge policy and no native merge. `version_id` stays `strict` in the derived entries of `zoe.plone` (ADR 0046, section 9): `max` would merge the Document with a number that does not match the history.

### 2. The edit subscriber of plone.app.versioningbehavior

`zoe.plone` registers its own handler for `IVersioningSupport` and `IObjectModifiedEvent`, and removes `plone.app.versioningbehavior.subscribers.create_version_on_save` from the global registry for the same pair. It does this when its ZCML loads, so an instance, its scripts and the test layers of plone.app.testing see the same handler. *(Question 2.)*

The handler:

1. Reads the merge policy of the database of the object (`zoe.policy.read`). When the policy has no entry for the class of the object, or its entry is not in mode `on`, the handler calls the stock subscriber, and nothing else changes. *(Question 5.)*
2. Else it makes the decision of the stock subscriber, with the same checks in the same order. There is no version for an `IContainerModifiedEvent` or for an object that is not versionable. A version is due when the request gives a change note, or when the type has the policy `at_edit_autoversion` and the object has no `version_id` or `isUpToDate` is false.
3. When a version is due, it does not call `portal_repository.save`. It adds an after-commit hook to the transaction, with the physical path of the object, the change note and the database.

So the transaction of the edit writes the Document only: its field and `modification_date`. Tier 2 merges two edits of different fields, and refuses two edits of one field.

### 3. The version transaction

The hook runs after the commit of the edit, in the same thread, before the response:

1. After a commit that failed, it does nothing.
2. It opens a connection of the same database with a transaction manager of its own. It finds the object by its physical path under the application, wrapped in the request of the thread. It sets the site of the object.
3. It runs as the same user, wrapped in the user folder of the new connection. The check of a permission by PAS needs the user folder in the acquisition chain of the object.
4. It calls `portal_repository.save(obj=obj, comment=note)` and commits, with the note `zoe.plone: save the version of <path>`.
5. On a `ConflictError`, it aborts and tries again from a new snapshot, at most 10 times. *(Question 7.)* Each retry is a log line at level INFO. After the last try, or on any other error, it logs an ERROR with the path and gives up: an after-commit hook must not raise.
6. It closes the connection, and gives the thread back its site and its security manager.

The version transaction starts after the edit committed. So its copy holds the edit and each edit that committed before it, and it numbers the version after every committed version.

### 4. What it guarantees, and what it does not

- When the version transaction of an edit commits before the next edit of the object commits, the history equals the history of the same edits one after the other. The acceptance test runs in that order.
- Else a version can hold a later edit too, and two versions can hold the same state. No change of the content is lost; the history is coarser. The prototype shows it: two edits that commit before both version transactions give the versions (A, B) and (A, B), not (A, old) and (A, B). *(Question 3.)*
- A crash of the process between the two commits loses the version of that edit. The next edit makes a version that holds the change, because `isUpToDate` compares the modification dates. *(Question 4.)*
- The request waits for one more commit for each edit of a versioned object.
- Two version transactions of one object conflict with each other on the history. They retry inside the hook. They are not `ConflictError`s of the request, and B5 counts them apart, from the log lines.

### 5. What does not change

- The code of CMFEditions, ZopeVersionControl and plone.app.versioningbehavior.
- The other callers of `portal_repository.save` save in their own transaction, as today: the first version at a creation (`create_initial_version_after_adding`), revert, plone.app.iterate, scripts. *(Question 6.)*
- A site without a merge policy, or whose policy does not merge the class in mode `on`, keeps the stock subscriber.
- The core, the merge policy and its derived entries.

### 6. The build

- The code is a module of `zoe.plone` and a line of its ZCML. The scope fence of the card holds it (`python/zoedb/src/zoe/plone/**`); the part `crates/zoe-core/src/merge/**` of the fence stays unused.
- The acceptance test stays as the planner wrote it. *(Question 8.)* The build adds tests in `zoe.plone.tests`: the edit transaction writes no record of the history; the hook does nothing after a failed commit; a conflict of two version transactions retries; the other order of section 4 gives the coarser history that it states; a class without an entry in mode `on` keeps the stock subscriber; the user is wrapped in the user folder of the new connection.
- The specification, in the section of `zoe.plone`, describes the behaviour, its limits and its log lines.
- Step 4 of the card measures B5 `contention-fields`, with the conflicts of the requests and the retries of the version transactions apart.

## Consequences

- Two edits of different fields of one versioned object commit at the first attempt where tier 2 merges the class. The acceptance test of the card can pass.
- Each edit of a versioned object costs one more commit, in the request.
- The history of an object can be coarser than the sequential one when edits overlap. A crash between the two commits loses one version.
- `zoe.plone` holds a copy of the decision of the stock subscriber. A new version of plone.app.versioningbehavior that changes that decision needs the same change in `zoe.plone`.
- The first version at a creation still saves in the transaction of the creation. Its conflicts are those of ZOE-219 (ADR 0048).

## Alternatives

- **A merge of the two saves in the core.** It would recompute the save of CMFEditions: renumber 14 records of 9 classes, edit the copy, edit a pickle inside a bytes value, recompute sizes that Python's pickler gives, and write a record that the transaction did not store. Shadow mode cannot verify it, because there is no Python reference to compare with (spike, section 4).
- **A strategy for `version_id`** (`max`, `sum`) or a coupled group of the Document with its history. The history refuses after the Document, and a coupled group spans one record only.
- **Version numbers from a sequence of the store (F29).** CMFEditions uses the number as a position, ZopeVersionControl computes its own numbers, and the copy still holds the old title (spike, section 5).
- **The version policy of CMFEditions.** `zoe.plone` could switch off `at_edit_autoversion` for the types and save the version itself. But the policy is site data, which a site without the profile of `zoe.plone` does not hold; the first version at a creation reads the same policy; and the stock subscriber still saves in the edit transaction when a change note is given.
- **A patch of `CopyModifyMergeRepositoryTool.save`.** It would defer every save, also revert and plone.app.iterate, which read the history right after the save in their transaction.
- **A durable queue and a worker,** as for queued indexing (ADR 0047). It closes the crash window, but the version is then not there when the request ends, and the acceptance test reads the history right after the commit.
- **The copy from the state at the commit of the edit.** The version transaction would clone the object from a connection at the tid of the edit, so the history would equal the sequential one in every order. `ArchivistTool.prepare` reads the object that it saves, and sets `version_id` on it, so this needs a change of CMFEditions.
- **Keep the conflict.** Zope retries the request, and the result is the sequential one. The acceptance test cannot pass, and the B5 criterion of the card goes back to the human.
- **Versions from the history of the store (F36).** It replaces the storage of CMFEditions on sites that opt in, in phase 7, after retention.

## Questions for the human

Each question gives Claude's recommendation first.

1. **No merge of two version saves at the vote** (section 1), and `version_id` stays `strict`. Recommended: yes.
2. **The version of an edit in a transaction of its own** (sections 2 and 3): `zoe.plone` replaces the edit subscriber of plone.app.versioningbehavior in the global registry. Recommended: yes. Other choices: keep the conflict (the acceptance test cannot pass, and the B5 criterion of the card goes back); or change the goal of the card (the B5 criterion on a type without versions, or wait for F36).
3. **The coarser history when edits overlap** (section 4). Recommended: accept it and state it in the specification. Other choice: clone from the state at the commit of the edit, which needs a change of CMFEditions.
4. **The crash window** (section 4). Recommended: accept it; the next edit makes a version with the change. Other choice: a durable record of the pending version in the durable namespace (ADR 0043), applied at the start of the process.
5. **When the mechanism is on** (section 2): when the merge policy of the database has an entry in mode `on` for the class of the object. Recommended: yes. Other choices: always when `zoe.plone` loads; or a backend of `ZOE_PLONE_BACKENDS`, which the recipe of the acceptance test does not set today.
6. **Only the edit subscriber** (section 5): the first version at a creation and the other callers of `portal_repository.save` keep their transaction. Recommended: yes.
7. **The retries of the version transaction** (section 3): at most 10, a line at level INFO for each retry, an ERROR at the end. Recommended: yes, as a constant of the module.
8. **The acceptance test** stays as the planner wrote it (section 6). It runs in the order where the history is exact; the build adds a test of the other order. Recommended: yes.
