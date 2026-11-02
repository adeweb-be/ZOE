"""The command line of the catalog fixture.

    python -m plonebench.catalog prepare [--preset catalog-mini] [--name NAME]
        [--seed N] [--force]
    python -m plonebench.catalog fixtures

`just bench catalog ...` runs it in the environment of bench/plone.
"""

from __future__ import annotations

import argparse
import dataclasses
import sys

from plonebench.catalog import fixture, plan


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="plonebench.catalog")
    commands = parser.add_subparsers(dest="command", required=True)
    prepare = commands.add_parser("prepare", help="build a catalog fixture")
    prepare.add_argument("--preset", choices=sorted(plan.PRESETS), default="catalog-1m")
    prepare.add_argument("--seed", type=int, help="the seed, over the preset")
    prepare.add_argument("--name", help="the name of the fixture directory")
    prepare.add_argument("--force", action="store_true")
    commands.add_parser("fixtures", help="list the catalog fixtures")
    args = parser.parse_args(sys.argv[1:] if argv is None else argv)
    if args.command == "prepare":
        spec = plan.PRESETS[args.preset]
        if args.seed is not None:
            spec = dataclasses.replace(spec, seed=args.seed)
        name = args.name or fixture.default_name(spec, args.preset)
        fixture.prepare(spec, fixture.fixture_dir(name), args.preset, force=args.force)
        print(name)
        return 0
    for line in fixture.fixtures():
        print(line)
    return 0


if __name__ == "__main__":
    sys.exit(main())
