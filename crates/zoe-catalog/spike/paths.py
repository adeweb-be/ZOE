"""Write the path of each entry of a catalog fixture plan: `n<TAB>path`."""

import sys

from plonebench.catalog import plan

with open(sys.argv[2], "w") as f:
    for n, values in enumerate(plan.Entries(plan.PRESETS[sys.argv[1]])):
        f.write(f"{n}\t{values['_path']}\n")
