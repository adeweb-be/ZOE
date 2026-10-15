"""Unit tests of the gate report: the left-out rows and the summary line."""

import io
import json

from shootout import gate


def write(root, storage, ops):
    """Write a result of storage with ops[name] in one cell of each name."""
    result = {
        "storage": storage,
        "machine": {"name": "reference"},
        "python": {
            "implementation": "cpython",
            "version": "3.13.0",
            "free_threaded": False,
        },
        "run": {"quick": False, "started": "2026-01-01T00:00:00+00:00", "commit": None},
        "scenarios": [
            {
                "name": name,
                "mode": "threads",
                "concurrency": 16,
                "object_size": 1024,
                "ops_per_second": value,
            }
            for name, value in ops.items()
        ],
    }
    path = root / "results" / "reference" / "cp313" / storage / "run.json"
    path.parent.mkdir(parents=True)
    path.write_text(json.dumps(result))


def report(root, ratio=1.0):
    out = io.StringIO()
    code = gate.report(root, "zoe-server", "zeo", ratio, out)
    return code, out.getvalue().splitlines()


def test_read_hot_in_every_size_is_the_only_cell_left_out():
    assert set(gate.LEFT_OUT) == {("read_hot", None)}
    assert "ZEO.ClientStorage" in gate.LEFT_OUT["read_hot", None]


def test_the_summary_counts_the_cells_left_out(tmp_path):
    write(tmp_path, "zeo", {"add": 1000.0, "read_hot": 1000.0})
    write(tmp_path, "zoe-server", {"add": 1000.0, "read_hot": 500.0})
    code, lines = report(tmp_path)
    assert code == 0
    hot = next(line for line in lines if line.startswith("read_hot "))
    assert hot.split()[-3:] == ["0.50", "left", "out"]
    assert any(line.startswith("left out: read_hot, because ") for line in lines)
    assert lines[-1].endswith("2 cells, 0 below, 0 missing, 1 left out")


def test_a_left_out_cell_without_a_counterpart_has_no_ratio(tmp_path):
    write(tmp_path, "zeo", {"add": 1000.0, "read_hot": 1000.0})
    write(tmp_path, "zoe-server", {"add": 1000.0})
    code, lines = report(tmp_path)
    assert code == 0
    hot = next(line for line in lines if line.startswith("read_hot "))
    assert hot.split()[4:] == ["-", "1000", "-", "left", "out"]


def test_an_explicit_ratio_sets_every_cell_that_counts(tmp_path):
    write(tmp_path, "zeo", {"add": 1000.0, "update": 1000.0, "read_hot": 1000.0})
    write(tmp_path, "zoe-server", {"add": 3000.0, "update": 2900.0, "read_hot": 0.1})
    code, lines = report(tmp_path, ratio=3.0)
    assert code == 1
    assert [line.split()[0] for line in lines if "FAIL" in line] == ["update"]
    assert lines[-1].startswith("gate failed: ")
    assert lines[-1].endswith("3 cells, 1 below, 0 missing, 1 left out")
