"""Unit tests of the summary of a campaign: what the acceptance test of
`report --json` does not reach.

The tests read the sample runs of `tests/data/runs` and change a copy of
their `result.json`. The test runs no benchmark.
"""

import json
import pathlib

import pytest

from plonebench import report, summary

RUNS = pathlib.Path(__file__).parent / "data" / "runs"
ZEO = RUNS / "20260101T000000Z-edit-zeo-r0"
ZOE = RUNS / "20260101T000500Z-edit-zoe-native-qi-kv-r0"


@pytest.fixture
def results(monkeypatch):
    monkeypatch.setenv("ZOE_BENCH_MACHINE", "reference")
    return report.load([ZEO, ZOE])


@pytest.mark.parametrize(
    ("change", "words"),
    [
        (
            lambda r: r["environment"].update(commit="f" * 40),
            ("more than one commit", "d939b3f5806", "f" * 40),
        ),
        (
            lambda r: r["stack"]["settings"].update(threads=4),
            ("more than one settings.threads", "2, 4"),
        ),
        (
            lambda r: r.update(trim_seconds=10.0),
            ("more than one settings.trim_seconds", "5.0, 10.0"),
        ),
        (
            lambda r: r["environment"]["machine"].update(name="other"),
            ("more than one machine", "reference, other"),
        ),
    ],
    ids=["commit", "setting", "option", "machine"],
)
def test_runs_of_another_campaign_are_refused(results, change, words):
    change(results[1])

    with pytest.raises(summary.NotOneCampaign) as refusal:
        summary.document(results)

    assert all(word in str(refusal.value) for word in words), refusal.value


def test_the_ports_and_the_switches_may_differ(results):
    results[1]["stack"]["settings"].update(base_port=18480, server_port=18500)

    document = summary.document(results)

    assert document["settings"]["instances"] == 2


@pytest.mark.parametrize(
    ("flags", "dirty"),
    [((False, True), True), ((False, None), None), ((True, None), True)],
)
def test_the_campaign_is_dirty_when_a_run_is(results, flags, dirty):
    for result, flag in zip(results, flags, strict=True):
        result["environment"]["dirty"] = flag

    assert summary.document(results)["run"]["dirty"] is dirty


def test_a_run_without_a_record_of_its_tree_has_no_clean_tree(results):
    del results[0]["environment"]["dirty"]

    assert summary.document(results)["run"]["dirty"] is None


def test_the_machine_is_the_name_that_the_runs_recorded(results, monkeypatch):
    for result in results:
        result["environment"]["machine"]["name"] = "recorded"

    assert summary.document(results)["machine"]["name"] == "recorded"

    for result in results:
        del result["environment"]["machine"]["name"]
    monkeypatch.delenv("ZOE_BENCH_MACHINE")

    assert summary.document(results)["machine"]["name"] == "synthetic-cpu"


def test_a_spec_of_no_preset_is_custom(results):
    for result in results:
        result["fixture"]["spec"]["documents"] = 9

    assert summary.document(results)["fixture"]["preset"] == "custom"


def test_a_failed_run_is_in_runs_and_has_no_step(results):
    failed = json.loads(json.dumps(results[1]))
    failed["run_id"] = "20260101T001000Z-edit-zoe-native-qi-kv-r1"
    failed["started"] += 600
    failed["repetition"] = 1
    for key in ("steps_results", "measured", "verification"):
        del failed[key]

    document = summary.document([*results, failed])

    assert document["runs"][-1] == {
        "id": failed["run_id"],
        "scenario": "edit",
        "stack": "zoe-native-qi-kv",
        "repetition": 1,
        "verification": "not run",
        "violations": None,
    }
    assert {step["repetitions"] for step in document["steps"]} == {1}


def test_a_step_without_the_probe_has_no_objects_per_second(results):
    for result in results:
        for step in result["steps_results"]:
            step["all"]["server"] = {"conflict_classes": {}, "resolve_reasons": {}}

    steps = summary.document(results)["steps"]

    assert {step["objects_loaded_per_second"] for step in steps} == {None}
    assert {step["objects_stored_per_second"] for step in steps} == {None}


def test_no_run_is_a_refusal():
    with pytest.raises(summary.NotOneCampaign, match="no run"):
        summary.document([])
