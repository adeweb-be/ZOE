# Finding: text queries with a sort, and the filter of every row

Card ZOE-314. Found by the gate run of phase 6 (G4).

**Verdict.** The time of a text clause with a sort went into conversions between rids, rows and documents, not into the search of the index or the sort. The mirror gave the text clause its filter as a list of rids, which it made from the rows of the result at each query: 5 ms for the 1,000,000 rows of a query with no other clause. The text clause gave back its hits as rids. The mirror found the row of each hit with a binary search over the rids of the base: 8 to 16 ms on average for the words of the query set. In the compatibility mode, the set of the hits also got an Okapi score for each hit, then a sort of all hits by score: 9 ms on average. The fix gives the text clause the rows of the result as its filter, and the clause gives back the rows of its hits, with no score and no rid. The results do not change: rids, order, counts and scores are those before the fix on the 60 queries of the scenario `text` in each mode, and the order rules of ADR 0060 do not change.

## Method

The gate run measures through `zoe serve` and gives no split of the time. A probe in the process ran the 60 text plans of each variant of the scenario `text` of `catalog-1m` (the plans of `plans.msgpack`, the same store, the same text indexes) on `Mirrors::query_with_text`, 5 rounds after a warm pass, one thread, with timers around each part of the clause. The probe stays out of the pull request. It ran on the machine of the gate, with the bench lock and the heavy lock, while other builders compiled. Its p99 over 60 calls is the slowest call and moves with the load of the machine, so the parts below are means.

## Where the time went

The parts before the fix, mean in ms for each call, in each mode:

| Part | enhanced `sort` | enhanced `listing_sort` | compat `alone` | compat `sort` | compat `listing_sort` |
| --- | ---: | ---: | ---: | ---: | ---: |
| the other clauses of the plan | 0.01 | 0.84 | 0.01 | 0.01 | 0.95 |
| the filter as a list of rids, from the rows | 4.88 | 2.19 | 5.35 | 5.43 | 2.63 |
| the filter as a bitset of documents | 1.19 | 1.66 | 1.59 | 1.69 | 2.30 |
| the search of the index | 2.70 | 3.31 | 3.11 | 3.21 | 3.68 |
| the hits in the order of their scores (compatibility mode) | | | 0.81 | 9.27 | 5.14 |
| the row of each hit, a binary search | 12.79 | 7.88 | | 15.73 | 10.48 |
| the sort of the rows on `effective`, first 20 | 0.05 | 0.20 | | 0.06 | 0.23 |
| the whole clause | 21.84 | 16.21 | 11.11 | 35.64 | 25.53 |

- The sort was never the cost: the walk of the sort array stops at 20 rows, in 0.05 to 0.23 ms.
- The binary search over the 1,000,000 rids of the base cost about 25 ns for each hit. A word of the top band has about 500,000 hits alone, and about 240,000 in the filter of the listing.
- The compatibility mode scored each hit of a set, then sorted every hit by score and rid with the collector of `Want::Top(None)`. A set needs neither.
- The list of rids for the filter took 5 ms for 1,000,000 rows, more than the search. It also slowed the variants with no sort, and with the server it put the compatibility mode alone over 20 ms.

## The fix

- The filter of a text clause in a plan is the rows of the result of the other clauses (`Filter::Rows`). The build numbers the rows of a base in rid order, so the search reads the rows in batches of 256 with the rid of each row, and merges them with the documents of the index in rid order, as it merged the list of rids. The rows that a rebase or the overrides of a version add after the others come apart, in rid order, in the same merge. `text_query` keeps its list of rids (`Filter::Rids`).
- Each rid of a filter has a key: its row, or its place in a list. With a sort, the text clause gives the keys of its hits (`Want::Keys`), as a roaring bitmap of rows, which the sort of the mirror reads. With the bitset strategy, the search reads the hits with no filter, and one merge of the filter with the documents of the hits gives the keys, into a bitset and then the bitmap: the merge applies the filter. With few hits, the key of each hit comes from a binary search. The hits of the text overlay get their keys the same way.
- In the compatibility mode, a set reads the postings of each word with no frequency and gives no score. It sorts nothing. The rules of `ParseTree` decide which documents match, not the scores, so the set is the same.
- `Want::Set`, which the tests use, gives its hits in rid order.

The results stay those of ZCatalog: the text clause gives the same set of rows, and the sort of the mirror, its paths and its ties, did not change. The probe wrote each answer of the scenario (rids, `len`, `actual_result_count`, scores) before and after the fix: the 60 answers of each mode, 12 words in 5 variants, are identical. A new test of `tests/text.rs` checks, in each mode, a text clause with a sort, `sort_limit`, `b_start` and `b_size` on an index with many ties against the same sort on a clause that selects the same rows, also with rows that a commit adds below the rids of the base. The unit tests of `search.rs` check the keys of each kind of filter.

## The numbers

The parts after the fix, mean in ms for each call, with the same probe:

| Part | enhanced `sort` | enhanced `listing_sort` | compat `alone` | compat `sort` | compat `listing_sort` |
| --- | ---: | ---: | ---: | ---: | ---: |
| the other clauses of the plan | 0.01 | 0.81 | 0.01 | 0.01 | 0.82 |
| the filter as a bitset of documents | | | 1.90 | | |
| the search of the index | 6.09 | 6.27 | 2.70 | 2.11 | 2.39 |
| the keys of the hits, or the first hits by score | (in the search) | (in the search) | 0.65 | 3.64 | 3.57 |
| the sort of the rows on `effective`, first 20 | 0.04 | 0.19 | | 0.04 | 0.20 |
| the whole clause | 6.29 | 7.37 | 5.42 | 5.93 | 7.07 |

In the enhanced mode, the search of a set holds the read of the hits and the merge that gives their keys. The p99 of the probe, the slowest of 60 calls, before and after:

| Mode | alone | listing | selective | sort | listing_sort |
| --- | ---: | ---: | ---: | ---: | ---: |
| enhanced, before | 17.42 | 13.87 | 20.94 | 42.97 | 30.14 |
| enhanced, after | 8.71 | 19.00 | 4.92 | 7.94 | 9.91 |
| compat, before | 16.87 | 21.64 | 1.56 | 74.50 | 51.44 |
| compat, after | 9.82 | 18.50 | 1.32 | 7.68 | 9.91 |

The slowest call of `listing` and of `selective` is a single call each time, of another word in each run: the load of the machine. Their means fell (enhanced `listing` from 8.75 to 6.51 ms, compat `listing` from 10.24 to 7.15 ms).

The scenario `text` of `just bench catalog` on `catalog-1m`, through `zoe serve`, p99 in ms: the gate run of phase 6 at `56030a82`, and a run of the fix on a clean tree, on the same machine, without a resource wrapper.

| Mode | Cell | alone | listing | selective | sort | listing_sort |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| enhanced | c1, gate | 18.0 | 19.2 | 11.1 | 47.9 | 40.6 |
| enhanced | c1, fix | 11.0 | 14.9 | 7.3 | 12.9 | 18.0 |
| enhanced | c16, gate | 46.9 | 68.9 | 37.6 | 107.5 | 85.7 |
| enhanced | c16, fix | 31.3 | 37.0 | 23.6 | 40.2 | 40.1 |
| compat | c1, gate | 20.6 | 20.8 | 3.8 | 80.8 | 72.4 |
| compat | c1, fix | 12.8 | 19.2 | 3.8 | 19.3 | 17.9 |
| compat | c16, gate | 53.4 | 53.3 | 10.1 | 232.9 | 258.0 |
| compat | c16, fix | 42.0 | 45.5 | 10.7 | 44.1 | 69.4 |

Each variant meets 20 ms at p99 at concurrency 1 in both modes. The p50 of the sorted variants is 8.6 to 9.8 ms; their p99 in the compatibility mode stays close to the line (19.3 ms for `sort`, 11.5 ms for the same variant after the commits of the run).

## Limits

- The variants with no sort keep the search with the scores: the compatibility mode alone scores each hit, about 3 ms for a word of the top band.
- Rows that rebases add out of rid order come apart in each query. A base that a build makes again holds them in rid order.
