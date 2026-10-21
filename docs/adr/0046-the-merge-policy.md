# 0046. The merge policy: a persistent object under the root, read at the vote, and the field merges of tier 2

Status: accepted. Sections 11 and 12 add the strategy `keys` and the option `state` of `append`.

## Context

Tier 2 of the specification merges the fields of a class that the merge policy marks mergeable, after tier 1 (ADR 0042) and before the client (tier 3). The specification asks for a policy that is a versioned object in the store, which the server reloads on change.

- The root object, oid 0, is a `PersistentMapping` whose state is `{'data': {name: value}}`. Zope uses only the name `'Application'`.
- A Dexterity item keeps its fields as attributes, the keys of its state dict. Each save changes `modification_date`, so a merge of two saves needs a strategy for that field.
- A Dexterity item holds instances of other classes, such as `DateTime`, which ZODB pickles with `NEWOBJ` or `REDUCE`, then `BUILD`.
- Tier 2 has no resolver in ZODB to match. Its tests compare it with a Python reference of each strategy.

## Decision

### 1. Where the policy is

The policy is the persistent object `root['zoe.merge_policy']`, of the class `zoe.policy.MergePolicy`. It needs no key space, no `meta` key and no frame field: it is a record with its history, its undo and its place in a pack. A store without the name has no policy. Its keys and the functions of `zoe.policy`: specification, "Storage core", the paragraph "The merge policy is a versioned object in the store". `zoe.policy.install` adds 1 to `version`, and the caller commits.

### 2. How the core reads it, and when it reads it again

The core reads the policy only at a vote with a conflict or an undo that merges, and loads it again when the serial of oid 0 or of the policy changed. So a committed change applies from the next conflict, in both modes. The vote does not lock the policy: a vote during the commit of a new policy uses one of the two versions, and the resolution log names it. A record that does not decode, or a policy that breaks the rules of section 1 (an unknown key of an entry among them), counts as no policy, with one warning for each serial. `zoe.policy.install` refuses such a policy first.

### 3. The policy and the settings of ADR 0042

The keys `structural-merges`, `structural-merge-class` and `posting-dicts` stay, as the defaults. A switch that the policy gives wins over its key (specification, "Storage core", "Conflict resolution dispatch"). A shadow mode from the policy without `resolution-log` does not fail: the store writes one warning for that version of the policy.

### 4. Tier 2: the field merge

Tier 2 runs at the vote, after tier 1, on a stale written oid whose class is not a class of tier 1 and whose entry has `tier2` `on` or `shadow`. Any other class never merges in tier 2. The details: specification, "Storage core", "Conflict resolution dispatch", item 3.

1. `zoe_pickle::python` decodes the three states, old, committed and new, with the instances of other classes. Each must be a dict with `str` keys. A mutable object that two fields share gives no answer: a merge would split it in two (section 10, one walk for sharing).
2. A field changed on one side when its value does not have the structure of the old value, or when that side added or deleted it. The merge errs toward a conflict.
3. A field that only one side changed takes that change. A field that both sides changed to equal values takes it.
4. A field that both sides changed to different values takes the strategy of its entry in `fields`, else `default`: `strict` refuses, `lww` takes the new value, `max` the greater (no answer for values that it cannot order), `sum` adds the two changes of an int, `union` the old items that neither side removed, then the items of the committed side, then those of the new side, each once, and `append` the new items after the committed value when the old value is a prefix of both, else it refuses. In an undo, `lww` refuses as `strict` does, so the undo never drops a later change without a word. A delete against a change refuses.
5. When both sides changed fields of one group of `coupled`, the merge refuses, unless they agree on every field of the group.
6. The merged state is the committed dict with the merged values, then the fields that only the new side added. The record is the class pickle of the new record, then that state, stored over the committed serial as a merge of tier 1.

A refusal fails the vote with `ConflictError` at once. No answer leaves the conflict to tier 3. No Python runs in the core or on the server.

### 5. F30: the merge primitives as strategies

F30 asks for append-only fields, CRDT sets and registers: the strategies `append`, `union` and `lww`, with no change of the protocol or of the format. A stored primitive `append(oid, field, record)` comes only if B5 shows that a rewrite of the history costs too much. Counters are the sequences of F29.

### 6. Shadow mode, and a shadow mode that verifies

In shadow mode (`tier2: shadow`), tier 2 writes its result to the resolution log, and the conflict goes on as without it. The second vote compares the record of the client with the merge of shadow mode, of tier 1 or tier 2 (specification, "Storage core", the paragraph "The merge policy is a versioned object in the store"). A conflict that the client does not resolve writes no second line: there is nothing to compare.

### 7. The resolution log

A line of tier 2 has `"tier": 2`, the class, the outcome, the reason, the changed fields in `keys`, and the `version` of the policy in `policy`. A line of tier 1 also has `policy`, `null` without a policy.

### 8. Both modes

Tier 2 and the policy are in `zoe-core`, so `zoe.Storage` and `zoe serve` have the same merges and log. A stock `ZEO.ClientStorage` sees a resolved oid, as from a ZEO 6 server.

### 9. The classes of `zoe.plone`

The classes, the profile and the derivation of the policy: specification, "Storage core", "The classes and the profile of `zoe.plone`".

- **`zoe.plone.history.WorkflowHistory`** holds `workflow_history`, a `Persistent` `MutableMapping` of workflow id to statuses, so each workflow is a field. Entry (`zoe.plone.history.policy()`): `{"tier2": "on", "default": "append", "options": {"append": {"state": "review_state"}}}` (section 12). The opt-in behaviour `zoe.plone.history` gives it to an object at its next status (section 10).
- **`zoe.plone.properties.PrincipalProperties`** holds the properties of one member or group of a mutable property sheet of PAS, as a `Persistent` `MutableMapping`. Both classes intern their keys in `__setstate__`. Entry (`zoe.plone.properties.policy()`): `{"tier2": "on", "default": "strict", "fields": {"login_time": "max", "last_login_time": "max"}}`. `update_policy` adds it to the policy of each site.
  - On from the install, an exception to shadow first (a decision of the human). The evidence: `zoe/plone/tests/test_concurrent_logins.py`, and B6 (specification, "ConflictError strategy", "Lever 4: structural, in Plone").
  - The import step `zoe.plone.properties` runs after `zoe.plone.merge_policy`, commits after each batch of 100 values, and marks the sheet with `_zoe_principal_properties`. A principal created during the import can keep a dict until its next write. `zoe.plone.properties.install`, a ZCML action, patches `setPropertiesForUser` and `getPropertiesForUser` of a marked sheet. Products.PlonePAS reads `_storage` only in the sheet, with methods that a mapping supports.
  - The uninstall retires each record with the one item `{0: None}`, which tier 2 does not take, so a login from the same snapshot conflicts with it. The entry stays: another site of the database can use the class.
  - A site imports `zoe.plone:uninstall` and packs with GC before it leaves `zoedb` (a decision of the human). The guard of `zoe convert --to-data-fs`: specification, "Loupe: desktop inspector and editor", "Converters, `zoe convert` and a Loupe screen".
- **Audit entries** get no class: Plone 6.1 keeps no audit log in the ZODB.
- **The derived entries** (`zoe.plone.merges`) are in shadow mode. The import step `zoe.plone.merge_policy` runs for each profile with a `zoe-merge-policy.json`, after `typeinfo`.
- **The stock container classes**, `plone.app.contenttypes.content.Folder` and `Products.CMFPlone.Portal.PloneSite`, are on from the install, with `folder_orders` and `split_merges`: another exception to shadow first (a decision of the human). The evidence: B5 `create-shared` in one folder, 0 and 1 `ConflictError`s at 4 and 16 users, against 106 and 42 on ZEO (ADR 0052, section 14). These classes also merge two edits of different fields; the evidence covers creations only.
  - The sites of a database compose the two switches and the `tier2` of the two classes, four modes, to the lowest (ADR 0052, section 6). A site without an entry for a class asks for no mode. A rise of a class logs at INFO when the database had no root for it and its stored `tier2` is the derived `shadow`, else a warning, as `tier2 of <class>`. No line reports a rise of a blocked class.
  - A later profile of the site lowers these classes with its key `tier2` or with their entries. The overrides of the profiles apply in the order of their first import.
  - The uninstall sets the four modes `off` for its site (a decision of the human). Its overrides replace the kept overrides of `zoe.plone:default`, and the reverse holds (`zoe.plone.merges.keep_overrides`).

### 10. The canonical ids, the budget, and the rules of `zoe.plone`

The core:

- **No recursion, a budget.** Values compare by their canonical ids (`zoe_pickle::python::Canon`), built once for each object with an explicit stack. A set compares without order. The budget, 64 steps for each byte of the three records, holds for the comparisons of tier 1 too. Past it, the merge gives no answer. The loader loads a large record in linear time.
- **One walk for sharing.** One walk of the state finds an object that two fields share. It stops the merge only when the object is mutable or holds a mutable object.

`zoe.plone`:

- **The status adapter.** `zoe.plone.status` moves the history before it adds a status, so a history that an import puts back moves at the next transition. An object without a workflow gets no history.
- **The moves.** A move marks the stock history as changed, so a move and a transition from one snapshot conflict. The import step `zoe.plone.history` moves the histories at each import, as the profile has one version, in batches of 100 objects, one transaction each.
- **The import of the policy.** The site keeps the overrides of each profile under its profile id. The policy composes with the policy in the database: the groups and fields of a class join, a blocked class stays blocked, and the other keys take the new value, but for section 9.
- **The way back loses no status.** `restore` retires the `WorkflowHistory` with the one item `{0: ()}`, which tier 2 does not take. So a transaction that writes the old history from the same snapshot conflicts with the restore.
- **A blocked class stays blocked.** No override, of one profile or several, can switch it on, unblock it or remove it. The import logs a warning.

### 11. The strategy `keys`: a mapping merges key by key

ZODB gives `PersistentMapping` no `_p_resolveConflict`. Its state has one field, `data`, so `strict` refuses two changes of different keys. In the B5 log, `PersistentMapping` gives 6 of the 64 `ConflictError`s left after tier 1.

1. **The strategy.** `keys` is a strategy of step 4 of section 4. It merges three dicts key by key with the rules of steps 2 and 3. A key that both sides changed differently refuses with `Key of a mapping changed on both sides`, and a delete against a change with `Key of a mapping deleted on one side and changed on the other`.
2. **No answer.** A value that is not a dict, a key of another kind (point 3), one mutable object under two keys (step 1 of section 4), or a merge past its budget gives no answer.
3. **The keys.** A `str`, an `int`, a `bytes`, or a tuple of those. A `bool` or a `float` can equal an `int`, and an object can define `__eq__`, so other keys give no answer.
4. **The merged dict.** The committed keys in their order, then the keys that only the new side added. Each key and value is the object of the side whose change it takes. The record pickles as in step 6 of section 4.
5. **Which fields.** Any field whose entry names `keys`, so the core needs no list of mapping classes.
6. **The resolution log.** `keys` lists the changed keys of the dict in place of the field, written as tier 1 writes a key, so a key never looks like a field name. The ints come first, then the strs, the bytes and the tuples.
7. **How a policy marks a mapping class.** An entry gives `data` the strategy `keys`: `{"classes": {"mysite.registry.Registry": {"tier2": "on", "fields": {"data": "keys"}}}}`. A subclass does not take the entry of its base, so a site names a subclass of its own for the mappings whose keys are independent.
8. **Why a site should not mark the stock classes.** An entry for `persistent.mapping.PersistentMapping` or `Persistence.mapping.PersistentMapping` merges every object of the class, the WebDAV locks of `OFS.Lockable` among them: two transactions that lock one object would both commit. So `zoe.plone` marks no stock class. `zoe.policy.validate` does not refuse such an entry; its documentation warns against it.
9. **Shadow mode.** A `PersistentMapping` has no `_p_resolveConflict`, so no line `verified` follows a merge of `keys`.
10. **Undo.** `keys` is `strict` for each key in an undo: a key that a later transaction changed refuses with `UndoError`.
11. **The code.** `merge::policy::Strategy::Keys` reads `"keys"`, and `zoe.policy.STRATEGIES` holds it. Only the model of tier 2 (`Heap::with_instances`) loads tuple keys.

### 12. The option `state` of `append`: a history that refuses two changes of state from one state

A DCWorkflow status is a dict that holds the new state under the state variable of the workflow (`review_state` in Plone). DCWorkflow treats two transitions from one state as exclusive. `append` alone merges them: both commit, and a stale comment that lands after a change of state puts the old state back.

1. **The rule.** With the option `state`, `append` merges two appends only when every status that the committed side appended keeps the state of the last old status. Else it refuses with `ConflictError`, and Zope retries from the state that committed. The new side can change the state.
2. **The cases.** A committed comment, then a new change of state, merges. A committed change of state refuses against any new status, also when a later committed status changes the state back.
3. **The option.** The entry key `options` maps a strategy to its options. Only `append` takes one, `state`, a non-empty `str`. It applies to each field of strategy `append` of the entry. A key of `options` that the core does not know makes the policy unreadable, as an unknown key of an entry does (section 2): a server that cannot read an option must not merge without it.
4. **An option of `append`, not a named strategy.** The core learns the name of a key, not the shape of a DCWorkflow history.
5. **No answer.** A status without the key `state`, or an old value without a status, gives no answer: the store does not know the initial state. The checks of `append` run first, then the last old status, then the committed statuses in order. A status of the new side is not checked.
6. **No update is lost.** When the committed statuses keep the state, the merge gives the history of the serial order.
7. **A chain of two workflows.** Each workflow is a field and merges alone. One refusal refuses the record.
8. **Undo.** The undo of a status that a later transaction followed refuses with `UndoError` on the prefix check of `append`.
9. **A site on ZEO.** A ZEO server runs no tier 2: two transitions from one snapshot conflict there.
10. **The resolution log.** A refusal gives the reason `State changed by a status of the committed side`. A case of point 5 gives `undecided`, with `a status without the state key` or `an old value without a status`. The workflow id is in `fields`.
11. **A site that has the old entry.** It gets the option at its next import of `zoe.plone:default`.
12. **The tests.** `python/zoedb/tests/acceptance/test_workflow_states.py`, in-process and through `zoe serve`.

## Consequences

- A wrong policy can merge two changes that the application meant to exclude each other. So a class is strict until its entry says otherwise, and shadow mode comes first.
- Shadow mode verifies a merge only when the client resolves the conflict, never on a Dexterity class.
- A site that marks a mapping class with `keys` says that its keys are independent. An entry cannot couple two keys.
- The option `state` errs toward a conflict.
- A core of an older build reads a policy with `options` as no policy. A site updates its servers first.

## Alternatives

- **A key space or a `meta` key.** A format change, and no history, undo or transaction.
- **A policy file, or a server setting.** It differs between modes and does not commit with the profile.
- **Watch the commits for the oid of the policy.** A check on every commit.
- **The keys of a nested mapping as fields.** `keys` merges the dict of one field instead.
- **`keys` only for the `data` of the mapping classes.** The core would need a list of classes.
- **A strategy for each key.** A key space under each field.
- **A refusal of the stock mapping classes in `zoe.policy.validate`.** It would stop a site that knows its mappings.
- **A named native merge per class.** The strategies cover the classes that need one.
- **Verify in the client.** A change of the protocol.
- **Refuse when both sides appended a status of another state.** A stale comment would lose a change of state.
- **Rewrite the stale status.** No transaction wrote it.
- **Read only the last committed status.** It merges a change of state and a change back.
- **A named strategy, a flat key `state`, or a strategy as a dict.** A second name for `append`, a key that names no strategy, or a new type for every strategy value.
- **Read the state variable from the workflow.** The core runs no Python.
- **A refusal for an old value without a status.** Both end in `ConflictError`.
- **A core that skips an unknown key of `options`.** It would merge without the option.
