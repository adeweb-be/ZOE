"""The gate report: one storage against a baseline, cell by cell.

`report` reads the result documents of a results tree. For each machine,
Python build and set (quick or full), it keeps the last result of each
storage. It then divides the `ops_per_second` of the storage by the one of
the baseline in each scenario and cell of the baseline. The gate passes
when every ratio that counts is at least the ratio of the gate. A cell of
the baseline without a counterpart in the storage fails the gate, and so
does a tree without a result of the baseline. With a Python build, only
the groups of that build count. The report prints the other groups beside
them.

`default_ratio` gives the default ratio of a storage, and `left_out` the
scenarios that the gate leaves out for it. Both read constants of this
module, not options. The gate leaves out the scenarios of `LEFT_OUT` for
each storage but those of `COUNTS_EVERY_SCENARIO`, because no read of
these scenarios reaches the server through `ZEO.ClientStorage`. The report
prints their cells with the verdict `left out`. These cells count neither
as below nor as missing.
"""

import datetime
import json
from pathlib import Path
from typing import TextIO

from shootout.environment import python_tag

ROW = "{:<10} {:<9} {:>3} {:>7} {:>12} {:>12} {:>7}  {}"

# The scenarios that the gate leaves out, each with its reason.
LEFT_OUT = {
    "read_hot": "it empties the object cache of the connection, then reads "
    "the objects from the cache of ZEO.ClientStorage, so no read reaches "
    "the server",
}

# The storages for which every scenario counts. The native client of
# zoe-native has a cache of its own, so its read_hot is a number of ZOE.
COUNTS_EVERY_SCENARIO = frozenset({"zoe-native"})

# The default ratio of each storage: 3 for the phase 3 gate. Any other
# storage gets parity, the phase 2 gate.
RATIOS = {"zoe-native": 3.0}
PARITY = 1.0


def default_ratio(storage: str) -> float:
    """Return the default ratio of the gate of a storage."""
    return RATIOS.get(storage, PARITY)


def left_out(storage: str) -> dict[str, str]:
    """Return the scenarios that the gate leaves out for a storage."""
    return {} if storage in COUNTS_EVERY_SCENARIO else LEFT_OUT


def report(
    root: Path,
    storage: str,
    baseline: str,
    ratio: float,
    out: TextIO,
    python: str | None = None,
) -> int:
    """Write the gate report to out. Return 0 when the gate passes, else 1.

    With python, such as cp314t, only the groups of that Python build count.
    """
    last = _last_results(root)
    groups = sorted({group for group, name in last if name == baseline})
    counted = [group for group in groups if python in (None, group[1])]
    if not counted:
        where = f" on {python}" if python else ""
        print(f"gate: no result of {baseline}{where} under {root}", file=out)
        return 1
    skipped = left_out(storage)

    below = missing = left = total = 0
    for group in counted + [g for g in groups if g not in counted]:
        machine, build, kind = group
        counts = group in counted
        base = last[group, baseline]
        other = last.get((group, storage))
        beside = "" if counts else f" (beside {python}, does not count)"
        print(f"== {machine} {build} {kind}{beside}", file=out)
        print(f"{baseline}: {_name(base)}", file=out)
        print(f"{storage}: {_name(other) if other else 'no result'}", file=out)
        print(
            ROW.format("scenario", "mode", "c", "size", storage, baseline, "ratio", ""),
            file=out,
        )
        cells = _cells(other) if other else {}
        for cell, base_ops in sorted(_cells(base).items()):
            ops = cells.get(cell)
            value = None if ops is None else ops / base_ops
            if cell[0] in skipped:
                verdict = "left out"
            elif value is None:
                verdict = "missing"
            elif value < ratio:
                verdict = "FAIL"
            else:
                verdict = "pass"
            if counts:
                total += 1
                left += verdict == "left out"
                missing += verdict == "missing"
                below += verdict == "FAIL"
            else:
                # A row beside the gate gives its verdict in lower case.
                verdict = verdict.lower()
            row = ROW.format(
                *cell,
                "-" if ops is None else f"{ops:.0f}",
                f"{base_ops:.0f}",
                "-" if value is None else f"{value:.2f}",
                verdict,
            )
            print(row, file=out)

    for name, reason in skipped.items():
        print(f"left out: {name}, because {reason}", file=out)
    passed = below == 0 and missing == 0
    cells = f"each cell but {', '.join(skipped)}" if skipped else "each cell"
    where = f" on {python}" if python else ""
    print(
        f"gate {'passed' if passed else 'failed'}: {storage} against {baseline}"
        f"{where}, at least {ratio:g}x in {cells}; "
        f"{total} cells, {below} below, {missing} missing, {left} left out",
        file=out,
    )
    return 0 if passed else 1


def _last_results(root: Path) -> dict[tuple[tuple[str, str, str], str], dict]:
    """Return the last result of each group and storage under root.

    A group is a machine, a Python build and a set. The last result is the
    one with the latest start.
    """
    last = {}
    for kind in ("results", "quick"):
        for path in sorted((root / kind).glob("*/*/*/*.json")):
            result = json.loads(path.read_text())
            group = (
                result["machine"]["name"],
                python_tag(result["python"]),
                "quick" if result["run"]["quick"] else "full",
            )
            key = (group, result["storage"])
            if key not in last or _started(result) > _started(last[key]):
                last[key] = result
    return last


def _started(result: dict) -> datetime.datetime:
    return datetime.datetime.fromisoformat(result["run"]["started"])


def _name(result: dict) -> str:
    """Name a result by its start and its commit."""
    commit = result["run"]["commit"]
    return f"{result['run']['started']} {commit[:12] if commit else 'no commit'}"


def _cells(result: dict) -> dict[tuple[str, str, int, int], float]:
    """Return the ops per second of each scenario and cell of a result."""
    return {
        (s["name"], s["mode"], s["concurrency"], s["object_size"]): s["ops_per_second"]
        for s in result["scenarios"]
    }
