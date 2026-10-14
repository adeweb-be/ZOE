"""The command line of the harness. `just bench` calls it."""

import argparse
import json
import os
import sys
from pathlib import Path

import jsonschema

from shootout import gate, run
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
        help="the storages to run (default: all; relstorage only with a DSN)",
    )
    run_cmd.add_argument(
        "--dsn",
        default=os.environ.get("ZOE_BENCH_PG_DSN"),
        help="the PostgreSQL DSN for relstorage (default: $ZOE_BENCH_PG_DSN)",
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
        default=1.0,
        help="the smallest ratio of ops/s in each cell but "
        f"{', '.join(gate.LEFT_OUT)} (default: %(default)g, parity)",
    )
    gate_cmd.add_argument("root", type=Path, help="the root of a results tree")

    args = parser.parse_args(argv)
    if args.command == "index":
        print(write_index(args.root))
        return 0
    if args.command == "gate":
        return gate.report(
            args.root, args.storage, args.baseline, args.ratio, sys.stdout
        )

    storages = args.storage or [
        s for s in STORAGES if args.dsn or not STORAGES[s].needs_dsn
    ]
    if "relstorage" in storages and not args.dsn:
        parser.error("relstorage needs --dsn or ZOE_BENCH_PG_DSN")
    if "relstorage" not in storages:
        print("relstorage skipped: no PostgreSQL DSN", file=sys.stderr)

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
