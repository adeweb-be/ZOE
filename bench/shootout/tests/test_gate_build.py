"""Unit tests of the rules of the gate by storage and by Python build."""

import io

from conftest import write_gate_result

from shootout import KB, gate


def report(root, storage, python=None):
    out = io.StringIO()
    code = gate.report(root, storage, "zeo", gate.PARITY, out, python)
    return code, out.getvalue().splitlines()


def verdicts(lines, scenario):
    """Return the object size and the verdict of each row of scenario."""
    return [
        (int(line.split()[3]), " ".join(line.split()[7:]))
        for line in lines
        if line.startswith(f"{scenario} ")
    ]


def test_zoe_server_leaves_out_read_hot_in_both_sizes(tmp_path):
    sizes = (KB, 100 * KB)
    write_gate_result(
        tmp_path, "zeo", "cp313", {"add": 1000.0, "read_hot": 1000.0}, sizes
    )
    write_gate_result(
        tmp_path, "zoe-server", "cp313", {"add": 1000.0, "read_hot": 500.0}, sizes
    )
    code, lines = report(tmp_path, "zoe-server")
    assert code == 0
    assert verdicts(lines, "read_hot") == [(KB, "left out"), (100 * KB, "left out")]
    assert [line for line in lines if line.startswith("left out: ")] == [
        f"left out: read_hot, because {gate.LEFT_OUT['read_hot', None]}"
    ]
    assert "at least 1x in each cell but read_hot;" in lines[-1]
    assert lines[-1].endswith("4 cells, 0 below, 0 missing, 2 left out")


def test_a_build_counts_and_the_other_prints_beside_it(tmp_path):
    write_gate_result(tmp_path, "zeo", "cp314t", {"add": 1000.0})
    write_gate_result(tmp_path, "zoe-server", "cp314t", {"add": 1000.0})
    write_gate_result(tmp_path, "zeo", "cp313", {"add": 1000.0})
    write_gate_result(tmp_path, "zoe-server", "cp313", {"add": 500.0})
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
    write_gate_result(tmp_path, "zeo", "cp313", {"add": 1000.0})
    write_gate_result(tmp_path, "zoe-server", "cp313", {"add": 1000.0})
    code, lines = report(tmp_path, "zoe-server", "cp314t")
    assert code == 1
    assert lines == [f"gate: no result of zeo on cp314t under {tmp_path}"]
