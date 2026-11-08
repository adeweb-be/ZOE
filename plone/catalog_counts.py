"""Print the counts of the catalog backend of `zoe.plone` in the suites,
and fail when the mode `compare` found a mismatch.

    python3.13 plone/catalog_counts.py FILE

FILE holds one JSON line for each process of `bin/test`: the counts that
`zoe.plone.catalog` writes at the exit of a process when
`ZOE_PLONE_CATALOG_COUNTS` names the file. The script adds them up and
prints first the shares of the searches that the mode `compare` compared
and that the mirror answered, then the searches by path, the reasons of
the Python path with their side, the overlays past the bound, the text of
each refusal of a throwaway build, the comparisons, the throwaway mirrors,
the differences of the cache of the request of ZCatalog, the searches left
unchecked by their reason, and the mismatches with the indexes and the
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
unchecked = collections.Counter()
refused = collections.Counter()
mismatched = collections.Counter()
for line in lines:
    queries.update(line["queries"])
    for reason, side, n in line["fallbacks"]:
        fallbacks[(reason, side)] += n
    for name in ("compared", "throwaways", "mismatches", "cache_differences"):
        total[name] += line[name]
    total["overlay_bound"] += line.get("overlay_bound", 0)
    total["stock_errors"] += line.get("stock_errors", 0)
    unchecked.update(line.get("unchecked", {}))
    refused.update(line.get("refused", {}))
    mismatched.update(line["mismatched"])
searches = sum(queries.values())


def share(n):
    return f"{100 * n / max(1, searches):.1f} %"


print(f"catalog: {len(lines)} processes, {searches} searches")
# The coverage first: the share of the searches that the mode compare
# compared, and the share that the mirror answered.
print(
    f"catalog: compared {total['compared']} of {searches} ({share(total['compared'])})"
)
native = queries.get("native", 0)
print(f"catalog: native {native} of {searches} ({share(native)})")
for name, n in sorted(queries.items()):
    print(f"catalog: path {name}: {n} ({share(n)})")
for (reason, side), n in sorted(fallbacks.items(), key=lambda item: -item[1]):
    print(f"catalog: reason {reason} ({side}): {n} ({share(n)})")
print(f"catalog: overlay past the bound: {total['overlay_bound']}")
print(f"catalog: native answers whose stock search raised: {total['stock_errors']}")
for why, n in refused.most_common():
    print(f"catalog: refused, {n} times: {why}")
for why, n in sorted(unchecked.items()):
    print(f"catalog: unchecked ({why}): {n}")
total["unchecked"] = sum(unchecked.values())
for name in ("compared", "throwaways", "cache_differences", "unchecked", "mismatches"):
    print(f"catalog: {name}: {total[name]}")
for shape, n in mismatched.most_common():
    print(f"catalog: mismatch, {n} times: {shape}")
sys.exit(1 if total["mismatches"] else 0)
