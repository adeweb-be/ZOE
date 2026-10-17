# Spike: the posting dicts of ZCTextIndex, the baseline of B5 and a store with tree postings

Card ZOE-203, step 1; requirements F5 and F6; items 1 and 5 of the ranked table of `docs/findings/plone-end-to-end.md`. The rule that follows from it is ADR 0042.

**Verdict.** On `main`, most saves that fail at their first attempt fail on the posting dicts of the text indexes, on both stacks: `IOBucket: Conflicting changes` is 279 of the 285 failed client resolutions of `edit` on `zoe-native`, and 172 of the 212 of `create-spread`. ZOE's clients resolved 4,257 conflicts in `edit` and 8,518 in `create-spread`, and every one of them was lost in an attempt that then failed. A store in which every posting is an `IIBTree` of its own changes almost nothing: in a run of two minutes, ZCTextIndex makes a new dict posting for each word that enters an index, and each save of the harness takes words in and out. `zoe-native` still had 241 `IOBucket: Conflicting changes` in `edit` and 129 in `create-spread`. So a one-time migration of the postings cannot remove these conflicts. A merge at the vote can: the posting-dict rule of ADR 0042.

## Question

Step 1 of the card: the baseline of B5 `edit` and `create-spread` on `zeo` and `zoe-native`, at 4 and 16 users, on `main`; then the experiment of item 1 of the ranked table: in a scratch fixture, convert each dict value of `_wordinfo` to an `IIBTree`, and count the conflicts that remain.

## Method

- `main` at `3b5164f`. The Plone instances of `just plone zeo build`, `zoeclient build` and `zeoclient build`, and the small fixture made again with `just bench plone prepare --force`, so its ZOE store holds raw records (ADR 0041).
- The baseline: `just bench plone campaign --stacks zeo,zoe-native --scenarios edit,create-spread --reps 1 --steps 4:60,16:60 --keep-store`. One repetition, as for the finding of B5.
- The experiment: a copy of the fixture, `small-ba01ff255313-iib`. A script inside Zope, on the ZEO store of the copy, put every dict posting of each ZCTextIndex of the site in an `IIBTree` (`IIBTree(posting)`, as `_add_wordinfo` converts a dict at 10 documents), in one transaction: 1,268 of the 1,377 words of `Description`, 69 of the 2,066 of `SearchableText` and 679 of the 706 of `Title`. `zoe convert` then made its ZOE store again, and the digests of both stores were equal. The same campaign ran on it with `--fixture small-ba01ff255313-iib`. The scripts stay outside the repository: the experiment changes no code.
- Machine: Apple M3 Pro, CPython 3.13 in the instances, both servers with `fsync(2)`. Every run passed its verification.

## Results

`1st-try ok` is the share of operations that succeeded at their first attempt; conflicts are per 1,000 operations.

| fixture | scenario | stack | users | ok ops/s | 1st-try ok | conflicts/1k |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| baseline | edit | zeo | 4 | 7.4 | 80.6 % | 317.2 |
| baseline | edit | zeo | 16 | 7.5 | 77.9 % | 350.1 |
| baseline | edit | zoe-native | 4 | 7.9 | 78.0 % | 329.5 |
| baseline | edit | zoe-native | 16 | 8.5 | 80.3 % | 288.1 |
| baseline | create-spread | zeo | 4 | 5.9 | 79.8 % | 400.6 |
| baseline | create-spread | zeo | 16 | 5.5 | 79.7 % | 398.7 |
| baseline | create-spread | zoe-native | 4 | 6.3 | 75.7 % | 418.1 |
| baseline | create-spread | zoe-native | 16 | 6.7 | 77.6 % | 357.3 |
| tree postings | edit | zeo | 4 | 7.9 | 79.5 % | 320.5 |
| tree postings | edit | zeo | 16 | 7.6 | 80.2 % | 301.9 |
| tree postings | edit | zoe-native | 4 | 8.0 | 81.9 % | 286.4 |
| tree postings | edit | zoe-native | 16 | 8.5 | 83.1 % | 273.3 |
| tree postings | create-spread | zeo | 4 | 5.9 | 79.5 % | 386.9 |
| tree postings | create-spread | zeo | 16 | 6.1 | 76.3 % | 407.5 |
| tree postings | create-spread | zoe-native | 4 | 6.2 | 78.8 % | 415.5 |
| tree postings | create-spread | zoe-native | 16 | 7.3 | 82.2 % | 308.1 |

The conflicts of the two steps, and the failed resolutions in the clients of `zoe-native` and in the ZEO server:

| fixture | scenario | stack | conflict classes | failed resolutions |
| --- | --- | --- | --- | --- |
| baseline | edit | zeo | `IOBucket` 274, `IOBTree` 2, `OOBTree` 1 | `IOBucket` changes 303, `OOBucket` changes 3, `IOBTree` inserts 2, `OOBTree` internal node 1 |
| baseline | edit | zoe-native | `IOBucket` 264, read conflicts 16, `IOBTree` 1 | `IOBucket` changes 279, `OOBucket` changes 4, `IOBTree` inserts 2 |
| baseline | create-spread | zeo | `UniqueIdGeneratorTool` 261 | `UniqueIdGeneratorTool` 293, `OOBucket` changes 3 |
| baseline | create-spread | zoe-native | `IOBucket` 160, read conflicts 89, `OOBTree` 28, `IOBTree` 2, `IITreeSet` 2, `OIBTree` 1 | `IOBucket` changes 172, `OOBTree` internal node 30, `IITreeSet` internal node 5, `OOBucket` changes 3, `IOBTree` internal node 2 |
| tree postings | edit | zeo | `IOBucket` 260, `OOBTree` 5, `IIBTree` 3, `IOBTree` 1 | `IOBucket` changes 284, `IOBucket` delete and change 10, `OOBTree` internal node 5, `OOBucket` changes 3, `IIBTree` delete of first key 3 |
| tree postings | edit | zoe-native | `IOBucket` 231, read conflicts 20, `OOBTree` 5, `IOBTree` 1 | `IOBucket` changes 241, `IOBucket` delete and change 13, `OOBTree` internal node 5, `OOBucket` changes 3, `IOBucket` bucket split 1 |
| tree postings | create-spread | zeo | `UniqueIdGeneratorTool` 271 | `UniqueIdGeneratorTool` 308, `OOBucket` changes 4 |
| tree postings | create-spread | zoe-native | read conflicts 129, `IOBucket` 117, `OOBTree` 16, `IIBTree` 5, `IOBTree` 2, `IITreeSet` 1, `OIBTree` 1 | `IOBucket` changes 129, `OOBTree` internal node 23, `IIBTree` internal node 5, `OOBucket` changes 3, `IOBTree` internal node 2 |

Resolutions that succeeded and were then lost, because a later conflict failed the attempt: `edit` 4,576 in the ZEO server and 4,257 in the clients of `zoe-native` in the baseline, 7,087 and 6,066 with tree postings; `create-spread` 0 and 8,518 in the baseline, 0 and 8,890 with tree postings. In `create-spread` on ZEO each overlapping creation fails on `portal_uidgenerator` before any bucket resolves (finding item 24); `zoe-native` takes its ids from a sequence (F29).

## Why the tree postings do not help

- `_add_wordinfo` and `_mass_add_wordinfo` make a new dict for a word that the index does not hold, and convert it to an `IIBTree` only when it holds 10 documents. `_del_wordinfo` deletes the word when its last document leaves. The harness rewrites the title and the description of a Document at each save, from a generated vocabulary of Zipf frequencies, so rare words enter and leave the indexes all the time: the postings of the conversion are soon not the postings of the run.
- The bucket of `_wordinfo` changes at every indexing, also when the posting is a tree: `self._wordinfo[wid] = doc2score` stores the value again, and an object bucket has no check of the same value, so it is marked changed. Two saves that touch one bucket conflict on it; BTrees merges them when no key has two changed values. With tree postings, the remaining failures are the keys whose values are new dicts on both sides, and a delete of a word against a change of it.
- `IIBTree` postings of 10 entries or fewer hold one bucket inline, and their own conflicts merge (inserts of different documents); 3 to 5 of them failed (`delete of first key`, `internal node`).

## Consequences for the card

- The posting-dict rule of ADR 0042 is the change that can remove most of these failures; a migration of the postings cannot.
- Tier 1 without the rule moves the resolutions of the buckets and of the `Length` counters from the clients to the vote, and refuses the posting dicts where the clients refused them: it saves the lost client resolutions, not the failed attempts.
- The remaining failures to watch after the rule: the read conflicts of `zoe-native` (finding item 26), `OOBTree` and `IITreeSet` internal nodes, and the words that both saves add.

## Not measured

- More than one repetition: differences of a few percent are noise.
- A text index that never makes a dict posting: it needs a change of ZCTextIndex in the instances.
- The medium fixture, Linux.
