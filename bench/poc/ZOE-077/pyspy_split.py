"""Split the py-spy samples of the zodbshootout client by layer. Not for merge.

Input: the raw output of wrap-pyspy.sh (one collapsed stack and a count per
line). A sample of the I/O thread of ZEO.ClientStorage counts as "ZEO I/O
thread". Any other sample counts for the package of its deepest frame that
is not in the standard library. With --strip, the script also rewrites the
file without the process frames, which hold the command lines and paths.

    python pyspy_split.py results/pyspy/*.txt
"""

import argparse
import re
from collections import Counter

LAYERS = [
    ("ZEO/", "ZEO client, caller threads"),
    ("ZODB/", "ZODB"),
    ("persistent/", "ZODB"),
    ("BTrees/", "ZODB"),
    ("zope/", "ZODB"),
    ("transaction/", "transaction"),
    ("zodbshootout/", "benchmark"),
    ("pyperf/", "benchmark"),
]
ORDER = ["ZEO I/O thread", "ZEO client, caller threads", "ZODB", "transaction", "benchmark", "standard library"]


def frames(line):
    stack, count = line.rsplit(" ", 1)
    parts = [f for f in stack.split(";") if not f.startswith("process ")]
    return parts, int(count)


def layer(parts):
    if any("run_io_thread (ZEO/asyncio" in f for f in parts):
        return "ZEO I/O thread"
    for f in reversed(parts):
        m = re.search(r"\(([^:]+):\d+\)$", f)
        path = m.group(1) if m else ""
        for prefix, name in LAYERS:
            if path.startswith(prefix):
                return name
    return "standard library"


def main():
    p = argparse.ArgumentParser()
    p.add_argument("files", nargs="+")
    p.add_argument("--strip", action="store_true")
    args = p.parse_args()
    print("| profile | samples | " + " | ".join(ORDER) + " | torn lines |")
    print("|---|---|" + "---|" * (len(ORDER) + 1))
    for path in args.files:
        lines = [x for x in open(path, encoding="utf-8", errors="replace").read().splitlines() if x.strip()]
        shares = Counter()
        kept = []
        torn = 0
        for line in lines:
            # Sampling without a stop gives a few torn stacks. Skip them.
            if not re.search(r" \d+$", line) or "\ufffd" in line:
                torn += 1
                continue
            parts, count = frames(line)
            shares[layer(parts)] += count
            kept.append(";".join(parts) + f" {count}")
        total = sum(shares.values())
        cells = " | ".join(f"{100 * shares[k] / total:.0f} %" for k in ORDER)
        print(f"| {path.rsplit('/', 1)[-1]} | {total} | {cells} | {torn} |")
        if args.strip:
            with open(path, "w") as f:
                f.write("\n".join(kept) + "\n")


if __name__ == "__main__":
    main()
