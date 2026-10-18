"""The command line of the replay.

    python -m replay run --store FILE --probes DIR [options]
    python -m replay report FILE

`run` reads the log of a kept store, replays it, writes the result and
prints its report. With `--policy FILE`, a JSON merge policy of
`zoe.policy`, each run of `zoe serve` installs it in its store after the
base. `report` prints the report of a result. Both exit with
0 when the gate passes and with 1 when it fails. `report` exits with 2
when FILE is not a result, and `run` when the policy is not valid, when a timed
transaction writes the root with a policy, or when the probe records
explain no transaction of the store.
"""

import argparse
import json
import sys
import time
from pathlib import Path

from replay import log as replay_log
from replay import report, runner


def _speeds(text):
    speeds = tuple(float(s) if "." in s else int(s) for s in text.split(","))
    if any(speed <= 0 for speed in speeds) or len(set(speeds)) != len(speeds):
        raise argparse.ArgumentTypeError("the speeds must be positive and distinct")
    return speeds


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
    play.add_argument(
        "--zoe-serve-arg",
        action="append",
        default=[],
        help="an argument for zoe serve, after its own; repeat it for more",
    )
    play.add_argument(
        "--policy",
        type=Path,
        help="a JSON merge policy that each run of zoe serve installs after the base",
    )
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

    policy = None
    if args.policy is not None:
        import zoe.policy

        try:
            policy = zoe.policy.validate(json.loads(args.policy.read_text()))
        except (OSError, ValueError) as error:
            message = f"replay: {args.policy} is not a merge policy: {error}"
            print(message, file=sys.stderr)
            return 2
    probes = sorted(args.probes.glob("instance*.jsonl"))
    if not probes:
        print(
            f"replay: no probe records instance*.jsonl in {args.probes}",
            file=sys.stderr,
        )
        return 2
    starts, matched = replay_log.starts_of_probes(args.store, probes)
    log = replay_log.read_log(args.store, starts)
    background = sum(t.description in replay_log.BACKGROUND for t in log.transactions)
    print(
        f"replay: {len(log.base)} transactions in the base, "
        f"{len(log.transactions)} timed; starts matched {matched['matched']}, "
        f"unmatched {matched['unmatched']}, of a background job {background}",
        flush=True,
    )
    # Without a request that explains a transaction, every transaction
    # starts at its commit and overlaps nothing: the gate would pass.
    if not log.transactions or not matched["matched"]:
        print(
            "replay: the probe records explain no transaction of the store",
            file=sys.stderr,
        )
        return 2
    # The root has no resolver: such a transaction would store the root of
    # the log, without the name of the policy.
    if policy is not None and (written := runner.root_writes(log)):
        print(
            f"replay: --policy: the timed transaction {written[0].hex()} writes the "
            "root, oid 0, and its record would remove the merge policy from the root",
            file=sys.stderr,
        )
        return 2
    started = time.time()
    result = runner.run(
        log,
        workdir=args.workdir,
        stacks=args.stacks,
        speeds=args.speeds,
        repetitions=args.repetitions,
        serve_args=args.zoe_serve_arg,
        policy=policy,
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
    printed = report.lines(result)
    print("\n".join(printed))
    return 0 if printed[-1].startswith("gate passed:") else 1


if __name__ == "__main__":
    sys.exit(main())
