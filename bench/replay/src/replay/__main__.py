"""The command line of the replay.

    python -m replay run --store FILE --probes DIR [options]
    python -m replay report FILE

`run` reads the log of a kept store, replays it and writes the result.
`report` prints the report of a result; it exits with 0 when the gate
passes, with 1 when it fails and with 2 when FILE is not a result.
"""

import argparse
import json
import sys
import time
from pathlib import Path

from replay import log as replay_log
from replay import report, runner


def _speeds(text):
    return tuple(float(s) if "." in s else int(s) for s in text.split(","))


def _stacks(text):
    stacks = tuple(text.split(","))
    for stack in stacks:
        if stack not in runner.STACKS:
            raise argparse.ArgumentTypeError(f"unknown stack {stack}")
    return stacks


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(prog="python -m replay")
    commands = parser.add_subparsers(dest="command", required=True)
    play = commands.add_parser("run", help="replay a kept store")
    play.add_argument(
        "--store", required=True, type=Path, help="the Data.fs of the run"
    )
    play.add_argument(
        "--probes", required=True, type=Path, help="the probe directory of the run"
    )
    play.add_argument("--stacks", type=_stacks, default=runner.STACKS)
    play.add_argument("--speeds", type=_speeds, default=runner.SPEEDS)
    play.add_argument("--repetitions", type=int, default=1)
    play.add_argument("--workdir", type=Path, required=True)
    play.add_argument("--out", type=Path, required=True)
    show = commands.add_parser("report", help="print the report of a result")
    show.add_argument("file", type=Path)
    args = parser.parse_args(argv)

    if args.command == "report":
        try:
            result = report.load(args.file)
        except report.NotAResult as error:
            print(error, file=sys.stderr)
            return 2
        printed = report.lines(result)
        print("\n".join(printed))
        return 0 if printed[-1].startswith("gate passed:") else 1

    probes = sorted(args.probes.glob("instance*.jsonl"))
    starts, matched = replay_log.starts_of_probes(args.store, probes)
    log = replay_log.read_log(args.store, starts)
    print(
        f"replay: {len(log.base)} transactions in the base, "
        f"{len(log.transactions)} timed; starts matched {matched['matched']}, "
        f"unmatched {matched['unmatched']}",
        flush=True,
    )
    started = time.time()
    result = runner.run(
        log,
        workdir=args.workdir,
        stacks=args.stacks,
        speeds=args.speeds,
        repetitions=args.repetitions,
    )
    result["started"] = started
    result["log"] = {
        "store": str(args.store),
        "base": len(log.base),
        "timed": len(log.transactions),
        "starts": matched,
    }
    args.out.parent.mkdir(parents=True, exist_ok=True)
    partial = args.out.with_name(f".{args.out.name}.partial")
    partial.write_text(json.dumps(result, indent=1))
    partial.replace(args.out)
    print("\n".join(report.lines(result)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
