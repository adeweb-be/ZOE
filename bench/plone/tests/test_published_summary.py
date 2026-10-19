"""Acceptance test of the B5 summary of `just bench plone report --json`.

ADR 0054 makes the summary of a B5 campaign a document that
`just bench publish` copies into the results repository. One document
holds the runs of one campaign on one fixture, and it validates against
`bench/schema/plone.schema.json`. It holds:

- the machine, the Python of the instances, the software, the commit and
  whether the tree was clean;
- the fixture: its name, its preset and its object counts;
- the settings, and each stack with its switches and its merge policy;
- per scenario, stack and step, the numbers that `report` prints, and the
  objects loaded and stored per second from the storage calls that the
  probe counts;
- the verification verdict of each run.

It holds no host name, user name, path or password. `report --json`
exits with 2 and writes no summary when its runs hold more than one
fixture.

The sample run directories in `tests/data/runs` hold what the runner
writes for one repetition of `edit` on `zeo` and on `zoe-native-qi-kv`,
two steps each, on a synthetic fixture: `result.json` and the ledger.
Their `zope_conf` holds placeholder paths under `/nonexistent`, which the
summary leaves out. The test runs no benchmark.
"""

import json
import pathlib
import shutil
import socket

import pytest

from plonebench import __main__ as cli

BENCH = pathlib.Path(__file__).parents[2]
SCHEMA = BENCH / "schema" / "plone.schema.json"
RUNS = pathlib.Path(__file__).parent / "data" / "runs"
ZEO = RUNS / "20260101T000000Z-edit-zeo-r0"
ZOE = RUNS / "20260101T000500Z-edit-zoe-native-qi-kv-r0"
COMMIT = "d939b3f580637ba1f457c9041a12b2f26b0a5273"
FIXTURE = "small-ba01ff255313"


def load(run):
    return json.loads((run / "result.json").read_text())


def report(tmp_path, *runs):
    """Run `report --json` on runs. Return the exit code and the path of
    the summary."""
    out = tmp_path / "summary.json"
    argv = ["report", *map(str, runs), "--out", str(tmp_path / "report.md")]
    try:
        code = cli.main([*argv, "--json", str(out)])
    except SystemExit as exit:
        code = exit.code
    return code, out


@pytest.fixture
def summary(tmp_path, monkeypatch):
    monkeypatch.setenv("ZOE_BENCH_MACHINE", "reference")
    code, out = report(tmp_path, ZEO, ZOE)
    assert code in (0, None), code
    return json.loads(out.read_text())


def test_the_summary_of_a_campaign_is_valid_against_its_schema(summary):
    assert SCHEMA.exists(), f"{SCHEMA} is missing"
    import jsonschema

    jsonschema.validate(summary, json.loads(SCHEMA.read_text()))


def test_the_summary_holds_the_campaign(summary):
    zeo, zoe = load(ZEO), load(ZOE)

    assert summary["schema_version"] == 1
    assert summary["machine"]["name"] == "reference"
    assert summary["machine"]["cpu"] == zeo["environment"]["machine"]["cpu"]
    assert summary["python"] == {
        "implementation": "cpython",
        "version": "3.13.9",
        "free_threaded": False,
        "gil_enabled": True,
    }
    assert summary["software"]["Plone"] == "6.1.3"
    assert summary["run"] == {
        "started": "2026-01-01T00:00:00Z",
        "commit": COMMIT,
        "dirty": False,
    }
    assert summary["fixture"] == {
        "name": FIXTURE,
        "preset": "small",
        "objects": 412,
        "catalog_entries": 398,
        "spec": zeo["fixture"]["spec"],
    }
    settings = summary["settings"]
    assert settings["instances"] == 2
    assert settings["trim_seconds"] == 5.0
    # The ports and the switches of a stack are not settings of the
    # campaign.
    for key in ("base_port", "server_port", "admin_port", "split_merges"):
        assert key not in settings

    stacks = {stack["name"]: stack for stack in summary["stacks"]}
    assert list(stacks) == ["zeo", "zoe-native-qi-kv"]
    assert stacks["zeo"]["switches"] == {
        "backends": [],
        "member_properties": False,
        "split_merges": None,
        "folder_orders": None,
    }
    assert stacks["zeo"]["merge_policy"] is None
    assert stacks["zoe-native-qi-kv"]["switches"] == {
        "backends": ["indexing", "locking"],
        "member_properties": True,
        "split_merges": "on",
        "folder_orders": "on",
    }
    assert stacks["zoe-native-qi-kv"]["merge_policy"] == zoe["merge_policy"]

    assert [scenario["name"] for scenario in summary["scenarios"]] == ["edit"]
    assert summary["runs"] == [
        {
            "id": result["run_id"],
            "scenario": "edit",
            "stack": result["stack"]["stack"]["name"],
            "repetition": 0,
            "verification": "pass",
            "violations": 0,
        }
        for result in (zeo, zoe)
    ]


@pytest.mark.parametrize("run", [ZEO, ZOE], ids=["zeo", "zoe-native-qi-kv"])
def test_each_step_holds_the_numbers_of_the_report(summary, run):
    result = load(run)
    stack = result["stack"]["stack"]["name"]
    rows = {
        (row["scenario"], row["stack"], row["step"]): row for row in summary["steps"]
    }
    assert len(rows) == len(summary["steps"]) == 4

    for index, step in enumerate(result["steps_results"]):
        row = rows["edit", stack, index]
        total = step["all"]
        server = total["server"]
        processes = step["resources"]["processes"]
        ok = total["outcomes"]["ok"]
        rate = total["ok_per_second"]
        conflicts = server.get("conflicts_write", 0) + server.get("conflicts_read", 0)
        zope_cpu = processes["instance0"]["cpu_seconds"]
        zope_cpu += processes["instance1"]["cpu_seconds"]

        assert row["users"] == step["users"]
        assert row["seconds"] == pytest.approx(step["seconds"])
        assert row["repetitions"] == 1
        assert row["operations"] == total["operations"]
        assert row["ok_per_second"] == pytest.approx(
            {"median": rate, "min": rate, "max": rate}
        )
        for name in ("p50", "p95", "p99"):
            assert row[f"{name}_seconds"] == pytest.approx(total["latency_ok"][name])
        assert row["failure_rate"] == pytest.approx(total["failure_rate"])
        assert row["first_attempt_share"] == pytest.approx(
            total["first_attempt_ok"] / total["operations"]
        )
        assert row["conflicts_per_1000_ops"] == pytest.approx(
            1000 * conflicts / total["operations"]
        )
        assert row["exhausted"] == server.get("exhausted", 0)
        assert row["zope_cpu_ms_per_op"] == pytest.approx(1000 * zope_cpu / ok)
        assert row["server_cpu_ms_per_op"] == pytest.approx(
            1000 * processes["server"]["cpu_seconds"] / ok
        )
        assert row["store_write_kb_per_op"] == pytest.approx(
            processes["server"]["store_write_bytes"] / 1024 / ok
        )
        # The storage calls that the probe counts, over the seconds of the
        # step.
        assert row["objects_loaded_per_second"] == pytest.approx(
            server["loads"] / step["seconds"]
        )
        assert row["objects_stored_per_second"] == pytest.approx(
            server["stores"] / step["seconds"]
        )


def test_the_summary_holds_no_host_user_path_or_password(summary, tmp_path):
    # The parts of the document that a path could reach.
    assert {"machine", "software", "settings", "stacks"} <= set(summary)
    text = json.dumps(summary)
    for word in (
        "/nonexistent",
        str(RUNS),
        str(tmp_path),
        str(pathlib.Path.home()),
        "zope_conf",
        "password",
        "credentials",
    ):
        assert word not in text, word
    host = socket.gethostname()
    if len(host) >= 6:
        assert host not in text


def test_a_run_without_its_verification_is_listed_as_not_run(tmp_path, monkeypatch):
    monkeypatch.setenv("ZOE_BENCH_MACHINE", "reference")
    unverified = tmp_path / ZOE.name
    shutil.copytree(ZOE, unverified)
    result = load(unverified)
    del result["verification"]
    (unverified / "result.json").write_text(json.dumps(result))

    code, out = report(tmp_path, ZEO, unverified)

    assert code in (0, None), code
    runs = {run["id"]: run for run in json.loads(out.read_text())["runs"]}
    assert runs[result["run_id"]]["verification"] == "not run"
    assert runs[result["run_id"]]["violations"] is None


def test_the_runs_of_one_summary_share_one_fixture(tmp_path, monkeypatch, capsys):
    monkeypatch.setenv("ZOE_BENCH_MACHINE", "reference")
    other = tmp_path / ZOE.name
    shutil.copytree(ZOE, other)
    result = load(other)
    result["fixture"]["name"] = "medium-000000000000"
    (other / "result.json").write_text(json.dumps(result))

    code, out = report(tmp_path, ZEO, other)

    assert code == 2
    assert not out.exists()
    error = capsys.readouterr().err
    assert FIXTURE in error and "medium-000000000000" in error, error
