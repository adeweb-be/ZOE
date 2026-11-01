"""The plan of the showcase: the entries of the set, in the order of their
runs, their estimates, and the directory that a start can resume."""

from __future__ import annotations

import dataclasses
import json
from pathlib import Path

from plonebench import corpus, fixture, stack

ROOT = stack.REPO / "target" / "bench" / "showcase"

# The stacks of B5, and the profile that each imports before its instances
# start. A decision of the human: the ZOE stack runs with the merge policy
# that the profile of zoe.plone gives a new site.
STACKS = ("zeo", "zoe-native-qi-kv")
PROFILES = {"zeo": None, "zoe-native-qi-kv": "zoe.plone:default"}

TWO = (4, 16)
# The users of the curve of the dashboard.
CURVE = (1, 4, 8, 16, 32)
# The scenarios of each fixture of the default set, with the users of
# their steps.
SET = {
    "small": (
        ("browse", TWO),
        ("catalog", TWO),
        ("mixed", TWO),
        ("edit", CURVE),
        ("create-spread", CURVE),
        ("contention-folder", TWO),
        ("contention-fields", TWO),
        ("contention-same", TWO),
    ),
    "medium": (
        ("browse", TWO),
        ("catalog", TWO),
        ("edit", TWO),
        ("create-spread", TWO),
        ("mixed", TWO),
    ),
}
# The capacity set, a decision of the human: the users of the curve of the
# dashboard, and of the other scenarios.
CAPACITY_CURVE = (4, 8, 16, 32, 64)
CAPACITY_TWO = (8, 32)
CAPACITY = (
    ("edit", CAPACITY_CURVE),
    ("create-spread", CAPACITY_CURVE),
    ("contention-folder", CAPACITY_TWO),
    ("contention-fields", CAPACITY_TWO),
    ("contention-same", CAPACITY_TWO),
    ("browse", CAPACITY_TWO),
    ("catalog", CAPACITY_TWO),
    ("mixed", CAPACITY_TWO),
)
BUILDS = ("zoe", "plone-zeo", "plone-zoeclient")
PYTHONS = ("3.13", "3.14t")
KINDS = ("build", "prepare", "b5", "summary", "native", "client")

# The defaults of the options of the set. The steps take the defaults of
# `just bench plone run`, and the client benchmark the default of its
# `--objects`. A decision of the human: the server, each instance and
# Locust run on separate physical cores of the reference machine.
STEP_SECONDS = 60.0
WARMUP = 60.0
TRIM = 5.0
CLIENT_OBJECTS = 1000
SERVER_CPUS = "0"
ZOPE_CPUS = "1;3"
LOCUST_CPUS = "6"
MAX_LOAD = 8.0


@dataclasses.dataclass(frozen=True)
class Set:
    """One set of the showcase: the scenarios of each of its fixtures, and
    the defaults of its options."""

    fixtures: dict[str, tuple]
    default_fixtures: tuple[str, ...]
    # True when the set runs the native gate and the client benchmark.
    shootout: bool
    instances: int
    threads: int
    server_cpus: str
    zope_cpus: str
    locust_cpus: str


SETS = {
    "default": Set(
        fixtures=SET,
        default_fixtures=tuple(SET),
        shootout=True,
        instances=2,
        threads=2,
        server_cpus=SERVER_CPUS,
        zope_cpus=ZOPE_CPUS,
        locust_cpus=LOCUST_CPUS,
    ),
    # A decision of the human, for the reference machine: the server on
    # core 0, each instance on a performance core of its own whose sibling
    # CPU stays idle, Locust on efficiency cores, and core 5 (CPUs 10 and
    # 11) free for the system. `xlarge-mini` is for the tests.
    "capacity": Set(
        fixtures={"xlarge": CAPACITY, "xlarge-mini": CAPACITY},
        default_fixtures=("xlarge",),
        shootout=False,
        instances=4,
        threads=2,
        server_cpus="0",
        zope_cpus="1;3;6;8",
        locust_cpus="12,13",
    ),
}

# The estimates in seconds, from runs on the reference machine.
BUILD_SECONDS = {"zoe": 60, "plone-zeo": 35, "plone-zoeclient": 35}
PREPARE_SECONDS = {"small": 90, "medium": 343, "xlarge-mini": 240, "xlarge": 23_400}
# A B5 run takes its warmup and its steps, a fixed time (the copy of the
# store and its sync, the start and the stop), and a time for each second
# of warmup and steps (the drain of the queue and the verification of the
# operations). The fixed time depends on the server of the stack: the ZOE
# store of a fixture is larger than its FileStorage.
B5_SECONDS = {
    "small": {"zeo": 15, "zoe": 15},
    "medium": {"zeo": 45, "zoe": 45},
    "xlarge-mini": {"zeo": 10, "zoe": 10},
    "xlarge": {"zeo": 120, "zoe": 180},
}
B5_SECONDS_PER_SECOND = 1.25
SUMMARY_SECONDS = 5
NATIVE_SECONDS = 300
CLIENT_SECONDS = 180
CLIENT_SECONDS_PER_OBJECT = 0.6


@dataclasses.dataclass(frozen=True)
class SetOptions:
    """The options of the set. DIR records them with the commit, so the
    runs of one directory share them."""

    fixtures: tuple[str, ...]
    step_seconds: float
    warmup: float
    trim: float
    client_objects: int
    server_cpus: str
    zope_cpus: tuple[str, ...]
    locust_cpus: str
    set: str = "default"
    instances: int = 2
    threads: int = 2

    def to_json(self) -> dict:
        return json.loads(json.dumps(dataclasses.asdict(self)))

    def pinning(self) -> dict:
        """The pinning of a B5 run, as in the settings of the B5 summary."""
        return {
            "server_cpus": self.server_cpus,
            "zope_cpus": list(self.zope_cpus),
            "locust_cpus": self.locust_cpus,
        }

    def settings(self, port_offset: int = 0) -> stack.Settings:
        defaults = stack.Settings()
        return dataclasses.replace(
            defaults,
            instances=self.instances,
            threads=self.threads,
            server_cpus=self.server_cpus,
            zope_cpus=self.zope_cpus,
            locust_cpus=self.locust_cpus,
            base_port=defaults.base_port + port_offset,
            server_port=defaults.server_port + port_offset,
            admin_port=defaults.admin_port + port_offset,
        )


def fixture_name(preset: str) -> str:
    return fixture.default_name(corpus.PRESETS[preset], preset)


def switches(name: str, settings: stack.Settings) -> dict:
    """Return the switches of the stack `name`, in the shape of the B5
    summary (ADR 0054)."""
    config = stack.STACKS[name]
    settings = stack.campaign_settings(config, settings)
    return {
        "backends": [backend for backend in config.backends.split(",") if backend],
        "member_properties": config.member_properties,
        "split_merges": settings.split_merges,
        "folder_orders": settings.folder_orders,
    }


def entries(options: SetOptions) -> list[dict]:
    """Return the entries of the set, in the order of their runs, without
    their state."""
    definition = SETS[options.set]
    runs = [
        {"id": f"build-{target}", "kind": "build", "target": target}
        for target in BUILDS
    ]
    settings = options.settings()
    turn = 0
    for preset in options.fixtures:
        name = fixture_name(preset)
        prepare = {"id": f"prepare-{preset}", "kind": "prepare"}
        runs.append({**prepare, "fixture": name, "preset": preset})
        measured = []
        for scenario, users in definition.fixtures[preset]:
            # With one repetition, the stack that goes first changes from
            # one scenario to the next, so a drift of the machine does not
            # favour one stack.
            order = STACKS if turn % 2 == 0 else tuple(reversed(STACKS))
            turn += 1
            for name_of_stack in order:
                measured.append(
                    {
                        "id": f"b5-{preset}-{scenario}-{name_of_stack}",
                        "kind": "b5",
                        "fixture": name,
                        "preset": preset,
                        "stack": name_of_stack,
                        "scenario": scenario,
                        "users": list(users),
                        "step_seconds": options.step_seconds,
                        "warmup_seconds": options.warmup,
                        "trim_seconds": options.trim,
                        "instances": options.instances,
                        "threads": options.threads,
                        "pinning": options.pinning(),
                        "profile": PROFILES[name_of_stack],
                        "switches": switches(name_of_stack, settings),
                    }
                )
        runs += measured
        runs.append(
            {
                "id": f"summary-{preset}",
                "kind": "summary",
                "fixture": name,
                "preset": preset,
                "runs": [run["id"] for run in measured],
            }
        )
    if not definition.shootout:
        return runs
    for kind in ("native", "client"):
        for python in PYTHONS:
            run = {"id": f"{kind}-{python}", "kind": kind, "python": python}
            if kind == "client":
                run["objects"] = options.client_objects
            runs.append(run)
    return runs


def estimate(run: dict) -> int:
    """Return the estimate of the duration of a run, in seconds."""
    kind = run["kind"]
    if kind == "build":
        return BUILD_SECONDS[run["target"]]
    if kind == "prepare":
        if (fixture.fixture_dir(run["fixture"]) / "equivalence.json").exists():
            return 0
        return PREPARE_SECONDS[run["preset"]]
    if kind == "b5":
        load = run["warmup_seconds"] + run["step_seconds"] * len(run["users"])
        fixed = B5_SECONDS[run["preset"]][stack.STACKS[run["stack"]].server]
        return round(fixed + (1 + B5_SECONDS_PER_SECOND) * load)
    if kind == "summary":
        return SUMMARY_SECONDS
    if kind == "native":
        return NATIVE_SECONDS
    return round(CLIENT_SECONDS + CLIENT_SECONDS_PER_OBJECT * run["objects"])


def done(directory: Path | None, id_: str) -> bool:
    return directory is not None and (directory / "state" / f"{id_}.done").exists()


def plan(
    options: SetOptions, directory: Path | None, commit: str | None, dirty
) -> dict:
    """Return the plan of `directory`, or of a new directory when it is
    None. The state of an entry depends only on the directory."""
    runs = []
    for run in entries(options):
        state = "done" if done(directory, run["id"]) else "todo"
        runs.append(
            {
                "id": run["id"],
                "kind": run["kind"],
                "state": state,
                "estimate_seconds": estimate(run),
                **{k: v for k, v in run.items() if k not in ("id", "kind")},
            }
        )
    return {
        "directory": None if directory is None else str(directory),
        "commit": commit,
        "dirty": dirty,
        "options": options.to_json(),
        "estimate_seconds": sum(
            run["estimate_seconds"] for run in runs if run["state"] == "todo"
        ),
        "runs": runs,
    }


def describe(run: dict) -> str:
    """Return one line that tells what a run does."""
    kind = run["kind"]
    if kind == "build":
        if run["target"] == "zoe":
            return "the release zoe binary"
        return f"just plone {run['target'].removeprefix('plone-')} build"
    if kind == "prepare":
        return f"the fixture {run['fixture']}, when it is missing"
    if kind == "b5":
        users = ",".join(map(str, run["users"]))
        text = (
            f"B5 {run['scenario']} on {run['stack']}, users {users}, "
            f"{run['step_seconds']:g} s a step, warmup {run['warmup_seconds']:g} s"
        )
        if run["profile"]:
            text += f", profile {run['profile']}"
        return text
    if kind == "summary":
        return f"the B5 summary of {run['fixture']}"
    if kind == "native":
        return f"the native gate on CPython {run['python']}"
    return f"the client benchmark on CPython {run['python']}, {run['objects']} objects"


def duration(seconds: float) -> str:
    seconds = round(seconds)
    if seconds < 60:
        return f"{seconds} s"
    minutes = round(seconds / 60)
    if minutes < 60:
        return f"{minutes} min"
    return f"{minutes // 60} h {minutes % 60:02d} min"


def print_plan(document: dict) -> None:
    directory = document["directory"] or f"a new directory under {ROOT}"
    print(f"showcase: {directory}")
    tree = {
        False: "clean",
        True: "dirty: `just bench publish` refuses its documents",
        None: "no record of the tree",
    }[document["dirty"]]
    print(f"commit: {document['commit'] or 'unknown'} ({tree})")
    options = document["options"]
    print(
        f"set: {options['set']}, {options['instances']} instances of "
        f"{options['threads']} threads; server CPUs {options['server_cpus']}, "
        f"Zope CPUs {';'.join(options['zope_cpus'])}, "
        f"Locust CPUs {options['locust_cpus']}"
    )
    runs = document["runs"]
    width = max(len(run["id"]) for run in runs)
    for run in runs:
        print(
            f"{run['id']:<{width}}  {run['state']:<4}  "
            f"{duration(run['estimate_seconds']):>9}  {describe(run)}"
        )
    todo = [run for run in runs if run["state"] == "todo"]
    print(
        f"total: {len(todo)} of {len(runs)} runs to do, "
        f"about {duration(document['estimate_seconds'])}"
    )


def fixtures_of(name: str, names: tuple[str, ...] | None) -> tuple[str, ...]:
    """Return the fixtures `names` of the set `name` in the order of the
    set, or its default fixtures when `names` is None. Raise ValueError for
    a fixture that the set does not have."""
    definition = SETS[name]
    if names is None:
        return definition.default_fixtures
    unknown = sorted(set(names) - set(definition.fixtures))
    if unknown:
        raise ValueError(
            f"the fixtures of the set {name} are some of "
            f"{', '.join(definition.fixtures)}: {','.join(names)}"
        )
    return tuple(preset for preset in definition.fixtures if preset in names)


def _option(key: str, value) -> str:
    """Return an option of the set as the command line gives it."""
    if key == "fixtures":
        value = ",".join(value)
    elif key == "zope_cpus":
        value = ";".join(value)
    return f"--{key.replace('_', '-')} {value}"


def refusal(directory: Path, commit: str | None, options: SetOptions) -> str | None:
    """Return why the showcase cannot resume `directory`, or None when it
    can. A directory belongs to one commit and one set of options, so the
    runs of one summary share them."""
    path = directory / "state" / "set.json"
    if not path.exists():
        return None
    recorded = json.loads(path.read_text())
    if recorded["commit"] != commit:
        return (
            f"{directory} belongs to the commit {recorded['commit']}, not to "
            f"{commit}: give another --dir"
        )
    wanted = options.to_json()
    other = [
        f"{_option(key, recorded['options'].get(key))} (not {_option(key, value)})"
        for key, value in wanted.items()
        if recorded["options"].get(key) != value
    ]
    if other:
        return f"{directory} belongs to other options of the set: " + "; ".join(other)
    return None
