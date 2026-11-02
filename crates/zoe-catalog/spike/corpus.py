"""Write the texts of a catalog fixture plan as JSON lines.

Usage: python corpus.py PRESET OUT.jsonl

Each line holds the position of the entry (`rid`), its type, its state,
its creator, its dates in minutes, and the texts of `Title`, `Description`
and `SearchableText`. The plan is the plan of `plonebench.catalog.plan`,
so the texts equal the texts of the fixture.
"""

import json
import sys

from plonebench.catalog import plan


def main() -> None:
    preset, out = sys.argv[1], sys.argv[2]
    spec = plan.PRESETS[preset]
    with open(out, "w") as f:
        for n, values in enumerate(plan.Entries(spec)):
            row = {
                "rid": n,
                "type": values["portal_type"],
                "state": values.get("review_state", ""),
                "creator": values["Creator"],
                "effective": values["effective"],
                "modified": values["modified"],
                "Title": values["Title"],
                "Description": values["Description"],
                "SearchableText": values["SearchableText"],
            }
            f.write(json.dumps(row, separators=(",", ":")))
            f.write("\n")


if __name__ == "__main__":
    main()
