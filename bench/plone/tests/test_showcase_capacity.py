"""Acceptance test of `just bench showcase --set capacity`: the capacity set,
4 Zope instances on the site of 250,000 items.

`--set NAME` chooses the set of the showcase: `default` (the set of
`tests/test_showcase.py`, also without the option) or `capacity`. The set
`capacity` is a decision of the human:

- the builds of the default set, then the fixture `xlarge`, prepared when
  it is missing;
- B5 on `xlarge`, on the stacks `zeo` and `zoe-native-qi-kv`: `edit` and
  `create-spread` at 4, 8, 16, 32 and 64 users, the curve of the
  dashboard; `contention-folder`, `contention-fields` and
  `contention-same` at 8 and 32 users; `browse`, `catalog` and `mixed` at
  8 and 32 users. The stack that goes first changes from one scenario to
  the next, as in the default set;
- one B5 summary of `xlarge`. No native gate and no client benchmark.

The layout of the capacity set on the reference machine (6 performance
cores of 2 threads, 8 efficiency cores): the server on CPU 0 (core 0);
4 instances of 2 threads on CPUs 1, 3, 6 and 8, one physical core each,
whose sibling CPUs stay idle; Locust on efficiency cores (CPUs 12 to 19);
core 5 (CPUs 10 and 11) stays free for the system.

The options of a set: `--fixtures`, `--step-seconds`, `--warmup`,
`--trim`, `--client-objects`, `--instances`, `--threads`,
`--server-cpus`, `--zope-cpus` and `--locust-cpus`. Each set gives its
own defaults. The capacity set takes `xlarge` and `xlarge-mini`, the
shape of `xlarge` at 1/100, as fixtures. A directory records the set
with its options. The plan JSON records them in `options`: `set`,
`instances` and `threads` beside the options of today. Each `b5` entry
holds `instances` and `threads` beside `pinning`, which keeps its shape.

A list of Zope CPUs shorter than the instances is an error, before
anything runs or is written: in the showcase, in `just bench plone run`
before it looks for the fixture, and in the stack before it starts an
instance.

`result.json` of a B5 run holds `zope_clock_mhz`: the mean clock, in MHz,
of the CPUs of the Zope instances over the samples of the steps, trim
excluded; null when the machine does not give it. The line `done` of a
B5 run in `showcase.log` gives it, rounded, as `N MHz`, and each run of
the B5 summary holds it as `runs[].zope_clock_mhz`, an optional key of
`bench/schema/plone.schema.json`.

The tests run no benchmark. `tests/test_showcase_capacity_smoke.py` runs
the set on `xlarge-mini`, on demand.
"""

import json
import pathlib
import re
import shutil
import subprocess
import sys
import types

import jsonschema
import pytest

from plonebench import __main__ as cli
from plonebench import corpus, fixture, runner, showcase, stack

REPO = pathlib.Path(__file__).resolve().parents[3]
DATA = pathlib.Path(__file__).parent / "data"
SCHEMA = REPO / "bench" / "schema" / "plone.schema.json"
PUBLISHED = REPO / "bench" / "shootout" / "tests" / "data" / "publish" / "plone.json"
RUNS = DATA / "runs"
ZEO_RUN = RUNS / "20260101T000000Z-edit-zeo-r0"
ZOE_RUN = RUNS / "20260101T000500Z-edit-zoe-native-qi-kv-r0"

STACKS = ("zeo", "zoe-native-qi-kv")
CURVE = [4, 8, 16, 32, 64]
TWO = [8, 32]
CAPACITY = {
    "edit": CURVE,
    "create-spread": CURVE,
    "contention-folder": TWO,
    "contention-fields": TWO,
    "contention-same": TWO,
    "browse": TWO,
    "catalog": TWO,
    "mixed": TWO,
}
INSTANCES = 4
THREADS = 2
SERVER_CPUS = "0"
ZOPE_CPUS = ["1", "3", "6", "8"]
# The efficiency cores of the reference machine.
E_CORES = set(range(12, 20))
KINDS = {"build", "prepare", "b5", "summary"}


def fixture_name(preset):
    return fixture.default_name(corpus.PRESETS[preset], preset)


def cpus(text):
    """Return the CPUs of a taskset list, such as `12,13` or `12-15`."""
    found = set()
    for part in text.split(","):
        first, _, last = part.partition("-")
        found.update(range(int(first), int(last or first) + 1))
    return found


@pytest.fixture(autouse=True)
def lock(tmp_path, monkeypatch):
    """Give each test a bench lock of its own."""
    monkeypatch.setenv("ZOE_BENCH_LOCK", str(tmp_path / "bench.lock"))


def snapshot():
    """Return what the showcase could write outside its directory."""
    trees = [
        REPO / "target" / "bench" / "showcase",
        REPO / "target" / "bench" / "native",
        REPO / "target" / "bench" / "client",
        stack.WORK / "runs",
        stack.WORK / "fixtures",
        stack.PLONE,
    ]
    state = {}
    for tree in trees:
        if tree.exists():
            names = sorted(path.name for path in tree.iterdir())
            state[str(tree)] = (tree.stat().st_mtime_ns, names)
        else:
            state[str(tree)] = None
    return state


def just_showcase(*options):
    """Run `just bench showcase OPTIONS` in the repository."""
    just = shutil.which("just")
    assert just, "run the tests inside `devenv shell`: `just` is not on PATH"
    return subprocess.run(
        [just, "bench", "showcase", *options],
        cwd=REPO,
        capture_output=True,
        text=True,
        timeout=600,
    )


def call(capsys, argv, execute=None):
    """Run the showcase in-process. Return its exit code, stdout and
    stderr. An exit of argparse gives its code."""
    capsys.readouterr()
    try:
        code = showcase.main(argv, execute=execute)
    except SystemExit as exit:
        code = exit.code
    out = capsys.readouterr()
    return code, out.out, out.err


def plan(capsys, *options):
    """Return the plan as `--plan --json` prints it, in-process."""
    code, out, err = call(capsys, ["--plan", "--json", *options])
    assert code == 0, err
    return json.loads(out)


def check_capacity(document, preset="xlarge", step_seconds=60, warmup=60, trim=5):
    """Check that the plan holds the capacity set of the card on `preset`."""
    runs = document["runs"]
    ids = [run["id"] for run in runs]
    assert len(ids) == len(set(ids)), "the ids of the plan are not unique"
    assert {run["kind"] for run in runs} == KINDS, "no native gate, no client"
    assert document["estimate_seconds"] == sum(
        run["estimate_seconds"] for run in runs if run["state"] == "todo"
    )
    builds = [run for run in runs if run["kind"] == "build"]
    assert sorted(run["target"] for run in builds) == [
        "plone-zeo",
        "plone-zoeclient",
        "zoe",
    ]
    assert runs[: len(builds)] == builds, "the builds come first"

    name = fixture_name(preset)
    (prepare,) = [run for run in runs if run["kind"] == "prepare"]
    (summary,) = [run for run in runs if run["kind"] == "summary"]
    b5 = [run for run in runs if run["kind"] == "b5"]
    assert (prepare["preset"], prepare["fixture"]) == (preset, name)
    assert (summary["preset"], summary["fixture"]) == (preset, name)
    assert ids.index(prepare["id"]) < min(ids.index(run["id"]) for run in b5)
    assert ids[-1] == summary["id"]
    assert sorted(summary["runs"]) == sorted(run["id"] for run in b5)
    if not (fixture.fixture_dir(name) / "equivalence.json").exists():
        # A build of the site of 250,000 items takes a night.
        assert preset != "xlarge" or prepare["estimate_seconds"] >= 3600, prepare

    got = {(run["scenario"], run["stack"]): tuple(run["users"]) for run in b5}
    wanted = {
        (scenario, name_of_stack): tuple(users)
        for scenario, users in CAPACITY.items()
        for name_of_stack in STACKS
    }
    assert got == wanted
    assert len(b5) == len(wanted)
    for run in b5:
        assert run["fixture"] == name, run
        assert run["preset"] == preset, run
        assert run["step_seconds"] == step_seconds, run
        assert run["warmup_seconds"] == warmup, run
        assert run["trim_seconds"] == trim, run
        floor = warmup + step_seconds * len(run["users"])
        assert run["estimate_seconds"] >= floor, run

    # The two runs of a scenario follow each other, and the stack that goes
    # first changes from one scenario to the next.
    firsts = []
    for one, two in zip(b5[::2], b5[1::2], strict=True):
        assert one["scenario"] == two["scenario"], (one["id"], two["id"])
        assert {one["stack"], two["stack"]} == set(STACKS)
        firsts.append(one["stack"])
    assert all(a != b for a, b in zip(firsts, firsts[1:], strict=False)), firsts
    return b5


def check_layout(document, b5):
    """Check the layout of the card in the options and in each B5 run."""
    options = document["options"]
    assert options["set"] == "capacity"
    assert options["instances"] == INSTANCES
    assert options["threads"] == THREADS
    assert options["server_cpus"] == SERVER_CPUS
    assert options["zope_cpus"] == ZOPE_CPUS
    locust = cpus(options["locust_cpus"])
    assert locust and locust <= E_CORES, options["locust_cpus"]
    for run in b5:
        assert run["instances"] == INSTANCES, run
        assert run["threads"] == THREADS, run
        assert run["pinning"] == {
            "server_cpus": SERVER_CPUS,
            "zope_cpus": ZOPE_CPUS,
            "locust_cpus": options["locust_cpus"],
        }, run


def test_the_capacity_plan_lists_every_run_with_its_layout_and_writes_nothing():
    before = snapshot()
    result = just_showcase("--set", "capacity", "--plan", "--json")
    assert result.returncode == 0, result.stderr
    document = json.loads(result.stdout)

    b5 = check_capacity(document)
    check_layout(document, b5)
    assert document["options"]["fixtures"] == ["xlarge"]
    assert [run["id"] for run in document["runs"] if run["state"] == "done"] == []
    assert snapshot() == before


def test_the_capacity_plan_prints_each_run_and_the_layout():
    before = snapshot()
    listed = just_showcase("--set", "capacity", "--plan", "--json")
    assert listed.returncode == 0, listed.stderr
    ids = [run["id"] for run in json.loads(listed.stdout)["runs"]]

    result = just_showcase("--set", "capacity", "--plan")
    assert result.returncode == 0, result.stderr
    lines = [line.strip() for line in result.stdout.splitlines()]
    for id_ in ids:
        assert any(re.match(re.escape(id_) + r"\s", line) for line in lines), (
            f"no line of the plan starts with {id_}"
        )
    assert any("total" in line.lower() for line in lines)
    layout = [line for line in lines if "4 instances" in line and "1;3;6;8" in line]
    assert layout, "no line of the plan gives the 4 instances and their CPUs"
    assert snapshot() == before


def test_the_default_set_runs_the_runs_of_today(capsys):
    """The plan without `--set` holds the runs of the plan of today, in
    the same order, each with the fields of today. An entry may hold more
    fields."""
    today = json.loads((DATA / "showcase" / "default-plan.json").read_text())
    document = plan(capsys)

    assert [run["id"] for run in document["runs"]] == [
        run["id"] for run in today["runs"]
    ]
    for run, before in zip(document["runs"], today["runs"], strict=True):
        assert {key: run.get(key) for key in before} == before, run["id"]
    options = document["options"]
    assert {key: options.get(key) for key in today["options"]} == today["options"]


def test_the_set_default_is_the_set_without_the_option(capsys):
    without = plan(capsys)
    named = plan(capsys, "--set", "default")

    assert named == without
    assert named["options"]["set"] == "default"
    assert named["options"]["instances"] == 2
    assert named["options"]["threads"] == 2
    b5 = [run for run in named["runs"] if run["kind"] == "b5"]
    assert {(run["instances"], run["threads"]) for run in b5} == {(2, 2)}


def test_the_options_of_the_capacity_set_change_its_runs(capsys):
    document = plan(
        capsys,
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
        "--instances",
        "3",
        "--threads",
        "4",
        "--server-cpus",
        "0,5",
        "--zope-cpus",
        "1,2;3,4;6,7",
        "--locust-cpus",
        "12-15",
    )
    b5 = check_capacity(document, "xlarge-mini", step_seconds=5, warmup=0, trim=1)
    options = document["options"]
    assert options["fixtures"] == ["xlarge-mini"]
    assert (options["set"], options["instances"], options["threads"]) == (
        "capacity",
        3,
        4,
    )
    for run in b5:
        assert (run["instances"], run["threads"]) == (3, 4), run
        assert run["pinning"] == {
            "server_cpus": "0,5",
            "zope_cpus": ["1,2", "3,4", "6,7"],
            "locust_cpus": "12-15",
        }, run


@pytest.mark.parametrize(
    ("refused", "accepted", "instances"),
    [
        (
            ["--set", "capacity", "--zope-cpus", "1;3;6"],
            ["--set", "capacity", "--zope-cpus", "1;3;6", "--instances", "3"],
            3,
        ),
        (
            ["--set", "capacity", "--instances", "5"],
            ["--set", "capacity", "--instances", "5", "--zope-cpus", "1;3;6;8;10"],
            5,
        ),
        (["--instances", "3"], ["--instances", "3", "--zope-cpus", "1;3;6"], 3),
    ],
    ids=["capacity-3-lists", "capacity-5-instances", "default-3-instances"],
)
def test_a_zope_cpu_list_shorter_than_the_instances_is_refused(
    tmp_path, capsys, refused, accepted, instances
):
    before = snapshot()
    code, out, err = call(capsys, ["--plan", "--json", *refused])
    assert code == 2, out
    assert "unrecognized" not in err, err
    error = err.strip().splitlines()[-1]
    assert "--zope-cpus" in error and "--instances" in error, err

    directory = tmp_path / "showcase"
    calls = []
    code, out, err = call(
        capsys,
        ["--dir", str(directory), "--max-load", "1000", *refused],
        execute=lambda run, directory: calls.append(run["id"]),
    )
    assert code == 2, out
    assert calls == []
    assert not directory.exists()
    assert snapshot() == before

    # One list for each instance is accepted.
    document = plan(capsys, *accepted)
    b5 = [run for run in document["runs"] if run["kind"] == "b5"]
    assert {run["instances"] for run in b5} == {instances}


def test_a_run_refuses_a_zope_cpu_list_shorter_than_the_instances(capsys):
    """`just bench plone run` refuses the lists before it looks for the
    fixture, so nothing starts."""
    argv = ["run", "--stack", "zeo", "--scenario", "edit", "--steps", "4:5"]
    argv += ["--fixture", "no-such-fixture", "--instances", "4"]
    argv += ["--zope-cpus", "1;3"]
    capsys.readouterr()
    with pytest.raises((SystemExit, ValueError, RuntimeError)) as caught:
        cli.main(argv)
    text = f"{caught.value} {capsys.readouterr().err}".lower()
    assert "zope" in text and "instances" in text, text


class Started(Exception):
    """The fake Popen of the stack: an instance started."""


def test_a_stack_refuses_a_zope_cpu_list_shorter_than_its_instances(
    tmp_path, monkeypatch
):
    def popen(command, **kwargs):
        raise Started(" ".join(map(str, command)))

    fake = types.SimpleNamespace(Popen=popen, STDOUT=subprocess.STDOUT)
    monkeypatch.setattr(stack.processes, "subprocess", fake)
    monkeypatch.setattr(
        stack.processes, "zope_serve_script", lambda: tmp_path / "serve.py"
    )
    config = stack.STACKS["zeo"]

    with pytest.raises(Exception) as caught:
        settings = stack.Settings(instances=4, zope_cpus=("1", "3"))
        stack.Stack(
            config, settings, tmp_path / "run", tmp_path / "store"
        ).start_instances()
    assert not isinstance(caught.value, Started), (
        f"an instance started on a list that wraps: {caught.value}"
    )
    text = str(caught.value).lower()
    assert "zope" in text and "instances" in text, text

    # One list for each instance starts the first instance on the first list.
    settings = stack.Settings(instances=4, zope_cpus=tuple(ZOPE_CPUS))
    with pytest.raises(Started) as started:
        stack.Stack(
            config, settings, tmp_path / "run2", tmp_path / "store"
        ).start_instances()
    if sys.platform.startswith("linux"):
        assert str(started.value).startswith("taskset -c 1 "), started.value


def test_a_capacity_run_gets_the_layout_of_the_set_and_logs_its_clock(
    tmp_path, capsys, monkeypatch
):
    """The real runs of the set, with a fake `runner.run`: each B5 run gets
    the instances, the threads and the CPU lists of the set, and the line
    `done` of each B5 run gives the mean clock of its Zope CPUs."""
    seen = []

    def fake_run(options):
        seen.append(options)
        options.out.mkdir(parents=True)
        result = {"verification": {"verdict": "pass"}, "zope_clock_mhz": 4321.4}
        (options.out / "result.json").write_text(json.dumps(result))
        return options.out

    def nothing(self, run, directory):
        return None

    def forbidden(self, run, directory):
        raise AssertionError(f"the capacity set runs no {run['kind']}")

    monkeypatch.setattr(runner, "run", fake_run)
    for kind in ("build", "prepare", "summary"):
        monkeypatch.setattr(showcase.Execute, kind, nothing)
    for kind in ("native", "client"):
        monkeypatch.setattr(showcase.Execute, kind, forbidden)
    directory = tmp_path / "showcase"
    options = ["--set", "capacity", "--fixtures", "xlarge-mini"]
    options += ["--step-seconds", "5", "--warmup", "0", "--trim", "1"]
    argv = [*options, "--dir", str(directory), "--max-load", "1000"]

    code, out, err = call(capsys, [*argv, "--port-offset", "600"])

    assert code == 0, out + err
    document = plan(capsys, *options)
    b5 = [run for run in document["runs"] if run["kind"] == "b5"]
    assert len(seen) == len(b5) == 2 * len(CAPACITY)
    defaults = stack.Settings()
    for run_options in seen:
        settings = run_options.settings
        assert settings.instances == INSTANCES
        assert settings.threads == THREADS
        assert settings.server_cpus == SERVER_CPUS
        assert settings.zope_cpus == tuple(ZOPE_CPUS)
        assert settings.locust_cpus == document["options"]["locust_cpus"]
        assert settings.base_port == defaults.base_port + 600
        assert settings.server_port == defaults.server_port + 600
        assert run_options.fixture == fixture_name("xlarge-mini")
    text = (directory / "showcase.log").read_text()
    for run in b5:
        line = re.search(rf"done {re.escape(run['id'])} in .*", text)
        assert line, f"no line done for {run['id']}"
        assert "verification pass" in line.group(0), line.group(0)
        assert re.search(r"\b4321 MHz\b", line.group(0)), line.group(0)


def test_the_summary_records_the_mean_clock_of_each_run(tmp_path, monkeypatch):
    monkeypatch.setenv("ZOE_BENCH_MACHINE", "reference")
    runs = []
    for source, clock in ((ZEO_RUN, 4321.5), (ZOE_RUN, None)):
        copy = tmp_path / source.name
        shutil.copytree(source, copy)
        result = json.loads((copy / "result.json").read_text())
        if clock is not None:
            # A run that recorded no clock, such as a run of today, gives null.
            result["zope_clock_mhz"] = clock
        (copy / "result.json").write_text(json.dumps(result))
        runs.append(copy)
    out = tmp_path / "summary.json"
    argv = ["report", *map(str, runs), "--out", str(tmp_path / "report.md")]
    try:
        code = cli.main([*argv, "--json", str(out)])
    except SystemExit as exit:
        code = exit.code
    assert code in (0, None), code

    document = json.loads(out.read_text())
    clocks = {
        run["stack"]: run.get("zope_clock_mhz", "absent") for run in document["runs"]
    }
    assert clocks == {"zeo": 4321.5, "zoe-native-qi-kv": None}
    schema = json.loads(SCHEMA.read_text())
    jsonschema.validate(document, schema)
    # The key is optional: a summary published before it stays valid.
    jsonschema.validate(json.loads(PUBLISHED.read_text()), schema)
