"""The loads of Plone requests on the stack zoe-native, in one Zope thread.

    python -m plonebench.loadpath [--fixture NAME] [--requests 200] [--py-spy]

It copies the store of the fixture, starts `zoe serve` on it as a B5 run
does, writes the `zope.conf` of an instance of the stack with the
settings of B5, and runs `inzope/loadpath.py` in the interpreter of the
zoeclient scenario: one Zope thread that sends the requests through the
publisher of Zope in its own process. The paths are the published Folders
(their listing) and Documents (their view) of the first section of the
fixture. With `--py-spy`, py-spy samples that process (`record --native`,
raw format), and `python -m shootout.loadpath frames FILE --root
_request_folder` (or `_request_document`) prints the time per frame of the
requests of one kind. It writes `loadpath.json` and, with `--py-spy`,
`py-spy.txt` into `target/bench/plone/loadpath/`.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import shutil
import subprocess
import sys

from plonebench import corpus, fixture, stack

# The ports of the defaults plus this offset, apart from those of a B5 run.
PORT_OFFSET = 700
# The paths of each kind.
PATHS = 10


def targets(manifest: dict) -> list[list[str]]:
    """The published Folders and Documents of the first section, in turn."""
    site = manifest["site"]
    first = manifest["items"][0]["section"]
    folders, documents = [], []
    for item in manifest["items"]:
        if item["state"] != corpus.PUBLISHED or item["section"] != first:
            continue
        path = f"/{site}/{item['path']}"
        if item["portal_type"] == "Folder" and len(folders) < PATHS:
            folders.append(["folder", path])
        elif item["portal_type"] == "Document" and len(documents) < PATHS:
            documents.append(["document", path])
    out = []
    for pair in zip(folders, documents, strict=False):
        out.extend(pair)
    return out


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="plonebench.loadpath", description=__doc__)
    parser.add_argument("--fixture", default=None)
    parser.add_argument("--requests", type=int, default=200)
    parser.add_argument("--py-spy", action="store_true")
    parser.add_argument("--rate", type=int, default=499)
    parser.add_argument(
        "--reader",
        action="store_true",
        help="give each connection the reader in Rust of zoe.reader",
    )
    parser.add_argument(
        "--oracle",
        action="store_true",
        help="run the oracle of the reader on the FileStorage of the fixture",
    )
    args = parser.parse_args(argv)
    name = args.fixture or fixture.default_name(corpus.PRESETS["small"], "small")
    source = fixture.fixture_dir(name)
    config = stack.STACKS["zoe-native"]
    defaults = stack.Settings()
    settings = dataclasses.replace(
        defaults,
        instances=1,
        threads=1,
        probe=False,
        base_port=defaults.base_port + PORT_OFFSET,
        server_port=defaults.server_port + PORT_OFFSET,
        admin_port=defaults.admin_port + PORT_OFFSET,
    )
    run = stack.WORK / "loadpath"
    if run.exists():
        shutil.rmtree(run)
    store = fixture.restore(source, config, run)
    paths = run / "paths.json"
    paths.write_text(
        json.dumps(targets(json.loads((source / "manifest.json").read_text())))
    )
    st = stack.Stack(config, settings, run, store)
    st.start_server()
    try:
        home = run / "instance"
        conf = st.prepare_home(home, "loadpath")
        command = [
            str(stack.interpreter(config)),
            str(stack.INZOPE / "run.py"),
            str(conf),
            "loadpath.py",
            str(run / "loadpath.json"),
            f"127.0.0.1:{settings.admin_port}",
            str(paths),
            str(args.requests),
            "reader" if args.reader else "python",
        ]
        if args.oracle:
            command = [
                str(stack.interpreter(config)),
                str(stack.INZOPE / "run.py"),
                str(conf),
                "reader_oracle.py",
                str(source / "store" / "filestorage" / "Data.fs"),
                str(run / "oracle.json"),
            ]
        if args.py_spy:
            command = [
                "py-spy",
                "record",
                "--native",
                "--format",
                "raw",
                "--rate",
                str(args.rate),
                "--output",
                str(run / "py-spy.txt"),
                "--",
                *command,
            ]
        done = subprocess.run(command, env=stack.clean_environment(), cwd=home)
    finally:
        st.stop()
    print(f"plonebench.loadpath: {run / 'loadpath.json'}", file=sys.stderr)
    return done.returncode


if __name__ == "__main__":
    sys.exit(main())
