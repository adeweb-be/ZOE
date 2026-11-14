"""The command line of the catalog fixture and of the query log.

    python -m plonebench.catalog prepare [--preset catalog-mini] [--name NAME]
        [--seed N] [--force]
    python -m plonebench.catalog fixtures
    python -m plonebench.catalog inventory LOG... [--json FILE]
    python -m plonebench.catalog replay LOG... STORE [--legs inprocess,serve]
    python -m plonebench.catalog replay --text DIR... [--legs inprocess,serve]
    python -m plonebench.catalog plans [--fixture NAME] [--log LOG...] [--expect]
        [--catalog-memory SIZE] [--rows N]
    python -m plonebench.catalog [run] [--fixture NAME] [ZOE-BENCH OPTIONS...]
    python -m plonebench.catalog SCENARIO [--fixture NAME] [ZOE-BENCH OPTIONS...]

`run`, the default, runs `zoe-bench catalog` with its default scenarios on
a copy of the fixture, `catalog-1m` by default; a SCENARIO (cold,
intersect, mix, text or text-rebuild) runs that scenario alone. The other
options go to `zoe-bench catalog`.

`just bench catalog ...` runs it in the environment of bench/plone.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import sys

from plonebench.catalog import fixture, plan


def main(argv: list[str] | None = None) -> int:
    from plonebench.catalog import plans, replay

    argv = sys.argv[1:] if argv is None else list(argv)
    if (
        not argv
        or argv[0] == "run"
        or argv[0] in plans.SCENARIOS
        or argv[0].startswith("-")
    ):
        return _run(argv)
    defaults = replay.Settings()
    parser = argparse.ArgumentParser(prog="plonebench.catalog")
    commands = parser.add_subparsers(dest="command", required=True)
    prepare = commands.add_parser("prepare", help="build a catalog fixture")
    prepare.add_argument("--preset", choices=sorted(plan.PRESETS), default="catalog-1m")
    prepare.add_argument("--seed", type=int, help="the seed, over the preset")
    prepare.add_argument("--name", help="the name of the fixture directory")
    prepare.add_argument("--force", action="store_true")
    commands.add_parser("fixtures", help="list the catalog fixtures")
    inventory = commands.add_parser(
        "inventory", help="the index types, options and sorts of query logs"
    )
    inventory.add_argument("logs", nargs="+", help="log files or directories")
    inventory.add_argument("--json", help="also write the inventory to this file")
    inventory.add_argument("--title", default="", help="the title of the tables")
    replaying = commands.add_parser(
        "replay", help="replay query logs in the mode compare on a ZOE store"
    )
    replaying.add_argument("paths", nargs="+", help="LOG... STORE")
    replaying.add_argument("--legs", default=",".join(defaults.legs))
    for name in (
        "rebase_rows",
        "window_tids",
        "window_seconds",
        "pinned_every",
        "pinned_sync",
        "rounds",
        "seed",
    ):
        replaying.add_argument(
            f"--{name.replace('_', '-')}", type=int, default=getattr(defaults, name)
        )
    replaying.add_argument("--limit", type=int, default=None)
    replaying.add_argument("--catalog-memory", default=None)
    replaying.add_argument(
        "--writer-interval", type=float, default=defaults.writer_interval
    )
    replaying.add_argument("--no-flip", action="store_true")
    replaying.add_argument(
        "--text",
        action="store_true",
        help="replay the text queries of the directories of"
        " `just plone zoe catalog text`, against ZCTextIndex",
    )
    replaying.add_argument("--json", help="also write the report to this file")
    making = commands.add_parser(
        "plans", help="make the plans of a fixture for zoe-bench catalog"
    )
    making.add_argument("--fixture", help="the name of the fixture")
    making.add_argument("--log", nargs="*", default=[], help="query logs")
    making.add_argument("--expect", action="store_true")
    making.add_argument("--catalog-memory", default=None)
    making.add_argument("--rows", type=int, default=None)
    args = parser.parse_args(argv)
    if args.command == "prepare":
        spec = plan.PRESETS[args.preset]
        if args.seed is not None:
            spec = dataclasses.replace(spec, seed=args.seed)
        name = args.name or fixture.default_name(spec, args.preset)
        fixture.prepare(spec, fixture.fixture_dir(name), args.preset, force=args.force)
        print(name)
        return 0
    if args.command == "inventory":
        from plonebench.catalog import log

        totals = {}
        lines = log.read_all(replay.log_files(args.logs), totals)
        found = log.inventory(lines, totals.get("unrecorded", 0))
        print(log.format_inventory(found, args.title))
        if args.json:
            with open(args.json, "w") as out:
                json.dump(found, out, indent=1)
        return 0
    if args.command == "plans":
        target = fixture.fixture_dir(args.fixture or plans.default_fixture())
        out = plans.make(
            target,
            args.log,
            expect=args.expect,
            catalog_memory=args.catalog_memory,
            rows=args.rows,
        )
        print(out)
        return 0
    if args.command == "replay" and args.text:
        return _replay_text(args)
    if args.command == "replay":
        return _replay(args)
    for line in fixture.fixtures():
        print(line)
    return 0


def _run(argv: list[str]) -> int:
    """`run` or a scenario alone: the options of `zoe-bench catalog`, and
    `--fixture NAME`."""
    from plonebench.catalog import plans

    if argv and argv[0] == "run":
        argv = argv[1:]
    args = []
    if argv and argv[0] in plans.SCENARIOS:
        args += ["--scenario", argv[0]]
        argv = argv[1:]
    name = None
    rest = iter(argv)
    for arg in rest:
        if arg == "--fixture":
            name = next(rest, None)
        elif arg.startswith("--fixture="):
            name = arg.split("=", 1)[1]
        else:
            args.append(arg)
    try:
        plans.run(name, args)
    except (FileNotFoundError, RuntimeError) as error:
        print(f"plonebench.catalog: {error}", file=sys.stderr)
        return 1
    return 0


def _replay(args) -> int:
    from plonebench.catalog import log, replay

    if len(args.paths) < 2:
        print("plonebench.catalog replay: give LOG... STORE", file=sys.stderr)
        return 2
    settings = replay.Settings(
        legs=tuple(filter(None, args.legs.split(","))),
        rebase_rows=args.rebase_rows,
        window_tids=args.window_tids,
        window_seconds=args.window_seconds,
        writer_interval=args.writer_interval,
        pinned_every=args.pinned_every,
        pinned_sync=args.pinned_sync,
        rounds=args.rounds,
        limit=args.limit,
        catalog_memory=args.catalog_memory,
        flip=not args.no_flip,
        seed=args.seed,
    )
    try:
        report = replay.replay(args.paths[:-1], args.paths[-1], settings)
    except log.LogError as error:
        print(f"plonebench.catalog replay: {error}", file=sys.stderr)
        return 2
    print(replay.format_report(report))
    print(f"replay: the report is in {report['directory']}/report.json")
    if args.json:
        with open(args.json, "w") as out:
            json.dump(report, out, indent=1, sort_keys=True)
    return 1 if replay.failed(report) else 0


def _replay_text(args) -> int:
    from plonebench.catalog import log, text_replay

    defaults = text_replay.Settings()
    settings = text_replay.Settings(
        legs=tuple(filter(None, args.legs.split(","))),
        window_tids=args.window_tids,
        window_seconds=args.window_seconds,
        writer_interval=args.writer_interval,
        pinned_every=args.pinned_every,
        pinned_sync=args.pinned_sync,
        staged_every=defaults.staged_every,
        rounds=args.rounds,
        limit=args.limit,
        seed=args.seed,
    )
    try:
        report = text_replay.replay_text(args.paths, settings)
    except log.LogError as error:
        print(f"plonebench.catalog replay: {error}", file=sys.stderr)
        return 2
    print(text_replay.format_report(report))
    print(f"replay text: the report is in {report['directory']}/report.json")
    if args.json:
        with open(args.json, "w") as out:
            json.dump(report, out, indent=1, sort_keys=True)
    return 1 if text_replay.failed(report) else 0


if __name__ == "__main__":
    sys.exit(main())
