"""The summary of a campaign: the document that `report --json` writes and
that `just bench publish` copies into the results repository (ADR 0054).

One document holds the runs of one fixture, at one commit, on one machine,
with one Python of the instances and one set of settings. It validates against
bench/schema/plone.schema.json. It holds no host name, user name, path or
password: it takes only the keys below from each `result.json`.
"""

from __future__ import annotations

import datetime
import json
import os
from pathlib import Path

import jsonschema

from plonebench import corpus, report
from plonebench.environment import slug

SCHEMA = Path(__file__).resolve().parents[3] / "schema" / "plone.schema.json"
SCHEMA_VERSION = 1

# The constants of stack.Settings that a campaign shares. The ports and the
# switches are not in it: they can differ from one run to the next.
STACK_SETTINGS = (
    "instances",
    "threads",
    "zodb_cache_objects",
    "pool_size",
    "client_cache",
    "server_cpus",
    "zope_cpus",
    "locust_cpus",
    "probe",
    "zeo_fsync",
    "zoe_cache_size",
    "zoe_compression",
)
# The options of a run that a campaign shares.
RUN_SETTINGS = (
    "warmup_seconds",
    "warmup_users",
    "trim_seconds",
    "think",
    "client_retries",
)


class NotOneCampaign(Exception):
    """The runs do not share one fixture, commit, machine, Python or
    settings."""


def document(results: list[dict]) -> dict:
    """Return the summary of the runs of one campaign, valid against its
    schema. Raise NotOneCampaign when the runs are not of one campaign, and
    jsonschema.ValidationError when the summary is not valid."""
    if not results:
        raise NotOneCampaign("no run holds a result.json")
    results = sorted(results, key=lambda r: (r["started"], r["run_id"]))
    first = results[0]
    fixture = _shared(results, "fixture", lambda r: r["fixture"]["name"])
    commit = _shared(results, "commit", lambda r: r["environment"].get("commit"))
    machine = _shared(results, "machine", _machine_name)
    _shared(results, "python", _python_build)
    settings = {
        key: _shared(results, f"settings.{key}", lambda r, key=key: _settings(r)[key])
        for key in STACK_SETTINGS + RUN_SETTINGS
    }
    flags = [r["environment"].get("dirty") for r in results]
    dirty = (
        True
        if any(flag is True for flag in flags)
        else None
        if any(flag is None for flag in flags)
        else False
    )
    started = datetime.datetime.fromtimestamp(first["started"], datetime.UTC)
    summary = {
        "schema_version": SCHEMA_VERSION,
        "machine": _machine(first, machine),
        "python": _python(first),
        "software": _software(results),
        "run": {
            "started": started.strftime("%Y-%m-%dT%H:%M:%SZ"),
            "commit": commit,
            "dirty": dirty,
        },
        "fixture": _fixture(first, fixture),
        "settings": settings,
        "stacks": _stacks(results),
        "scenarios": _scenarios(results),
        "steps": _steps(results),
        "runs": [_run(r) for r in results],
    }
    jsonschema.validate(summary, json.loads(SCHEMA.read_text()))
    return summary


def _shared(results: list[dict], key: str, value_of):
    """Return the value of key that every run gives. Raise NotOneCampaign
    with each value when the runs give more than one."""
    values = []
    for result in results:
        value = value_of(result)
        if value not in values:
            values.append(value)
    if len(values) > 1:
        text = ", ".join(v if isinstance(v, str) else json.dumps(v) for v in values)
        raise NotOneCampaign(f"the runs hold more than one {key}: {text}")
    return values[0]


def _machine_name(result: dict) -> str:
    """The name that the run recorded, else ZOE_BENCH_MACHINE, else the
    CPU model of the run as a slug."""
    machine = result["environment"]["machine"]
    return (
        machine.get("name")
        or os.environ.get("ZOE_BENCH_MACHINE")
        or slug(machine["cpu"])
    )


def _python_build(result: dict) -> str:
    """The Python build of the instances of a run, such as cpython 3.13 or
    cpython 3.14t: its implementation, its major and minor version and
    whether it is free-threaded."""
    python = _python(result)
    version = ".".join(str(python["version"]).split(".")[:2])
    return (
        f"{python['implementation']} {version}{'t' if python['free_threaded'] else ''}"
    )


def _settings(result: dict) -> dict:
    stack_settings = result["stack"]["settings"]
    return {key: stack_settings.get(key) for key in STACK_SETTINGS} | {
        key: result.get(key) for key in RUN_SETTINGS
    }


def _machine(result: dict, name: str) -> dict:
    machine = result["environment"]["machine"]
    disk = machine.get("store_disk") or {}
    return {
        "name": name,
        "cpu": machine["cpu"],
        "logical_cpus": machine["logical_cpus"],
        "memory_bytes": machine.get("memory_bytes"),
        "os": machine["os"],
        "kernel": machine["kernel"],
        "core_kinds": machine.get("core_kinds"),
        "cpu_pinning": machine.get("cpu_pinning", "taskset"),
        "durability": machine.get("durability") or {},
        "store_disk": {
            "rotational": disk.get("device_rotational"),
            "filesystem": disk.get("filesystem"),
        },
    }


def _python(result: dict) -> dict:
    """The interpreter of the Plone instances."""
    python = result["environment"]["software"].get("zope_python") or {}
    return {
        "implementation": python.get("implementation", "cpython"),
        "version": python.get("version"),
        "free_threaded": python.get("free_threaded"),
        "gil_enabled": python.get("gil_enabled"),
    }


def _software(results: list[dict]) -> dict:
    """The versions that the runs recorded. The first run that records a
    name gives its version."""
    versions: dict[str, str] = {}
    for result in results:
        software = result["environment"]["software"]
        recorded = dict(software.get("eggs") or {})
        for name in ("zoedb", "zoe", "locust"):
            recorded[name] = software.get(name)
        for name, version in recorded.items():
            if version:
                versions.setdefault(name, version)
    return dict(sorted(versions.items()))


def _fixture(result: dict, name: str) -> dict:
    fixture = result["fixture"]
    spec = fixture["spec"]
    preset = next(
        (
            preset
            for preset, preset_spec in corpus.PRESETS.items()
            if json.dumps(preset_spec.to_json(), sort_keys=True)
            == json.dumps(spec, sort_keys=True)
        ),
        "custom",
    )
    equivalence = fixture["equivalence"]
    return {
        "name": name,
        "preset": preset,
        "objects": equivalence["objects"],
        "catalog_entries": equivalence["catalog_entries"],
        "spec": spec,
    }


def _order(stack: str) -> tuple:
    """The order of the stacks in the report."""
    order = report.STACK_ORDER
    return (order.index(stack) if stack in order else len(order), stack)


def _stacks(results: list[dict]) -> list[dict]:
    groups: dict[str, list[dict]] = {}
    for result in results:
        groups.setdefault(result["stack"]["stack"]["name"], []).append(result)
    stacks = []
    for name in sorted(groups, key=_order):
        group = groups[name]
        config = group[0]["stack"]["stack"]
        settings = group[0]["stack"]["settings"]
        stacks.append(
            {
                "name": name,
                "description": config["description"],
                "server": config["server"],
                "client": config["section"],
                "native": config.get("native"),
                "switches": {
                    "backends": [b for b in config.get("backends", "").split(",") if b],
                    "member_properties": config.get("member_properties", False),
                    "split_merges": settings.get("split_merges"),
                    "folder_orders": settings.get("folder_orders"),
                },
                "merge_policy": next(
                    (r["merge_policy"] for r in group if r.get("merge_policy")), None
                ),
            }
        )
    return stacks


def _scenarios(results: list[dict]) -> list[dict]:
    scenarios = {}
    for result in results:
        scenario = result["scenario"]
        scenarios.setdefault(
            scenario["name"],
            {
                "name": scenario["name"],
                "description": scenario["description"],
                "interface": scenario["interface"],
                "mix": scenario["mix"],
                "background": scenario["background"],
            },
        )
    return [scenarios[name] for name in sorted(scenarios)]


def _steps(results: list[dict]) -> list[dict]:
    """The numbers that the report prints, per scenario, stack and step."""
    rows = sorted(
        report.summarize(results).values(),
        key=lambda row: (row["scenario"], _order(row["stack"])),
    )
    steps = []
    for row in rows:
        for step in row["steps"]:
            latency = step["latency_ok"]
            rate = step["ok_per_second"]
            steps.append(
                {
                    "scenario": row["scenario"],
                    "stack": row["stack"],
                    "step": step["step"],
                    "users": step["users"],
                    "seconds": step["seconds"],
                    "repetitions": step["repetitions"],
                    "operations": step["operations"],
                    "ok_per_second": {
                        "median": rate["median"],
                        "min": rate["min"],
                        "max": rate["max"],
                    },
                    "p50_seconds": latency["p50"],
                    "p95_seconds": latency["p95"],
                    "p99_seconds": latency["p99"],
                    "p99_reliable": latency["p99_reliable"],
                    "failure_rate": step["failure_rate"],
                    "first_attempt_share": step["first_attempt_share"],
                    "conflicts_per_1000_ops": step["conflicts_per_1000_ops"],
                    "exhausted": step["exhausted"],
                    "zope_cpu_ms_per_op": step["zope_cpu_ms_per_op"],
                    "server_cpu_ms_per_op": step["server_cpu_ms_per_op"],
                    "store_write_kb_per_op": step["store_write_kb_per_op"],
                    "objects_loaded_per_second": step["objects_loaded_per_second"],
                    "objects_stored_per_second": step["objects_stored_per_second"],
                }
            )
    return steps


def _run(result: dict) -> dict:
    verification = result.get("verification") or {}
    return {
        "id": result["run_id"],
        "scenario": result["scenario"]["name"],
        "stack": result["stack"]["stack"]["name"],
        "repetition": result.get("repetition", 0),
        "verification": verification.get("verdict", "not run"),
        "violations": verification.get("violations"),
    }
