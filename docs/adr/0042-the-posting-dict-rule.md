# 0042. The posting-dict rule, the first-key rule, and the settings of the structural merges

Status: accepted. The posting-dict rule and the first-key rule merge where ZODB's resolver refuses, under one key, `posting-dicts`, off by default. The resolution log is a file key in both modes.

## Context

ZCTextIndex keeps the postings of a word, a dict of document id to score, as a value of the IOBTree `_wordinfo`. At `DICT_CUTOFF`, 10 entries, it converts the dict to an `IIBTree` of its own. The bucket resolver of BTrees compares two values with `<`, and `dict < dict` raises `TypeError`. So ZODB's resolver refuses every bucket in which it must compare two dicts. On B5 this is most of the failed saves: `IOBucket: Conflicting changes` is 172 of 212 failed client resolutions in `create-spread` and 279 of 285 in `edit` (`docs/findings/plone-end-to-end.md`, items 3, 24 and 25). With the dicts merged, most of what tier 1 still refuses on B5 is the delete of the first key of the inline bucket of a posting tree.

A rule that merges where ZODB's resolver refuses rests on ZCTextIndex, not on BTrees, so a store opts in.

## Decision

### The rule

The rule applies in a bucket, or a BTree that holds one bucket inline, of a family with integer keys and object values (`IO`, `LO`, `UO`, `QO`).

- A posting dict is a dict of int to int. Two posting dicts are equal when they hold the same entries, in any order. A posting dict is never equal to a value that is not a dict. With these comparisons, the merge of BTrees runs as for other values.
- When both sides changed one key and the three values are posting dicts, the rule merges them entry by entry against the old dict. The merged dict keeps the order of the committed dict, then the entries that the new side added.
- The refusals: specification, "ConflictError strategy", "Lever 1: resolve in Rust, never in Python on the server". The rule never changes the number of words of an index.
- A dict with other keys or values is not a posting dict: tier 1 gives no answer.

Why it is safe. Only a transaction that indexes or unindexes a document D writes the entry of D, and it also writes the key D of `_docwords`. Two such transactions conflict there. So a merge only joins the entries of different documents.

### The first-key rule

BTrees refuses the delete of the first key of a bucket (reason 13), because a parent node keeps the first keys of its children. The root of a BTree or a TreeSet that holds one bucket inline has no parent node.

With the rule on, tier 1 lets a delete of the first key through when the committed transaction deleted it, in the bucket that a BTree or a TreeSet of a family of tier 1 holds inline, with no next bucket. A delete of the first key by the new transaction, or in a standalone bucket, stays a refusal. The other refusals of BTrees stay.

Why a delete by the new transaction stays a refusal. A record cannot tell a root from the node of the last bucket of a tree, where a delete of the first key can move the separator of the parent node. No check of the vote then sees a key that the committed transaction added below the new separator, so a merge would lose it. When the committed transaction moved the separator, the new transaction marked the parent node read-current on its path, and the vote fails there. This holds for writes that enter the tree at its root, as the API of BTrees does, not for a node that a program takes from `__getstate__()`.

### How a store switches it on

One key, `posting-dicts`, switches both rules: `off` (the default), `shadow` or `on`. It is a key of `<zoe>`, of `<zoe-server>` and of the configuration file of `zoe serve` (`ZOE_POSTING_DICTS`, `--posting-dicts`, the query key of `zoe://`, the argument `posting_dicts` of `zoe.Storage`). In `zoe serve` it applies to every store.

- `on` merges with both rules. In `shadow`, the rules write their result in the field `shadow` of the line, and the conflict ends as with the rules off (specification, "Storage core", "Conflict resolution dispatch"). The other merges of tier 1 stay live.
- The rules run only where the mode of the class is `on` or `shadow`.
- The switch `posting_dicts` of the merge policy wins over the key (ADR 0046, section 3).

A stock client needs nothing: the server merges at the vote and replies with the bare oid.

### Shadow mode and the mode of each class

- `structural-merges` (`on` by default) is the mode of every class of tier 1, and `structural-merge-class CLASS MODE` the mode of one class (specification, "Storage core", "Conflict resolution dispatch"). `zoe serve` takes `--structural-merge-class CLASS=MODE`, repeated, and a table `[structural-merge-classes]` in its configuration file.
- A shadow mode without `resolution-log` tells nobody what it found, so the open refuses it: `ValueError` in-process, a usage error for `zoe serve`.

### The resolution log

`resolution-log FILE` (`ZOE_RESOLUTION_LOG`, the argument `resolution_log` of `zoe.Storage`, `--resolution-log` of `zoe serve`) names the log. Without it, no line is written.

- Each result of tier 1 writes one line of JSON: the time, the storage, the oid, the class, the tier, the mode, the outcome, the reason, `shadow`, whether the outcome was that of the vote, `undo`, the keys that the new transaction changed (at most 20, each cut at 80 characters, and their number), the committed and the read serial, and the user and the description of both transactions.
- The file opens in append mode, and each line is written whole. It is never synced, and it is not in the store.
- The log holds text of the site, which the server log never holds, so the operator turns it on.

## Consequences

- With the rule on, two saves that index documents with shared rare words commit at the first attempt, and the text index stays equal to a reindex (`test_posting_dicts.py`).
- A store without the key gets the merges of BTrees and nothing more. A refusal is a conflict, and Zope retries.
- The resolution log grows without bound. Rotation is the operator's job; copytruncate works.
- Tier 1 gives the bytes of ZODB's resolver, except for these rules, the split merge (ADR 0048) and the order merge (ADR 0052). Their tests check the merged trees.

## Alternatives

- **A key of its own for the first-key rule.** It departs from ZODB in the same way, for the same index.
- **The first-key rule in standalone buckets.** Outside the first bucket of a tree, the vote fails anyway.
- **Convert every posting to an `IIBTree`.** It needs a change of ZCTextIndex or a migration of each site, and more objects per word.
- **No limit of 10.** The index would grow dicts that never become trees.
- **Convert a merged dict of more than 10 entries to a tree at the vote.** The vote would mint an oid and add a record that no client stored.
- **The rule on by default.** It merges where ZODB refuses.
- **A boolean key.** The rule could run in shadow mode only with its whole class.
- **The log in the event log of Zope, or in the server log.** The server log has no user or description; `zoe serve` has no event log of Zope.
- **A switch that `zoe.plone` sends to the server, or a method of `zoe.Storage`.** A protocol extension, or a public method for one mode. The merge policy serves both modes.
