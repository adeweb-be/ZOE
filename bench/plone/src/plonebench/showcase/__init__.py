"""The publishable set of the benchmarks, in one command.

    python -m plonebench showcase --plan [--json] [--set NAME] [OPTIONS]
    python -m plonebench showcase [--dir DIR] [--set NAME] [OPTIONS]

`just bench showcase OPTIONS` runs it in the environment of bench/plone.
`--set` chooses the set: `default` (also without the option) or
`capacity`. The default set, in the order of its runs:

1. the builds: the release `zoe` binary, and the Plone instances `zeo` and
   `zoeclient`;
2. for each fixture, `small` then `medium`: the fixture, when it is
   missing; the B5 runs of the fixture on the stacks `zeo` and
   `zoe-native-qi-kv`; the B5 summary of the fixture (`report --json`,
   ADR 0054);
3. the native gate and the client benchmark, each on CPython 3.13 and on
   3.14t.

The capacity set has the builds, then the same three steps for the
fixture `xlarge` (or `xlarge-mini`), with 4 instances of 2 threads, and
no native gate and no client benchmark.

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
import json
import signal
import sys
import time
from collections.abc import Callable
from pathlib import Path

from benchcommon import files

# The commit of the showcase and whether the tree differs from it, as every
# harness records them.
from benchcommon.provenance import commit as commit_of
from benchcommon.provenance import dirty as dirty_of

from plonebench import stack
from plonebench.showcase.execute import Execute, child_environment
from plonebench.showcase.host import (
    PROVENANCE,
    Log,
    bench_lock,
    check_provenance,
    keep_awake,
    provenance,
    record_of,
    release,
    wait_for_load,
)
from plonebench.showcase.plan import (
    CLIENT_OBJECTS,
    MAX_LOAD,
    ROOT,
    SETS,
    STEP_SECONDS,
    TRIM,
    WARMUP,
    SetOptions,
    describe,
    done,
    duration,
    entries,
    estimate,
    fixtures_of,
    plan,
    print_plan,
    refusal,
)

__all__ = [
    "Execute",
    "Log",
    "SetOptions",
    "bench_lock",
    "child_environment",
    "entries",
    "estimate",
    "main",
    "plan",
    "refusal",
]


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
        "--set",
        choices=tuple(SETS),
        default="default",
        help="the set: default (small and medium, the native gate and the "
        "client benchmark) or capacity (xlarge with 4 instances); each set "
        "gives the defaults of --fixtures, --instances, --threads and the "
        "CPU lists (default: %(default)s)",
    )
    parser.add_argument(
        "--fixtures",
        type=_names,
        help="the fixtures of the set, separated by ',' (default: small,medium "
        "for the set default, xlarge for the set capacity)",
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
    parser.add_argument(
        "--instances",
        type=_positive,
        help="the Zope instances of each B5 run (default: 2, 4 for capacity)",
    )
    parser.add_argument(
        "--threads",
        type=_positive,
        help="the threads of each instance (default: 2)",
    )
    parser.add_argument("--server-cpus", help="the CPU list of the server (default: 0)")
    parser.add_argument(
        "--zope-cpus",
        help="the CPU list of each instance, separated by ';', one for each "
        "instance (default: 1;3, 1;3;6;8 for capacity)",
    )
    parser.add_argument(
        "--locust-cpus",
        help="the CPU list of Locust (default: 6, 12,13 for capacity)",
    )
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


def _names(text: str) -> tuple[str, ...]:
    names = tuple(name for name in text.split(",") if name)
    if not names:
        raise argparse.ArgumentTypeError(f"no fixture: {text!r}")
    return names


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
    definition = SETS[args.set]
    try:
        fixtures = fixtures_of(args.set, args.fixtures)
    except ValueError as error:
        command.error(str(error))

    def given(value, default):
        return default if value is None else value

    options = SetOptions(
        fixtures=fixtures,
        step_seconds=args.step_seconds,
        warmup=args.warmup,
        trim=args.trim,
        client_objects=args.client_objects,
        server_cpus=given(args.server_cpus, definition.server_cpus),
        zope_cpus=tuple(given(args.zope_cpus, definition.zope_cpus).split(";")),
        locust_cpus=given(args.locust_cpus, definition.locust_cpus),
        set=args.set,
        instances=given(args.instances, definition.instances),
        threads=given(args.threads, definition.threads),
    )
    # Each instance runs on a CPU list of its own: refuse a short list
    # before the plan, and before anything is written.
    short = stack.cpus_refusal(options.settings())
    if short is not None:
        command.error(short)
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
        record_ = {"commit": commit, "options": options.to_json()}
        files.write_json(record, record_, indent=1)
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

    check_provenance(document, directory, log)
    awake = keep_awake(log)
    # SIGTERM, and SIGHUP when the terminal closes, stop the current run as
    # Ctrl-C does.
    previous = {}
    try:
        for sig in (signal.SIGTERM, signal.SIGHUP):
            previous[sig] = signal.signal(sig, _interrupt)
    except ValueError:  # not the main thread
        pass
    try:
        return _run(document, directory, execute, args.max_load, log)
    except KeyboardInterrupt:
        log("interrupted: the current run stays to do")
        return 130
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)
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
        if done(directory, run["id"]) or _waits(run, directory, log):
            continue
        with bench_lock(log):
            # Another start on the same directory can end a run while this
            # one waits for the lock.
            if done(directory, run["id"]):
                log(f"skip {run['id']}: another start ran it")
                continue
            if not wait_for_load(max_load, stop, log):
                break
            started = time.time()
            log(f"run {run['id']}: {describe(run)}")
            ran += 1
            try:
                outcome = execute(run, directory)
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
                record = dict(outcome) if isinstance(outcome, dict) else {}
                record.update(provenance(run))
                files.write_json(
                    directory / "state" / f"{run['id']}.done",
                    {"started": started, "seconds": round(seconds, 1), **record},
                    indent=1,
                )
                log(f"done {run['id']} in {duration(seconds)}{_told(run, record)}")
        if stop.exists():
            stop.unlink()
            log(f"STOP: the showcase stops after {run['id']}")
            break
    if stop.exists():
        stop.unlink()
        log("STOP: the showcase stops")
    records = {run["id"]: record_of(directory, run["id"]) for run in document["runs"]}
    left = sum(record is None for record in records.values())
    unverified = [
        run["id"]
        for run in document["runs"]
        if run["kind"] == "b5"
        and records[run["id"]] is not None
        and records[run["id"]].get("verdict", "pass") != "pass"
    ]
    unmet = [
        run["id"]
        for run in document["runs"]
        if run["kind"] == "native"
        and records[run["id"]] is not None
        and records[run["id"]].get("exit") == 1
    ]
    log(
        f"end: {ran} runs, {len(failed)} failed, {left} to do; "
        f"{len(unverified)} B5 runs not verified pass; "
        f"{len(unmet)} native gates not met"
    )
    for id_ in unverified:
        # A failed verification is a result: no start runs it again.
        log(
            f"{id_} did not pass its verification: `just bench publish` "
            "refuses the summary of its fixture"
        )
    if left == 0 and not unverified:
        log(f"to publish: just bench publish {directory} CHECKOUT")
    return 1 if failed else 0


def _waits(run: dict, directory: Path, log: Log) -> bool:
    """Tell if `run` is the summary of a fixture whose B5 runs are not all
    done."""
    if run["kind"] != "summary":
        return False
    missing = [id_ for id_ in run["runs"] if not done(directory, id_)]
    if missing:
        log(f"skip {run['id']}: {len(missing)} B5 runs of {run['fixture']} are to do")
    return bool(missing)


def _told(run: dict, record: dict) -> str:
    """Return what the log says of a run that ended well."""
    parts = []
    if "verdict" in record:
        parts.append(f"verification {record['verdict']}")
    if record.get("zope_clock_mhz") is not None:
        parts.append(f"zope clock {round(record['zope_clock_mhz'])} MHz")
    if record.get("zope_throttle_seconds") is not None:
        parts.append(f"zope throttle {record['zope_throttle_seconds']:.1f} s")
    if "exit" in record:
        text = f"exit {record['exit']}"
        if run["kind"] == "native" and record["exit"] == 1:
            text += ", a cell does not pass the gate"
        parts.append(text)
    for key in PROVENANCE:
        if record.get(key):
            parts.append(f"{key} {record[key][:12]}")
    return ": " + ", ".join(parts) if parts else ""
