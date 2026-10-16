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

`left_out` gives the cells that the gate leaves out, by scenario and object
size. It reads a constant of this module, not an option. The gate leaves
out `read_hot` in every size, because no read of it reaches the server
through `ZEO.ClientStorage`. The report prints these cells with the verdict
`left out`. They count neither as below nor as missing.

This is the parity gate of phase 2. The phase 3 gate is the native gate,
in `shootout.native`.
"""

import datetime
import json
from pathlib import Path
from typing import TextIO

from shootout import KB
from shootout.environment import python_tag
from shootout.layout import run_set

ROW = "{:<10} {:<9} {:>3} {:>7} {:>12} {:>12} {:>7}  {}"

# The cells that the gate leaves out, as (scenario, object size) with a
# reason. A size of None means every size.
LeftOut = dict[tuple[str, int | None], str]

LEFT_OUT: LeftOut = {
    ("read_hot", None): "it empties the object cache of the connection, then "
    "reads the objects from the cache of ZEO.ClientStorage, so no read "
    "reaches the server",
}

# The default ratio of the gate: parity, the phase 2 gate.
PARITY = 1.0


def left_out() -> LeftOut:
    """Return the cells that the gate leaves out."""
    return LEFT_OUT


def _is_left_out(cell: tuple[str, str, int, int], skipped: LeftOut) -> bool:
    name, _, _, size = cell
    return (name, None) in skipped or (name, size) in skipped


def _label(name: str, size: int | None) -> str:
    """Name the cells of a scenario that the gate leaves out."""
    return name if size is None else f"{name} at {size // KB} KB"


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
    skipped = left_out()

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
            if _is_left_out(cell, skipped):
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

    labels = [_label(*key) for key in skipped]
    for label, reason in zip(labels, skipped.values(), strict=True):
        print(f"left out: {label}, because {reason}", file=out)
    passed = below == 0 and missing == 0
    cells = f"each cell but {', '.join(labels)}" if skipped else "each cell"
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
                run_set(result["run"]),
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
