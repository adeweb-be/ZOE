# 0052. The order of a folder of plone.folder merges at the vote

Status: accepted. It adds the order merge to the vote, under the switch `folder_orders` of the merge policy, off by default in the core. It changes no on-disk format, no pack and no wire protocol.

## Context

ADR 0048 and ADR 0049 merge the splits of BTrees, but two creations in one folder still conflict. The spike ([folder-order.md](../spikes/folder-order.md)) finds:

- A creation writes six records of the folder. `_tree`, `_count` and `_mt_index[meta_type]` merge with tier 1 and the split merge. On the `Folder` the creation changes only `modification_date`. The order list is a `PersistentList`, which ZODB does not resolve. The positions `pos` are an `OIBTree`.
- `notifyAdded` gives the new id the position `len(order) - 1`, so two creations from one snapshot give their ids one position. In rounds of eight creations from one snapshot, a merge of the order list alone commits all 80, but `pos` is wrong for 70 of 81 ids. A later `notifyRemoved` then removes the wrong id.
- In each commit, the id of the transaction is in a record of `pos` that the commit writes.

A tier 2 entry for a stock class merges every object of that class (ADR 0046, section 11, point 8).

## Decision

### 1. Words

- The **folder** F holds `__annotations__`, a reference to its **annotations** A, an `OOBTree`.
- The **order** L is the `persistent.list.PersistentList` under `plone.folder.ordered.order` of A, with the state `{'data': [id, ...]}`.
- The **positions** P are the `OIBTree` under `plone.folder.ordered.pos` of A. plone.folder keeps `pos == {id: index}` of the order.
- o, c and n are the old, committed and new states of L (ADR 0048). N is `n[len(o):]` and C is `c[len(o):]`.

### 2. The order merge

The rule runs in the vote that takes the locks, at its first conflict, with the plan of the split merge. The steps as built: specification, "Storage core", "Conflict resolution dispatch", item 2.

1. **The order.** Each stale written L whose old, committed and new records have the class `persistent.list.PersistentList`.
2. **The folder.** A record that the transaction stored, not of a class of tier 1, whose `__annotations__` names A. The current A is an `OOBTree` with its bucket inline that names L and P. A staged A names the same L and P. Each record of P is an `OIBTree` or an `OIBucket`. Else no answer.
3. **The list.** Each state is `{'data': [str, ...]}`, o is a prefix of c and of n, and N is not empty. The merged state is `{'data': c + N}`, with the class pickle of the new record.
4. **The positions that the transaction wrote.** S holds the buckets of P that the transaction stored, its new buckets among them, and the root of P when its old or new state holds its bucket inline. **The union of the new states of S equals the union of the old states of S plus exactly `{N[i]: len(o) + i}`, which the old union does not hold.** The node check: each stored node of P keeps each child of its old state that the transaction did not store. The rule reads only the nodes of P that the transaction stored or marked read-current. So a rename on create rebuilds `pos` and gives no answer in a folder of more than 60 items. With at most 60 items, P holds its bucket inline, and it merges.
5. **The renumbering.** After the check of every serial, each N[i] gets the value `len(c) + i` in the records of P that the vote stages: the merges of tier 1 and of the split merge, the bucket Y of rule 3 of ADR 0049, and each record of P that the transaction stored and that is not stale. Each id of N is in exactly one of them.
6. **What the vote stages.** L, a stale record of P and Y go over their committed serial. A record of P that is not stale goes over the serial that the transaction read. A record over a size limit gives no answer. The reply lists each of them as resolved.
7. **The self-check.** Before it stages, the rule reads its records back: each N[i] is in one record with `len(c) + i`, each other item is the item of tier 1 or of the split merge, and the list holds each id once. Else no answer. In mode `shadow` the rule runs the renumbering and this check on the stored records of P (section 9).
8. **Before tier 2.** Tier 2 never merges an order that step 2 found: an order merged alone corrupts `pos`. `zoe.policy.validate` refuses a `tier2` other than `off` for `persistent.list.PersistentList` while `folder_orders` is not `off`.

The rule writes no record beyond those that the transaction stored and Y. It takes no new lock and never writes A or F. A refusal fails the vote with `ConflictError` on L. No answer leaves the conflict on L as without the rule. A record of P that the vote stages again goes through `staged.put`, which drops its encoded payload.

### 3. Which changes merge

Only appends on both sides merge. An id that both sides appended refuses. These give no answer: a delete, a move, a sort or a rename on either side; a rename on create on the committed side in a folder of more than 60 items (`ReadConflict` on the root of P); an empty N, an item that is not a str, an id twice in N or in C, an id of N or C in o; a change of P that fails section 2, step 4; a record of P merged onto a commit in flight (ADR 0044). The rule does not take the first order of a folder or the first item of a meta type in a folder: tier 1 refuses both, and each happens once.

### 4. The order of the merged items

The merged order is c, then N in the order of the transaction. Each id of the committed side keeps its position.

### 5. The record of the folder

The rule does not merge F. A creation changes only `modification_date` of F, and tier 2 merges it with an entry of the class of the folder (ADR 0046):

```python
{"plone.app.contenttypes.content.Folder": {"tier2": "on", "default": "strict", "fields": {"modification_date": "max"}}}
```

The profile of `zoe.plone` switches on the entries of `plone.app.contenttypes.content.Folder` and `Products.CMFPlone.Portal.PloneSite` (ADR 0046, section 9). A site switches on the entry of an add-on container class itself. The rule reads the annotations, not the class, so it covers every folder whose ordering is `DefaultOrdering`. Another ordering keeps the stock behaviour.

### 6. The switch

- `folder_orders` takes `off` (the default of the core), `shadow` or `on`. `zoe.policy.SWITCHES` holds it. It has no key of `<zoe>`, `zoe.Storage` or `zoe serve`: the policy is a record of the store.
- `zoe-merge-policy.json` of `zoe.plone:default` sets it `on`, with `split_merges` and the two entries of section 5. The overrides of a site can set it `off`.
- The rule changes a record of P only where the mode of tier 1 of its class is not `off`, in the lower of the two modes. The vote finds the orders whenever the switch is not `off`.
- The database keeps the latest mode that each site asked for, in `root['zoe.plone.folder_orders']`, `root['zoe.plone.split_merges']` and `root['zoe.plone.tier2:<class>']`. The policy takes the lowest (`zoe.plone.merges.lowest_mode`). `zoe.plone:uninstall` asks `off` for all four. The warnings and the gone sites: specification, "Storage core", "Conflict resolution dispatch", item 2.
- An update logs nothing when the policy holds no mode of the switch. In a BTreeFolder2, `has_key` of the tree confirms a missing item. A `ConflictError` of the walk goes to the caller. A database mounted in a second Zope instance loses, at an update in the first, the modes that the second wrote below its mount point: a site of the first instance then gives the lower mode in its overrides. `del root['zoe.plone.folder_orders'][path]` clears a mode.
- The switch does not need `split_merges`, but a folder whose `pos` or `_tree` splits does.
- In mode `shadow` the vote goes on as without the rule. No client resolution verifies it (ADR 0046, section 6): the self-check of section 2, step 7 is the only check.
- The companions are queued indexing (ADR 0047) and `split_merges`.
- An older core reads a policy with `folder_orders` as no policy, with a warning (ADR 0046, section 12, point 3). A site updates its servers before it writes the switch.

### 7. Why no update is lost

- `c + N` is the order that the transaction gives if it runs after the committed side.
- Tier 1 and the split merge give the committed positions plus the inserts of the transaction (ADR 0048, section A3; ADR 0048, section B6). The renumbering makes them `{id: index in c + N}`. It changes only values of ids of N, which only the transaction inserted.
- One vote stages all these records, or none. L is not a class of tier 1, so its lock is never handed off (ADR 0044).
- The argument assumes that writes go through `DefaultOrdering`, and that no other record holds a value derived from a new position. The catalog of Plone 6.1 holds none: `getObjPositionInParent` is a `GopipIndex`, which stores no value.

### 8. A site on ZEO, a stock client, a client with `server_writes`

- ZEO, a FileStorage or a store without the switch keep the stock behaviour: each creation but the first from one snapshot raises `ConflictError` and retries.
- No client declares anything.
- `server_writes` (ADR 0049) matters only to rule 3. Without it, creations conflict where both sides split one bucket: about once in 15 creations for `_tree`.

### 9. The resolution log and the counters

The fields `order` and `shadow`: specification, "Storage core", "Conflict resolution dispatch".

- L gives a line of tier 1: `merged`, `refused` ("an id that both sides added", `Reason::OrderSameId`) or `undecided`; `keys` the ids of N; `"order":"list"`.
- A record of P that is not stale gets its own line: `merged`, `read` equal to `committed`, `"order":"pos"`.
- In mode `shadow`, L is `undecided` ("an order in shadow mode"), and `shadow` is `merged` or `undecided` ("a self-check that failed").
- A stale `PersistentList` without a folder gives no line.
- `zoe_structural_merges_total` counts these lines with `tier="1"`, with no new label.

### 10. A copy, a pack and an undo

A copy keeps the records and the policy as they are. A pack needs no change. An undo runs no rule: after a later commit of L it fails with `UndoError`, as in ZODB.

### 11. What the build changes, and where (the scope fence)

`merge::order`, `commit::order`, `merge::policy` and `zoe.policy`. No change of `format/`, `pack/`, `lock/` or the wire protocol.

### 12. The helpers of the acceptance test

The switch keeps the name of the test, `folder_orders`. Both helpers install the entry of section 5 and `"split_merges": "on"` with `zoe.policy.install`, commit, and return `{}`: no keyword argument of `zoe.Storage`. Only `mechanism_on` also writes `folder_orders` `on`, when `zoe.policy.SWITCHES` holds it. So without the switch the test fails on the order list. The test reads `_mt_index["Dexterity Item"]`, the `meta_type` of a Dexterity item.

### 13. The tests

Specification, "Performance and testing harness", "A. Conformance (correctness against ZODB's own definition)", the row of the order merge. The acceptance test is `zoe/plone/tests/test_creations_in_one_folder.py`.

### 14. The measure

B5 `create-shared` (one folder) and `create-spread`, `--steps 4:60,16:60`, one repetition, on `zeo` and on `zoe-native-qi` with `--split-merges on --folder-orders on`. The option `--folder-orders MODE` writes the switch and the entry of section 5, and `result.json` records the policy.

- `create-shared`: 106 and 42 `ConflictError`s on `zeo` at 4 and 16 users (779.4 and 368.4 per 1,000 operations); 0 and 1 on `zoe-native-qi` (0.0 and 8.8). The one is a read conflict on an `OIBTree` that the split merge does not take. The order merge merged 152 orders.
- `create-spread`: 107 and 93 on `zeo`, 0 and 0 on `zoe-native-qi`.

## Consequences

- With the profile of `zoe.plone`, concurrent creations in one folder of a stock site commit at the first attempt, and the order and `pos` agree.
- The core knows the annotation keys of plone.folder. A change of them gives no answer, never a wrong merge.
- A vote can stage again a record that its transaction stored and that is not stale.
- A `PersistentList` merges only as an order. Tier 2 never merges an order.
- Creations in one folder commit one for each sync: the locks of the `Folder` and its order are never handed off.
- Deletes, moves, sorts and renames still conflict, as with ZODB.

## Alternatives

- **A merge of the order list alone**: it marks a stock class, and `pos` stays wrong.
- **A rebuild of `pos` from the merged order**: it writes buckets that the transaction did not store.
- **An ordering of `zoe.plone`**: it changes the data of each folder and needs a migration.
- **Positions that need no renumbering**: they change the meaning of `pos`.
- **The pair recognised by its content**: `_mt_index[meta_type]` has the same shape.
- **The rule merges `modification_date` of F**: it duplicates tier 2.
- **The switch on by default in the core, or off in the profile**: the core opts in where ZODB refuses; a stock site gets the merge from the profile alone.
- **The switch implies `split_merges`**: the rule does not need it.
- **A key of `<zoe>` for the switch**: the policy is a record of the store.
- **`mechanism_off` without the entry of the `Folder`**: the test then fails on the `Folder`.
- **The first order of a folder and the first item of a meta type**: each happens once.
- **A delete against an append**: it needs a rebuild of `pos`.
- **No answer for a record of `pos` that is not stale**: creations in different buckets then conflict.
- **An amendment of ADR 0048**: the rule changes no rule of ADR 0048 or ADR 0049.
- **A policy file or a new stack name for the measure**: one option for each switch keeps the stack.
- **Only the guard of the core, or only `validate`**: one misses a dead entry, the other a policy written by other means.
- **No answer when the transaction wrote to the catalog**: the rule would need to know the catalog.
