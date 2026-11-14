# Finding: the cold build of the mirror waits on the sort of the keys of `object_provides`

Card ZOE-313. Gate of phase 6, G3. ADR 0059, section 2.

**Verdict.** The walks of the trees are not the cause. From a cold page cache, the 53 trees of `catalog-1m` (`paths`, `uids`, `data` and the `_unindex` of the 50 mirrored indexes) are read and decoded in 15.9 s, with the 22 threads of the pool busy. Then one thread builds the structure of the KeywordIndex `object_provides` for 59.7 s, and the pool waits. The build sorted each pair of a str key by its text: about 30 million pairs for `object_provides`, the dotted names of interfaces with long shared prefixes, and each comparison read the two texts from their arena again and checked their UTF-8. The build now sorts only the distinct texts as strs; each pair takes the slot of its text, and the pairs sort as numbers. The structure of `object_provides` then builds in 3.0 s. The build from a cold page cache takes 17.2 s in place of 74.9 s, and the scenario `cold` of `just bench catalog` gives `mirror_ready_seconds` 21.9 s in place of 80.4 s.

## The numbers of each phase

A probe build timed each phase of `build` on a copy of the store of `catalog-1m` (17.6 GB), in one process, from a cold page cache (`POSIX_FADV_DONTNEED` on each file of the store), with the 22 threads of the machine and no bound. It summed the time of each thread of a walk in the read of a record (`load_before`), the decode of the pickle, the read of the node and the hand-off of its items, and it sampled the busy threads of the pool every 100 ms. The probe stays out of the diff. Before: main at e951173f. After: the fix of this card.

| Phase | Before | After |
| --- | ---: | ---: |
| The walk of `paths` (33,137 records) | 0.33 s | 0.36 s |
| The walk of `uids` (33,599 records) | 0.36 s | 0.38 s |
| The walk of `data` (33,137 records, 772 MB of pickles) | 3.84 s | 3.87 s |
| The walks of the 50 index trees, one after the other | 11.34 s | 10.37 s |
| All the walks: their end after the start | 15.88 s | 14.98 s |
| The metadata table, beside the walks of the indexes | 9.42 s | 8.98 s |
| The structures of the indexes, beside the walks: the sum of their threads | 102.9 s | 10.0 s |
| The structure of `object_provides` | 59.66 s | 2.98 s |
| The end of the last structure after the end of the last walk | 58.79 s | 2.06 s |
| **The build** | **74.86 s** | **17.18 s** |
| Peak RSS of the process | 6.89 GB | 7.07 GB |

In the walks, the threads spend 164 s in the reads of the store, 88 s in the decode, 17 s in the read of the nodes and 13 s in the hand-off of the items, of 1,367,643 records (1.87 GB of pickles). The pool keeps its 22 threads busy from the first walk to the last, except for a few hundred milliseconds at the end of each tree. The fix changes no walk; the differences between the two columns of the walks are the noise of the disk.

The structures of the other KeywordIndexes took 1.8 to 3.7 s each before (`Subject`, `allowedRolesAndUsers`, the six `bench_keywords_*`) and 0.2 to 0.3 s after; `UID` took 1.05 s and 0.70 s. They built beside the next walks, so they did not delay the build.

## Compared with the prototype

The prototype of the spike of ADR 0059 built in 53.3 s: its walks took 43.9 s and the structure of `object_provides` 20.4 s (`docs/spikes/catalog-mirror.md`). The walks of the mirror take 15.9 s from a cold page cache, so the read and the decode of each record are faster than in the prototype. The structure of `object_provides` was three times slower than in the prototype: the build keeps the keys of a str index in an arena (ADR 0059, section 2), and its sort read each text from the arena at each comparison.

## The cause

`group` (`crates/zoe-catalog/src/base.rs`) gives the distinct keys of an index in ascending order and the rows of each. For str keys, it sorted the pairs `(place in the arena, row)` with a comparison of the texts. A KeywordIndex gives one pair for each key of each row: about 30 interfaces for each entry of `object_provides`, so about 30 million pairs and some 750 million comparisons. Each comparison called `StrArena::get` twice, which checks that the span is UTF-8, and the names of the interfaces share long prefixes (`plone.app.contenttypes.interfaces.`), so a comparison read dozens of bytes on each side: about 80 ns for each, by an estimate from the time of the structure. Then `KeywordIndex::build` sorted the pairs `(row, slot)` again, to lay out the slots of each row.

## The fix

- `group` gives each pair of str keys the number of its text, through a map of the distinct texts. It sorts the distinct texts once, gives each pair the slot of its text, and sorts the pairs as numbers. The keys, their slots, the rows of each key and the order of the calls of `each` stay the same: the slots follow the order of the texts, and a tie of two pairs of one text sorts by row, as before.
- `KeywordIndex::build` counts the slots of each row and lays them out in one pass over the pairs, without a sort. `group` gives the slots in ascending order, so the slots of each row stay in ascending order, and a repeated slot of a row (a list with a key twice) is next to its twin and goes.

The map of the distinct texts holds a reference and a number for each distinct text while the keys group: a few hundred entries for a KeywordIndex, one million for `UID`, freed before the postings build. The bound of section 6 counts the pairs and the arena of the texts, as before. The peak RSS of the build stays within 0.2 GB of the build before the fix, and the peak RSS of `zoe serve` goes from 7.05 GB to 6.91 GB.

The shadow rebuild and the rebase build their structures with the same code, so they gain the same time.

`tests/build_keys.rs` checks the structures of a KeywordIndex of str keys (from one part and from the parts of several threads), of a KeywordIndex of int keys, of a KeywordIndex without a pair and of a FieldIndex of str keys against a model: the distinct keys in order (texts with long shared prefixes and texts outside ASCII, whose order is the order of their code points), the rows of each key, and the key or the keys of each row, with repeated pairs and rows without a key.

## In `zoe serve`

The scenario `cold` of `just bench catalog` on `catalog-1m`, `--catalog-memory 8GB`, on a clean tree, without a resource wrapper. It starts `zoe serve` on a copy of the store whose pages it dropped (17.6 GB), and commits to the store during the build.

| Measure | Before (the gate run, main at 56030a82) | After (6b9feece) |
| --- | ---: | ---: |
| `mirror_ready_seconds` | 80.36 s | **21.94 s** |
| The build (`zoe_catalog_mirror_build_seconds`) | 80.28 s | 21.87 s |
| `first_answer_seconds` | 80.40 s | 22.00 s |
| The mirror | 1,496,709,839 bytes | 1,496,709,839 bytes |
| Peak RSS of the server | 7.05 GB | 6.91 GB |

## Limits

- One machine (Intel Core Ultra 7 155H, 22 hardware threads, NVMe), one catalog shape. A machine with fewer threads walks slower; the walks then take most of the build.
- The structure of `object_provides` still builds in one thread after its walk, so it ends the build 2 s after the last walk. The order of the trees could hide it; the build meets G3 without that.
