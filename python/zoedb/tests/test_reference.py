"""The reference pages of `docs/reference/` hold every setting of the code.

Each test reads one list of names from the code and fails with the names
that the reference lacks:

- each subcommand of `zoe` and each of its options, from the help of the
  `zoe` binary (`ZOE_BINARY`): the binary is what a user runs, and clap
  writes each option on a line of its own;
- each `ZOE_*` twin of an option, from the `[env: ...]` lines of the same
  help, and each twin of a key of `<zoe>`, from `zoe.config`;
- each section and key of `component.xml`;
- each metric of `zoe serve`, from the `# TYPE` lines of `GET /metrics` on
  a running server: the server writes these lines for each metric, also
  before the first sample.

A name counts only in the first cell of a table row of its page, and an
option or a key only in the section of its subcommand or section type. So
the test fails when one row goes.
"""

import os
import re
import subprocess
import xml.etree.ElementTree as ElementTree
from pathlib import Path

from support import TIMEOUT, metrics

import zoe
from zoe.config import KEYS, twin
from zoe.testing import ZoeServer

ROOT = Path(__file__).resolve().parents[3]
REFERENCE = ROOT / "docs" / "reference"
COMPONENT = Path(zoe.__file__).with_name("component.xml")


def page(name):
    """Return the text of the reference page `name`, or "" when it is
    missing, so the test lists every name that it lacks."""
    path = REFERENCE / name
    return path.read_text(encoding="utf-8") if path.is_file() else ""


def sections(text, heading):
    """Return the lines of each section of `text` whose heading matches
    the pattern `heading`, by the name that the pattern captures.

    A section ends at the next heading of its level or above.
    """
    found = {}
    name = level = None
    for line in text.splitlines():
        match = re.match(r"(#+) ", line)
        if match:
            depth = len(match.group(1))
            named = re.match(heading, line)
            if named:
                name, level = named.group(1), depth
                found.setdefault(name, [])
                continue
            if level is not None and depth <= level:
                name = level = None
        if name is not None:
            found[name].append(line)
    return found


def row_names(lines, pattern):
    """Return the names that the first cell of each table row of `lines`
    starts with: the start of its first code span that matches
    `pattern`."""
    names = set()
    for line in lines:
        if not line.startswith("|"):
            continue
        cell = line.split("|")[1].strip()
        match = re.match(r"`(" + pattern + ")", cell)
        if match:
            names.add(match.group(1))
    return names


def zoe_help(*args):
    """Return the help of `zoe` with `args`, without the `ZOE_*` variables
    of the caller."""
    environ = {k: v for k, v in os.environ.items() if not k.startswith("ZOE_")}
    result = subprocess.run(
        [ZoeServer.binary(), *args, "--help"],
        env=environ,
        capture_output=True,
        text=True,
        check=True,
    )
    return result.stdout


def subcommands():
    """Return the subcommands of `zoe`, from the help of the binary."""
    text = zoe_help().split("Commands:\n", 1)[1].split("\n\n", 1)[0]
    names = [line.split()[0] for line in text.splitlines() if line.strip()]
    return [name for name in names if name != "help"]


def options(subcommand):
    """Return the long options of a subcommand, without `--help`."""
    found = re.findall(r"^\s+(?:-\w, )?(--[\w-]+)", zoe_help(subcommand), re.M)
    return [name for name in found if name != "--help"]


def missing(kind, names):
    """Return the message of the names of `kind` that the reference
    lacks."""
    listed = ", ".join(sorted(names))
    return f"docs/reference/ lacks these {kind} ({len(names)}): {listed}"


def test_cli_lists_every_subcommand_and_option():
    rows = sections(page("cli.md"), r"## `zoe (\w+)`")
    absent = []
    for subcommand in subcommands():
        if subcommand not in rows:
            absent.append(f"the section of zoe {subcommand}")
        known = row_names(rows.get(subcommand, []), r"--[\w-]+")
        absent += [
            f"zoe {subcommand} {name}"
            for name in options(subcommand)
            if name not in known
        ]
    assert not absent, missing("options of cli.md", absent)


def test_environment_lists_every_twin():
    twins = set()
    for subcommand in subcommands():
        twins.update(re.findall(r"\[env: (ZOE_[A-Z0-9_]+)=", zoe_help(subcommand)))
    assert twins, "the help of zoe names no twin"
    twins.update(twin(key) for key in KEYS)
    known = row_names(page("environment.md").splitlines(), r"ZOE_[A-Z0-9_]+")
    assert twins <= known, missing("variables of environment.md", twins - known)


def test_zope_conf_lists_every_section_and_key():
    rows = sections(page("zope-conf.md"), r"#+ `<([\w-]+)")
    component = ElementTree.parse(COMPONENT).getroot()
    absent = []
    for sectiontype in component.iter("sectiontype"):
        name = sectiontype.get("name")
        if name not in rows:
            absent.append(f"the section of <{name}>")
        known = row_names(rows.get(name, []), r"[\w-]+")
        for key in [*sectiontype.iter("key"), *sectiontype.iter("multikey")]:
            if key.get("name") not in known:
                absent.append(f"<{name}> {key.get('name')}")
    assert not absent, missing("keys of zope-conf.md", absent)


def exported_metrics(tmp_path):
    """Return the name of each metric that `zoe serve` exports, from
    `GET /metrics` on its admin port."""
    args = ["--admin-listen", "127.0.0.1:0"]
    server = ZoeServer(str(tmp_path / "store"), args=args).start(TIMEOUT)
    try:
        assert server.admin, "zoe serve wrote no admin address"
        text = metrics(server)
    finally:
        server.kill()
    return set(re.findall(r"^# TYPE (\S+) ", text, re.M))


def test_metrics_lists_every_metric(tmp_path):
    exported = exported_metrics(tmp_path)
    assert exported, "zoe serve exports no metric"
    known = row_names(page("metrics.md").splitlines(), r"zoe_\w+")
    assert exported <= known, missing("metrics of metrics.md", exported - known)
