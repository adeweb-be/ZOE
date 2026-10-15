"""The gate report: one storage against a baseline, cell by cell.

`report` reads the result documents of a results tree. For each machine,
Python build and set (quick, gate or full), it keeps the last result of each
storage. It then divides the `ops_per_second` of the storage by the one of
the baseline in each scenario and cell of the baseline. The gate passes
when every ratio that counts is at least the ratio of the gate. A cell of
the baseline without a counterpart in the storage fails the gate, and so
does a tree without a result of the baseline. With a Python build, only
the groups of that build count. The report prints the other groups beside
them.

`default_ratio` gives the default ratio of a storage, and `left_out` the
cells that the gate leaves out for it, by scenario and object size. Both
read constants of this module, not options. For most storages the gate
leaves out `read_hot` in every size, because no read of it reaches the
server through `ZEO.ClientStorage`. The report prints these cells with the
verdict `left out`. They count neither as below nor as missing.

`zoe-native` has the cost rule of ADR 0037 instead, on the gate set only.
The cost of a storage is its time minus the time of the storage `floor`
of the same round. For each cell, the report takes the last run of the
baseline that holds the cell, and the values of the storage and of the
floor of the same run, paired by round. Each scenario and cell of the gate
set counts: a cell that no run holds is missing. A round that lacks one of the
three does not count. The ratio of a round is the cost of the baseline
divided by the cost of the storage. A cost of the storage at or below 0
gives an infinite ratio. A cell passes when the median of its ratios is
at least the ratio of the gate; with an even number of rounds, the median
is the lower of the two middle ratios. A cell where the median cost of the
baseline is below 10 % of its median time gets the verdict `below the
noise`, and counts neither as below nor as missing. A cell without a
complete round is missing. The time is the time of the slowest worker
for one transaction, and the report prints it per object.
"""

import datetime
import json
import math
import statistics
from pathlib import Path
from typing import TextIO

from shootout import GATE, KB, SCENARIOS
from shootout.environment import python_tag
from shootout.layout import run_set

ROW = "{:<10} {:<9} {:>3} {:>7} {:>12} {:>12} {:>7}  {}"
# A row of the cost rule: the cell, the median times per object in µs of
# the baseline, the storage and the floor, the ratio of the whole times,
# the median and the range of the ratios of the costs, the rounds that
# count and the verdict.
COST_ROW = "{:<10} {:<9} {:>3} {:>7} {:>10} {:>10} {:>10} {:>6} {:>6} {:>11} {:>2}  {}"

# The cells that the gate leaves out, as (scenario, object size) with a
# reason. A size of None means every size.
LeftOut = dict[tuple[str, int | None], str]

# The cells that the gate leaves out for a storage that LEFT_OUT_OF does
# not name.
LEFT_OUT: LeftOut = {
    ("read_hot", None): "it empties the object cache of the connection, then "
    "reads the objects from the cache of ZEO.ClientStorage, so no read "
    "reaches the server",
}

# The cells that the gate leaves out for each other storage. The cost rule
# of zoe-native counts every cell.
LEFT_OUT_OF: dict[str, LeftOut] = {"zoe-native": {}}

# The storages of the cost rule (ADR 0037), the storage of the floor, and
# the share of the time of the baseline below which its cost is noise.
COST = frozenset({"zoe-native"})
FLOOR = "floor"
NOISE = 0.10

# The default ratio of each storage: 3 for the phase 3 gate. Any other
# storage gets parity, the phase 2 gate.
RATIOS = {"zoe-native": 3.0}
PARITY = 1.0


def default_ratio(storage: str) -> float:
    """Return the default ratio of the gate of a storage."""
    return RATIOS.get(storage, PARITY)


def left_out(storage: str) -> LeftOut:
    """Return the cells that the gate leaves out for a storage."""
    return LEFT_OUT_OF.get(storage, LEFT_OUT)


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
    if storage in COST:
        return _cost_report(root, storage, baseline, ratio, out, python)
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


def _cost_report(
    root: Path,
    storage: str,
    baseline: str,
    ratio: float,
    out: TextIO,
    python: str | None,
) -> int:
    """Write the report of the cost rule. Return 0 when the gate passes."""
    runs = _gate_results(root)
    groups = sorted(group for group in runs if baseline in runs[group])
    counted = [group for group in groups if python in (None, group[1])]
    where = f" on {python}" if python else ""
    if not counted:
        print(f"gate: no gate result of {baseline}{where} under {root}", file=out)
        return 1

    below = missing = noise = total = 0
    for group in counted + [g for g in groups if g not in counted]:
        machine, build, kind = group
        counts = group in counted
        beside = "" if counts else f" (beside {python}, does not count)"
        print(f"== {machine} {build} {kind}{beside}", file=out)
        for name in (baseline, storage, FLOOR):
            results = runs[group].get(name, [])
            names = ", ".join(_name(r) for r in results) or "no result"
            print(f"{name}: {names}", file=out)
        print(
            COST_ROW.format(
                "scenario",
                "mode",
                "c",
                "size",
                f"{baseline} us",
                f"{storage} us",
                f"{FLOOR} us",
                "whole",
                "cost",
                "range",
                "n",
                "",
            ),
            file=out,
        )
        for cell in _every_cell():
            row = _cost_row(runs[group], cell, storage, baseline, ratio)
            verdict = row[-1]
            if counts:
                total += 1
                missing += verdict == "missing"
                below += verdict == "FAIL"
                noise += verdict == "below the noise"
            else:
                # A row beside the gate gives its verdict in lower case.
                row[-1] = verdict.lower()
            print(COST_ROW.format(*row), file=out)

    passed = below == 0 and missing == 0
    print(
        f"gate {'passed' if passed else 'failed'}: {storage} against {baseline}"
        f"{where}, a cost ratio of at least {ratio:g} over {FLOOR} in each cell "
        f"(ADR 0037); {total} cells, {below} below, {missing} missing, "
        f"{noise} below the noise",
        file=out,
    )
    return 0 if passed else 1


def _cost_row(runs, cell, storage: str, baseline: str, ratio: float) -> list:
    """Return the fields of the row of a cell under the cost rule."""
    held = [(_started(r), e) for r in runs[baseline] if (e := _entry(r, cell))]
    if not held:
        return [*cell, "-", "-", "-", "-", "-", "-", 0, "missing"]
    base_started, base = max(held, key=lambda pair: pair[0])
    rounds = {}
    for name in (baseline, storage, FLOOR):
        entry = base if name == baseline else None
        for result in runs.get(name, []):
            if entry is None and _started(result) == base_started:
                entry = _entry(result, cell)
        for value in entry["values"] if entry else ():
            rounds.setdefault(value["round"], {})[name] = value["seconds"]
    complete = [r for r in rounds.values() if len(r) == 3]
    if not complete:
        return [*cell, "-", "-", "-", "-", "-", "-", 0, "missing"]
    objects = base["objects_per_transaction"]
    times = {
        name: statistics.median(r[name] for r in complete)
        for name in (baseline, storage, FLOOR)
    }
    ratios = [_cost_ratio(r[baseline], r[storage], r[FLOOR]) for r in complete]
    # With an even number of rounds, the lower of the two middle ratios.
    median = statistics.median_low(ratios)
    cost = statistics.median(r[baseline] - r[FLOOR] for r in complete)
    if cost < NOISE * times[baseline]:
        verdict = "below the noise"
    elif median < ratio:
        verdict = "FAIL"
    else:
        verdict = "pass"
    return [
        *cell,
        *(f"{times[name] / objects * 1e6:.1f}" for name in (baseline, storage, FLOOR)),
        f"{times[baseline] / times[storage]:.2f}",
        _ratio(median),
        f"{_ratio(min(ratios))}-{_ratio(max(ratios))}",
        len(complete),
        verdict,
    ]


def _cost_ratio(base: float, storage: float, floor: float) -> float:
    """Return the cost of base over the cost of storage, above floor."""
    cost = storage - floor
    return math.inf if cost <= 0 else (base - floor) / cost


def _ratio(value: float) -> str:
    return "inf" if math.isinf(value) else f"{value:.2f}"


def _entry(result: dict, cell: tuple[str, str, int, int]) -> dict | None:
    """Return the scenario entry of a cell of a result, when it has values."""
    for s in result["scenarios"]:
        if (s["name"], s["mode"], s["concurrency"], s["object_size"]) == cell:
            return s if s.get("values") else None
    return None


def _every_cell() -> list[tuple[str, str, int, int]]:
    """Return each scenario and cell of the gate set, in the order of the report."""
    return sorted(
        (name, cell.mode, cell.concurrency, cell.object_size)
        for name, _ in SCENARIOS.values()
        for cell in GATE.cells
    )


def _gate_results(root: Path) -> dict[tuple[str, str, str], dict[str, list[dict]]]:
    """Return every result of the gate set under root, by group and storage.

    A gate job can run some cells only, so a group can hold several runs of
    one storage.
    """
    runs = {}
    for path in sorted((root / "gate").glob("*/*/*/*.json")):
        result = json.loads(path.read_text())
        group = (result["machine"]["name"], python_tag(result["python"]), "gate")
        runs.setdefault(group, {}).setdefault(result["storage"], []).append(result)
    for group in runs.values():
        for results in group.values():
            results.sort(key=_started)
    return runs


def _last_results(root: Path) -> dict[tuple[tuple[str, str, str], str], dict]:
    """Return the last result of each group and storage under root.

    A group is a machine, a Python build and a set. The last result is the
    one with the latest start.
    """
    last = {}
    for kind in ("results", "quick", "gate"):
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
