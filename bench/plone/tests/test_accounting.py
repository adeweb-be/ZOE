"""The accounting of a run: percentiles, attempts, conflicts, windows,
server counters and samples."""

import importlib.util
import json
from pathlib import Path

from plonebench import analysis, environment, sampler, servers, stack
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


def test_resolution_failure_reasons():
    path = Path(analysis.__file__).parent / "probe" / "plonebench_probe.py"
    spec = importlib.util.spec_from_file_location("plonebench_probe", path)
    probe_module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(probe_module)

    class OOBucket:
        pass

    bucket = probe_module.failure_reason(
        (
            OOBucket,
            "BTreesConflictError",
            "BTrees conflict error at 3/-1/2: Conflicting inserts",
        )
    )
    assert bucket == "OOBucket: BTreesConflictError: Conflicting inserts"
    plain = probe_module.failure_reason(("n/a", "ConflictError", "database conflict"))
    assert plain == "n/a: ConflictError: database conflict"
    totals = analysis.server_totals(
        [
            probe("a", resolve_failed=2, resolve_reasons=[bucket, bucket]),
            probe("b", resolve_failed=1, resolve_reasons=[plain]),
            probe("c"),
        ]
    )
    assert totals["resolve_failed"] == 3
    assert totals["resolve_reasons"] == {bucket: 2, plain: 1}


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


def test_serve_options_and_durability(monkeypatch):
    assert stack.serve_options(stack.Settings()) == []
    settings = stack.Settings(zoe_cache_size="128MB", zoe_compression="zstd:3")
    assert stack.serve_options(settings) == [
        "--cache-size",
        "128MB",
        "--compression",
        "zstd:3",
    ]
    assert stack.Settings().zeo_fsync == "plain"
    monkeypatch.setattr(environment.sys, "platform", "darwin")
    plain = environment.durability("plain")
    assert plain["zeo"].startswith("fsync(2)")
    assert plain["zoe"].startswith("fsync(2)")
    assert environment.durability("full")["zeo"].startswith("F_FULLFSYNC")
    monkeypatch.setattr(environment.sys, "platform", "linux")
    assert environment.durability("plain") == {
        "zeo": "fsync(2) in FileStorage",
        "zoe": "fdatasync(2)",
    }


def test_the_switch_split_merges_goes_into_the_policy_of_the_store():
    import argparse

    from plonebench.__main__ import add_run, settings_of

    # By default a run writes nothing, and the store keeps its policy.
    assert stack.Settings().split_merges is None
    default = stack.Settings()
    assert stack.split_merges_refusal(stack.STACKS["zoe-native-qi"], default) is None

    parser = argparse.ArgumentParser()
    add_run(parser)
    settings = settings_of(parser.parse_args(["--split-merges", "on"]))
    assert settings.split_merges == "on"
    assert settings.to_json()["split_merges"] == "on"
    # Only a stack of zoe serve with zoedb reads the policy.
    assert stack.split_merges_refusal(stack.STACKS["zoe-native-qi"], settings) is None
    assert "zeo" in stack.split_merges_refusal(stack.STACKS["zeo"], settings)
    assert stack.split_merges_refusal(stack.STACKS["zoe-stock"], settings)
    bad = stack.Settings(split_merges="sometimes")
    assert stack.split_merges_refusal(stack.STACKS["zoe-native-qi"], bad)
    # A store without a policy gets one with the switch alone.
    assert stack.split_merges_policy(None, "on") == {
        "classes": {},
        "split_merges": "on",
    }
    # The classes and the other switches of a policy stay; the version
    # goes, since `zoe.policy.install` sets it.
    current = {
        "version": 3,
        "posting_dicts": "on",
        "classes": {"m.Document": {"tier2": "shadow"}},
    }
    assert stack.split_merges_policy(current, "on") == {
        "posting_dicts": "on",
        "classes": {"m.Document": {"tier2": "shadow"}},
        "split_merges": "on",
    }
    assert current["version"] == 3


def test_a_campaign_gives_the_switch_only_to_the_stacks_that_take_it():
    import argparse

    from plonebench import __main__ as cli

    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command")
    campaign = commands.add_parser("campaign")
    cli.add_run(campaign)
    args = parser.parse_args(["campaign", "--split-merges", "on", "--fixture", "f"])
    zeo = cli.options_of(args, "zeo", "edit", 0, campaign=True)
    qi = cli.options_of(args, "zoe-native-qi", "edit", 0, campaign=True)
    assert zeo.settings.split_merges is None
    assert qi.settings.split_merges == "on"
    # So the runner refuses neither run.
    for options in (zeo, qi):
        config = stack.STACKS[options.stack]
        assert stack.split_merges_refusal(config, options.settings) is None
    # One run still refuses the switch on a stack that does not take it.
    one = cli.options_of(args, "zeo", "edit", 0)
    assert stack.split_merges_refusal(stack.STACKS["zeo"], one.settings)
    # `prepare` and `serve` do not take the switch.
    serve = argparse.ArgumentParser()
    cli.add_settings(serve)
    assert not any(a.dest == "split_merges" for a in serve._actions)
    assert cli.settings_of(serve.parse_args([])).split_merges is None
