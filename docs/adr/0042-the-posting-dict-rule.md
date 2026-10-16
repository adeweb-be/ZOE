# 0042. The posting-dict rule, and the settings of the structural merges

Status: proposed. A checkpoint of ZOE-203: the human approves it on the pull request before the rule and the keys are built.

## Context

ZCTextIndex keeps the postings of a word, a dict of document id to score, as a value of the IOBTree `_wordinfo` (`LOBTree` with `family64`) until the dict holds `DICT_CUTOFF` entries, 10. `_add_wordinfo` converts a dict of exactly 10 entries to an `IFBTree` of its own before it adds the next entry. `_del_wordinfo` deletes the key of the word when its dict becomes empty, and each change of the set of words changes the `Length` of the index by one.

When two transactions index documents that share a rare word, both change the dict of that word in the same bucket. The bucket resolver of BTrees compares two values with `object_compare`, which calls `dict < dict`, and that raises `TypeError`. So ZODB's resolver refuses every bucket in which it must compare two dicts, also when only one side changed the dict. On B5 these refusals are most of the failed saves (finding items 3, 24 and 25 of `docs/findings/plone-end-to-end.md`): `IOBucket: Conflicting changes` is 172 of 212 failed client resolutions in `create-spread` and 279 of 285 in `edit` on `zoe-native` in the baseline of ZOE-203.

Tier 1 (ZOE-203) merges buckets, sets, one-bucket BTrees and `Length` in Rust with the rules of BTrees, and gives the bytes that ZODB's resolver gives. Without more, it refuses these buckets as BTrees does. A rule that merges them is a merge where ZODB's resolver refuses, so its correctness rests on what ZCTextIndex does, not on BTrees.

The card also asks for shadow mode and a resolution log, and for a setting of each class: off, shadow or on. These are new keys of `<zoe>` and of `zoe serve`: a public API.

## Decision

### The rule

In a bucket, or a BTree that holds one bucket inline, of a family with integer keys and object values (`IO`, `LO`, `UO`, `QO`), and only with the rule on:

1. A posting dict is a dict whose keys are ints and whose values are ints or floats. Two posting dicts are equal when they hold the same entries, in any order. A posting dict is never equal to a value that is not a dict. With these comparisons, the merge of BTrees runs as it does for other values: a key that only one side changed takes the change of that side, and the deletes and inserts of keys keep the rules of BTrees.
2. When both sides changed the value of one key and the three values are posting dicts, the rule merges them entry by entry against the old dict. An entry that only one side changed, added or deleted takes that change. An entry that both sides changed in the same way takes it. The merged dict keeps the order of the committed dict, and the entries that the new transaction added come after them, as if the new transaction had run after the committed one.
3. The rule refuses, with a conflict:
   - an entry that both sides changed to different values, or that one side changed and the other deleted;
   - a value that is not a posting dict on one side when both sides changed the key: a dict against the reference to the `IFBTree` of a converted posting;
   - a merged dict of more than 10 entries: ZCTextIndex converts a dict only at exactly 10, so a dict of 11 would never become a tree;
   - a merged dict of no entry: ZCTextIndex deletes the word instead.
4. The rule never changes the number of words of an index. A word that both sides add stays a refusal (reason 4 or 6 of BTrees), and so does a word that one side deletes and the other changes (2 or 3): both change `length`. A score of an int against a float on the two sides gives no answer: the conflict goes on as without the rule.

A dict with other keys or values is not a posting dict, and stays a value that BTrees cannot compare: tier 1 gives no answer, and the client resolves the conflict as today.

The rule is off by default, because it merges where ZODB refuses: a class of another program can keep dicts of ints in an `IOBTree` with other invariants.

### How a store switches it on

One boolean key, `merge-posting-dicts`, false by default:

- In-process: the key `merge-posting-dicts` of `<zoe>`, its environment twin `ZOE_MERGE_POSTING_DICTS`, the query key of `zoe://`, and the argument `merge_posting_dicts` of `zoe.Storage`.
- `zoe serve`: `--merge-posting-dicts`, `ZOE_MERGE_POSTING_DICTS`, the key `merge-posting-dicts` of the configuration file and of `<zoe-server>`. It applies to every store of the server.

A stock client and the native client need nothing: the server merges at the vote and replies with the bare oid, as ZEO 6 does for a conflict that its server resolved.

A Plone site sets the key in its `<zoe>` stanza or in the configuration of its server. `zoe.plone` runs in the client, so it cannot switch a setting of `zoe serve` without a protocol extension. ZOE-205 moves the switches of the structural merges into the merge policy, a versioned object in the store that `zoe.plone` writes with its profile; then `zoe.plone` switches the rule on in both modes, and this key stays the default of a store without a policy.

### Shadow mode and the mode of each class

- `structural-merges` (`on`, `shadow` or `off`; `on` by default) is the mode of every class of tier 1. `on` stores the merge or fails the vote on a refusal. `shadow` runs tier 1, writes its result to the resolution log, and goes on as without it: the client resolves the conflict. `off` does not run tier 1.
- `structural-merge-class CLASS MODE` sets the mode of one class, such as `structural-merge-class BTrees.IOBTree.IOBucket shadow`. In `<zoe>` it is a key that comes once for each class; its environment twin `ZOE_STRUCTURAL_MERGE_CLASSES` and `zoe serve --structural-merge-class CLASS=MODE` (repeated; `ZOE_STRUCTURAL_MERGE_CLASSES` as `CLASS=MODE,CLASS=MODE`; a table `[structural-merge-classes]` in the configuration file) take the pairs.
- The posting-dict rule applies only where the mode of the class is `on` or `shadow`.

### The resolution log

Each result of tier 1 (merged, refused, or no answer, in mode `on` or `shadow`) writes one line of JSON to the resolution log: the time, the storage, the oid, the class, the tier, the mode, the outcome, the reason (the code and the message of BTrees, or of the rule), whether the outcome was the outcome of the vote, the keys that the new transaction changed (at most 20, each cut at 80 characters, and their number), the committed and the read serial, and the user and the description of both transactions.

- `resolution-log FILE`: in-process the key of `<zoe>` (`ZOE_RESOLUTION_LOG`, the query of `zoe://`, the argument `resolution_log` of `zoe.Storage`); for `zoe serve` the option `--resolution-log` (`ZOE_RESOLUTION_LOG`, the key `resolution-log` of the configuration file and of `<zoe-server>`). Without it, no line is written.
- The file is opened in append mode and each line is written whole; it is never synced. It is not in the store, and pack never reads it.
- The server log keeps its rule: no user, description or pickle bytes there. The resolution log holds user, description and keys, which can be text of the site, so it is a file of its own that the operator turns on.
- `zoe serve` counts in `/metrics`: `zoe_conflicts_resolved_total{path="native"}`, the oids that tier 1 merged in a vote that passed, and `zoe_structural_merges_total{mode, outcome}` for every result.

## Consequences

- With the rule on, two saves of a Plone site that index documents sharing rare words commit at the first attempt. The text index stays equal to a reindex from scratch (`test_posting_dicts.py`).
- A site that does not set the key gets the merges of BTrees and nothing more.
- A refusal of the rule is a conflict as today: Zope retries the request.
- The resolution log can grow without bound; rotation is the operator's (copytruncate works, because each write appends).

## Alternatives

- **Convert every posting to an `IFBTree`.** Step 1 of ZOE-203 measures this in a scratch fixture (`docs/spikes/posting-dicts.md`). It needs a change of ZCTextIndex or a migration of each site, and more objects per word.
- **A merge of posting dicts without the limit of 10.** A dict of more than 10 entries would never become a tree, and the index would grow dicts without bound.
- **The rule on by default.** It merges where ZODB refuses, so a store opts in.
- **The resolution log in the Python logging of Zope, or in the server log.** The server log has no user or description by rule; Zope's event log is not available to `zoe serve`. One file key for both modes is the simplest that holds.
- **A switch that `zoe.plone` sends to the server.** It would be a protocol extension for a setting that the deployment already has.
- **`zoe.plone` switches the rule on in-process at the open of the database**, through a method of `zoe.Storage`. It works only in-process, adds a public method, and makes the setting of a store change while it is open; the merge policy of ZOE-205 does it for both modes.
