"""Acceptance test of `just bench showcase`: the publishable set in one
command.

The set:

- the builds: the release `zoe` binary and the Plone instances `zeo` and
  `zoeclient`;
- the fixtures `small` and `medium`, prepared when they are missing;
- B5 on `small`, on the stacks `zeo` and `zoe-native-qi-kv`: `browse`,
  `catalog` and `mixed` at 4 and 16 users; `edit` and `create-spread` at
  1, 4, 8, 16 and 32 users; `contention-folder`, `contention-fields` and
  `contention-same` at 4 and 16 users;
- B5 on `medium`, on the same stacks: `browse`, `catalog`, `edit`,
  `create-spread` and `mixed` at 4 and 16 users;
- one B5 summary for each fixture (`report --json`, ADR 0054);
- the native gate and the client benchmark, each on CPython 3.13 and on
  3.14t.

Before the instances of a run of `zoe-native-qi-kv` start, a script in
the interpreter of the zoeclient instance imports the profile
`zoe.plone:default` into the copy of the store, as the owner of a site
does. The result records the policy that the import gives. The stack
`zeo` imports no profile and runs with no switch. Both stacks run on
separate physical cores of the reference machine: the server on CPU 0,
the instances on CPUs 1 and 3, Locust on CPU 6.

`python -m plonebench showcase [OPTIONS]` calls
`plonebench.showcase.main(OPTIONS)`. `just bench showcase OPTIONS` runs it
in the environment of bench/plone and builds nothing first. The options:

- `--plan`: print each run of the set and an estimate of its duration.
  Run nothing, write nothing, take no lock. `--json` prints the plan as
  one JSON document on stdout.
- `--dir DIR`: the directory of the showcase. A start creates it. A later
  start with the same DIR resumes it. The default is a new directory
  `target/bench/showcase/<started>/`.
- `--fixtures small,medium` (the default), `--step-seconds 60`,
  `--warmup 60` and `--trim 5` (the defaults of `just bench plone run`),
  `--client-objects N` (the `--objects` of the client benchmark), and
  `--server-cpus 0`, `--zope-cpus "1;3"` and `--locust-cpus 6` (the
  pinning of each B5 run). These are the options of the set. DIR records
  them, with the commit.
- `--max-load L` (8 by default): before each run, wait while the
  1-minute load average is L or more. `--port-offset N` goes to each B5
  run. These two are not options of the set.
- `ZOE_BENCH_LOCK`: the lock file (`target/bench/bench.lock` by default).
  The showcase holds an exclusive `flock(2)` on it during each run.
- `ZOE_BENCH_COMMIT`: the commit of the showcase, as the runs record it.
  Without it, `git rev-parse HEAD`.

The plan, as `--plan --json` prints it:

    {"directory": DIR or null, "commit": "...", "dirty": false,
     "estimate_seconds": the sum of the estimates of the runs to do,
     "runs": [ENTRY, ...]}

Each ENTRY has `id` (unique, stable, a file name), `kind`, `state`
(`todo` or `done`) and `estimate_seconds` (an int), and by kind:

- `build`: `target`, one of `zoe`, `plone-zeo` and `plone-zoeclient`;
- `prepare`, `summary`: `fixture` (the fixture name) and `preset`;
- `b5`: `fixture`, `preset`, `stack`, `scenario`, `users` (the users of
  each step), `step_seconds`, `warmup_seconds`, `trim_seconds`,
  `pinning` (`server_cpus`, `zope_cpus` as a list, `locust_cpus`, as in
  the settings of the B5 summary), `profile` (`zoe.plone:default`, or
  null for no profile), and `switches` in the shape of the B5 summary
  (ADR 0054);
- `native`, `client`: `python`, the interpreter of the run: `3.13` or
  `3.14t`.

The state of an entry depends only on DIR: an entry is `done` when it
ended well in DIR. The showcase runs the entries to do in the order of
the plan, one at a time. After each run, a file `STOP` in DIR ends the
showcase: it removes the file and exits with 0. A failed `b5`, `native`,
`client` or `summary` run stays to do, and the showcase goes on with the
next entry, but not with the summary of a fixture whose B5 run failed.
A failed `build` or `prepare` run stops the showcase at once. The exit
code is 0 when no run failed, 1 when a run failed, and 2 when DIR belongs
to another commit or to other options of the set; then nothing runs.

The seam of these tests: `showcase.main(argv, execute=FAKE)`. The
showcase calls `FAKE(entry, directory)` in place of each run, with the
entry as the plan gives it and the Path of DIR. A return is a run that
ended well; an exception is a failed run. The tests run no benchmark.
`tests/test_showcase_smoke.py` runs the real set on `small`, on demand.
"""

import fcntl
import importlib
import json
import pathlib
import re
import shutil
import subprocess
import sys

import pytest

from plonebench import corpus, fixture, stack

REPO = pathlib.Path(__file__).resolve().parents[3]

STACKS = ("zeo", "zoe-native-qi-kv")
TWO = [4, 16]
CURVE = [1, 4, 8, 16, 32]
SET = {
    "small": {
        "browse": TWO,
        "catalog": TWO,
        "mixed": TWO,
        "edit": CURVE,
        "create-spread": CURVE,
        "contention-folder": TWO,
        "contention-fields": TWO,
        "contention-same": TWO,
    },
    "medium": {
        "browse": TWO,
        "catalog": TWO,
        "edit": TWO,
        "create-spread": TWO,
        "mixed": TWO,
    },
}
# The switches of `zeo`. The profile gives the policy of the ZOE stack, so
# the test reads only its backends.
ZEO_SWITCHES = {
    "backends": [],
    "member_properties": False,
    "split_merges": None,
    "folder_orders": None,
}
BACKENDS = {"zeo": [], "zoe-native-qi-kv": ["indexing", "locking"]}
PROFILE = {"zeo": None, "zoe-native-qi-kv": "zoe.plone:default"}
# A decision of the human: separate physical cores on the reference
# machine, for both stacks.
PINNING = {"server_cpus": "0", "zope_cpus": ["1", "3"], "locust_cpus": "6"}
PYTHONS = {"3.13", "3.14t"}
KINDS = {"build", "prepare", "b5", "summary", "native", "client"}


def fixture_name(preset):
    return fixture.default_name(corpus.PRESETS[preset], preset)


def showcase():
    """Return the module of the showcase."""
    return importlib.import_module("plonebench.showcase")


@pytest.fixture(autouse=True)
def lock(tmp_path, monkeypatch):
    """Give each test a lock file of its own, so that no test waits for a
    benchmark of the machine."""
    path = tmp_path / "bench.lock"
    monkeypatch.setenv("ZOE_BENCH_LOCK", str(path))
    return path


def held(path):
    """Tell if another open file holds an exclusive flock on `path`."""
    with open(path, "a") as file:
        try:
            fcntl.flock(file, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        fcntl.flock(file, fcntl.LOCK_UN)
        return False


def snapshot():
    """Return what the showcase could write outside its directory: the
    entries and the times of the trees of the benchmarks, and the time of
    the `zoe` binary."""
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
    binary = stack.zoe_binary()
    state[str(binary)] = binary.stat().st_mtime_ns if binary.exists() else None
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


def plan(capsys, *options, directory=None):
    """Return the plan as `--plan --json` prints it, in-process."""
    argv = ["--plan", "--json", *options]
    if directory is not None:
        argv += ["--dir", str(directory)]
    capsys.readouterr()
    code = showcase().main(argv)
    out = capsys.readouterr().out
    assert code == 0, out
    return json.loads(out)


def start(directory, execute, *options):
    """Start or resume the showcase in `directory`, with a fake runner."""
    argv = ["--dir", str(directory), "--max-load", "1000", *options]
    return showcase().main(argv, execute=execute)


def todo(document):
    return [run["id"] for run in document["runs"] if run["state"] == "todo"]


def done(document):
    return [run["id"] for run in document["runs"] if run["state"] == "done"]


class Fake:
    """A runner that runs nothing: it records the id of each entry, and
    fails or writes STOP when the test asks for it."""

    def __init__(self, directory, lock=None, stop_after=None, fail=()):
        self.directory = directory
        self.lock = lock
        self.stop_after = stop_after
        self.fail = set(fail)
        self.calls = []
        self.directories = []
        self.locked = []

    def __call__(self, run, directory):
        self.calls.append(run["id"])
        self.directories.append(pathlib.Path(directory).resolve())
        if self.lock is not None:
            self.locked.append(held(self.lock))
        if self.stop_after is not None and len(self.calls) == self.stop_after:
            (self.directory / "STOP").touch()
        if run["id"] in self.fail:
            raise RuntimeError(f"a fake failure of {run['id']}")


def first(document, kind, preset=None):
    """Return the id of the first entry of `kind`, of `preset` if given."""
    for run in document["runs"]:
        if run["kind"] == kind and preset in (None, run.get("preset")):
            return run["id"]
    raise AssertionError(f"the plan has no entry of kind {kind} {preset or ''}")


def check_set(document, presets, step_seconds=60, warmup=60, trim=5):
    """Check that the plan holds the set of the card for `presets`."""
    runs = document["runs"]
    ids = [run["id"] for run in runs]
    assert len(ids) == len(set(ids)), "the ids of the plan are not unique"
    for run in runs:
        assert re.fullmatch(r"[a-z0-9][a-z0-9._-]*", run["id"]), run["id"]
        assert run["kind"] in KINDS, run
        assert run["state"] in ("todo", "done"), run
        assert isinstance(run["estimate_seconds"], int), run
        assert run["estimate_seconds"] >= 0, run
    assert document["estimate_seconds"] == sum(
        run["estimate_seconds"] for run in runs if run["state"] == "todo"
    )

    builds = [run for run in runs if run["kind"] == "build"]
    assert sorted(run["target"] for run in builds) == [
        "plone-zeo",
        "plone-zoeclient",
        "zoe",
    ]
    # The builds come first: no build runs between two measurements.
    assert {run["kind"] for run in runs[: len(builds)]} == {"build"}

    wanted = {
        (preset, name, scenario, tuple(users))
        for preset in presets
        for scenario, users in SET[preset].items()
        for name in STACKS
    }
    b5 = [run for run in runs if run["kind"] == "b5"]
    got = {
        (run["preset"], run["stack"], run["scenario"], tuple(run["users"]))
        for run in b5
    }
    assert got == wanted
    assert len(b5) == len(wanted)
    for run in b5:
        assert run["fixture"] == fixture_name(run["preset"])
        assert run["switches"]["backends"] == BACKENDS[run["stack"]], run
        if run["stack"] == "zeo":
            assert run["switches"] == ZEO_SWITCHES, run
        assert run["profile"] == PROFILE[run["stack"]], run
        assert run["pinning"] == PINNING, run
        assert run["step_seconds"] == step_seconds
        assert run["warmup_seconds"] == warmup
        assert run["trim_seconds"] == trim
        floor = warmup + step_seconds * len(run["users"])
        assert run["estimate_seconds"] >= floor, run

    for preset in presets:
        name = fixture_name(preset)
        prepares = [
            i
            for i, run in enumerate(runs)
            if run["kind"] == "prepare" and run["fixture"] == name
        ]
        summaries = [
            i
            for i, run in enumerate(runs)
            if run["kind"] == "summary" and run["fixture"] == name
        ]
        measured = [
            i
            for i, run in enumerate(runs)
            if run["kind"] == "b5" and run["fixture"] == name
        ]
        assert len(prepares) == 1, f"one prepare entry for {name}"
        assert len(summaries) == 1, f"one summary entry for {name}"
        assert prepares[0] < min(measured)
        assert max(measured) < summaries[0]
        assert runs[prepares[0]]["preset"] == preset
        assert runs[summaries[0]]["preset"] == preset
    for kind in ("prepare", "summary"):
        presets_of_kind = {run["preset"] for run in runs if run["kind"] == kind}
        assert presets_of_kind == set(presets)

    for kind in ("native", "client"):
        pythons = [run["python"] for run in runs if run["kind"] == kind]
        assert sorted(pythons) == sorted(PYTHONS), (kind, pythons)


def test_the_plan_lists_every_run_of_the_set_and_writes_nothing():
    before = snapshot()
    result = just_showcase("--plan", "--json")
    assert result.returncode == 0, result.stderr
    document = json.loads(result.stdout)

    check_set(document, ("small", "medium"))
    # Without --dir, the plan is the plan of a new directory.
    assert done(document) == []
    assert (
        document["directory"] is None
        or not pathlib.Path(document["directory"]).exists()
    )
    assert snapshot() == before


def test_the_plan_prints_each_run_with_an_estimate():
    before = snapshot()
    listed = just_showcase("--plan", "--json")
    assert listed.returncode == 0, listed.stderr
    ids = [run["id"] for run in json.loads(listed.stdout)["runs"]]

    result = just_showcase("--plan")
    assert result.returncode == 0, result.stderr
    lines = [line.strip() for line in result.stdout.splitlines()]
    for id_ in ids:
        assert any(re.match(re.escape(id_) + r"\s", line) for line in lines), (
            f"no line of the plan starts with {id_}"
        )
    assert any("total" in line.lower() for line in lines)
    assert snapshot() == before


def test_the_plan_of_a_directory_takes_no_lock_and_creates_nothing(tmp_path, lock):
    directory = tmp_path / "showcase"
    before = snapshot()
    with open(lock, "a") as file:
        # Another benchmark holds the lock: the plan does not wait for it.
        fcntl.flock(file, fcntl.LOCK_EX)
        result = subprocess.run(
            [
                sys.executable,
                "-m",
                "plonebench",
                "showcase",
                "--plan",
                "--json",
                "--dir",
                str(directory),
            ],
            cwd=REPO,
            capture_output=True,
            text=True,
            timeout=120,
        )
    assert result.returncode == 0, result.stderr
    document = json.loads(result.stdout)
    assert pathlib.Path(document["directory"]).resolve() == directory.resolve()
    assert done(document) == []
    assert not directory.exists()
    assert snapshot() == before


def test_the_options_of_the_set_change_the_plan(capsys):
    document = plan(
        capsys,
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
    )
    check_set(document, ("small",), step_seconds=5, warmup=0, trim=1)
    assert not any(run.get("preset") == "medium" for run in document["runs"])


def test_stop_ends_the_showcase_after_the_current_run_and_a_restart_resumes(
    tmp_path, capsys
):
    directory = tmp_path / "showcase"
    wanted = todo(plan(capsys, directory=directory))

    fake = Fake(directory, stop_after=2)
    assert start(directory, fake) == 0
    assert fake.calls == wanted[:2]
    assert set(fake.directories) == {directory.resolve()}
    assert not (directory / "STOP").exists()

    document = plan(capsys, directory=directory)
    assert done(document) == wanted[:2]
    assert todo(document) == wanted[2:]
    # The documents of the set go directly in DIR, for `just bench
    # publish`. The state of the showcase is not a document.
    assert list(directory.glob("*.json")) == []
    logs = list(directory.glob("*.log"))
    assert logs, "the showcase writes a log in DIR"
    text = "".join(log.read_text() for log in logs)
    for id_ in wanted[:2]:
        assert id_ in text

    again = Fake(directory)
    assert start(directory, again) == 0
    assert again.calls == wanted[2:]
    document = plan(capsys, directory=directory)
    assert todo(document) == []
    assert document["estimate_seconds"] == 0
    assert list(directory.glob("*.json")) == []

    # A start of a complete showcase runs nothing.
    idle = Fake(directory)
    assert start(directory, idle) == 0
    assert idle.calls == []


def test_each_run_holds_the_bench_lock(tmp_path, lock):
    directory = tmp_path / "showcase"
    fake = Fake(directory, lock=lock, stop_after=3)
    assert start(directory, fake) == 0
    assert fake.locked == [True, True, True]
    assert not held(lock)


def test_a_failed_run_runs_again_at_the_restart(tmp_path, capsys):
    directory = tmp_path / "showcase"
    document = plan(capsys, directory=directory)
    wanted = todo(document)
    failing = first(document, "b5", "small")
    summary = first(document, "summary", "small")

    fake = Fake(directory, fail=[failing])
    assert start(directory, fake) == 1
    # The showcase goes on after a failed B5 run, but the summary of its
    # fixture waits.
    assert fake.calls == [id_ for id_ in wanted if id_ != summary]
    document = plan(capsys, directory=directory)
    assert todo(document) == [id_ for id_ in wanted if id_ in (failing, summary)]

    again = Fake(directory)
    assert start(directory, again) == 0
    assert again.calls == [id_ for id_ in wanted if id_ in (failing, summary)]
    assert todo(plan(capsys, directory=directory)) == []


@pytest.mark.parametrize("kind", ["build", "prepare"])
def test_a_failed_build_or_prepare_stops_the_showcase(tmp_path, capsys, kind):
    directory = tmp_path / "showcase"
    document = plan(capsys, directory=directory)
    wanted = todo(document)
    failing = first(document, kind)

    fake = Fake(directory, fail=[failing])
    assert start(directory, fake) == 1
    assert fake.calls == wanted[: wanted.index(failing) + 1]
    assert todo(plan(capsys, directory=directory)) == wanted[wanted.index(failing) :]


def test_a_directory_belongs_to_one_commit_and_one_set_of_options(
    tmp_path, capsys, monkeypatch
):
    directory = tmp_path / "showcase"
    monkeypatch.setenv("ZOE_BENCH_COMMIT", "a" * 40)
    wanted = todo(plan(capsys, directory=directory))
    fake = Fake(directory, stop_after=1)
    assert start(directory, fake) == 0
    assert fake.calls == wanted[:1]

    # Another commit: the runs of one summary share one commit.
    monkeypatch.setenv("ZOE_BENCH_COMMIT", "b" * 40)
    other = Fake(directory)
    assert start(directory, other) == 2
    assert other.calls == []

    # Other options of the set: the runs of one summary share one setting.
    monkeypatch.setenv("ZOE_BENCH_COMMIT", "a" * 40)
    assert start(directory, other, "--step-seconds", "7") == 2
    assert other.calls == []

    # The load and the ports are not options of the set.
    again = Fake(directory)
    assert start(directory, again, "--port-offset", "400") == 0
    assert again.calls == wanted[1:]
