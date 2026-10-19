"""Unit tests of the summary of a campaign: what the acceptance test of
`report --json` does not reach.

The tests read the sample runs of `tests/data/runs` and change a copy of
their `result.json`. The test runs no benchmark.
"""

import json
import pathlib
import shutil
import subprocess

import pytest

from plonebench import __main__ as cli
from plonebench import environment, report, stack, summary

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
        (
            lambda r: r["environment"]["software"]["zope_python"].update(
                version="3.14.0", free_threaded=True
            ),
            ("more than one python", "cpython 3.13, cpython 3.14t"),
        ),
    ],
    ids=["commit", "setting", "option", "machine", "python"],
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


def test_a_store_write_a_little_below_0_is_valid(results):
    # The sampler reads wchar and the socket bytes one after the other. On a
    # step that writes almost nothing, the difference can be below 0.
    server = results[1]["steps_results"][0]["resources"]["processes"]["server"]
    server["store_write_bytes"] = -1186

    steps = summary.document(results)["steps"]

    row = next(s for s in steps if s["stack"] == "zoe-native-qi-kv" and s["step"] == 0)
    assert -0.1 < row["store_write_kb_per_op"] < 0


def test_another_number_below_0_is_not_valid(results):
    server = results[1]["steps_results"][0]["resources"]["processes"]["server"]
    server["cpu_seconds"] = -1.0

    with pytest.raises(summary.jsonschema.ValidationError, match="minimum of 0"):
        summary.document(results)


def test_no_run_is_a_refusal():
    with pytest.raises(summary.NotOneCampaign, match="no run"):
        summary.document([])


def test_another_patch_release_of_python_is_one_python(results):
    results[1]["environment"]["software"]["zope_python"]["version"] = "3.13.10"

    assert summary.document(results)["python"]["version"] == "3.13.9"


def test_a_refused_summary_keeps_the_markdown_report(tmp_path, monkeypatch, capsys):
    monkeypatch.setenv("ZOE_BENCH_MACHINE", "reference")
    other = tmp_path / ZOE.name
    shutil.copytree(ZOE, other)
    result = json.loads((other / "result.json").read_text())
    result["environment"]["commit"] = "f" * 40
    (other / "result.json").write_text(json.dumps(result))
    markdown, document = tmp_path / "report.md", tmp_path / "summary.json"

    code = cli.main(
        [
            "report",
            str(ZEO),
            str(other),
            "--out",
            str(markdown),
            "--json",
            str(document),
        ]
    )

    assert code == 2
    assert "more than one commit" in capsys.readouterr().err
    assert markdown.read_text().startswith("# Plone end-to-end benchmark")
    assert not document.exists()


def test_the_report_marks_a_tree_without_a_record(tmp_path):
    unknown = tmp_path / ZEO.name
    shutil.copytree(ZEO, unknown)
    result = json.loads((unknown / "result.json").read_text())
    result["environment"]["dirty"] = None
    (unknown / "result.json").write_text(json.dumps(result))

    text, _ = report.build([unknown])

    assert "(tree unknown)" in text.splitlines()[2]
    assert "(tree unknown)" not in report.build([ZEO])[0]


def _has_git() -> bool:
    try:
        done = subprocess.run(
            ["git", "rev-parse", "--verify", "--quiet", "HEAD"],
            cwd=stack.REPO,
            capture_output=True,
            check=False,
        )
    except OSError:
        return False
    return done.returncode == 0


@pytest.mark.skipif(not _has_git(), reason="the tree has no git history")
def test_a_commit_that_is_not_the_tree_has_no_record_of_a_clean_tree(monkeypatch):
    head = environment.git("rev-parse", "HEAD")

    monkeypatch.setenv("ZOE_BENCH_COMMIT", "0" * 40)
    assert environment.dirty() is None
    monkeypatch.setenv("ZOE_BENCH_COMMIT", head)
    assert environment.dirty() in (True, False)
    monkeypatch.delenv("ZOE_BENCH_COMMIT")
    assert environment.dirty() in (True, False)


def test_two_versions_of_one_egg_give_the_highest(monkeypatch):
    eggs = [
        pathlib.Path(f"/eggs/{name}-py3.13.egg")
        for name in ("ZODB-6.10.0", "ZODB-6.9.1", "ZEO-6.0.0", "other-1.0")
    ]
    for order in (eggs, eggs[::-1]):
        monkeypatch.setattr(stack, "egg_paths", lambda pattern, order=order: order)

        assert environment.eggs() == {"ZEO": "6.0.0", "ZODB": "6.10.0"}
