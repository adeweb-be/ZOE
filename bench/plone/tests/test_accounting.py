"""The accounting of a run: percentiles, attempts, conflicts, windows,
server counters and samples."""

import json

from plonebench import analysis, sampler, servers
from plonebench.ledger import FAILED, OK, Ledger, Record, read


def test_percentiles():
    assert analysis.percentile([], 0.5) is None
    assert analysis.percentile([3.0], 0.99) == 3.0
    assert analysis.percentile([1.0, 2.0, 3.0, 4.0], 0.5) == 2.5
    values = list(range(1, 101))
    assert analysis.percentile(values, 0.99) == 99.01
    assert not analysis.latency([1.0] * 10)["p99_reliable"]
    assert analysis.latency([1.0] * 200)["p99_reliable"]


def probe(op, attempts=1, errors=(), **values):
    record = {
        "kind": "request",
        "op": op,
        "file": "instance0",
        "attempts": attempts,
        "retries": attempts - 1,
        "errors": list(errors),
        "seconds": 1.0,
        "cpu": 0.5,
        "commit_seconds": 0.1,
    }
    record.update(values)
    return record


def record(op, outcome=OK, start=0.0, end=1.0, attempts=1, kind="edit", step=0):
    return Record(
        op=op,
        kind=kind,
        user=0,
        role="edit",
        account="a",
        instance=0,
        step=step,
        start=start,
        end=end,
        outcome=outcome,
        attempts=attempts,
        requests=2,
    )


def test_first_attempt_and_eventual_success_are_apart():
    conflict = {
        "attempt": 1,
        "error": "ConflictError",
        "retry": True,
        "conflict": "write",
        "class": "BTrees.IOBTree.IOBucket",
    }
    exhausted = dict(conflict, retry=False)
    probes = analysis.probe_by_op(
        [
            probe("a"),
            probe("b", attempts=2, errors=[conflict]),
            probe("c", attempts=4, errors=[conflict, conflict, conflict, exhausted]),
        ]
    )
    records = [record("a"), record("b"), record("c", outcome=FAILED)]
    summary = analysis.op_summary(records, probes, 10.0)
    assert summary["eventual_ok"] == 2
    assert summary["first_attempt_ok"] == 1
    assert summary["ok_per_second"] == 0.2
    assert summary["server_attempts_per_op"] == 7 / 3
    server = summary["server"]
    assert server["conflicts_write"] == 5
    assert server["exhausted"] == 1
    assert server["retried_requests"] == 2
    assert server["conflict_classes"] == {"BTrees.IOBTree.IOBucket": 5}


def test_steps_trim_their_start():
    records = [
        record("a", start=100.5, step=0),
        record("b", start=103.0, step=0),
        record("c", start=113.0, step=1),
    ]
    windows = analysis.step_windows(100.0, [(1, 10.0), (4, 10.0)])
    steps = analysis.summarize_steps(
        records, {}, [(1, 10.0), (4, 10.0)], windows, trim=2.0
    )
    assert steps[0]["all"]["operations"] == 1
    assert steps[1]["all"]["operations"] == 1
    assert steps[0]["seconds"] == 8.0


def test_the_ledger_round_trip(tmp_path):
    path = tmp_path / "l.jsonl"
    ledger = Ledger(str(path))
    ledger.write(record("a"))
    ledger.close()
    (back,) = read([str(path)])
    assert back.op == "a" and back.seconds == 1.0


def test_prometheus_text_and_histograms(monkeypatch):
    text = "\n".join(
        [
            "# HELP zoe_loads_total loads",
            'zoe_loads_total{storage="1"} 10',
            'zoe_conflicts_resolved_total{storage="1",path="python"} 3',
            'zoe_vote_latency_seconds_bucket{storage="1",le="0.001"} 2',
            'zoe_vote_latency_seconds_bucket{storage="1",le="0.01"} 9',
            'zoe_vote_latency_seconds_bucket{storage="1",le="+Inf"} 10',
            'zoe_vote_latency_seconds_sum{storage="1"} 0.05',
            'zoe_vote_latency_seconds_count{storage="1"} 10',
        ]
    )

    class Response:
        def __enter__(self):
            return self

        def __exit__(self, *args):
            return False

        def read(self):
            return text.encode()

    monkeypatch.setattr(servers.urllib.request, "urlopen", lambda *a, **k: Response())
    metrics = servers.prometheus(1)
    assert metrics["zoe_loads_total"] == 10
    assert metrics['zoe_conflicts_resolved_total{path="python"}'] == 3
    hist = servers.histogram(metrics, "zoe_vote_latency_seconds")
    assert hist["count"] == 10
    assert servers.histogram_quantile(hist, 0.5) == 0.01
    assert servers.histogram_quantile(hist, 0.1) == 0.001
    assert servers.delta({"a": 1, "b": "x"}, {"a": 4, "b": "y", "c": 2}) == {"a": 3}


def test_sampler_summary(tmp_path):
    samples = []
    for t, cpu, rss in ((0.0, 1.0, 100), (1.0, 1.5, 300), (2.0, 2.5, 200)):
        samples.append(
            {
                "t": t,
                "cpu": {"user": t, "idle": 2 * t, "steal": 0.1 * t},
                "disk": {"writes": int(t * 10)},
                "processes": {
                    "server": {
                        "cpu": cpu,
                        "user": cpu,
                        "system": 0.0,
                        "rss": rss,
                        "run_delay": t / 10,
                        "threads": 3,
                        "write_bytes": int(t * 1000),
                        "read_bytes": 0,
                        "voluntary": 0,
                        "involuntary": 0,
                    }
                },
            }
        )
    path = tmp_path / "s.jsonl"
    path.write_text("\n".join(json.dumps(s) for s in samples))
    summary = sampler.summarize(sampler.load(path), 0.0, 2.0)
    server = summary["processes"]["server"]
    assert server["cpu_seconds"] == 1.5
    assert server["rss_max"] == 300
    assert server["write_bytes"] == 2000
    assert summary["disk"]["writes"] == 20
    assert summary["machine"]["steal_seconds"] == 0.2
