"""The publishable set of the benchmarks, in one command.

    python -m plonebench showcase --plan [--json] [OPTIONS]
    python -m plonebench showcase [--dir DIR] [OPTIONS]

`just bench showcase OPTIONS` runs it in the environment of bench/plone.
The set, in the order of its runs:

1. the builds: the release `zoe` binary, and the Plone instances `zeo` and
   `zoeclient`;
2. for each fixture, `small` then `medium`: the fixture, when it is
   missing; the B5 runs of the fixture on the stacks `zeo` and
   `zoe-native-qi-kv`; the B5 summary of the fixture (`report --json`,
   ADR 0054);
3. the native gate and the client benchmark, each on CPython 3.13 and on
   3.14t.

Before the instances of a run of `zoe-native-qi-kv` start, a client of
the run imports the profile `zoe.plone:default` into the copy of the
store, so the stack runs with the merge policy of a new site. The stack
`zeo` imports nothing.

The directory of a showcase, DIR, holds:

    showcase.log            one line for each start and end of a run
    <id>.json               the documents that `just bench publish` takes:
                            the B5 summary of each fixture, and the result
                            of each native gate and client benchmark
    <id>.md                 the report of the B5 summary of a fixture
    logs/<id>.log           the output of each run
    runs/<id>/              the directory of each B5 run
    failed/<id>-<started>/  the directory of a B5 run that failed
    state/set.json          the commit and the options of the set
    state/<id>.done         a run that ended well

A start runs the entries to do in the order of the plan, one at a time.
For each run, it holds an exclusive lock on `ZOE_BENCH_LOCK` and waits
while the 1-minute load average is `--max-load` or more. A file `STOP` in
DIR ends the showcase after the current run. A later start with the same
DIR runs only the entries to do.
"""

from __future__ import annotations

import argparse
import contextlib
import dataclasses
import fcntl
import json
import os
import shutil
import signal
import subprocess
import sys
import time
from collections.abc import Callable
from pathlib import Path

from plonebench import corpus, environment, fixture, runner, stack

ROOT = stack.REPO / "target" / "bench" / "showcase"
LOCK = stack.REPO / "target" / "bench" / "bench.lock"

# The stacks of B5, and the profile that each imports before its instances
# start. A decision of the human: the ZOE stack runs with the merge policy
# that the profile of zoe.plone gives a new site.
STACKS = ("zeo", "zoe-native-qi-kv")
PROFILES = {"zeo": None, "zoe-native-qi-kv": "zoe.plone:default"}

TWO = (4, 16)
# The users of the curve of the dashboard.
CURVE = (1, 4, 8, 16, 32)
# The scenarios of each fixture, with the users of their steps.
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

# The estimates in seconds, from runs on the reference machine.
BUILD_SECONDS = {"zoe": 60, "plone-zeo": 35, "plone-zoeclient": 35}
PREPARE_SECONDS = {"small": 90, "medium": 343}
# A B5 run takes its warmup and its steps, a fixed time (the copy of the
# store, the start and the stop), and a time for each second of warmup and
# steps (the drain of the queue and the verification of the operations).
B5_SECONDS = {"small": 15, "medium": 45}
B5_SECONDS_PER_SECOND = 1.25
SUMMARY_SECONDS = 5
NATIVE_SECONDS = 300
CLIENT_SECONDS = 180
CLIENT_SECONDS_PER_OBJECT = 0.6

# The interval between two reads of the load average, in seconds.
LOAD_POLL = 15.0


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
        for scenario, users in SET[preset]:
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
        return round(B5_SECONDS[run["preset"]] + (1 + B5_SECONDS_PER_SECOND) * load)
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


def commit_of() -> str | None:
    """The commit of the showcase: ZOE_BENCH_COMMIT, else the HEAD of the
    repository."""
    if os.environ.get("ZOE_BENCH_COMMIT"):
        return os.environ["ZOE_BENCH_COMMIT"]
    return _git("rev-parse", "HEAD")


def dirty_of() -> bool | None:
    """Tell if the tree differs from the commit of the showcase, as the B5
    runs record it. None when the tree is no record of that commit."""
    return environment.dirty()


def _git(*args: str) -> str | None:
    try:
        out = subprocess.run(
            ["git", *args], cwd=stack.REPO, capture_output=True, text=True
        )
    except OSError:
        return None
    if out.returncode:
        return None
    return out.stdout.strip() or None


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


class Log:
    """The log of a showcase: each line goes to stdout and to
    DIR/showcase.log."""

    def __init__(self, path: Path):
        self.path = path

    def __call__(self, message: str) -> None:
        line = f"{time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())} {message}"
        print(line, flush=True)
        with open(self.path, "a") as file:
            file.write(line + "\n")


@contextlib.contextmanager
def bench_lock(log: Log):
    """Hold an exclusive flock(2) on the bench lock, so that no other
    benchmark runs beside the run."""
    path = Path(os.environ.get("ZOE_BENCH_LOCK") or LOCK)
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "a") as file:
        try:
            fcntl.flock(file, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            log(f"wait for the bench lock {path}")
            fcntl.flock(file, fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(file, fcntl.LOCK_UN)


def wait_for_load(max_load: float, stop: Path, log: Log) -> bool:
    """Wait while the 1-minute load average is `max_load` or more. Return
    False when the file `stop` appears during the wait."""
    told = False
    while True:
        load = os.getloadavg()[0]
        if load < max_load:
            return True
        if stop.exists():
            return False
        if not told:
            log(
                f"wait: the 1-minute load average is {load:.2f}, not below {max_load:g}"
            )
            told = True
        time.sleep(LOAD_POLL)


def keep_awake(log: Log) -> subprocess.Popen | None:
    """Keep the machine awake until `release`, through systemd-inhibit on
    Linux. Warn when it cannot."""
    if not sys.platform.startswith("linux"):
        return None
    inhibit = shutil.which("systemd-inhibit")
    if inhibit is None:
        log("warning: systemd-inhibit is missing: turn off the automatic suspend")
        return None
    command = [inhibit, "--what=sleep:idle", "--who=zoe-showcase", "--mode=block"]
    check = subprocess.run(
        [*command, "--why=check", "true"], capture_output=True, text=True
    )
    if check.returncode != 0:
        log("warning: systemd-inhibit refuses: turn off the automatic suspend")
        return None
    # `cat` reads its stdin until the showcase closes it or ends. Then
    # systemd-inhibit ends and releases the lock of the sleep.
    return subprocess.Popen(
        [*command, "--why=a run of just bench showcase", "cat"],
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def release(process: subprocess.Popen | None) -> None:
    if process is None:
        return
    process.stdin.close()
    try:
        process.wait(10)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()


def _interrupt(signum, frame):
    raise KeyboardInterrupt


def parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="plonebench showcase",
        description="Run the publishable set of the benchmarks, or print its plan.",
    )
    parser.add_argument(
        "--plan",
        action="store_true",
        help="print each run of the set and an estimate of its duration; run "
        "nothing, write nothing, take no lock",
    )
    parser.add_argument(
        "--json", action="store_true", help="with --plan: print the plan as JSON"
    )
    parser.add_argument(
        "--dir",
        type=Path,
        help="the directory of the showcase; a later start with the same "
        "directory resumes it (default: a new directory "
        "target/bench/showcase/<started>)",
    )
    parser.add_argument(
        "--fixtures",
        type=_fixtures,
        default=tuple(SET),
        help="the fixtures of the set, separated by ',' (default: small,medium)",
    )
    parser.add_argument(
        "--step-seconds",
        type=_seconds(positive=True),
        default=STEP_SECONDS,
        help="the seconds of each step of a B5 run (default: %(default)g)",
    )
    parser.add_argument(
        "--warmup",
        type=_seconds(positive=False),
        default=WARMUP,
        help="the seconds of the warmup of a B5 run (default: %(default)g)",
    )
    parser.add_argument(
        "--trim",
        type=_seconds(positive=False),
        default=TRIM,
        help="the seconds left out of each step (default: %(default)g)",
    )
    parser.add_argument(
        "--client-objects",
        type=_positive,
        default=CLIENT_OBJECTS,
        help="the --objects of the client benchmark (default: %(default)s)",
    )
    parser.add_argument("--server-cpus", default=SERVER_CPUS)
    parser.add_argument(
        "--zope-cpus",
        default=ZOPE_CPUS,
        help="the CPU list of each instance, separated by ';' (default: 1;3)",
    )
    parser.add_argument("--locust-cpus", default=LOCUST_CPUS)
    parser.add_argument(
        "--max-load",
        type=float,
        default=MAX_LOAD,
        help="before each run, wait while the 1-minute load average is this "
        "or more (default: %(default)g)",
    )
    parser.add_argument(
        "--port-offset",
        type=int,
        default=0,
        help="add this number to every port of each B5 run",
    )
    return parser


def _fixtures(text: str) -> tuple[str, ...]:
    names = [name for name in text.split(",") if name]
    unknown = sorted(set(names) - set(SET))
    if unknown or not names:
        raise argparse.ArgumentTypeError(
            f"the fixtures are some of {', '.join(SET)}: {text}"
        )
    return tuple(name for name in SET if name in names)


def _seconds(positive: bool):
    def parse(text: str) -> float:
        value = float(text)
        if value < 0 or (positive and value == 0):
            raise argparse.ArgumentTypeError(f"not a duration in seconds: {text}")
        return value

    return parse


def _positive(text: str) -> int:
    value = int(text)
    if value < 1:
        raise argparse.ArgumentTypeError(f"the value must be positive: {text}")
    return value


def main(
    argv: list[str] | None = None,
    execute: Callable[[dict, Path], object] | None = None,
) -> int:
    """Run the showcase, or print its plan. `execute(entry, directory)`
    runs each entry of the plan; a return is a run that ended well, an
    exception a failed run. None runs the real set."""
    command = parser()
    args = command.parse_args(argv)
    if args.json and not args.plan:
        command.error("--json goes with --plan")
    if args.trim >= args.step_seconds:
        command.error("--trim must be shorter than --step-seconds")
    options = SetOptions(
        fixtures=args.fixtures,
        step_seconds=args.step_seconds,
        warmup=args.warmup,
        trim=args.trim,
        client_objects=args.client_objects,
        server_cpus=args.server_cpus,
        zope_cpus=tuple(args.zope_cpus.split(";")),
        locust_cpus=args.locust_cpus,
    )
    commit = commit_of()
    dirty = dirty_of()
    directory = None if args.dir is None else args.dir.absolute()

    if args.plan:
        why = None if directory is None else refusal(directory, commit, options)
        if why is not None:
            print(f"showcase: {why}", file=sys.stderr)
            return 2
        document = plan(options, directory, commit, dirty)
        if args.json:
            print(json.dumps(document, indent=1))
        else:
            print_plan(document)
        return 0

    if directory is None:
        directory = ROOT / time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    why = refusal(directory, commit, options)
    if why is not None:
        print(f"showcase: {why}", file=sys.stderr)
        return 2
    (directory / "state").mkdir(parents=True, exist_ok=True)
    record = directory / "state" / "set.json"
    if not record.exists():
        stack.write_json(record, {"commit": commit, "options": options.to_json()})
    if execute is None:
        execute = Execute(options, args.port_offset, commit)

    log = Log(directory / "showcase.log")
    document = plan(options, directory, commit, dirty)
    todo = [run for run in document["runs"] if run["state"] == "todo"]
    log(
        f"start: {directory}, commit {commit}, {len(todo)} of "
        f"{len(document['runs'])} runs to do, about "
        f"{duration(document['estimate_seconds'])}"
    )
    if dirty is not False:
        tree = "the tree differs from its commit" if dirty else "git gives no record"
        log(
            f"warning: {tree}: `just bench publish` refuses the documents of "
            "this showcase"
        )
    stop = directory / "STOP"
    if stop.exists():
        stop.unlink()
        log("removed the STOP of an earlier start")
    log(f"to stop after the current run: touch {stop}")

    awake = keep_awake(log)
    try:
        previous = signal.signal(signal.SIGTERM, _interrupt)
    except ValueError:  # not the main thread
        previous = None
    try:
        return _run(document, directory, execute, args.max_load, log)
    except KeyboardInterrupt:
        log("interrupted: the current run stays to do")
        return 130
    finally:
        if previous is not None:
            signal.signal(signal.SIGTERM, previous)
        release(awake)


def _run(
    document: dict,
    directory: Path,
    execute: Callable[[dict, Path], object],
    max_load: float,
    log: Log,
) -> int:
    """Run the entries to do of `document`, in order. Return the exit code
    of the showcase."""
    stop = directory / "STOP"
    failed = []
    ran = 0
    for run in document["runs"]:
        if run["state"] == "done":
            continue
        if run["kind"] == "summary":
            missing = [id_ for id_ in run["runs"] if not done(directory, id_)]
            if missing:
                log(
                    f"skip {run['id']}: {len(missing)} B5 runs of "
                    f"{run['fixture']} are to do"
                )
                continue
        with bench_lock(log):
            if not wait_for_load(max_load, stop, log):
                break
            started = time.time()
            log(f"run {run['id']}: {describe(run)}")
            ran += 1
            try:
                note = execute(run, directory)
            except (Exception, SystemExit) as error:
                seconds = time.time() - started
                failed.append(run["id"])
                log(
                    f"failed {run['id']} after {duration(seconds)}: {error}; "
                    f"see {directory / 'logs' / (run['id'] + '.log')}"
                )
                if run["kind"] in ("build", "prepare"):
                    log(f"the showcase stops: the runs after {run['id']} need it")
                    break
            else:
                seconds = time.time() - started
                stack.write_json(
                    directory / "state" / f"{run['id']}.done",
                    {"started": started, "seconds": round(seconds, 1)},
                )
                text = f"done {run['id']} in {duration(seconds)}"
                log(text + (f": {note}" if isinstance(note, str) else ""))
        if stop.exists():
            stop.unlink()
            log(f"STOP: the showcase stops after {run['id']}")
            break
    if stop.exists():
        stop.unlink()
        log("STOP: the showcase stops")
    left = sum(not done(directory, run["id"]) for run in document["runs"])
    log(f"end: {ran} runs, {len(failed)} failed, {left} to do")
    if left == 0:
        log(f"to publish: just bench publish {directory} CHECKOUT")
    return 1 if failed else 0


def _stop_group(process: subprocess.Popen, timeout: float = 120.0) -> None:
    """Stop the process group of `process`: SIGTERM, then SIGKILL when it
    does not end in `timeout` seconds."""
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            return
        try:
            process.wait(timeout)
            return
        except subprocess.TimeoutExpired:
            continue


def child_environment(commit: str | None, **extra: str) -> dict[str, str]:
    """Return the environment of a child command: not the environment of
    bench/plone, and the commit of the showcase. ZOE_BENCH_MACHINE and
    ZOE_CXX_LIBS go on, so every document names the same machine."""
    env = dict(os.environ)
    venv = env.pop("VIRTUAL_ENV", None)
    env.pop("UV_PROJECT_ENVIRONMENT", None)
    env.pop("UV_PYTHON", None)
    if venv:
        paths = env.get("PATH", "").split(os.pathsep)
        env["PATH"] = os.pathsep.join(p for p in paths if p != f"{venv}/bin")
    if commit:
        env["ZOE_BENCH_COMMIT"] = commit
    env.update(extra)
    return env


@contextlib.contextmanager
def _environ(**values: str | None):
    """Set the variables of `values` that are not None in os.environ, and
    restore them at the end."""
    before = {key: os.environ.get(key) for key in values}
    os.environ.update({key: value for key, value in values.items() if value})
    try:
        yield
    finally:
        for key, value in before.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value


class Execute:
    """Run the entries of the set. Each run writes its output to
    DIR/logs/<id>.log."""

    def __init__(self, options: SetOptions, port_offset: int, commit: str | None):
        self.options = options
        self.settings = options.settings(port_offset)
        self.commit = commit

    def __call__(self, run: dict, directory: Path):
        return getattr(self, run["kind"])(run, Path(directory))

    def log(self, run: dict, directory: Path) -> Path:
        path = directory / "logs" / f"{run['id']}.log"
        path.parent.mkdir(parents=True, exist_ok=True)
        return path

    def command(self, command: list[str], log: Path, env: dict[str, str]) -> int:
        """Run `command` in a process group of its own, and return its exit
        code. An interrupt stops the whole group, then goes on."""
        with open(log, "a") as out:
            out.write(f"$ {' '.join(command)}\n")
            out.flush()
            process = subprocess.Popen(
                command,
                cwd=stack.REPO,
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=out,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            try:
                return process.wait()
            except BaseException:
                _stop_group(process)
                raise

    @contextlib.contextmanager
    def inprocess(self, log: Path):
        """Send the output of an in-process run to its log, with the commit
        of the showcase."""
        with (
            open(log, "a") as out,
            contextlib.redirect_stdout(out),
            contextlib.redirect_stderr(out),
            _environ(ZOE_BENCH_COMMIT=self.commit),
        ):
            try:
                yield
            except SystemExit as error:
                # The runner exits on a missing prerequisite.
                raise RuntimeError(str(error)) from None

    def build(self, run: dict, directory: Path) -> None:
        log = self.log(run, directory)
        if run["target"] == "zoe":
            if os.environ.get("ZOE_BINARY"):
                binary = stack.zoe_binary()
                if not binary.exists():
                    raise RuntimeError(f"ZOE_BINARY names no file: {binary}")
                return
            command = ["cargo", "build", "--release", "--locked"]
            command += ["-p", "zoe-server", "--bin", "zoe"]
        else:
            command = ["just", "plone", run["target"].removeprefix("plone-"), "build"]
        code = self.command(command, log, child_environment(self.commit))
        if code != 0:
            raise RuntimeError(f"{' '.join(command)} exited with {code}")

    def prepare(self, run: dict, directory: Path) -> None:
        with self.inprocess(self.log(run, directory)):
            spec = corpus.PRESETS[run["preset"]]
            fixture.prepare(spec, run["fixture"], self.settings)

    def b5(self, run: dict, directory: Path) -> str:
        out = directory / "runs" / run["id"]
        if out.exists():
            # An earlier attempt failed or stopped. Keep its files for the
            # diagnosis.
            stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
            failed = directory / "failed" / f"{run['id']}-{stamp}"
            failed.parent.mkdir(parents=True, exist_ok=True)
            out.rename(failed)
        config = stack.STACKS[run["stack"]]
        options = runner.RunOptions(
            stack=run["stack"],
            scenario=run["scenario"],
            fixture=run["fixture"],
            steps=[(users, run["step_seconds"]) for users in run["users"]],
            warmup=run["warmup_seconds"],
            trim=run["trim_seconds"],
            settings=stack.campaign_settings(config, self.settings),
            out=out,
            profile=run["profile"],
        )
        with self.inprocess(self.log(run, directory)):
            runner.run(options)
        result = json.loads((out / "result.json").read_text())
        verdict = result.get("verification", {}).get("verdict", "not run")
        return f"verification {verdict}"

    def summary(self, run: dict, directory: Path) -> None:
        from plonebench import __main__ as cli

        document = directory / f"{run['id']}.json"
        partial = directory / "state" / f"{run['id']}.json"
        partial.unlink(missing_ok=True)
        argv = ["report", *(str(directory / "runs" / id_) for id_ in run["runs"])]
        argv += ["--out", str(directory / f"{run['id']}.md"), "--json", str(partial)]
        with self.inprocess(self.log(run, directory)):
            try:
                code = cli.main(argv)
            except SystemExit as error:
                code = error.code
        if code not in (0, None) or not partial.exists():
            raise RuntimeError(f"report exited with {code}")
        partial.replace(document)

    def native(self, run: dict, directory: Path) -> None:
        self.shootout(run, directory, ["just", "bench", "native"])

    def client(self, run: dict, directory: Path) -> None:
        command = ["just", "bench", "client", "--objects", str(run["objects"])]
        self.shootout(run, directory, command)

    def shootout(self, run: dict, directory: Path, command: list[str]) -> None:
        """Run the native gate or the client benchmark in the environment
        of bench/shootout, on the Python of the run. A run that writes its
        result ended well, also when it exits with 1: the verdict is the
        work of the report."""
        partial = directory / "state" / f"{run['id']}.json"
        partial.unlink(missing_ok=True)
        command = [*command, "--output", str(partial)]
        env = child_environment(
            self.commit,
            UV_PYTHON=run["python"],
            ZOE_BINARY=str(stack.zoe_binary().absolute()),
        )
        code = self.command(command, self.log(run, directory), env)
        if code not in (0, 1) or not partial.exists():
            raise RuntimeError(f"{' '.join(command[:3])} exited with {code}")
        partial.replace(directory / f"{run['id']}.json")
