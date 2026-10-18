"""Compare runs: one table per scenario, one row per stack and step.

The report reads the `result.json` of each run directory. Each number of a
row is the median over the repetitions of that stack, scenario and step,
with the lowest and the highest value beside the rate. A latency
percentile pools the operations of every repetition, from the ledgers. A
p99 from fewer than 200 operations is marked with `~`.
"""

from __future__ import annotations

import json
import statistics
from collections import defaultdict
from pathlib import Path

from plonebench import analysis, ledger

STACK_ORDER = [
    "zeo",
    "zoe-native",
    "zoe-native-kv",
    "zoe-native-qi",
    "zoe-asyncio",
    "zoe-stock",
]


def load(runs: list[Path]) -> list[dict]:
    results = []
    for path in runs:
        file = path / "result.json" if path.is_dir() else path
        if file.exists():
            data = json.loads(file.read_text())
            data["_dir"] = str(file.parent)
            results.append(data)
    return results


def _median(values):
    values = [v for v in values if v is not None]
    return statistics.median(values) if values else None


def _ms(value):
    return "–" if value is None else f"{value * 1000:.0f}"


def _num(value, digits=2):
    return "–" if value is None else f"{value:.{digits}f}"


def _pct(value):
    return "–" if value is None else f"{value * 100:.1f}%"


def pooled_latency(results: list[dict], step: int, trim: float, kinds=None) -> dict:
    values = []
    for result in results:
        windows = analysis.step_windows(result["measured"]["start"], result["steps"])
        start = windows[step][0] + trim
        path = Path(result["_dir"]) / "ledger" / "run.jsonl"
        for record in ledger.read([str(path)]):
            if (
                record.step != step
                or record.start < start
                or record.kind.startswith("login")
            ):
                continue
            if kinds is not None and record.kind not in kinds:
                continue
            if record.outcome == ledger.OK:
                values.append(record.seconds)
    return analysis.latency(values)


def cpu_per_op(step: dict, names) -> float | None:
    resources = step.get("resources", {}).get("processes", {})
    ok = step["all"]["outcomes"]["ok"]
    total = sum(resources[n]["cpu_seconds"] for n in names if n in resources)
    return total / ok if ok else None


def summarize(results: list[dict]) -> dict:
    groups = defaultdict(list)
    for result in results:
        if "steps_results" not in result:
            continue
        stack = result["stack"]["stack"]["name"]
        groups[(result["scenario"]["name"], stack)].append(result)
    summary = {}
    for (scenario, stack), group in sorted(groups.items()):
        steps = []
        for index, (users, _seconds) in enumerate(group[0]["steps"]):
            per = [
                r["steps_results"][index]
                for r in group
                if index < len(r["steps_results"])
            ]
            rates = [s["all"]["ok_per_second"] for s in per]
            write_rates = [s["writes"]["ok_per_second"] for s in per]
            ops = sum(s["all"]["operations"] for s in per)
            conflicts = sum(
                s["all"]["server"].get("conflicts_write", 0)
                + s["all"]["server"].get("conflicts_read", 0)
                for s in per
            )
            instances = [
                k
                for k in per[0].get("resources", {}).get("processes", {})
                if k.startswith("instance")
            ]
            server_disk = [
                s.get("resources", {})
                .get("processes", {})
                .get("server", {})
                .get("write_bytes")
                for s in per
            ]
            ok_ops = [s["all"]["outcomes"]["ok"] for s in per]
            entry = {
                "step": index,
                "users": users,
                "repetitions": len(per),
                "operations": ops,
                "ok_per_second": {
                    "median": _median(rates),
                    "min": min(rates) if rates else None,
                    "max": max(rates) if rates else None,
                    "values": rates,
                },
                "writes_ok_per_second": _median(write_rates),
                "latency_ok": pooled_latency(
                    group, index, group[0].get("trim_seconds", 0)
                ),
                "failure_rate": _median([s["all"]["failure_rate"] for s in per]),
                "first_attempt_share": _median(
                    [
                        s["all"]["first_attempt_ok"] / s["all"]["operations"]
                        if s["all"]["operations"]
                        else None
                        for s in per
                    ]
                ),
                "requests_per_op": _median([s["all"]["requests_per_op"] for s in per]),
                "server_attempts_per_op": _median(
                    [s["all"]["server_attempts_per_op"] for s in per]
                ),
                "conflicts_per_1000_ops": 1000 * conflicts / ops if ops else None,
                "exhausted": sum(s["all"]["server"].get("exhausted", 0) for s in per),
                "retried_seconds_share": _median(
                    [
                        s["all"]["server"].get("retried_seconds", 0)
                        / s["all"]["server"]["seconds"]
                        if s["all"]["server"].get("seconds")
                        else None
                        for s in per
                    ]
                ),
                "client_resolved": sum(
                    s["all"]["server"].get("resolved", 0) for s in per
                ),
                "zope_cpu_ms_per_op": _median(
                    [
                        1000 * x
                        if (x := cpu_per_op(s, instances)) is not None
                        else None
                        for s in per
                    ]
                ),
                "server_cpu_ms_per_op": _median(
                    [
                        1000 * x
                        if (x := cpu_per_op(s, ["server"])) is not None
                        else None
                        for s in per
                    ]
                ),
                "locust_cpu_share": _median(
                    [
                        s.get("resources", {})
                        .get("processes", {})
                        .get("locust", {})
                        .get("cpu_share")
                        for s in per
                    ]
                ),
                "server_write_kb_per_op": _median(
                    [
                        (w / 1024 / ok) if w is not None and ok else None
                        for w, ok in zip(server_disk, ok_ops, strict=True)
                    ]
                ),
                "zope_busy": _median(
                    [sum(s.get("zope_busy", {}).values()) for s in per]
                ),
                "conflict_classes": _merge(
                    [s["all"]["server"]["conflict_classes"] for s in per]
                ),
                "kinds": {
                    kind: {
                        "ok_per_second": _median(
                            [s["kinds"].get(kind, {}).get("ok_per_second") for s in per]
                        ),
                        "failure_rate": _median(
                            [s["kinds"].get(kind, {}).get("failure_rate") for s in per]
                        ),
                        "p95": _median(
                            [
                                s["kinds"]
                                .get(kind, {})
                                .get("latency_ok", {})
                                .get("p95")
                                for s in per
                            ]
                        ),
                    }
                    for kind in sorted({k for s in per for k in s["kinds"]})
                },
            }
            steps.append(entry)
        verdicts = [r.get("verification", {}).get("verdict", "not run") for r in group]
        violations = [r.get("verification", {}).get("violations") for r in group]
        server = [r.get("server", {}) for r in group]
        summary[f"{scenario}|{stack}"] = {
            "scenario": scenario,
            "stack": stack,
            "runs": [r["run_id"] for r in group],
            "steps": steps,
            "verification": verdicts,
            "violations": violations,
            "server_conflicts": [
                s.get("status_delta", {}).get("conflicts") for s in server
            ],
            "server_conflicts_resolved": [
                s.get("status_delta", {}).get("conflicts_resolved") for s in server
            ],
            "zeoserver_resolved": [
                r.get("probe", {}).get("zeoserver_resolution", {}).get("resolved")
                for r in group
            ],
            "resolution": [_resolution(r) for r in group],
            "indexing": [_indexing(r) for r in group],
            "fsync_batch_mean": [
                (h["sum"] / h["count"])
                if (h := s.get("histograms", {}).get("zoe_fsync_batch_size"))
                and h["count"]
                else None
                for s in server
            ],
        }
    return summary


def _indexing(result: dict) -> dict | None:
    """The indexer of a run with queued indexing: the seconds that its
    queue took to empty after the steps, and the delays of its keys from
    the start of each step, over the instances (the count, the highest p95
    of an instance, the highest delay). None for a run without it."""
    indexing = result.get("indexing")
    if not indexing:
        return None
    steps = {}
    for instance in indexing["instances"].values():
        for users, delays in instance["steps"].items():
            entry = steps.setdefault(users, {"count": 0, "p95": None, "max": None})
            if not delays["count"]:
                continue
            entry["count"] += delays["count"]
            entry["p95"] = max(entry["p95"] or 0.0, delays["p95"])
            entry["max"] = max(entry["max"] or 0.0, delays["max"])
    return {"drained_after": indexing["drained_after_seconds"], "steps": steps}


def _resolution(result: dict) -> dict:
    """The conflict resolutions of a run in the instances and in the ZEO
    server: how many, how many a failed attempt or vote lost, and the
    reasons of the failures. A counter that the probes of the run did not
    keep is None."""
    probe = result.get("probe", {})
    client = probe.get("server_totals", {})
    zeoserver = probe.get("zeoserver_resolution", {})
    resolved = client.get("resolved", 0) + zeoserver.get("resolved", 0)
    has_lost = "resolved_in_failed_attempts" in client or (
        "resolved_in_failed_votes" in zeoserver
    )
    lost = client.get("resolved_in_failed_attempts", 0) + zeoserver.get(
        "resolved_in_failed_votes", 0
    )
    has_reasons = "resolve_reasons" in client or "resolve_reasons" in zeoserver
    reasons = _merge(
        [client.get("resolve_reasons", {}), zeoserver.get("resolve_reasons", {})]
    )
    return {
        "resolved": resolved,
        "lost": lost if has_lost else None,
        "reasons": reasons if has_reasons else None,
    }


def _merge(dicts):
    total = defaultdict(int)
    for d in dicts:
        for k, v in d.items():
            total[k] += v
    return dict(sorted(total.items(), key=lambda kv: -kv[1]))


def durability(results: list[dict]) -> str:
    """How each server synced a commit, over all runs. A mix of ZEO fsync
    modes shows every mode, since the runs of one stack pool."""
    modes: dict[str, set] = {}
    for result in results:
        for server, mode in (
            result["environment"]["machine"].get("durability") or {}
        ).items():
            modes.setdefault(server, set()).add(mode)
    if not modes:
        return "not recorded"
    return "; ".join(f"{k}: {' | '.join(sorted(v))}" for k, v in sorted(modes.items()))


def build(runs: list[Path]) -> tuple[str, dict]:
    results = load(runs)
    summary = summarize(results)
    lines = ["# Plone end-to-end benchmark: ZEO and ZOE", ""]
    if results:
        env = results[0]["environment"]
        first = results[0]
        lines += [
            f"- Runs: {len(results)}; commit {env.get('commit', '?')[:12]}"
            + (" (dirty tree)" if env.get("dirty") else ""),
            f"- Machine: {env['machine']['cpu']}, "
            f"{env['machine']['logical_cpus']} logical CPUs, "
            f"{(env['machine']['memory_bytes'] or 0) / 2**30:.0f} GiB, kernel "
            f"{env['machine']['kernel']}"
            + (
                f", cores {env['machine']['core_kinds']}"
                if env["machine"].get("core_kinds")
                else ""
            ),
            f"- CPU pinning: {env['machine'].get('cpu_pinning', 'taskset')}",
            f"- Durability: {durability(results)}",
            f"- Zope Python: {env['software']['zope_python']}",
            f"- Eggs: {env['software']['eggs']}; zoedb {env['software']['zoedb']}; "
            f"{env['software']['zoe']}; Locust {env['software']['locust']}",
            f"- Settings: {first['stack']['settings']}",
            f"- Fixture: {first['fixture']['name']}, "
            f"{first['fixture']['equivalence']['objects']} objects, equivalent stores: "
            f"{first['fixture']['equivalence']['equivalent']}",
            f"- Workload model: {first['workload_model']}; "
            f"think time {first['think']}; "
            f"client retries {first['client_retries']}; each step trimmed by "
            f"{first['trim_seconds']} s",
            "",
        ]
    scenarios = sorted({v["scenario"] for v in summary.values()})
    for scenario in scenarios:
        rows = [v for v in summary.values() if v["scenario"] == scenario]
        rows.sort(
            key=lambda v: (
                STACK_ORDER.index(v["stack"]) if v["stack"] in STACK_ORDER else 99
            )
        )
        sample = next(r for r in results if r["scenario"]["name"] == scenario)
        lines += [
            f"## {scenario}",
            "",
            sample["scenario"]["description"]
            + f" (mix {sample['scenario']['mix']}"
            + (
                f", background {sample['scenario']['background']}"
                if sample["scenario"]["background"]
                else ""
            )
            + f"; interface {', '.join(sample['scenario']['interface'])})",
            "",
            "| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | "
            "failed | "
            "1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | "
            "server CPU ms/op | server KB written/op | Locust CPU | verified |",
            "| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | "
            "---: | ---: | "
            "---: | ---: | ---: | --- |",
        ]
        for row in rows:
            verdict = ",".join(sorted(set(row["verification"])))
            for step in row["steps"]:
                rate = step["ok_per_second"]
                lat = step["latency_ok"]
                p99 = _ms(lat["p99"]) + ("" if lat["p99_reliable"] else "~")
                lines.append(
                    f"| {row['stack']} | {step['users']} | {_num(rate['median'], 1)} "
                    f"({_num(rate['min'], 1)}–{_num(rate['max'], 1)}) | "
                    f"{_ms(lat['p50'])} | "
                    f"{_ms(lat['p95'])} | {p99} | {_pct(step['failure_rate'])} | "
                    f"{_pct(step['first_attempt_share'])} | "
                    f"{_num(step['server_attempts_per_op'])} | "
                    f"{_num(step['conflicts_per_1000_ops'], 1)} | "
                    f"{step['exhausted']} | "
                    f"{_num(step['zope_cpu_ms_per_op'], 1)} | "
                    f"{_num(step['server_cpu_ms_per_op'], 2)} | "
                    f"{_num(step['server_write_kb_per_op'], 1)} | "
                    f"{_pct(step['locust_cpu_share'])} | "
                    f"{verdict} (n={step['repetitions']}) |"
                )
        lines.append("")
        lines.append("Conflict classes (all steps, all repetitions):")
        lines.append("")
        for row in rows:
            classes = _merge([s["conflict_classes"] for s in row["steps"]])
            text = ", ".join(
                f"{k.rsplit('.', 1)[-1]} {v}" for k, v in list(classes.items())[:8]
            )
            lines.append(
                f"- {row['stack']}: {text or 'none'}; server resolved "
                f"{row['server_conflicts_resolved']}, ZEO server resolver "
                f"{row['zeoserver_resolved']}, mean fsync batch "
                f"{[_num(x) for x in row['fsync_batch_mean']]}"
            )
        for row in rows:
            for resolution in row["resolution"]:
                if resolution["lost"] is None and resolution["reasons"] is None:
                    continue
                reasons = "; ".join(
                    f"{k} {v}"
                    for k, v in list((resolution["reasons"] or {}).items())[:5]
                )
                lines.append(
                    f"- {row['stack']}: resolved {resolution['resolved']}, lost in "
                    f"failed attempts or votes {resolution['lost']}; failed "
                    f"resolutions: {reasons or 'none'}"
                )
        for row in rows:
            for run, indexing in zip(row["runs"], row["indexing"], strict=True):
                if indexing is None:
                    continue
                after = indexing["drained_after"]
                delays = "; ".join(
                    f"from the step of {users} users {d['count']} keys, p95 "
                    f"{_num(d['p95'], 3)} s, max {_num(d['max'], 3)} s"
                    for users, d in indexing["steps"].items()
                )
                lines.append(
                    f"- {row['stack']} {run}: the queue was empty "
                    f"{_num(after, 1)} s after the steps; {delays}"
                )
        lines.append("")
    return "\n".join(lines) + "\n", summary


def timeline(runs: list[Path], bucket: float) -> str:
    """Return, for each run, the successful operations per second and the
    p50 and p95 of their latency in buckets of `bucket` seconds from the
    start of the measured window, and the CPU of each process and the
    bytes the server wrote in each bucket."""
    from plonebench import sampler

    lines = []
    for result in load(runs):
        if "measured" not in result:
            continue
        start = result["measured"]["start"]
        end = result["measured"]["end"]
        records = list(
            ledger.read([str(Path(result["_dir"]) / "ledger" / "run.jsonl")])
        )
        samples = sampler.load(Path(result["_dir"]) / "samples.jsonl")
        lines += [
            f"### {result['run_id']}",
            "",
            "| from s | ok ops/s | p50 ms | p95 ms | failed | server CPU | "
            "Zope CPU | server MB written |",
            "| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
        ]
        t = start
        while t < end:
            chosen = [
                r for r in records if t <= r.start < t + bucket and r.kind != "login"
            ]
            ok = [r.seconds for r in chosen if r.outcome == ledger.OK]
            lat = analysis.latency(ok)
            res = sampler.summarize(samples, t, t + bucket).get("processes", {})
            zope = sum(
                v["cpu_share"] or 0 for k, v in res.items() if k.startswith("inst")
            )
            server = res.get("server", {})
            written = server.get("write_bytes")
            lines.append(
                f"| {t - start:.0f} | {len(ok) / bucket:.1f} | {_ms(lat['p50'])} | "
                f"{_ms(lat['p95'])} | {len(chosen) - len(ok)} | "
                f"{_pct(server.get('cpu_share'))} | {_pct(zope)} | "
                f"{'–' if written is None else f'{written / 2**20:.1f}'} |"
            )
            t += bucket
        lines.append("")
    return "\n".join(lines) + "\n"
