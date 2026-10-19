"""The command line of the Plone end-to-end benchmark.

    python -m plonebench prepare [--preset small] [--name NAME] [--force]
    python -m plonebench run --stack zeo --scenario browse --steps 1:60,4:60
    python -m plonebench campaign --stacks zeo,zoe-native --scenarios edit --reps 3
    python -m plonebench report RUN_DIR... [--out FILE.md] [--json FILE.json]
    python -m plonebench serve --stack zoe-native

`just bench plone ...` runs it in the environment of bench/plone.
"""

from __future__ import annotations

import argparse
import json
import signal
import sys
import time
from pathlib import Path

from plonebench import corpus, fixture, runner, stack, workload


def parse_steps(text: str) -> list[tuple[int, float]]:
    return workload.steps_from_env(text)


def settings_of(args) -> stack.Settings:
    defaults = stack.Settings()
    return stack.Settings(
        instances=args.instances,
        threads=args.threads,
        zodb_cache_objects=args.zodb_cache,
        client_cache=args.client_cache,
        server_cpus=args.server_cpus,
        zope_cpus=tuple(args.zope_cpus.split(";")),
        locust_cpus=args.locust_cpus,
        probe=not args.no_probe,
        zeo_fsync=args.zeo_fsync,
        zoe_cache_size=args.zoe_cache_size,
        zoe_compression=args.zoe_compression,
        # Only `run` and `campaign` take the switch.
        split_merges=getattr(args, "split_merges", None),
        folder_orders=getattr(args, "folder_orders", None),
        base_port=defaults.base_port + args.port_offset,
        server_port=defaults.server_port + args.port_offset,
        admin_port=defaults.admin_port + args.port_offset,
    )


def add_settings(parser: argparse.ArgumentParser) -> None:
    defaults = stack.Settings()
    parser.add_argument("--instances", type=int, default=defaults.instances)
    parser.add_argument("--threads", type=int, default=defaults.threads)
    parser.add_argument("--zodb-cache", type=int, default=defaults.zodb_cache_objects)
    parser.add_argument("--client-cache", default=defaults.client_cache)
    parser.add_argument("--server-cpus", default=defaults.server_cpus)
    parser.add_argument(
        "--zope-cpus",
        default=";".join(defaults.zope_cpus),
        help="the CPU list of each instance, separated by ';' (taskset lists)",
    )
    parser.add_argument("--locust-cpus", default=defaults.locust_cpus)
    parser.add_argument(
        "--no-probe", action="store_true", help="run without the WSGI probe"
    )
    parser.add_argument(
        "--port-offset",
        type=int,
        default=0,
        help="add this number to every port of the run, so that two runs share "
        "a machine",
    )
    parser.add_argument(
        "--zeo-fsync",
        choices=("full", "plain"),
        default=defaults.zeo_fsync,
        help="macOS: the ZEO server syncs with stock fsync(2) (plain, as zoe "
        "serve does) or with F_FULLFSYNC (full, a stronger sync than zoe serve)",
    )
    parser.add_argument(
        "--zoe-cache-size",
        default=defaults.zoe_cache_size,
        help="the --cache-size of zoe serve, such as 128MB; by default the "
        "server keeps the default of the store",
    )
    parser.add_argument(
        "--zoe-compression",
        default=defaults.zoe_compression,
        help="the --compression of zoe serve: none, zstd or zstd:LEVEL; by "
        "default the server keeps the default of the store (raw records)",
    )


def default_fixture() -> str:
    return fixture.default_name(corpus.PRESETS["small"], "small")


def add_run(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--fixture", default=None, help="the name of a prepared fixture"
    )
    parser.add_argument("--steps", default="1:60,4:60,16:60", help="users:seconds,...")
    parser.add_argument(
        "--warmup", type=float, default=60.0, help="seconds, 0 for a cold start"
    )
    parser.add_argument("--warmup-users", type=int, default=8)
    parser.add_argument(
        "--think", default="0,0", help="min,max seconds between operations"
    )
    parser.add_argument("--client-retries", type=int, default=0)
    parser.add_argument(
        "--trim", type=float, default=5.0, help="seconds left out of each step"
    )
    parser.add_argument("--keep-store", action="store_true")
    parser.add_argument("--no-verify", action="store_true")
    parser.add_argument("--label", default="")
    add_settings(parser)
    parser.add_argument(
        "--split-merges",
        choices=stack.SPLIT_MERGES,
        default=stack.Settings().split_merges,
        help="write the switch split_merges of the merge policy (ADR 0048) "
        "into the store before the instances start; a stack of zoe serve "
        "with zoedb only, and `campaign` gives it only to such stacks. By "
        "default the store keeps the policy of the fixture",
    )
    parser.add_argument(
        "--folder-orders",
        choices=stack.FOLDER_ORDERS,
        default=stack.Settings().folder_orders,
        help="write the switch folder_orders of the merge policy (ADR 0052), "
        "with the tier 2 entry of the class of the folders, into the store "
        "before the instances start; a stack of zoe serve with zoedb only, "
        "and `campaign` gives it only to such stacks",
    )


def options_of(
    args, stack_name: str, scenario: str, repetition: int, campaign: bool = False
) -> runner.RunOptions:
    settings = settings_of(args)
    if campaign:
        # A campaign gives the switch only to the stacks that take it. The
        # result of each run records its settings.
        settings = stack.campaign_settings(stack.STACKS[stack_name], settings)
    return runner.RunOptions(
        stack=stack_name,
        scenario=scenario,
        fixture=args.fixture or default_fixture(),
        steps=parse_steps(args.steps),
        warmup=args.warmup,
        warmup_users=args.warmup_users,
        repetition=repetition,
        think=args.think,
        client_retries=args.client_retries,
        trim=args.trim,
        keep_store=args.keep_store,
        verify=not args.no_verify,
        label=args.label,
        settings=settings,
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="plonebench")
    commands = parser.add_subparsers(dest="command", required=True)

    prepare = commands.add_parser("prepare", help="build a fixture")
    prepare.add_argument("--preset", choices=sorted(corpus.PRESETS), default="small")
    prepare.add_argument(
        "--spec", help="a JSON file of SiteSpec fields over the preset"
    )
    prepare.add_argument("--name")
    prepare.add_argument("--force", action="store_true")
    add_settings(prepare)

    one = commands.add_parser("run", help="one run")
    one.add_argument("--stack", choices=sorted(stack.STACKS), required=True)
    one.add_argument("--scenario", choices=sorted(workload.SCENARIOS), required=True)
    one.add_argument("--rep", type=int, default=0)
    add_run(one)

    campaign = commands.add_parser("campaign", help="interleaved runs")
    campaign.add_argument("--stacks", default="zeo,zoe-native")
    campaign.add_argument("--scenarios", default="browse")
    campaign.add_argument("--reps", type=int, default=3)
    campaign.add_argument("--first-rep", type=int, default=0)
    campaign.add_argument(
        "--list", default=None, help="append the run directories here"
    )
    add_run(campaign)

    report = commands.add_parser("report", help="compare runs")
    report.add_argument("runs", nargs="+", type=Path)
    report.add_argument("--out", type=Path)
    report.add_argument(
        "--json",
        type=Path,
        help="write the summary of the campaign, valid against "
        "bench/schema/plone.schema.json; the runs must share one fixture, "
        "commit, machine and settings",
    )
    report.add_argument(
        "--timeline",
        type=float,
        default=None,
        help="print each run in buckets of this many seconds instead",
    )

    serve = commands.add_parser(
        "serve", help="start a stack on a copy of a fixture and wait"
    )
    serve.add_argument("--stack", choices=sorted(stack.STACKS), required=True)
    serve.add_argument("--fixture", default=None)
    add_settings(serve)

    commands.add_parser("fixtures", help="list the fixtures")

    args = parser.parse_args(argv)
    if args.command == "prepare":
        spec = corpus.PRESETS[args.preset]
        if args.spec:
            data = spec.to_json()
            data.update(json.loads(Path(args.spec).read_text()))
            spec = corpus.SiteSpec.from_json(data)
        name = args.name or fixture.default_name(
            spec, args.preset if not args.spec else None
        )
        fixture.prepare(spec, name, settings_of(args), force=args.force)
        print(name)
        return 0
    if args.command == "run":
        runner.run(options_of(args, args.stack, args.scenario, args.rep))
        return 0
    if args.command == "campaign":
        stacks = args.stacks.split(",")
        failures = 0
        for rep in range(args.first_rep, args.first_rep + args.reps):
            for scenario in args.scenarios.split(","):
                # Alternate the order of the stacks from one repetition to
                # the next, so a drift of the machine does not favour one.
                order = stacks if rep % 2 == 0 else list(reversed(stacks))
                for name in order:
                    try:
                        options = options_of(args, name, scenario, rep, campaign=True)
                        run_dir = runner.run(options)
                        if args.list:
                            with open(args.list, "a") as file:
                                file.write(f"{run_dir}\n")
                    except Exception as error:  # keep going: the result keeps the error
                        failures += 1
                        print(
                            f"plonebench: {scenario} {name} rep {rep} failed: {error}"
                        )
        return 1 if failures else 0
    if args.command == "report":
        from plonebench import report as report_module

        if args.timeline:
            print(report_module.timeline(args.runs, args.timeline))
            return 0
        if args.json:
            # Make the summary first, so a refusal writes no file.
            from plonebench import summary

            try:
                document = summary.document(report_module.load(args.runs))
            except summary.NotOneCampaign as error:
                print(f"plonebench report: {error}", file=sys.stderr)
                return 2
            except summary.jsonschema.ValidationError as error:
                print(
                    "plonebench report: the summary is not valid against "
                    f"{summary.SCHEMA.name}: {error.message}",
                    file=sys.stderr,
                )
                return 1
        text, _ = report_module.build(args.runs)
        if args.out:
            args.out.write_text(text)
        else:
            print(text)
        if args.json:
            args.json.write_text(json.dumps(document, indent=2) + "\n")
        return 0
    if args.command == "serve":
        name = args.fixture or default_fixture()
        run = stack.WORK / "serve" / args.stack
        store = fixture.restore(
            fixture.fixture_dir(name), stack.STACKS[args.stack], run
        )
        st = stack.Stack(stack.STACKS[args.stack], settings_of(args), run, store)
        signal.signal(
            signal.SIGTERM, lambda *_: (_ for _ in ()).throw(KeyboardInterrupt())
        )
        st.start_server()
        st.start_instances()
        print("plonebench: serving", " ".join(st.urls()), flush=True)
        try:
            while True:
                time.sleep(1)
        except KeyboardInterrupt:
            print(st.stop())
        return 0
    if args.command == "fixtures":
        for path in sorted((stack.WORK / "fixtures").glob("*/equivalence.json")):
            data = json.loads(path.read_text())
            print(
                f"{path.parent.name}: {data['objects']} objects, "
                f"equivalent {data['equivalent']}"
            )
        return 0
    return 2


if __name__ == "__main__":
    sys.exit(main())
