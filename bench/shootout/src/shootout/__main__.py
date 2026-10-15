"""The command line of the harness. `just bench` calls it."""

import argparse
import json
import os
import sys
from pathlib import Path

import jsonschema

from shootout import client, gate, run
from shootout.layout import result_path, write_index
from shootout.storages import STORAGES

# The harness runs from its source tree, so the schema is beside it.
SCHEMA = Path(__file__).resolve().parents[3] / "schema" / "results.schema.json"


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
        "index", help="rewrite index.json of a results tree"
    )
    index_cmd.add_argument("root", type=Path)

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
        help="the smallest ratio of ops/s in each cell that counts (default: "
        + ", ".join(f"{k} {v:g}" for k, v in gate.RATIOS.items())
        + f", else {gate.PARITY:g}, parity)",
    )
    gate_cmd.add_argument(
        "--python",
        help="count only the results of this Python build, such as cp314t, "
        "and print the others beside them (default: every build counts)",
    )
    gate_cmd.add_argument("root", type=Path, help="the root of a results tree")

    client_cmd = commands.add_parser(
        "client",
        help="the client benchmark: ZEO.ClientStorage against the native client",
    )
    client_cmds = client_cmd.add_subparsers(dest="client_command", required=True)
    client_run = client_cmds.add_parser(
        "run", help="run both clients against each server and write one result"
    )
    client_run.add_argument(
        "--objects",
        type=_positive,
        default=client.OBJECTS,
        help="the objects that each worker loads in a cold or warm round; each "
        "worker commits max(1, objects // 100) times (default: %(default)s)",
    )
    client_run.add_argument(
        "--server",
        nargs="+",
        choices=client.SERVERS,
        default=list(client.SERVERS),
        help="the servers to run against (default: both)",
    )
    client_run.add_argument(
        "--rounds",
        type=_positive,
        default=client.ROUNDS,
        help="the rounds of each row, at least 3 (default: %(default)s)",
    )
    client_run.add_argument(
        "--machine",
        help="the machine name in the result "
        "(default: $ZOE_BENCH_MACHINE, else the CPU model)",
    )
    client_run.add_argument(
        "--workdir",
        type=Path,
        default=Path("target/bench/work"),
        help="where the servers and the caches write their files "
        "(default: %(default)s)",
    )
    client_run.add_argument(
        "--output",
        type=Path,
        help="the result file (default: target/bench/client/<started>.json)",
    )
    client_report = client_cmds.add_parser(
        "report", help="print the rows of a result and the verdict"
    )
    client_report.add_argument("result", type=Path)

    args = parser.parse_args(argv)
    if args.command == "client":
        return _client(args, parser)
    if args.command == "index":
        print(write_index(args.root))
        return 0
    if args.command == "gate":
        ratio = args.ratio or gate.default_ratio(args.storage)
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
        jsonschema.validate(result, schema)
        path = result_path(args.output, result)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(result, indent=2) + "\n")
        _summary(result)
        print(path, flush=True)
    write_index(args.output)
    return 0


def _client(args, parser) -> int:
    """Run the client benchmark, or report on a result of it."""
    if args.client_command == "report":
        result = json.loads(args.result.read_text())
        return client.report(result, sys.stdout)
    if args.rounds < client.ROUNDS:
        parser.error(f"--rounds must be at least {client.ROUNDS}")
    result = client.run(
        objects=args.objects,
        servers=tuple(args.server),
        workdir=args.workdir,
        machine=args.machine,
        rounds=args.rounds,
    )
    jsonschema.validate(result, json.loads(client.SCHEMA.read_text()))
    output = args.output
    if output is None:
        stamp = result["run"]["started"].replace("-", "").replace(":", "")
        output = Path("target/bench/client") / f"{stamp}.json"
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(result, indent=2) + "\n")
    # The verdict of a run is the work of the report, not of the run.
    client.report(result, sys.stdout)
    print(output, flush=True)
    return 0


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
