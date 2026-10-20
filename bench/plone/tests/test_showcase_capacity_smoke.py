"""End-to-end test of `just bench showcase --set capacity`, on demand.

It runs the capacity set on the fixture `xlarge-mini`, the shape of
`xlarge` at 1/100, with short steps: the builds, the fixture when it is
missing, the 16 B5 runs on `zeo` and on `zoe-native-qi-kv` with
4 instances of 2 threads (5 s a step, no warmup, 1 s of trim), and the
B5 summary. Then `just bench publish --main HEAD` takes the directory of
the showcase into an empty checkout, and the test reads what it
published:

- one B5 summary of `xlarge-mini` and no other document: the capacity set
  runs no native gate and no client benchmark;
- the layout of the set: 4 instances of 2 threads, the server on CPU 0,
  the instances on CPUs 1, 3, 6 and 8, Locust on efficiency cores;
- the scenarios of the set and the users of each step;
- every B5 run verified `pass`;
- the mean clock of the Zope CPUs of each run, in the summary, in the log
  of the run and in the line `done` of the run in `showcase.log`.

It takes about 25 minutes on the reference machine when the builds
exist, and 4 more minutes when it prepares the fixture. It takes the CPUs
of the machine. Run nothing else beside it. It needs a clean tree at a
commit of HEAD, because `just bench publish` refuses a run of a dirty
tree:

    ZOE_BENCH_SHOWCASE_SMOKE=1 just bench plone test \\
        bench/plone/tests/test_showcase_capacity_smoke.py

`ZOE_BENCH_SHOWCASE_PORT_OFFSET` gives the B5 runs a `--port-offset`.
`ZOE_BENCH_LOCK` names the lock that the showcase holds during each run.
The directory of the showcase stays under `target/bench/showcase/`, for
the diagnosis.
"""

import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
import time

import pytest

from plonebench import corpus, fixture

REPO = pathlib.Path(__file__).resolve().parents[3]

pytestmark = pytest.mark.skipif(
    os.environ.get("ZOE_BENCH_SHOWCASE_SMOKE") != "1",
    reason="runs Plone with 4 instances for about 25 minutes: set "
    "ZOE_BENCH_SHOWCASE_SMOKE=1",
)

CAPACITY = {
    "edit": [4, 8, 16, 32, 64],
    "create-spread": [4, 8, 16, 32, 64],
    "contention-folder": [8, 32],
    "contention-fields": [8, 32],
    "contention-same": [8, 32],
    "browse": [8, 32],
    "catalog": [8, 32],
    "mixed": [8, 32],
}
STACKS = ["zeo", "zoe-native-qi-kv"]
OPTIONS = [
    "--set",
    "capacity",
    "--fixtures",
    "xlarge-mini",
    "--step-seconds",
    "5",
    "--warmup",
    "0",
    "--trim",
    "1",
]
ZOPE_CPUS = ["1", "3", "6", "8"]
# The efficiency cores of the reference machine.
E_CORES = set(range(12, 20))


def just(*args, log=None, timeout=600):
    command = shutil.which("just")
    assert command, "run the test inside `devenv shell`: `just` is not on PATH"
    if log is None:
        return subprocess.run(
            [command, *args], cwd=REPO, capture_output=True, text=True, timeout=timeout
        )
    with open(log, "w") as out:
        return subprocess.run(
            [command, *args],
            cwd=REPO,
            stdout=out,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=timeout,
        )


def tail(path, lines=60):
    return "\n".join(pathlib.Path(path).read_text().splitlines()[-lines:])


def cpus(text):
    """Return the CPUs of a taskset list, such as `12,13` or `12-15`."""
    found = set()
    for part in text.split(","):
        first, _, last = part.partition("-")
        found.update(range(int(first), int(last or first) + 1))
    return found


def max_mhz():
    """The highest clock of the Zope CPUs, from sysfs; 10,000 when the
    machine does not give it."""
    found = []
    for cpu in ZOPE_CPUS:
        path = pathlib.Path(f"/sys/devices/system/cpu/cpu{cpu}/cpufreq")
        try:
            found.append(int((path / "cpuinfo_max_freq").read_text()) / 1000)
        except (OSError, ValueError):
            pass
    return max(found) if found else 10_000


def test_a_short_capacity_showcase_gives_a_tree_that_publish_takes(tmp_path):
    status = subprocess.run(
        ["git", "status", "--porcelain", "--untracked-files=no"],
        cwd=REPO,
        capture_output=True,
        text=True,
        check=True,
    )
    assert status.stdout == "", (
        "commit the changes first: `just bench publish` refuses a run of a "
        "dirty tree\n" + status.stdout
    )

    stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    directory = REPO / "target" / "bench" / "showcase" / f"capacity-smoke-{stamp}"
    options = [*OPTIONS, "--dir", str(directory)]
    offset = os.environ.get("ZOE_BENCH_SHOWCASE_PORT_OFFSET")
    extra = ["--port-offset", offset] if offset else []

    out = tmp_path / "showcase.out"
    result = just("bench", "showcase", *options, *extra, log=out, timeout=3 * 3600)
    assert result.returncode == 0, tail(out)

    listed = just("bench", "showcase", "--plan", "--json", *options)
    assert listed.returncode == 0, listed.stderr
    document = json.loads(listed.stdout)
    assert [run["id"] for run in document["runs"] if run["state"] != "done"] == []
    assert {run["kind"] for run in document["runs"]} == {
        "build",
        "prepare",
        "b5",
        "summary",
    }
    b5 = [run for run in document["runs"] if run["kind"] == "b5"]
    assert {(run["instances"], run["threads"]) for run in b5} == {(4, 2)}
    # The capacity set writes one document: the B5 summary of its fixture.
    assert sorted(path.name for path in directory.glob("*.json")) == [
        "summary-xlarge-mini.json"
    ]

    # The log of each B5 run and its line `done` give the mean clock.
    clock = r"\b\d{3,5} MHz\b"
    text = (directory / "showcase.log").read_text()
    for run in b5:
        line = re.search(rf"done {re.escape(run['id'])} in .*", text)
        assert line, f"no line done for {run['id']}"
        if sys.platform.startswith("linux"):
            assert re.search(clock, line.group(0)), line.group(0)
            log = (directory / "logs" / f"{run['id']}.log").read_text()
            assert re.search(clock, log), f"the log of {run['id']} gives no clock"

    checkout = tmp_path / "checkout"
    checkout.mkdir()
    published = just(
        "bench", "publish", "--main", "HEAD", str(directory), str(checkout)
    )
    assert published.returncode == 0, published.stdout + published.stderr

    index = json.loads((checkout / "index.json").read_text())
    assert index["index_version"] == 2
    name = fixture.default_name(corpus.PRESETS["xlarge-mini"], "xlarge-mini")
    assert [entry["family"] for entry in index["results"]] == ["plone"]
    (entry,) = index["results"]
    assert entry["fixture"] == name

    summary = json.loads((checkout / entry["path"]).read_text())
    assert summary["fixture"]["name"] == name
    assert summary["fixture"]["preset"] == "xlarge-mini"
    settings = summary["settings"]
    assert settings["instances"] == 4
    assert settings["threads"] == 2
    assert settings["server_cpus"] == "0"
    assert settings["zope_cpus"] == ZOPE_CPUS
    assert cpus(settings["locust_cpus"]) <= E_CORES, settings["locust_cpus"]
    assert settings["warmup_seconds"] == 0
    assert settings["trim_seconds"] == 1

    assert sorted(stack["name"] for stack in summary["stacks"]) == STACKS
    assert {scenario["name"] for scenario in summary["scenarios"]} == set(CAPACITY)
    for scenario, users in CAPACITY.items():
        for name_of_stack in STACKS:
            got = sorted(
                step["users"]
                for step in summary["steps"]
                if step["scenario"] == scenario and step["stack"] == name_of_stack
            )
            assert got == users, (scenario, name_of_stack, got)
    pairs = sorted((run["scenario"], run["stack"]) for run in summary["runs"])
    assert pairs == sorted((s, n) for s in CAPACITY for n in STACKS)
    assert {run["verification"] for run in summary["runs"]} == {"pass"}

    highest = max_mhz()
    for run in summary["runs"]:
        value = run.get("zope_clock_mhz", "absent")
        if sys.platform.startswith("linux"):
            assert isinstance(value, (int, float)), (run["id"], value)
            assert 0 < value <= highest * 1.05, (run["id"], value, highest)
        else:
            assert value is None, (run["id"], value)
