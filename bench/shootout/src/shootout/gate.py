"""The gate report: one storage against a baseline, cell by cell.

`report` reads the result documents of a results tree. For each machine,
Python build and set (quick or full), it keeps the last result of each
storage. It then divides the `ops_per_second` of the storage by the one of
the baseline in each scenario and cell of the baseline. The gate passes
when every ratio is at least the ratio of the gate. A cell of the baseline
without a counterpart in the storage fails the gate, and so does a tree
without a result of the baseline.
"""

import datetime
import json
from pathlib import Path
from typing import TextIO

from shootout.environment import python_tag

ROW = "{:<10} {:<9} {:>3} {:>7} {:>12} {:>12} {:>7}  {}"


def report(root: Path, storage: str, baseline: str, ratio: float, out: TextIO) -> int:
    """Write the gate report to out. Return 0 when the gate passes, else 1."""
    last = _last_results(root)
    groups = sorted({group for group, name in last if name == baseline})
    if not groups:
        print(f"gate: no result of {baseline} under {root}", file=out)
        return 1

    below = missing = total = 0
    for group in groups:
        machine, python, kind = group
        base = last[group, baseline]
        other = last.get((group, storage))
        print(f"== {machine} {python} {kind}", file=out)
        print(f"{baseline}: {_name(base)}", file=out)
        print(f"{storage}: {_name(other) if other else 'no result'}", file=out)
        print(
            ROW.format("scenario", "mode", "c", "size", storage, baseline, "ratio", ""),
            file=out,
        )
        cells = _cells(other) if other else {}
        for cell, base_ops in sorted(_cells(base).items()):
            total += 1
            ops = cells.get(cell)
            if ops is None:
                missing += 1
                print(
                    ROW.format(*cell, "-", f"{base_ops:.0f}", "-", "missing"), file=out
                )
                continue
            value = ops / base_ops
            verdict = "pass" if value >= ratio else "FAIL"
            if value < ratio:
                below += 1
            row = ROW.format(
                *cell, f"{ops:.0f}", f"{base_ops:.0f}", f"{value:.2f}", verdict
            )
            print(row, file=out)

    passed = below == 0 and missing == 0
    print(
        f"gate {'passed' if passed else 'failed'}: {storage} against {baseline}, "
        f"at least {ratio:g}x in each cell; {total} cells, {below} below, "
        f"{missing} missing",
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
