"""The stack zoe-native-qi: the wait for an empty queue after the steps, and
the report of the indexer."""

import json

from plonebench import report, runner, stack


def fixture_dir(tmp_path):
    (tmp_path / "credentials.json").write_text(json.dumps({"password": "secret"}))
    (tmp_path / "manifest.json").write_text(json.dumps({"site": "Plone"}))
    return tmp_path


def delays(count, p95=None, top=None):
    return {"count": count, "p50": p95, "p95": p95, "max": top}


def test_the_stack_installs_the_backend():
    config = stack.STACKS["zoe-native-qi"]
    assert config.backends == "indexing"
    assert config.server == "zoe"
    assert config.native is True


def test_the_runner_waits_for_an_empty_queue(tmp_path, monkeypatch):
    calls = []
    pending = iter([3, 1, 0])

    def status(url, site, password, since=None):
        calls.append((url, site, password, since))
        if since is None and url.endswith("0") and len(calls) <= 3:
            return {"pending": next(pending), "delays": delays(0)}
        if url.endswith("1"):
            return {"pending": 0, "delays": delays(5, 0.2, 0.3)}
        return {"pending": 0, "delays": delays(0)}

    monkeypatch.setattr(runner, "_status", status)
    monkeypatch.setattr(runner.time, "sleep", lambda seconds: None)
    result = runner._drain_queue(
        ["http://i0", "http://i1"],
        fixture_dir(tmp_path),
        100.0,
        220.0,
        [(4, 60.0), (16, 60.0)],
    )
    assert result["drained"] is True
    assert result["pending_at_end"] == 0
    # Three reads of the first instance until the queue is empty.
    assert [call[0] for call in calls[:3]] == ["http://i0"] * 3
    assert all(call[1:3] == ("Plone", "secret") for call in calls)
    # Then the status of each instance, for the run and from each step.
    sinces = [call[3] for call in calls if call[0] == "http://i1"]
    assert sinces == [None, 100.0, 160.0]
    assert result["instances"]["instance1"]["steps"] == {
        "4": delays(5, 0.2, 0.3),
        "16": delays(5, 0.2, 0.3),
    }


def test_the_runner_stops_waiting_at_its_timeout(tmp_path, monkeypatch):
    monkeypatch.setattr(
        runner, "_status", lambda *args: {"pending": 7, "delays": delays(0)}
    )
    monkeypatch.setattr(runner.time, "sleep", lambda seconds: None)
    result = runner._drain_queue(
        ["http://i0"], fixture_dir(tmp_path), 0.0, 1.0, [(4, 1.0)], timeout=0.0
    )
    assert result["drained"] is False
    assert result["drained_after_seconds"] is None
    assert result["pending_at_end"] in (None, 7)


def test_the_report_of_the_indexer():
    assert report._indexing({}) is None
    result = {
        "indexing": {
            "drained_after_seconds": 0.5,
            "instances": {
                "instance0": {"steps": {"4": delays(0), "16": delays(0)}},
                "instance1": {
                    "steps": {"4": delays(10, 0.2, 0.4), "16": delays(6, 0.25, 0.4)}
                },
                "instance2": {"steps": {"4": delays(2, 0.3, 0.35), "16": delays(0)}},
            },
        }
    }
    assert report._indexing(result) == {
        "drained_after": 0.5,
        "steps": {
            "4": {"count": 12, "p95": 0.3, "max": 0.4},
            "16": {"count": 6, "p95": 0.25, "max": 0.4},
        },
    }
