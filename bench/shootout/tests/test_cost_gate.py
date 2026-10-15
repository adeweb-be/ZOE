"""Acceptance test of the cost rule of the phase 3 gate (ADR 0037).

The gate set runs each cell in 7 rounds. A round runs one value of `zeo`,
`zoe-native` and `floor`, each on a fresh store, in an order that rotates
from round to round. Each scenario entry of a result keeps the round and
the time of each value, in `values`.

`gate.report` for `zoe-native` reads the gate set. For each round of a
cell, it computes `(t_zeo - t_floor) / (t_zoe - t_floor)`, where `t` is
the time of the slowest worker. A cost of `zoe-native` at or below 0
passes. A cell passes when the median of its ratios is at least 3. A cell
where the median cost of ZEO is below 10 % of the median time of ZEO is
`below the noise`, and counts neither as below nor as missing. A round
that lacks a storage does not count. The parity gate of phase 2 keeps its
rules.

The tests write synthetic results.
"""

import io
import json
import pathlib

import jsonschema
import pytest

import shootout
from shootout import GATE, KB, Cell, gate, run_gate

SCHEMA = json.loads(
    (pathlib.Path(__file__).parents[2] / "schema" / "results.schema.json").read_text()
)
ROUNDS = 7
CELL = ("threads", 16, KB)
STARTED = "2026-01-02T00:00:00Z"


def entry(name, cell, times):
    """Return a scenario entry with the time of each round in times.

    A time of None leaves its round out.
    """
    mode, concurrency, size = cell
    values = [
        {"round": number, "seconds": seconds}
        for number, seconds in enumerate(times)
        if seconds is not None
    ]
    seconds = [value["seconds"] for value in values]
    mean = sum(seconds) / len(seconds)
    return {
        "name": name,
        "mode": mode,
        "concurrency": concurrency,
        "object_size": size,
        "objects_per_transaction": 100,
        "samples": len(seconds),
        "ops_per_second": concurrency * 100 / mean,
        "mean_seconds": mean,
        "p50_seconds": sorted(seconds)[len(seconds) // 2],
        "p99_seconds": max(seconds),
        "values": values,
    }


def write(root, storage, scenarios, started=STARTED, python="cp314t"):
    """Write a gate result of storage. scenarios maps (name, cell) to times."""
    builds = {"cp313": ("3.13.0", False), "cp314t": ("3.14.0", True)}
    version, free_threaded = builds[python]
    result = {
        "schema_version": 1,
        "storage": storage,
        "machine": {
            "name": "reference",
            "cpu": "synthetic",
            "logical_cpus": 8,
            "memory_bytes": 1 << 34,
            "os": "linux",
            "kernel": "6",
            "arch": "x86_64",
            "disk": None,
        },
        "python": {
            "implementation": "cpython",
            "version": version,
            "free_threaded": free_threaded,
            "compiler": "gcc",
        },
        "software": {"zodbshootout": "0", "ZODB": "6"},
        "run": {"quick": False, "set": "gate", "started": started, "commit": None},
        "scenarios": [
            entry(name, cell, times) for (name, cell), times in scenarios.items()
        ],
    }
    jsonschema.validate(result, SCHEMA)
    stamp = started.replace(":", "").replace("-", "")
    path = root / "gate" / "reference" / python / storage / f"{stamp}.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(result))


# Each scenario and cell of the gate set, as (name, cell).
EVERY = [
    (name, (cell.mode, cell.concurrency, cell.object_size))
    for name, _ in shootout.SCENARIOS.values()
    for cell in GATE.cells
]
CELLS = len(EVERY)


def fill(root, python="cp314t"):
    """Write an earlier gate run that passes in each cell of the gate set.

    A test writes the cells that it checks in a later run, which wins.
    """
    earlier = "2026-01-01T00:00:00Z"
    for storage, seconds in (("zeo", 1.0), ("floor", 0.25), ("zoe-native", 0.5)):
        times = dict.fromkeys(EVERY, [seconds] * ROUNDS)
        write(root, storage, times, started=earlier, python=python)


def native_for(ratios, zeo=1.0, floor=0.25):
    """Return the times of zoe-native that give each cost ratio of ratios."""
    return [floor + (zeo - floor) / ratio for ratio in ratios]


def write_three(root, native, zeo=None, floor=None, name="add", cell=CELL):
    write(root, "zeo", {(name, cell): zeo or [1.0] * ROUNDS})
    write(root, "floor", {(name, cell): floor or [0.25] * ROUNDS})
    write(root, "zoe-native", {(name, cell): native})


def report(root, storage="zoe-native", python="cp314t"):
    out = io.StringIO()
    code = gate.report(root, storage, "zeo", gate.default_ratio(storage), out, python)
    return code, out.getvalue().splitlines()


def row(lines, name, cell=CELL):
    """Return the row of a scenario and cell, in the first group."""
    key = [name, cell[0], str(cell[1]), str(cell[2])]
    return next(line for line in lines if line.split()[:4] == key)


def test_the_gate_set_has_seven_rounds():
    assert GATE.values == ROUNDS
    assert GATE.processes == 1


def test_a_cell_passes_and_fails_on_the_median_of_its_ratios(tmp_path):
    # Three rounds below 3 and four above: the median passes.
    fill(tmp_path / "pass")
    write_three(tmp_path / "pass", native_for([2.0] * 3 + [3.5] * 4))
    code, lines = report(tmp_path / "pass")
    assert code == 0, lines
    line = row(lines, "add")
    assert line.endswith("pass"), line
    assert "3.50" in line.split() and "2.00-3.50" in line.split(), line
    # Four rounds below 3 and three above: the median fails.
    fill(tmp_path / "fail")
    write_three(tmp_path / "fail", native_for([2.5] * 4 + [4.0] * 3))
    code, lines = report(tmp_path / "fail")
    assert code == 1, lines
    line = row(lines, "add")
    assert line.endswith("FAIL"), line
    assert "2.50" in line.split(), line
    assert lines[-1].startswith("gate failed: zoe-native against zeo on cp314t")
    assert lines[-1].endswith(f"{CELLS} cells, 1 below, 0 missing, 0 below the noise")


def test_the_report_prints_the_times_the_whole_ratio_and_the_cost_ratio(tmp_path):
    fill(tmp_path)
    write_three(tmp_path, native_for([3.0] * ROUNDS))
    code, lines = report(tmp_path)
    assert code == 0, lines
    words = row(lines, "add").split()
    # The medians of the times per object, in microseconds: 1 s, 0.5 s and
    # 0.25 s per transaction of 100 objects.
    assert words[4:7] == ["10000.0", "5000.0", "2500.0"], words
    # The ratio of the whole times, then the median, the range and the
    # number of the ratios of the costs.
    assert words[7:11] == ["2.00", "3.00", "3.00-3.00", "7"], words


def test_a_cost_of_zoe_native_at_or_below_zero_passes(tmp_path):
    fill(tmp_path)
    write_three(tmp_path, [0.25] * 3 + [0.2] * 4)
    code, lines = report(tmp_path)
    assert code == 0, lines
    line = row(lines, "add")
    assert line.endswith("pass"), line
    assert "inf" in line.split(), line


def test_a_cell_below_the_noise_is_reported_and_does_not_count(tmp_path):
    # The cost of ZEO is 5 % of its time, and zoe-native is slower.
    fill(tmp_path)
    write(
        tmp_path,
        "zeo",
        {("add", CELL): [1.0] * ROUNDS, ("update", CELL): [1.0] * ROUNDS},
        started="2026-01-03T00:00:00Z",
    )
    write(
        tmp_path,
        "floor",
        {("add", CELL): [0.95] * ROUNDS, ("update", CELL): [0.25] * ROUNDS},
        started="2026-01-03T00:00:00Z",
    )
    write(
        tmp_path,
        "zoe-native",
        {("add", CELL): [1.2] * ROUNDS, ("update", CELL): native_for([3.0] * ROUNDS)},
        started="2026-01-03T00:00:00Z",
    )
    code, lines = report(tmp_path)
    assert code == 0, lines
    assert row(lines, "add").endswith("below the noise"), lines
    assert row(lines, "update").endswith("pass"), lines
    assert lines[-1].endswith(f"{CELLS} cells, 0 below, 0 missing, 1 below the noise")


def test_the_noise_is_the_median_of_the_costs_of_the_rounds(tmp_path):
    # The median cost of ZEO over the rounds is 0.2, 20 % of its median
    # time, so the cell counts. The difference of the median times would be
    # 0.05, 5 %.
    fill(tmp_path)
    zeo = [1.0] * 4 + [1.2] * 3
    write_three(tmp_path, zeo, zeo=zeo, floor=[0.8] * 3 + [0.95] * 4)
    code, lines = report(tmp_path)
    assert code == 1, lines
    assert row(lines, "add").endswith("FAIL"), lines


def test_with_an_even_number_of_rounds_the_median_is_the_lower_ratio(tmp_path):
    # Three rounds at 2 and three where zoe-native costs nothing.
    fill(tmp_path)
    write_three(tmp_path, native_for([2.0] * 3) + [0.2] * 3 + [None])
    code, lines = report(tmp_path)
    assert code == 1, lines
    words = row(lines, "add").split()
    assert words[8:11] == ["2.00", "2.00-inf", "6"], words


def test_a_cell_of_the_gate_set_that_no_run_holds_is_missing(tmp_path):
    # One job ran one cell only: the other cells of the gate set are missing.
    write_three(tmp_path, native_for([3.0] * ROUNDS))
    code, lines = report(tmp_path)
    assert code == 1, lines
    assert row(lines, "add").endswith("pass"), lines
    assert row(lines, "add", ("processes", 16, 100 * KB)).endswith("missing")
    assert lines[-1].endswith(
        f"{CELLS} cells, 0 below, {CELLS - 1} missing, 0 below the noise"
    )


def test_a_round_that_lacks_a_storage_does_not_count(tmp_path):
    # Round 6 of zoe-native is missing, and round 5 of the floor: the five
    # other rounds count. Rounds 5 and 6 of zoe-native would fail.
    native = native_for([4.0] * 5 + [1.0]) + [None]
    floor = [0.25] * 5 + [None, 0.25]
    fill(tmp_path)
    write_three(tmp_path, native, floor=floor)
    code, lines = report(tmp_path)
    assert code == 0, lines
    words = row(lines, "add").split()
    assert words[8:11] == ["4.00", "4.00-4.00", "5"], words


def test_a_cell_without_the_floor_or_zoe_native_is_missing(tmp_path):
    other = ("threads", 1, KB)
    fill(tmp_path)
    write(tmp_path, "zeo", {("add", CELL): [1.0] * ROUNDS, ("add", other): [1.0] * 7})
    write(tmp_path, "floor", {("add", CELL): [0.25] * ROUNDS})
    write(
        tmp_path,
        "zoe-native",
        {("add", CELL): native_for([3.0] * 7), ("add", other): [0.1] * 7},
    )
    code, lines = report(tmp_path)
    assert code == 1, lines
    assert row(lines, "add").endswith("pass"), lines
    assert row(lines, "add", other).endswith("missing"), lines
    assert lines[-1].endswith(f"{CELLS} cells, 0 below, 1 missing, 0 below the noise")


def test_each_cell_can_come_from_its_own_run(tmp_path):
    # Two gate runs, one per cell: the rounds of a cell pair within its run.
    other = ("processes", 16, 100 * KB)
    fill(tmp_path)
    write_three(tmp_path, native_for([3.0] * ROUNDS))
    later = "2026-01-05T00:00:00Z"
    write(tmp_path, "zeo", {("add", other): [1.0] * ROUNDS}, started=later)
    write(tmp_path, "floor", {("add", other): [0.25] * ROUNDS}, started=later)
    write(
        tmp_path, "zoe-native", {("add", other): native_for([2.0] * 7)}, started=later
    )
    code, lines = report(tmp_path)
    assert code == 1, lines
    assert row(lines, "add").endswith("pass"), lines
    assert row(lines, "add", other).endswith("FAIL"), lines


def test_the_left_out_cells_of_zoe_native_are_gone(tmp_path):
    assert gate.left_out("zoe-native") == {}
    hot = ("read_hot", ("threads", 1, KB))
    fill(tmp_path)
    write(tmp_path, "zeo", {hot: [1.0] * ROUNDS})
    write(tmp_path, "floor", {hot: [0.5] * ROUNDS})
    write(tmp_path, "zoe-native", {hot: [0.8] * ROUNDS})
    code, lines = report(tmp_path)
    assert code == 1, lines
    assert row(lines, "read_hot", hot[1]).endswith("FAIL"), lines
    assert not any("left out" in line for line in lines), lines


def test_a_build_counts_and_the_other_prints_beside_it(tmp_path):
    fill(tmp_path)
    fill(tmp_path, python="cp313")
    write_three(tmp_path, native_for([3.0] * ROUNDS))
    for storage, times in (
        ("zeo", [1.0] * 7),
        ("floor", [0.25] * 7),
        ("zoe-native", native_for([2.0] * 7)),
    ):
        write(tmp_path, storage, {("add", CELL): times}, python="cp313")
    code, lines = report(tmp_path)
    assert code == 0, lines
    headers = [line for line in lines if line.startswith("== ")]
    assert headers == [
        "== reference cp314t gate",
        "== reference cp313 gate (beside cp314t, does not count)",
    ]
    key = ["add", *map(str, CELL)]
    rows = [line for line in lines if line.split()[:4] == key]
    assert rows[0].endswith("pass") and rows[1].endswith("fail"), rows


class FakeValues:
    """Stand for `_run_value`: record the order of the values."""

    def __init__(self):
        self.order = []

    def __call__(self, store, cell, bench, workdir, dsn):
        self.order.append((bench, cell, store.name))
        seconds = {"zeo": 1.0, "zoe-native": 0.4, "floor": 0.1}.get(store.name, 1.0)
        return [seconds], [{"gil_enabled": False, "gil_enabled_by": []}]


def test_the_order_of_the_storages_rotates_from_round_to_round(monkeypatch, tmp_path):
    fake = FakeValues()
    monkeypatch.setattr(shootout, "_run_value", fake)
    storages = ["zeo", "zoe-native", "floor"]
    cells = [Cell("threads", 16, KB), Cell("processes", 16, KB)]
    results = run_gate(storages, workdir=tmp_path, cells=cells)
    per = {}
    for bench, cell, name in fake.order:
        per.setdefault((bench, cell), []).append(name)
    assert len(per) == len(shootout.SCENARIOS) * len(cells)
    for names in per.values():
        rounds = [names[i : i + 3] for i in range(0, len(names), 3)]
        assert len(rounds) == ROUNDS
        for number, order in enumerate(rounds):
            shift = number % 3
            assert order == storages[shift:] + storages[:shift], names
    # One result per storage, of one run, with the round of each value.
    assert [r["storage"] for r in results] == storages
    assert len({r["run"]["started"] for r in results}) == 1
    for result in results:
        jsonschema.validate(result, SCHEMA)
        assert result["run"]["set"] == "gate"
        for scenario in result["scenarios"]:
            assert [v["round"] for v in scenario["values"]] == list(range(ROUNDS))
    assert list(tmp_path.iterdir()) == []


def test_a_cell_that_no_storage_of_the_run_takes_is_skipped(monkeypatch, tmp_path):
    fake = FakeValues()
    monkeypatch.setattr(shootout, "_run_value", fake)
    # Only one process can open a store of zoe: it takes no processes cell.
    cells = [Cell("threads", 16, KB), Cell("processes", 16, KB)]
    [result] = run_gate(["zoe"], workdir=tmp_path, cells=cells)
    assert {cell.mode for _, cell, _ in fake.order} == {"threads"}
    assert {s["mode"] for s in result["scenarios"]} == {"threads"}


def test_a_cell_outside_the_gate_set_is_refused(tmp_path):
    with pytest.raises(ValueError, match="gate set"):
        run_gate(["zeo"], workdir=tmp_path, cells=[Cell("threads", 4, KB)])


def ops_result(root, storage, ops):
    """Write a gate result of storage with ops per name, as phase 2 reads."""
    write(root, storage, {(name, CELL): [100 * 16 / value] * 7 for name, value in ops})


def test_the_parity_gate_of_phase_2_keeps_its_rules(tmp_path):
    ops_result(tmp_path, "zeo", [("add", 1000.0), ("read_hot", 1000.0)])
    ops_result(tmp_path, "zoe-server", [("add", 1000.0), ("read_hot", 500.0)])
    code, lines = report(tmp_path, "zoe-server")
    assert code == 0, lines
    assert row(lines, "read_hot").split()[-3:] == ["0.50", "left", "out"]
    assert "at least 1x in each cell but read_hot;" in lines[-1]
    assert lines[-1].endswith("2 cells, 0 below, 0 missing, 1 left out")
