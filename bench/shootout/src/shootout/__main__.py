"""The command line of the harness. `just bench` calls it."""

import argparse
import json
import os
import sys
from pathlib import Path

import jsonschema
from benchcommon import files

from shootout import client, gate, native, publish, run
from shootout.layout import result_path, write_index
from shootout.storages import STORAGES

# The harness runs from its source tree, so the schema is beside it.
SCHEMA = publish.SCHEMAS / publish.SCHEMA_OF["shootout"]
# The families that write one result file, with the name of each in a
# message.
FAMILIES = {"client": "the client benchmark", "native": "the native gate"}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="shootout", description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)

    run_cmd = commands.add_parser("run", help="run the baseline and write the results")
    run_cmd.add_argument("--quick", action="store_true", help="run the quick set only")
    run_cmd.add_argument(
        "--storage",
        nargs="+",
        choices=sorted(STORAGES),
        help="the storages to run "
        "(default: all; relstorage and relstorage-hf only with a DSN)",
    )
    run_cmd.add_argument(
        "--dsn",
        default=os.environ.get("ZOE_BENCH_PG_DSN"),
        help="the PostgreSQL DSN for relstorage and relstorage-hf "
        "(default: $ZOE_BENCH_PG_DSN)",
    )
    run_cmd.add_argument(
        "--machine",
        help="the machine name in the results "
        "(default: $ZOE_BENCH_MACHINE, else the CPU model)",
    )
    run_cmd.add_argument(
        "--output",
        type=Path,
        default=Path("target/bench/results"),
        help="the root of the results tree (default: %(default)s)",
    )
    run_cmd.add_argument(
        "--workdir",
        type=Path,
        default=Path("target/bench/work"),
        help="where the storages write their files: each value of a cell runs on "
        "a fresh store there, and the harness removes it when the value ends "
        "(default: %(default)s)",
    )

    index_cmd = commands.add_parser(
        "index",
        help="rewrite index.json of a results tree, of version 2 (ADR 0054)",
    )
    index_cmd.add_argument("root", type=Path)

    publish_cmd = commands.add_parser(
        "publish",
        help="copy result documents into a checkout of the results repository "
        "and rewrite its index.json; it never commits and never pushes",
    )
    publish_cmd.add_argument(
        "--main",
        default=publish.MAIN,
        metavar="REF",
        help="the branch that holds the commit of each document (default: %(default)s)",
    )
    publish_cmd.add_argument(
        "sources",
        nargs="+",
        metavar="SRC",
        help="a document, or a directory whose *.json files are documents",
    )
    publish_cmd.add_argument(
        "checkout", metavar="CHECKOUT", help="a checkout of the results repository"
    )

    gate_cmd = commands.add_parser(
        "gate",
        help="compare the last results of a storage and a baseline, cell by cell",
    )
    gate_cmd.add_argument(
        "--storage",
        default="zoe-server",
        help="the storage under test (default: %(default)s)",
    )
    gate_cmd.add_argument(
        "--baseline", default="zeo", help="the baseline (default: %(default)s)"
    )
    gate_cmd.add_argument(
        "--ratio",
        type=_ratio,
        help=f"the smallest ratio of ops/s in each cell that counts "
        f"(default: {gate.PARITY:g}, parity)",
    )
    gate_cmd.add_argument(
        "--python",
        help="count only the results of this Python build, such as cp314t, "
        "and print the others beside them (default: every build counts)",
    )
    gate_cmd.add_argument("root", type=Path, help="the root of a results tree")

    # The options of a run of the client benchmark or of the native gate.
    family = argparse.ArgumentParser(add_help=False)
    family.add_argument(
        "--machine",
        help="the machine name in the result "
        "(default: $ZOE_BENCH_MACHINE, else the CPU model)",
    )
    family.add_argument(
        "--workdir",
        type=Path,
        default=Path("target/bench/work"),
        help="where the servers write their stores and the caches their "
        "files; the run removes them when it ends (default: %(default)s)",
    )
    family.add_argument(
        "--output",
        type=Path,
        help="the result file (default: target/bench/<family>/<started>.json)",
    )

    client_cmd = commands.add_parser(
        "client",
        help="the client benchmark: ZEO.ClientStorage against the native client",
    )
    client_cmds = client_cmd.add_subparsers(dest="action", required=True)
    client_run = client_cmds.add_parser(
        "run",
        parents=[family],
        help="run both clients against each server and write one result",
    )
    client_run.add_argument(
        "--objects",
        type=_positive,
        default=client.OBJECTS,
        help="the objects that each worker loads in a cold or warm round "
        "(default: %(default)s)",
    )
    client_run.add_argument(
        "--server",
        nargs="+",
        choices=client.SERVERS,
        default=list(client.SERVERS),
        help="the servers to run against (default: both)",
    )
    client_run.add_argument(
        "--commits",
        type=_positive,
        help="the commits of each worker in a commit round "
        "(default: max(1, objects // 50))",
    )
    client_run.add_argument(
        "--rounds",
        type=_positive,
        default=client.ROUNDS,
        help="the rounds of each row, at least 3 (default: %(default)s)",
    )
    client_report = client_cmds.add_parser(
        "report", help="print the rows of a result and the verdict"
    )
    client_report.add_argument("result", type=Path)

    native_cmd = commands.add_parser(
        "native",
        help="the phase 3 gate: zoe serve through the native client against ZEO",
    )
    native_cmds = native_cmd.add_subparsers(dest="action", required=True)
    native_run = native_cmds.add_parser(
        "run",
        parents=[family],
        help="run the gate, write one result and print the report",
    )
    native_run.add_argument(
        "--cell",
        nargs="+",
        type=_native_cell,
        help="run only these cells, as MODE:CONCURRENCY:SIZE, such as "
        "processes:16:102400 (default: the 6 cells of the gate)",
    )
    native_run.add_argument(
        "--scenario",
        nargs="+",
        choices=native.SCENARIOS,
        help="run only these scenarios (default: all 5)",
    )
    native_report = native_cmds.add_parser(
        "report", help="print the rows of a result and the verdict"
    )
    native_report.add_argument("result", type=Path)

    args = parser.parse_args(argv)
    if args.command in FAMILIES:
        return _family(args, parser)
    if args.command == "index":
        try:
            print(write_index(args.root))
        except ValueError as error:
            print(f"index: {error}", file=sys.stderr)
            return 1
        return 0
    if args.command == "publish":
        return publish.publish(args.sources, args.checkout, args.main)
    if args.command == "gate":
        ratio = args.ratio or gate.PARITY
        return gate.report(
            args.root, args.storage, args.baseline, ratio, sys.stdout, args.python
        )

    storages = args.storage or [
        s for s in STORAGES if args.dsn or not STORAGES[s].needs_dsn
    ]
    needs_dsn = [s for s in storages if STORAGES[s].needs_dsn]
    if needs_dsn and not args.dsn:
        verb = "needs" if len(needs_dsn) == 1 else "need"
        parser.error(f"{' and '.join(needs_dsn)} {verb} --dsn or ZOE_BENCH_PG_DSN")
    if not args.dsn and not args.storage:
        skipped = " and ".join(s for s in STORAGES if STORAGES[s].needs_dsn)
        print(f"{skipped} skipped: no PostgreSQL DSN", file=sys.stderr)

    schema = json.loads(SCHEMA.read_text())
    for storage in storages:
        print(f"== {storage}", flush=True)
        result = run(
            storage,
            quick=args.quick,
            workdir=args.workdir,
            dsn=args.dsn,
            machine=args.machine,
        )
        _write(result, args.output, schema)
    write_index(args.output)
    return 0


def _write(result: dict, root: Path, schema: dict) -> None:
    """Validate a result, write it under root and print its rows."""
    jsonschema.validate(result, schema)
    path = result_path(root, result)
    files.write_json(path, result)
    print(f"== {result['storage']}", flush=True)
    _summary(result)
    print(path, flush=True)


def _family(args, parser) -> int:
    """Run the client benchmark or the native gate, or report on a result
    of it."""
    family = args.command
    module = client if family == "client" else native
    schema = json.loads((publish.SCHEMAS / publish.SCHEMA_OF[family]).read_text())
    if args.action == "report":
        result = json.loads(args.result.read_text())
        try:
            jsonschema.validate(result, schema)
        except jsonschema.ValidationError as error:
            print(
                f"{args.result} is not a result of {FAMILIES[family]}: {error.message}",
                file=sys.stderr,
            )
            return 2
        return module.report(result, sys.stdout)
    if family == "client":
        if args.rounds < client.ROUNDS:
            parser.error(f"--rounds must be at least {client.ROUNDS}")
        result = client.run(
            objects=args.objects,
            servers=tuple(args.server),
            workdir=args.workdir,
            machine=args.machine,
            rounds=args.rounds,
            commits=args.commits,
        )
    else:
        result = native.run(
            cells=args.cell or native.CELLS,
            scenarios=args.scenario or native.SCENARIOS,
            workdir=args.workdir,
            machine=args.machine,
        )
    jsonschema.validate(result, schema)
    output = args.output
    if output is None:
        stamp = result["run"]["started"].replace("-", "").replace(":", "")
        output = Path("target/bench") / family / f"{stamp}.json"
    # A run that stops in the write leaves no part of the result.
    files.write_json(output, result)
    if family == "native":
        print(output, flush=True)
        return native.report(result, sys.stdout)
    # The verdict of a run of the client benchmark is the work of the
    # report, not of the run.
    client.report(result, sys.stdout)
    print(output, flush=True)
    return 0


def _native_cell(text: str) -> native.Cell:
    """Parse MODE:CONCURRENCY:SIZE, a cell of the native gate."""
    try:
        mode, concurrency, size = text.split(":")
        cell = native.Cell(mode, int(concurrency), int(size))
    except ValueError:
        raise argparse.ArgumentTypeError(
            f"a cell is MODE:CONCURRENCY:SIZE: {text}"
        ) from None
    if cell not in native.CELLS:
        raise argparse.ArgumentTypeError(f"not a cell of the native gate: {text}")
    return cell


def _positive(text: str) -> int:
    value = int(text)
    if value < 1:
        raise argparse.ArgumentTypeError(f"the value must be positive: {text}")
    return value


def _ratio(text: str) -> float:
    value = float(text)
    if not value > 0:
        raise argparse.ArgumentTypeError(f"the ratio must be positive: {text}")
    return value


ROW = "{:<10} {:<9} {:>3} {:>7} {:>10} {:>9} {:>9}"


def _summary(result: dict) -> None:
    print(ROW.format("scenario", "mode", "c", "size", "ops/s", "p50 ms", "p99 ms"))
    for s in result["scenarios"]:
        print(
            ROW.format(
                s["name"],
                s["mode"],
                s["concurrency"],
                s["object_size"],
                f"{s['ops_per_second']:.0f}",
                f"{s['p50_seconds'] * 1e3:.2f}",
                f"{s['p99_seconds'] * 1e3:.2f}",
            )
        )


if __name__ == "__main__":
    sys.exit(main())
