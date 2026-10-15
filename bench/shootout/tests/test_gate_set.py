"""Unit tests of the gate set: its cells, its values and where it goes."""

import io
import json

import shootout
from shootout import GATE, KB, Cell, gate, set_name
from shootout.layout import result_path, run_set


def test_the_gate_set_is_six_cells_with_seven_rounds():
    assert set(GATE.cells) == {
        Cell("threads", 1, KB),
        Cell("threads", 16, KB),
        Cell("threads", 1, 100 * KB),
        Cell("threads", 16, 100 * KB),
        Cell("processes", 16, KB),
        Cell("processes", 16, 100 * KB),
    }
    assert set(GATE.cells) <= set(shootout.FULL.cells)
    assert GATE.processes * GATE.values == 7


def test_the_name_of_a_set():
    assert set_name(quick=True, gate=False) == "quick"
    assert set_name(quick=False, gate=True) == "gate"
    assert set_name(quick=False, gate=False) == "full"


def result(storage, run, ops):
    return {
        "storage": storage,
        "machine": {"name": "reference"},
        "python": {
            "implementation": "cpython",
            "version": "3.14.0",
            "free_threaded": True,
        },
        "run": run,
        "scenarios": [
            {
                "name": "add",
                "mode": "threads",
                "concurrency": 16,
                "object_size": KB,
                "ops_per_second": ops,
            }
        ],
    }


GATE_RUN = {
    "quick": False,
    "set": "gate",
    "started": "2026-01-02T00:00:00Z",
    "commit": None,
}
FULL_RUN = {"quick": False, "started": "2026-01-01T00:00:00Z", "commit": None}


def test_a_gate_run_goes_under_gate_and_an_old_full_run_under_results(tmp_path):
    assert run_set(GATE_RUN) == "gate"
    assert run_set(FULL_RUN) == "full"
    gate_path = result_path(tmp_path, result("zeo", GATE_RUN, 1.0))
    full_path = result_path(tmp_path, result("zeo", FULL_RUN, 1.0))
    assert gate_path.relative_to(tmp_path).parts[0] == "gate"
    assert full_path.relative_to(tmp_path).parts[0] == "results"


def test_the_gate_report_counts_a_gate_run_as_its_own_set(tmp_path):
    for storage, ops in (("zeo", 1000.0), ("zoe-server", 3500.0)):
        doc = result(storage, GATE_RUN, ops)
        path = result_path(tmp_path, doc)
        path.parent.mkdir(parents=True)
        path.write_text(json.dumps(doc))
    out = io.StringIO()
    code = gate.report(tmp_path, "zoe-server", "zeo", 3.0, out)
    lines = out.getvalue().splitlines()
    assert code == 0, lines
    assert lines[0] == "== reference cp314t gate"
