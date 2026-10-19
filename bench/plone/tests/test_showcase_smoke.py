"""End-to-end test of `just bench showcase`, on demand.

It runs the set on the fixture `small` alone, with short steps: the
builds, the fixture when it is missing, the 16 B5 runs of `small` on
`zeo` and on `zoe-native-qi-kv` (5 s a step, no warmup, 1 s of trim),
the B5 summary of `small`, the native gate and the client benchmark with
50 objects for each worker. Then `just bench publish --main HEAD` takes
the directory of the showcase into an empty checkout, and the test reads
what it published:

- one B5 summary of `small`, with the two stacks, the scenarios of the
  set and the users of each step;
- the stack `zoe-native-qi-kv` with the merge policy that a new site gets
  from the profile `zoe.plone:default`: `split_merges` and
  `folder_orders` on, and tier 2 on for the two container classes of
  stock Plone and for `PrincipalProperties`;
- every B5 run verified `pass`;
- a result of the native gate and a result of the client benchmark.

It takes about 90 minutes and the CPUs of the machine. Run nothing else
beside it. It needs a clean tree at a commit of HEAD, because
`just bench publish` refuses a run of a dirty tree:

    ZOE_BENCH_SHOWCASE_SMOKE=1 just bench plone test \
        bench/plone/tests/test_showcase_smoke.py

`ZOE_BENCH_SHOWCASE_PORT_OFFSET` gives the B5 runs a `--port-offset`.
`ZOE_BENCH_LOCK` names the lock that the showcase holds during each run.
The directory of the showcase stays under `target/bench/showcase/`, for
the diagnosis.
"""

import json
import os
import pathlib
import shutil
import subprocess
import time

import pytest

from plonebench import corpus, fixture

REPO = pathlib.Path(__file__).resolve().parents[3]

pytestmark = pytest.mark.skipif(
    os.environ.get("ZOE_BENCH_SHOWCASE_SMOKE") != "1",
    reason="runs Plone, the native gate and the client benchmark for about "
    "90 minutes: set ZOE_BENCH_SHOWCASE_SMOKE=1",
)

SMALL = {
    "browse": [4, 16],
    "catalog": [4, 16],
    "mixed": [4, 16],
    "edit": [1, 4, 8, 16, 32],
    "create-spread": [1, 4, 8, 16, 32],
    "contention-folder": [4, 16],
    "contention-fields": [4, 16],
    "contention-same": [4, 16],
}
STACKS = ["zeo", "zoe-native-qi-kv"]
OPTIONS = [
    "--fixtures",
    "small",
    "--step-seconds",
    "5",
    "--warmup",
    "0",
    "--trim",
    "1",
    "--client-objects",
    "50",
]
CONTAINERS = (
    "plone.app.contenttypes.content.Folder",
    "Products.CMFPlone.Portal.PloneSite",
)
PROPERTIES = "zoe.plone.properties.PrincipalProperties"


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


def test_a_short_showcase_on_small_gives_a_tree_that_publish_takes(tmp_path):
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
    directory = REPO / "target" / "bench" / "showcase" / f"smoke-{stamp}"
    options = [*OPTIONS, "--dir", str(directory)]
    offset = os.environ.get("ZOE_BENCH_SHOWCASE_PORT_OFFSET")
    extra = ["--port-offset", offset] if offset else []

    out = tmp_path / "showcase.out"
    result = just("bench", "showcase", *options, *extra, log=out, timeout=4 * 3600)
    assert result.returncode == 0, tail(out)

    listed = just("bench", "showcase", "--plan", "--json", *options)
    assert listed.returncode == 0, listed.stderr
    document = json.loads(listed.stdout)
    assert [run["id"] for run in document["runs"] if run["state"] != "done"] == []
    assert list(directory.glob("*.log")), "the showcase writes a log in DIR"

    checkout = tmp_path / "checkout"
    checkout.mkdir()
    published = just(
        "bench", "publish", "--main", "HEAD", str(directory), str(checkout)
    )
    assert published.returncode == 0, published.stdout + published.stderr

    index = json.loads((checkout / "index.json").read_text())
    assert index["index_version"] == 2
    families = [entry["family"] for entry in index["results"]]
    name = fixture.default_name(corpus.PRESETS["small"], "small")
    plone = [entry for entry in index["results"] if entry["family"] == "plone"]
    assert [entry["fixture"] for entry in plone] == [name]
    assert families.count("native") >= 1
    assert families.count("client") >= 1
    assert set(families) == {"plone", "native", "client"}

    summary = json.loads((checkout / plone[0]["path"]).read_text())
    assert summary["fixture"]["name"] == name
    assert summary["fixture"]["preset"] == "small"
    assert summary["settings"]["warmup_seconds"] == 0
    assert summary["settings"]["trim_seconds"] == 1

    stacks = {entry["name"]: entry for entry in summary["stacks"]}
    assert sorted(stacks) == STACKS
    assert stacks["zeo"]["merge_policy"] is None
    zoe = stacks["zoe-native-qi-kv"]
    assert zoe["switches"] == {
        "backends": ["indexing", "locking"],
        "member_properties": True,
        "split_merges": "on",
        "folder_orders": "on",
    }
    policy = zoe["merge_policy"]
    assert policy["split_merges"] == "on"
    assert policy["folder_orders"] == "on"
    for name_of_class in (*CONTAINERS, PROPERTIES):
        entry = policy["classes"].get(name_of_class)
        assert entry is not None, f"the policy has no entry for {name_of_class}"
        assert entry["tier2"] == "on", (name_of_class, entry)
    for name_of_class in CONTAINERS:
        fields = policy["classes"][name_of_class].get("fields", {})
        assert fields.get("modification_date") == "max", (name_of_class, fields)

    assert {scenario["name"] for scenario in summary["scenarios"]} == set(SMALL)
    for scenario, users in SMALL.items():
        for name_of_stack in STACKS:
            got = sorted(
                step["users"]
                for step in summary["steps"]
                if step["scenario"] == scenario and step["stack"] == name_of_stack
            )
            assert got == users, (scenario, name_of_stack, got)
    pairs = sorted((run["scenario"], run["stack"]) for run in summary["runs"])
    assert pairs == sorted((s, n) for s in SMALL for n in STACKS)
    assert {run["verification"] for run in summary["runs"]} == {"pass"}
