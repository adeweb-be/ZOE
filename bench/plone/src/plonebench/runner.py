"""One run: a stack, a scenario, a fixture, from a fresh copy to a verdict.

1. Check the prerequisites and record the environment.
2. Restore the store of the stack from the fixture.
3. Start the server and the instances, and wait until each serves the site.
4. Warm up with the `warmup` scenario, unless the run is a cold start.
5. Run the scenario, with the server counters read before and after. With
   queued indexing, wait until the indexer applied the queue of the site.
6. Stop the instances, then read the site through a client of its own and
   check it against the ledger, before any repair.
7. Stop the server, write `result.json`, and remove the copy of the store.

A result names everything it needs to be compared: the stack, the
settings, the scenario and its mix, the steps, the fixture, the machine
and the software.
"""

from __future__ import annotations

import base64
import json
import os
import shutil
import subprocess
import sys
import time
import traceback
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path

from benchcommon import servers as common

from plonebench import (
    analysis,
    corpus,
    environment,
    fixture,
    ledger,
    sampler,
    servers,
    stack,
    verification,
    workload,
)

SCHEMA = "plonebench-result/1"

# The longest wait for the queue of queued indexing to empty after the
# measured steps, in seconds.
DRAIN_TIMEOUT = 300.0


@dataclass
class RunOptions:
    stack: str
    scenario: str
    fixture: str
    steps: list[tuple[int, float]]
    warmup: float = 60.0
    warmup_users: int = 8
    repetition: int = 0
    think: str = "0,0"
    client_retries: int = 0
    trim: float = 5.0
    keep_store: bool = False
    verify: bool = True
    label: str = ""
    settings: stack.Settings = field(default_factory=stack.Settings)
    out: Path | None = None
    seed: int = 1
    # A GenericSetup profile, such as `zoe.plone:default`, that a client of
    # the run imports into each site of the store before the instances
    # start. None imports nothing.
    profile: str | None = None


def locust_command(
    options: RunOptions,
    run: Path,
    scenario: str,
    steps,
    ledger_path: Path,
    urls: list[str],
    fixture_dir: Path,
    csv: Path | None,
) -> tuple[list[str], dict]:
    env = dict(os.environ)
    env.update(
        {
            "PLONEBENCH_MANIFEST": str(fixture_dir / "manifest.json"),
            "PLONEBENCH_CREDENTIALS": str(fixture_dir / "credentials.json"),
            "PLONEBENCH_HOSTS": ",".join(urls),
            "PLONEBENCH_SCENARIO": scenario,
            "PLONEBENCH_LEDGER": str(ledger_path),
            "PLONEBENCH_STEPS": ",".join(f"{u}:{s:g}" for u, s in steps),
            "PLONEBENCH_THINK": options.think,
            "PLONEBENCH_CLIENT_RETRIES": str(options.client_retries),
            "PLONEBENCH_SEED": str(options.seed + options.repetition),
        }
    )
    command = [
        *stack.pin(options.settings.locust_cpus),
        sys.executable,
        "-m",
        "locust",
        "-f",
        str(Path(__file__).with_name("locustfile.py")),
        "--headless",
        "--only-summary",
        "--loglevel",
        "WARNING",
        # At the end, let each user finish the operation in flight, so its
        # outcome reaches the ledger.
        "--stop-timeout",
        "120",
    ]
    if csv is not None:
        command += ["--csv", str(csv)]
    return command, env


def run_locust(
    command, env, log: Path, sample: sampler.Sampler | None
) -> tuple[int, float]:
    started = time.time()
    with open(log, "w") as out:
        process = subprocess.Popen(
            command, env=env, stdout=out, stderr=subprocess.STDOUT
        )
        try:
            if sample is not None:
                sample.watch("locust", process.pid)
            code = process.wait()
        except BaseException:
            # An interrupt stops Locust too, so that it sends no load to the
            # next run.
            common.stop(process, 30)
            raise
    return code, started


def run(options: RunOptions) -> Path:
    config = stack.STACKS[options.stack]
    scenario = workload.SCENARIOS[options.scenario]
    fixture_dir = fixture.fixture_dir(options.fixture)
    if not (fixture_dir / "equivalence.json").exists():
        raise SystemExit(
            f"plonebench: no fixture {options.fixture}: run `just bench plone prepare`"
        )
    refusal = stack.split_merges_refusal(config, options.settings)
    if refusal is None and options.profile is not None:
        refusal = stack.profile_refusal(config)
    if refusal is not None:
        raise SystemExit(f"plonebench: {refusal}")
    missing = stack.missing_prerequisites(config)
    if missing:
        raise SystemExit(
            "plonebench: missing prerequisites:\n  " + "\n  ".join(missing)
        )
    stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    run_id = f"{stamp}-{options.scenario}-{options.stack}-r{options.repetition}"
    run_dir = options.out or (stack.WORK / "runs" / run_id)
    run_dir.mkdir(parents=True, exist_ok=True)
    print(f"plonebench: run {run_id}", flush=True)

    result: dict = {
        "schema": SCHEMA,
        "run_id": run_id,
        "label": options.label,
        "started": time.time(),
        "stack": stack.Stack(
            config, options.settings, run_dir, run_dir / "store"
        ).describe(),
        "scenario": {
            "name": scenario.name,
            "description": scenario.description,
            "mix": scenario.mix,
            "background": list(scenario.background),
            "roles": workload.assign_roles(
                scenario.mix, max(u for u, _ in options.steps)
            ),
            "interface": sorted({workload.ROLES[r][1] for r in scenario.mix}),
        },
        "steps": options.steps,
        "warmup_seconds": options.warmup,
        "warmup_users": options.warmup_users,
        "trim_seconds": options.trim,
        "think": options.think,
        "client_retries": options.client_retries,
        "repetition": options.repetition,
        "fixture": {
            "name": options.fixture,
            "spec": json.loads((fixture_dir / "spec.json").read_text()),
            "equivalence": json.loads((fixture_dir / "equivalence.json").read_text()),
        },
        "environment": environment.record(stack.WORK, options.settings.zeo_fsync),
        "workload_model": "closed: each user waits for its response before its next "
        "operation, so a slower server lowers the offered rate",
        "exclusions": [],
    }
    store = fixture.restore(fixture_dir, config, run_dir)
    st = stack.Stack(config, options.settings, run_dir, store)
    sample = sampler.Sampler(run_dir / "samples.jsonl", sampler.disk_of(store))
    error = None
    try:
        t0 = time.time()
        st.start_server()
        # FileStorage only appends to Data.fs. zoe serve writes pages of
        # zoe.redb in place.
        layout = stack.store_layout(store, config)
        if config.server == "zeo":
            sample.watch("server", st.processes["server"].pid, layout["data_fs"], True)
        else:
            sample.watch(
                "server", st.processes["server"].pid, layout["zoe"] / "zoe.redb"
            )
        switches = (options.settings.split_merges, options.settings.folder_orders)
        if any(mode is not None for mode in switches):
            # The switches commit before the instances start, so every
            # transaction of the run votes with them. Their client does not
            # count in the startup of the stack.
            written = time.time()
            out = st.run_script(
                "split_merges.py",
                *(mode or "-" for mode in switches),
                name="policy",
            )
            result["merge_policy"] = json.loads(out.strip().splitlines()[-1])
            result["policy_seconds"] = time.time() - written
            t0 += result["policy_seconds"]
        if options.profile is not None:
            # The import commits before the instances start, as the switches
            # do. Its import steps write the merge policy and move the
            # properties of the members and groups, so member_properties.py
            # does not run.
            written = time.time()
            out = st.run_script("profile.py", options.profile, name="profile")
            lines = out.strip().splitlines()
            result["profile"] = options.profile
            result["member_properties"] = json.loads(lines[-2])
            result["merge_policy"] = json.loads(lines[-1])
            result["profile_seconds"] = time.time() - written
            t0 += result["profile_seconds"]
        elif config.member_properties:
            # The move commits before the instances start, as the switches
            # do.
            written = time.time()
            out = st.run_script("member_properties.py", name="properties")
            lines = out.strip().splitlines()
            result["member_properties"] = json.loads(lines[-2])
            result["merge_policy"] = json.loads(lines[-1])
            result["properties_seconds"] = time.time() - written
            t0 += result["properties_seconds"]
        sample.start()
        st.start_instances()
        for name, process in st.processes.items():
            sample.watch(name, process.pid)
        result["startup_seconds"] = time.time() - t0
        urls = st.urls()

        if options.warmup > 0:
            command, env = locust_command(
                options,
                run_dir,
                "warmup",
                [(options.warmup_users, options.warmup)],
                run_dir / "ledger" / "warmup.jsonl",
                urls,
                fixture_dir,
                None,
            )
            run_locust(command, env, run_dir / "log" / "locust-warmup.log", None)

        before = _counters(config, options.settings)
        command, env = locust_command(
            options,
            run_dir,
            options.scenario,
            options.steps,
            run_dir / "ledger" / "run.jsonl",
            urls,
            fixture_dir,
            run_dir / "locust",
        )
        code, locust_started = run_locust(
            command, env, run_dir / "log" / "locust.log", sample
        )
        locust_ended = time.time()
        after = _counters(config, options.settings)
        result["locust_exit"] = code
        result["measured"] = {"start": locust_started, "end": locust_ended}
        result["server"] = _server_result(config, before, after, run_dir)
        if "indexing" in config.backends.split(","):
            result["indexing"] = _drain_queue(
                urls, fixture_dir, locust_started, locust_ended, options.steps
            )

        result["stop_codes"] = st.stop_instances()
        if options.verify:
            result["verification"] = _verify(st, run_dir, fixture_dir)
    except Exception as exc:  # keep the files of the run for the diagnosis
        error = exc
        result["error"] = "".join(traceback.format_exception(exc))
    finally:
        sample.stop()
        result.setdefault("stop_codes", {}).update(st.stop())
        result["ended"] = time.time()

    if "measured" in result:
        _analyse(result, run_dir, options)
    (run_dir / "result.json").write_text(json.dumps(result, indent=1, default=str))
    if not options.keep_store and error is None:
        shutil.rmtree(store, ignore_errors=True)
    if error is not None:
        print(f"plonebench: run {run_id} failed: {error}", flush=True)
        raise error
    _print_summary(result)
    return run_dir


def _counters(config: stack.StackConfig, settings: stack.Settings) -> dict:
    # Call this only outside the measured windows. The admin port of zoe
    # serve answers with send(), which wchar does not count, but the sampler
    # subtracts its socket bytes. Inside a window, the store bytes are then
    # too low.
    counters = {"t": time.time(), "status": servers.server_status(settings.server_port)}
    if config.server == "zoe":
        try:
            counters["metrics"] = servers.prometheus(settings.admin_port)
        except OSError as error:
            counters["metrics_error"] = str(error)
    return counters


def _server_result(config, before: dict, after: dict, run_dir: Path) -> dict:
    result = {
        "status_before": before.get("status"),
        "status_after": after.get("status"),
        "status_delta": servers.delta(before.get("status"), after.get("status")),
    }
    if config.server == "zoe" and "metrics" in after:
        metrics = servers.delta(before.get("metrics"), after["metrics"])
        result["metrics_delta"] = {
            k: v for k, v in metrics.items() if "_bucket" not in k
        }
        histograms = {}
        for name in (
            "zoe_vote_latency_seconds",
            "zoe_finish_latency_seconds",
            "zoe_lock_wait_seconds",
            "zoe_load_latency_seconds",
            "zoe_fsync_batch_size",
        ):
            hist = servers.histogram(metrics, name)
            if hist:
                histograms[name] = {
                    "count": hist["count"],
                    "sum": hist["sum"],
                    "p50_bucket": servers.histogram_quantile(hist, 0.5),
                    "p99_bucket": servers.histogram_quantile(hist, 0.99),
                    "buckets": hist["buckets"],
                }
        result["histograms"] = histograms
    return result


def _status(url: str, site: str, password: str, since: float | None = None) -> dict:
    """Return the status of the queue of `site` through `@@zoe-indexing` of
    the instance at `url`, as the maintainer, a Manager."""
    query = "" if since is None else f"?since={since:.6f}"
    request = urllib.request.Request(f"{url}/{site}/@@zoe-indexing{query}")
    token = base64.b64encode(f"{corpus.MAINTAINER}:{password}".encode()).decode()
    request.add_header("Authorization", f"Basic {token}")
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.loads(response.read())


def _drain_queue(
    urls: list[str],
    fixture_dir: Path,
    started: float,
    ended: float,
    steps: list[tuple[int, float]],
    timeout: float = DRAIN_TIMEOUT,
) -> dict:
    """Wait until the indexer applied the queue of the site, before the
    instances stop for the verification. Return the time that the queue
    took to empty after the end of the measured steps, and the status of
    each instance: for the whole run, and for each step."""
    password = json.loads((fixture_dir / "credentials.json").read_text())["password"]
    site = json.loads((fixture_dir / "manifest.json").read_text())["site"]
    deadline = time.time() + timeout
    drained = None
    last: dict = {}
    while time.time() < deadline:
        last = _status(urls[0], site, password)
        if last.get("pending") == 0:
            drained = time.time()
            break
        time.sleep(0.2)
    result: dict = {
        "drained": drained is not None,
        "drained_after_seconds": None if drained is None else drained - ended,
        "pending_at_end": last.get("pending"),
        "instances": {},
    }
    starts = []
    clock = started
    for users, seconds in steps:
        starts.append((users, clock))
        clock += seconds
    for index, url in enumerate(urls):
        instance = {"run": _status(url, site, password), "steps": {}}
        for users, since in starts:
            instance["steps"][str(users)] = _status(url, site, password, since)[
                "delays"
            ]
        result["instances"][f"instance{index}"] = instance
    return result


def _verify(st: stack.Stack, run_dir: Path, fixture_dir: Path) -> dict:
    records = list(ledger.read([str(run_dir / "ledger" / "run.jsonl")]))
    manifest = json.loads((fixture_dir / "manifest.json").read_text())
    wanted = verification.request(records, manifest)
    request_path = run_dir / "verify-request.json"
    request_path.write_text(json.dumps(wanted))
    observed_path = run_dir / "observed.json"
    started = time.time()
    st.run_script("observe.py", str(request_path), str(observed_path), name="verifier")
    observed = json.loads(observed_path.read_text())
    verdict = verification.judge(records, manifest, observed)
    verdict["seconds"] = time.time() - started
    verdict["observed_paths"] = len(wanted["paths"])
    verdict["content_objects"] = observed["content_objects"]
    verdict["catalog_entries"] = observed["catalog_entries"]
    return verdict


def _analyse(result: dict, run_dir: Path, options: RunOptions) -> None:
    records = list(ledger.read([str(run_dir / "ledger" / "run.jsonl")]))
    requests, snapshots, zeoserver = analysis.load_probe(run_dir / "probe")
    measured = result["measured"]
    window_requests = [r for r in requests if r["t"] >= measured["start"] - 1]
    probes = analysis.probe_by_op(window_requests)
    windows = analysis.step_windows(measured["start"], options.steps)
    result["steps_results"] = analysis.summarize_steps(
        records, probes, options.steps, windows, trim=options.trim
    )
    samples = sampler.load(run_dir / "samples.jsonl")
    for step, (start, end) in zip(result["steps_results"], windows, strict=True):
        step["resources"] = sampler.summarize(samples, start + options.trim, end)
    result["resources"] = sampler.summarize(samples, measured["start"], measured["end"])
    result["logins"] = analysis.op_summary(
        [r for r in records if r.kind.startswith("login")], probes, 1.0
    )
    unmatched = [r for r in window_requests if not r.get("op")]
    result["probe"] = {
        "requests": len(window_requests),
        "requests_without_op": len(unmatched),
        "server_totals": analysis.server_totals(
            [p for r in records for p in probes.get(r.op, [])]
        ),
        "client_cache": {s["file"]: s.get("client_cache") for s in snapshots},
    }
    if zeoserver:
        first = [z for z in zeoserver if z["t"] <= measured["start"]]
        last = [z for z in zeoserver if z["t"] <= measured["end"] + 3]
        base = (
            first[-1]
            if first
            else {"resolved": 0, "resolve_failed": 0, "resolve_seconds": 0}
        )
        end = last[-1] if last else zeoserver[-1]
        result["probe"]["zeoserver_resolution"] = {
            key: end[key] - base.get(key, 0)
            for key in ("resolved", "resolve_failed", "resolve_seconds")
        }
        if "resolved_in_failed_votes" in end:
            result["probe"]["zeoserver_resolution"]["resolved_in_failed_votes"] = end[
                "resolved_in_failed_votes"
            ] - base.get("resolved_in_failed_votes", 0)
        if "resolve_reasons" in end:
            before = base.get("resolve_reasons", {})
            reasons = {
                reason: count - before.get(reason, 0)
                for reason, count in end["resolve_reasons"].items()
            }
            result["probe"]["zeoserver_resolution"]["resolve_reasons"] = {
                reason: count for reason, count in reasons.items() if count
            }


def _print_summary(result: dict) -> None:
    verdict = result.get("verification", {}).get("verdict", "not verified")
    print(f"plonebench: {result['run_id']}: verification {verdict}")
    for step in result.get("steps_results", []):
        total = step["all"]
        server = total["server"]
        conflicts = server.get("conflicts_write", 0) + server.get("conflicts_read", 0)
        lat = total["latency_ok"]
        p95 = lat["p95"] * 1000 if lat["p95"] is not None else float("nan")
        print(
            f"  step {step['step']} users {step['users']:>3}: "
            f"{total['ok_per_second'] or 0:7.2f} ok ops/s, "
            f"p50 {1000 * (lat['p50'] or 0):6.0f} ms, "
            f"p95 {p95:6.0f} ms, failures {total['failure_rate'] or 0:.2%}, "
            f"conflicts {conflicts}"
        )
    indexing = result.get("indexing")
    if indexing:
        after = indexing["drained_after_seconds"]
        print(
            "  indexer: queue "
            + (
                f"empty {after:.1f} s after the steps"
                if after is not None
                else "not empty"
            )
        )
        for name, instance in indexing["instances"].items():
            for users, delays in instance["steps"].items():
                if delays["count"]:
                    print(
                        f"  indexer {name} from step users {users:>3}: "
                        f"{delays['count']} keys, delay p50 {delays['p50']:.3f} s, "
                        f"p95 {delays['p95']:.3f} s, max {delays['max']:.3f} s"
                    )
