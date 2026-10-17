# 0046. The merge policy: a persistent object under the root, read at the vote, and the field merges of tier 2

Status: accepted. The first checkpoint of ZOE-205. The human asked Claude to settle the questions that had no proposal and to mark each answer as its own choice, then approved the ADR as written, with the answers of "Questions for the human". During the build the human approved two changes, which the text below holds: the digest of a merge of shadow mode is BLAKE3, the hash that the core already links, in place of sha256; and the strs and bytes that CPython shares in every record (the empty ones, those of one character below U+0100, and the interned keys of dict states) do not count as an object that two fields share. The second checkpoint of the card, the classes of `zoe.plone` (section 9), is approved too.

## Context

Section 5 of the specification names three tiers. Tier 1 (ZOE-203, ADR 0042) merges the buckets, the sets, the one-bucket trees and the `Length`s of BTrees in Rust, at the vote. Tier 3 sends the conflict to the client, which runs the `_p_resolveConflict` of the class. Tier 2 is between them: "for any persistent object whose class the merge policy marks mergeable, decode the old, committed and new states, compute the two changed-key sets, merge when they are disjoint or agree, and apply the per-field strategies the policy names". The specification also says: "The merge policy is a versioned object in the store, so it is transactional and the server reloads it on change."

The card asks the ADR to say:

- where the server finds the policy without a new key space;
- how the server loads it again when it changes;
- what it holds: the mergeable classes, the coupled fields, the strategy of each field, and the switches of shadow mode and of the posting-dict rule of ZOE-203.

Facts that the design rests on:

- A ZODB database has one root object, oid 0, a `persistent.mapping.PersistentMapping` whose state is `{'data': {name: value}}`. Zope keeps its application under `'Application'`; other names are free. The root record is an ordinary record: the core can load it and `zoe_pickle` can decode it.
- The state of an ordinary persistent object is the dict of its attributes, from `Persistent.__getstate__`. A Dexterity item keeps its fields as attributes. So the "keys" of tier 2 are the keys of that dict: the fields.
- Each save of a Dexterity item changes `modification_date`, so two saves of one item always change at least one field on both sides. A merge of two saves needs a strategy for that field.
- `zoe_pickle::python` models the values of BTrees: `None`, bools, ints, floats, `str`, `bytes`, tuples, lists, dicts, classes and persistent references. A Dexterity item also holds instances of other classes, such as `DateTime` and `RichTextValue`, which ZODB pickles with `NEWOBJ` or `REDUCE`, then `BUILD`.
- Tier 1 gives the bytes of ZODB's resolver. Tier 2 has no resolver in ZODB to match. The acceptance test compares tier 2 with a Python reference of each strategy, which pickles its result with ZODB's pickler.
- Point 5 of the review of #418: a shadow mode that verifies. In shadow mode the client resolves the conflict and stores its record again, so the server sees the answer of ZODB and can compare it with its own merge.

## Decision

### 1. Where the policy is

The policy is the persistent object `root['zoe.merge_policy']`, of the class `zoe.policy.MergePolicy`, a `Persistent` subclass that `zoedb` ships. It needs no key space, no `meta` key and no frame field: it is a record like any other, with its history, its undo and its place in a pack. A store without the name has no policy.

Its state holds plain values only (`str`, ints, lists, tuples, dicts), so that the core reads it with `zoe_pickle::python` and a client loads it without `zoe.plone`:

```python
{
    "version": 3,                      # an int; each write adds 1
    "structural_merges": "on",         # optional: the mode of tier 1
    "posting_dicts": "on",             # optional: the rules of ADR 0042
    "classes": {
        "BTrees.IOBTree.IOBucket": {"tier1": "shadow"},
        "plone.app.contenttypes.content.Document": {
            "tier2": "on",              # on, shadow or off
            "default": "strict",       # the strategy of a field that "fields" does not name
            "fields": {"modification_date": "max", "subject": "union"},
            "coupled": [["start", "end"]],
        },
    },
}
```

- `zoe.policy.install(connection, policy)` validates a dict of this form, writes it to the object (it creates the object and the name of the root the first time), and adds 1 to `version`. The caller commits, so a policy commits or aborts with its transaction. `zoe.policy.read(connection)` gives the dict, or `None`.
- `zoe.plone` writes the policy with its profile (step 4 of the card); a site without `zoe.plone` writes it with `zoe.policy.install`.

### 2. How the core reads it, and when it reads it again

- The core reads the policy only on the conflict path: in the vote that takes the locks, when at least one written oid is stale. A commit with no conflict reads nothing new (an invariant of the card).
- At such a vote, the store reads the current serials of oid 0 and of the oid of the policy (one backend read). When both are the serials of the policy that it holds, it uses that policy. Else it loads the root, finds the reference under `'zoe.merge_policy'`, loads that record, decodes it and keeps it, with both serials. So a committed change of the policy applies from the next vote with a conflict, in both modes and on every connection.
- The vote does not lock the policy. A vote that runs while another transaction commits a new policy uses one of the two versions; the resolution log names it.
- A record that does not decode, or a policy that breaks the rules of section 1, counts as no policy, and the store writes one warning to its log for each serial of it. `zoe.policy.install` refuses such a policy in the client first.

### 3. The policy and the settings of ZOE-203

The keys `structural-merges`, `structural-merge-class` and `posting-dicts` of `<zoe>` and of `zoe serve` stay. They are the defaults: a switch that the policy gives wins over its key, and a switch that it does not give keeps the value of the key. The tier 1 mode of a class is the `tier1` of its entry, else `structural-merge-class`, else `structural_merges` of the policy, else `structural-merges`. So a site that writes no policy works as with ZOE-203 alone. A shadow mode from the policy without a `resolution-log` changes nothing at the open; the store writes one warning to its log for that version of the policy, because the policy can change while the store is open.

### 4. Tier 2: the field merge

Tier 2 runs at the vote, after tier 1, on a stale written oid whose class is not a class of tier 1 and whose entry has `tier2` `on` or `shadow`. A class without an entry, or with `tier2` `off`, never merges in tier 2 (strict by default). The class comes from the class pickle of the staged record, as in tier 1.

1. The three states (old, committed, new) are decoded with `zoe_pickle::python`, which also models an instance of another class as its class, its arguments and its state (`NEWOBJ` or `REDUCE`, then `BUILD`), so that a `DateTime` compares and pickles again. Each state must be a dict with `str` keys; a state with slots, a value that the model does not hold, or an object that two fields share in one state (the same memo object under two keys) gives no answer. The strs and bytes that CPython shares in every record (the empty ones, those of one character below U+0100, and the interned keys of dict states, which `load_build` and `Persistent.__setstate__` intern) are not such an object: they are the same object whatever record they come from (approved by the human). The model interns the keys of dict states as CPython does, so a merge writes a key once, as a client that loads the record does. In the build, only a mutable object counts (a list, a dict, a set, or an object of another class but `DateTime.DateTime`): a Plone Document shares strs between its fields (the user id in `creators`, `_owner` and its local roles) and, until its first change, one `DateTime` between `creation_date` and `modification_date`, and splitting an immutable value changes neither the meaning nor the record that a client writes. This wider rule is Claude's choice, for the human's review.
2. A field changed on one side when its value there is not equal to its value in the old state, with the equality of the model: the same kind, the same items in the same order, the same class and state for an instance, the same oid for a reference. A field that is in one state and not in the other is changed too: added or deleted. Two values that Python finds equal but that the model does not (two dicts in another order, for example) count as changed, so the merge errs toward a conflict, never toward a lost change.
3. A field that only one side changed takes that change. A field that both sides changed to equal values takes it.
4. A field that both sides changed to different values takes the strategy of its entry in `fields`, else `default`:
   - `strict`: refuses.
   - `lww`: the value of the new record: the transaction that votes now is the last writer.
   - `max`: the greater of the two values, with the order of `Heap::compare` of tier 1; values that it cannot order give no answer. For `modification_date`, two `DateTime`s order by their time.
   - `sum`: committed + new − old, for ints only.
   - `union`: for lists, tuples, sets and frozensets of values that the model orders: the items of the old value that neither side removed, then the items that the committed side added, then the items that the new side added, each item once, in the kind of the new value. A set keeps the order of the model.
   - `append`: for lists and tuples: the old value must be a prefix of both; the result is the committed value, then the items that the new side added after the old prefix. Else it refuses.
   A delete of a field against a change on the other side refuses, whatever the strategy.
5. Coupled fields stay strict: when the committed side changed a field of a group of `coupled` and the new side changed a field of the same group, the merge refuses, unless the two sides gave every field of the group the same value.
6. The merged state is the dict of the committed state, with the merged values in place and the fields that only the new side added after them, in the order of the new state. The record is the class pickle of the new record, then that state, pickled as `zoe_pickle` pickles for tier 1. It is stored over the committed serial under the locks of the vote, as a merge of tier 1, and the reply of the vote gives the oid as resolved, as in tier 1.

A refusal of tier 2 fails the vote with `ConflictError` at once. No answer leaves the conflict to tier 3: the client resolves it if the class has a `_p_resolveConflict`, else raises `ConflictError`, as today.

The merge is linear in the size of the three states. No Python runs in the core or on the server.

### 5. F30: the merge primitives as strategies

F30 asks for append-only fields, CRDT sets and registers. They are the strategies `append`, `union` and `lww` of tier 2, with no change of the wire protocol and no change of the format. The stored primitive `append(oid, field, record)`, materialised on load, comes only if B5 shows that rewriting the history costs too much (the human's answer before Ready). Counters are the sequences of F29.

### 6. Shadow mode, and a shadow mode that verifies

- In shadow mode (`tier2: shadow`), tier 2 merges, writes its result to the resolution log, and the conflict goes on as without it.
- When a merge of tier 1 or tier 2 in shadow mode gives a record, the transaction keeps a digest of that record for the oid (BLAKE3, the hash that the core already links). When the client then stores its own resolution of the oid and votes again (the vote after `vote_resolving`), the store compares the digest of the record of the client with it, and writes a second line to the resolution log for the oid, with the outcome `verified` and the field `same`: `true` or `false`. So shadow mode proves, on the conflicts of a real site, that a class of tier 1 gives the record of ZODB's resolver, before a site switches it on. A conflict that the client does not resolve (a class without `_p_resolveConflict`, or the posting-dict rule, which ZODB refuses) writes no second line: there is nothing to compare.
- `zoe serve` counts `zoe_shadow_verifications_total{tier, same}`.

### 7. The resolution log

A line of tier 2 has `"tier": 2`, the class, the outcome, the reason (the strategy that refused, the coupled group, or why there is no answer), the fields that the new record changed in `keys`, and the `version` of the policy in the new field `policy`. A line of tier 1 also has `policy` (or `null` without a policy). Section 5 of the specification lists the fields.

### 8. Both modes

Tier 2 and the policy are in `zoe-core`, so `zoe.Storage` and `zoe serve` have the same merges, the same switches and the same log. A stock `ZEO.ClientStorage` sees a vote that resolved an oid, as from a ZEO 6 server.

### 9. The classes of `zoe.plone` (the second checkpoint)

- `zoe.plone.history.WorkflowHistory` holds `workflow_history`: a `Persistent` mapping of workflow id to the tuple of its status dicts, whose state is the mapping itself, so each workflow is a field. Its `__setstate__` interns the keys. Its entry is `{"tier2": "on", "default": "append"}` (`zoe.plone.history.policy()`). The opt-in Dexterity behaviour `zoe.plone.history` gives a new object of a type with the behaviour a `WorkflowHistory` when it is created, before its first status; an upgrade step moves the stock `PersistentMapping` histories of the content with the behaviour. A type without the behaviour keeps the stock history.
- Comments get no class of their own: the `Conversation` of plone.app.discussion keeps them in BTrees and a `Length`, which tier 1 merges.
- Audit entries get no class: Plone 6.1 keeps no audit log in the ZODB. A package that keeps an append-only list in an object gives its class an entry with `append`.
- The profile of `zoe.plone` writes the policy with `zoe.policy.install`. It derives an entry for the class of each Dexterity type: the class merges field by field; the fields that an `@invariant` of the schema or of a behaviour of a type of that class names are a coupled group; a class with an invariant whose fields the derivation cannot find gets no entry; `modification_date` takes `max`, and the other fields `strict`. An overrides file of the profile changes the mode or the strategies of a class.
- As built (`zoe.plone.merges`): the derivation reads the names that the code of an invariant uses, and a field that the factory of its behaviour stores under another name (`effective` as `effective_date`) takes both names in its group. The derived entries are in shadow mode, as the specification asks a class to merge only after shadow mode showed that its merges are right; the key `tier2` of the overrides gives another mode. The overrides are the file `zoe-merge-policy.json` of a profile, and the import step `zoe.plone.merge_policy` runs for each profile that holds one, so a policy package of a site gives its own. The upgrade step moves the histories in batches of 100 objects, one transaction each.

## Consequences

- Section 5 of the specification changes: tier 2, the policy object and its reading, the precedence of the keys, the verifying shadow mode, the fields of the log; F5, F6 and F30 and the feature triage change to match.
- `zoedb` gets a public module, `zoe.policy`, with the class `MergePolicy` and the functions `install` and `read`.
- `zoe_pickle::python` models instances of other classes. Tier 1 refuses or gives no answer on them as today: the merges of BTrees do not change.
- A wrong policy can merge two changes that the application meant to exclude each other. That is why a class is strict until its entry says otherwise, and why shadow mode comes first.
- A change of the policy applies from the next conflict, without a restart.

## Alternatives

- **A key space or a `meta` key for the policy.** It would be a change of the on-disk format (a red zone), and the policy would lose its history, its undo and the transaction that writes it. The card asks for none.
- **A policy file next to the store, or in the configuration of the server.** It would differ between the two modes and between the instances of a site, and it would not commit with the profile that writes it.
- **Watch the commits for the oid of the policy.** It puts a check on every commit; reading two serials at a vote with a conflict costs nothing on the path of a commit without one.
- **Fields as the keys of a nested mapping** (for example the `data` of a `PersistentMapping`). The fields of a persistent object are its attributes; a mapping of its own can be a class with its own entry.
- **A named native merge per class** (the specification's "or a named native merge such as an append-only `workflow_history`"). The five strategies cover the classes of step 4 when `workflow_history`, comments and audit entries move to classes of their own; a named merge comes when a class needs one.
- **Verify in the client.** The client would need to know the merge of the server: a change of the protocol. The server already sees both records.

## Questions for the human

Each answer is Claude's choice, for review.

1. The policy at `root['zoe.merge_policy']`, a `zoe.policy.MergePolicy` with plain values: yes, as above.
2. Read again at the next vote with a conflict, through the serials of oid 0 and of the policy, not on every commit: yes.
3. The policy wins over the keys of ZOE-203, and a switch that it does not give keeps its key: yes.
4. The six strategies (`strict`, `lww`, `max`, `sum`, `union`, `append`), no named native merge yet, and a delete against a change always refuses: yes.
5. `lww` gives the value of the transaction that votes now: yes.
6. The verifying shadow mode as a second line of the resolution log and a counter, with no change of the wire: yes.
7. The public module `zoe.policy` with `MergePolicy`, `install` and `read`: yes.
