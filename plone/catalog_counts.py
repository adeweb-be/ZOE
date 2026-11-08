"""Print the counts of the catalog backend of `zoe.plone` in the suites,
and fail when the mode `compare` found a mismatch.

    python3.13 plone/catalog_counts.py FILE

FILE holds one JSON line for each process of `bin/test`: the counts that
`zoe.plone.catalog` writes at the exit of a process when
`ZOE_PLONE_CATALOG_COUNTS` names the file. The script adds them up and
prints the searches by path, the reasons of the Python path with their
side, the comparisons, the throwaway mirrors, the differences of the cache
of the request of ZCatalog, and the mismatches with the indexes and the
options of each, never a value. It exits with 1 when a mismatch occurred
or when the file holds no line.
"""

import collections
import json
import sys

(path,) = sys.argv[1:]
lines = []
try:
    with open(path) as f:
        lines = [json.loads(line) for line in f if line.strip()]
except FileNotFoundError:
    pass
if not lines:
    print("catalog: the suites wrote no counts of the catalog backend")
    sys.exit(1)
queries = collections.Counter()
fallbacks = collections.Counter()
total = collections.Counter()
mismatched = collections.Counter()
for line in lines:
    queries.update(line["queries"])
    for reason, side, n in line["fallbacks"]:
        fallbacks[(reason, side)] += n
    for name in ("compared", "throwaways", "mismatches", "cache_differences"):
        total[name] += line[name]
    mismatched.update(line["mismatched"])
searches = sum(queries.values())
print(f"catalog: {len(lines)} processes, {searches} searches")
for name, n in sorted(queries.items()):
    print(f"catalog: path {name}: {n} ({100 * n / max(1, searches):.1f} %)")
for (reason, side), n in sorted(fallbacks.items(), key=lambda item: -item[1]):
    print(
        f"catalog: reason {reason} ({side}): {n} ({100 * n / max(1, searches):.1f} %)"
    )
for name in ("compared", "throwaways", "cache_differences", "mismatches"):
    print(f"catalog: {name}: {total[name]}")
for shape, n in mismatched.most_common():
    print(f"catalog: mismatch, {n} times: {shape}")
sys.exit(1 if total["mismatches"] else 0)
