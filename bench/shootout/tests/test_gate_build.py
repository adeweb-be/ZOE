"""Unit tests of the rules of the gate by storage and by Python build."""

import io
import json

from shootout import KB, gate


def write(root, storage, python, ops, sizes=(KB,), cells=(("threads", 16),)):
    """Write a result of storage on python, such as cp314t, with ops per name.

    Each name has one cell in each object size of sizes and each (mode,
    concurrency) of cells.
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
                "mode": mode,
                "concurrency": concurrency,
                "object_size": size,
                "ops_per_second": value,
            }
            for name, value in ops.items()
            for mode, concurrency in cells
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


def test_only_zoe_native_counts_read_hot_at_100_kb():
    assert set(gate.left_out("zoe-native")) == {
        ("read_hot", KB),
        ("read_hot", 100 * KB, "threads", 1),
        ("read_hot", 100 * KB, "processes", None),
    }
    assert gate.left_out("zoe-server") == gate.LEFT_OUT
    assert gate.left_out("zoe") == gate.LEFT_OUT


def verdicts(lines, scenario):
    """Return the object size and the verdict of each row of scenario."""
    return [
        (int(line.split()[3]), " ".join(line.split()[7:]))
        for line in lines
        if line.startswith(f"{scenario} ")
    ]


def cell_verdicts(lines, scenario):
    """Return the mode, concurrency, size and verdict of each row of scenario."""
    return {
        (line.split()[1], int(line.split()[2]), int(line.split()[3])): " ".join(
            line.split()[7:]
        )
        for line in lines
        if line.startswith(f"{scenario} ")
    }


LEFT_OUT_OF_NATIVE = [
    "left out: read_hot at 1 KB, because ZODB itself takes most of the time of a "
    "hit there",
    "left out: read_hot at 100 KB with threads at concurrency 1, because "
    + gate.HIT_AT_100_KB,
    "left out: read_hot at 100 KB with processes, because " + gate.HIT_AT_100_KB,
]
SUMMARY_OF_NATIVE = (
    "at least 3x in each cell but read_hot at 1 KB, read_hot at 100 KB with threads "
    "at concurrency 1, read_hot at 100 KB with processes;"
)


def test_zoe_native_counts_read_hot_at_100_kb_only_with_threads_above_concurrency_1(
    tmp_path,
):
    sizes = (KB, 100 * KB)
    cells = (("threads", 1), ("threads", 16), ("processes", 4), ("processes", 16))
    write(tmp_path, "zeo", "cp313", {"read_hot": 1000.0}, sizes, cells)
    write(tmp_path, "zoe-native", "cp313", {"read_hot": 2900.0}, sizes, cells)
    code, lines = report(tmp_path, "zoe-native")
    assert code == 1
    assert cell_verdicts(lines, "read_hot") == {
        ("threads", 1, KB): "left out",
        ("threads", 16, KB): "left out",
        ("processes", 4, KB): "left out",
        ("processes", 16, KB): "left out",
        ("threads", 1, 100 * KB): "left out",
        ("threads", 16, 100 * KB): "FAIL",
        ("processes", 4, 100 * KB): "left out",
        ("processes", 16, 100 * KB): "left out",
    }
    assert [
        line for line in lines if line.startswith("left out: ")
    ] == LEFT_OUT_OF_NATIVE
    assert SUMMARY_OF_NATIVE in lines[-1]
    assert lines[-1].endswith("8 cells, 1 below, 0 missing, 7 left out")


def test_zoe_native_leaves_out_read_hot_at_1_kb_and_counts_it_at_100_kb_with_threads_16(
    tmp_path,
):
    sizes = (KB, 100 * KB)
    write(tmp_path, "zeo", "cp313", {"add": 1000.0, "read_hot": 1000.0}, sizes)
    write(tmp_path, "zoe-native", "cp313", {"add": 3000.0, "read_hot": 2900.0}, sizes)
    code, lines = report(tmp_path, "zoe-native")
    assert code == 1
    assert verdicts(lines, "read_hot") == [(KB, "left out"), (100 * KB, "FAIL")]
    assert verdicts(lines, "add") == [(KB, "pass"), (100 * KB, "pass")]
    assert [
        line for line in lines if line.startswith("left out: ")
    ] == LEFT_OUT_OF_NATIVE
    assert SUMMARY_OF_NATIVE in lines[-1]
    assert lines[-1].endswith("4 cells, 1 below, 0 missing, 1 left out")


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
    write(tmp_path, "zoe-native", "cp314t", {"add": 3000.0})
    write(tmp_path, "zeo", "cp313", {"add": 1000.0})
    write(tmp_path, "zoe-native", "cp313", {"add": 1500.0})
    code, lines = report(tmp_path, "zoe-native", "cp314t")
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
    code, lines = report(tmp_path, "zoe-native")
    assert code == 1
    assert lines[-1].endswith("2 cells, 1 below, 0 missing, 0 left out")


def test_a_build_without_a_result_of_the_baseline_fails(tmp_path):
    write(tmp_path, "zeo", "cp313", {"add": 1000.0})
    write(tmp_path, "zoe-native", "cp313", {"add": 3000.0})
    code, lines = report(tmp_path, "zoe-native", "cp314t")
    assert code == 1
    assert lines == [f"gate: no result of zeo on cp314t under {tmp_path}"]
