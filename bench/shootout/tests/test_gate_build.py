"""Unit tests of the rules of the gate by storage and by Python build."""

import io
import json

from shootout import KB, gate


def write(root, storage, python, ops, sizes=(KB,)):
    """Write a result of storage on python, such as cp314t, with ops per name.

    Each name has one cell in each object size of sizes.
    """
    builds = {"cp313": ("3.13.0", False), "cp314t": ("3.14.0", True)}
    version, free_threaded = builds[python]
    result = {
        "storage": storage,
        "machine": {"name": "reference"},
        "python": {
            "implementation": "cpython",
            "version": version,
            "free_threaded": free_threaded,
        },
        "run": {"quick": False, "started": "2026-01-01T00:00:00+00:00", "commit": None},
        "scenarios": [
            {
                "name": name,
                "mode": "threads",
                "concurrency": 16,
                "object_size": size,
                "ops_per_second": value,
            }
            for name, value in ops.items()
            for size in sizes
        ],
    }
    path = root / "results" / "reference" / python / storage / "run.json"
    path.parent.mkdir(parents=True)
    path.write_text(json.dumps(result))


def report(root, storage, python=None):
    out = io.StringIO()
    ratio = gate.default_ratio(storage)
    code = gate.report(root, storage, "zeo", ratio, out, python)
    return code, out.getvalue().splitlines()


def test_the_default_ratios_are_constants_of_the_gate():
    assert gate.default_ratio("zoe-native") == 3.0
    assert gate.default_ratio("zoe-server") == 1.0
    assert gate.default_ratio("zoe") == 1.0


def test_zoe_native_leaves_out_no_cell():
    # The cost rule of ADR 0037 counts every cell of zoe-native.
    assert gate.left_out("zoe-native") == {}
    assert gate.left_out("zoe-server") == gate.LEFT_OUT
    assert gate.left_out("zoe") == gate.LEFT_OUT


def verdicts(lines, scenario):
    """Return the object size and the verdict of each row of scenario."""
    return [
        (int(line.split()[3]), " ".join(line.split()[7:]))
        for line in lines
        if line.startswith(f"{scenario} ")
    ]


def test_the_cost_rule_of_zoe_native_reads_the_gate_set_only(tmp_path):
    write(tmp_path, "zeo", "cp313", {"add": 1000.0})
    write(tmp_path, "zoe-native", "cp313", {"add": 3000.0})
    code, lines = report(tmp_path, "zoe-native")
    assert code == 1
    assert lines == [f"gate: no gate result of zeo under {tmp_path}"]


def test_zoe_server_leaves_out_read_hot_in_both_sizes(tmp_path):
    sizes = (KB, 100 * KB)
    write(tmp_path, "zeo", "cp313", {"add": 1000.0, "read_hot": 1000.0}, sizes)
    write(tmp_path, "zoe-server", "cp313", {"add": 1000.0, "read_hot": 500.0}, sizes)
    code, lines = report(tmp_path, "zoe-server")
    assert code == 0
    assert verdicts(lines, "read_hot") == [(KB, "left out"), (100 * KB, "left out")]
    assert [line for line in lines if line.startswith("left out: ")] == [
        f"left out: read_hot, because {gate.LEFT_OUT['read_hot', None]}"
    ]
    assert "at least 1x in each cell but read_hot;" in lines[-1]
    assert lines[-1].endswith("4 cells, 0 below, 0 missing, 2 left out")


def test_a_build_counts_and_the_other_prints_beside_it(tmp_path):
    write(tmp_path, "zeo", "cp314t", {"add": 1000.0})
    write(tmp_path, "zoe-server", "cp314t", {"add": 1000.0})
    write(tmp_path, "zeo", "cp313", {"add": 1000.0})
    write(tmp_path, "zoe-server", "cp313", {"add": 500.0})
    code, lines = report(tmp_path, "zoe-server", "cp314t")
    assert code == 0, lines
    headers = [line for line in lines if line.startswith("== ")]
    assert headers == [
        "== reference cp314t full",
        "== reference cp313 full (beside cp314t, does not count)",
    ]
    rows = [line for line in lines if line.startswith("add ")]
    assert rows[0].endswith("pass")
    assert rows[1].endswith("fail")
    assert lines[-1].endswith("1 cells, 0 below, 0 missing, 0 left out")
    assert " on cp314t," in lines[-1]

    # Without a build, both count.
    code, lines = report(tmp_path, "zoe-server")
    assert code == 1
    assert lines[-1].endswith("2 cells, 1 below, 0 missing, 0 left out")


def test_a_build_without_a_result_of_the_baseline_fails(tmp_path):
    write(tmp_path, "zeo", "cp313", {"add": 1000.0})
    write(tmp_path, "zoe-server", "cp313", {"add": 1000.0})
    code, lines = report(tmp_path, "zoe-server", "cp314t")
    assert code == 1
    assert lines == [f"gate: no result of zeo on cp314t under {tmp_path}"]
