# Finding: the tail of the five-index intersection is the range of `generic-numbers`

Card ZOE-312. Gate of phase 6, G2. ADR 0059, decision 5.

**Verdict.** One shape makes the tail. At concurrency 1, `generic-numbers` takes 22.5 ms at p50 and 34.6 ms at p99, and the five other intersections stay under 5 ms at p99. The scenario sends the six queries in turn, so one query in six is slow, and the p99 of the cell is the time of that query. The cause is the rule of the range of ZOE-286: it compared the rows of the result set with the number of the keys of the range, not with the rows of their postings. The range of `generic-numbers` has 3,000 keys and 300,331 rows, and the other clauses leave 9,915 rows. So the rule took the union of the 3,000 postings (19.6 ms), where the test of the key of each of the 9,915 rows costs 0.35 ms. The rule now compares the result set with the rows of the postings of the keys, and stops the count when it passes the result set. With it, `generic-numbers` takes 1.5 ms at p50, and the cell gives 3.1 ms at p99 (24.8 ms before).

## The numbers of each query

`zoe-bench catalog intersect` gives p50, p99 and max for each query of the set, by name. `catalog-1m`, warm, `--catalog-memory 8GB`, without a resource wrapper. Before: main at 56030a82 (the run of the gate gave p50 1.49 ms and p99 23.1 ms at c1). After: the fix of this card, at 91010aea, on a clean tree.

| query | c1 p50 before | c1 p99 before | c1 p50 after | c1 p99 after |
| --- | ---: | ---: | ---: | ---: |
| `listing` | 1.509 ms | 3.123 ms | 1.362 ms | 2.441 ms |
| `drafts` | 0.568 ms | 1.139 ms | 0.402 ms | 0.914 ms |
| `subject` | 1.449 ms | 3.120 ms | 1.327 ms | 2.555 ms |
| `generic-words` | 1.572 ms | 2.980 ms | 1.413 ms | 2.893 ms |
| `generic-numbers` | 22.527 ms | 34.645 ms | 1.387 ms | 2.626 ms |
| `folders` | 2.883 ms | 4.923 ms | 2.440 ms | 4.192 ms |

| cell | p50 before | p99 before | max before | p50 after | p99 after | max after |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| c1 | 1.580 ms | 24.757 ms | 39.709 ms | 1.378 ms | 3.077 ms | 24.375 ms |
| c16 | 2.693 ms | 56.393 ms | 119.133 ms | 2.455 ms | 6.420 ms | 39.786 ms |

## The cause

The clauses of a plan apply in turn, and the ranges come last, so a range sees the rows of the other clauses (`View::result_set`). A range then takes one of two paths (`View::range`):

- by keys: the union of the postings of each key from the lowest to the highest bound;
- by rows: the test of the slot of each row of the result set against the slots of the range.

ZOE-286 took the path by rows when the result set held fewer rows than the range had keys. A key of a FieldIndex of numbers below 10,000 holds about 100 rows at one million entries, so that rule took the union of 300,331 rows to answer 3,058.

A probe build timed both paths on each range of the set, beside the answer, for 100 queries of each range (the probe stays out of the diff):

| range (query) | result set | keys | rows of the postings | hits | union of the postings, p50 | test of the rows, p50 | count of the rule, p50 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `bench_field_01` (`generic-numbers`) | 9,915 | 3,000 | 300,331 | 3,058 | 19.6 ms | 0.35 ms | 0.011 ms |
| `created` (`generic-numbers`) | 19,757 | 500,000 | 500,000 | 9,915 | 5.5 ms | 0.43 ms | 0.013 ms |
| `bench_date_00` (`generic-words`) | 13,247 | 273,242 | 300,125 | 4,044 | 16.9 ms | 0.50 ms | 0.035 ms |
| `modified` (`drafts`) | 237 | 153,318 | 244,974 | 18 | 46.7 ms | 0.005 ms | 0.002 ms |

The union costs 11 to 190 ns for each row of a posting: each posting merges into a bitmap that grows. The test costs 20 to 40 ns for each row of the result set: one read of the slot of the row. Only `bench_field_01` took the union before the fix: the three other ranges have more keys than their result set, so the old rule already tested their rows. The intersection of the result with the result set costs 0.01 ms, and the other parts of the query (the clauses of keys, the sort, the reply) stay near 1.5 ms.

So no lock, sort, encoding or allocator explains the tail: the p50 of the five other queries is 0.6 to 2.9 ms, and their p99 is at most 4.9 ms.

## The fix

`View::range` tests the rows of the result set when the postings of the keys of the range hold more rows than the result set. A row of a KeywordIndex counts once for each of its keys, which is the cost of the union. The count reads the length of each posting and stops as soon as it passes the size of the result set: 0.002 to 0.035 ms on the ranges above. A range with no result set keeps its rule: the union of the postings up to 4,096 keys, else the test of each row of the index.

The rule changes no result: both paths give the rows of the result set whose key is in the range, and the overrides apply after either path. `generic_numbers_tests_the_key_of_each_row_of_the_other_clauses` keeps its answer of ZCatalog, and `a_range_of_large_postings_tests_the_key_of_each_row_of_the_other_clauses` checks a result set with more rows than the keys of the range and fewer than their postings: the path by rows, and the rows that each clause alone gives.

## Limits

- The max of each query at c1 stays near 18 to 24 ms, a few queries of each thousand, whatever the shape. This card did not look for their cause.
- At concurrency 16, the p99 of each query is 2.2 to 8.9 ms after the fix. G2 is a number at concurrency 1.
- One machine (Intel Core Ultra 7 155H), one run before and one after.
