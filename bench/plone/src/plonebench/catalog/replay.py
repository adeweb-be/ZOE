"""The replay of a query log in the mode `compare` on a ZOE store, with
commits during the replay (ADR 0059, decision 6).

    python -m plonebench.catalog replay LOG... STORE [--legs inprocess,serve]

`replay` reads the logs first and refuses a line of another version of the
format. Then, for each leg, it copies the ZOE store, and runs
`replay_zope.py` in a Zope process of the zoeclient scenario on the copy,
with the backend `catalog` of zoe.plone in the mode `compare`:

- `inprocess`: the store is a `zoe.Storage` of the process (`<zoe>`), with
  its mirror in the process;
- `serve`: `zoe serve` serves the copy, and the process is its client
  through `<zoeclient>` with the native client, so the mirror is in the
  server and each query goes over the wire.

Each leg sets `catalog-rebase-rows`, `catalog-window-tids` and
`catalog-window-seconds` low, so the run holds rebases and queries outside
the window. A writer commits saves of the shapes of B5 during the replay,
and one flip of a BooleanIndex, which makes the mirror stale.

The report gives, for each leg, the counts by path, by reason and side of
the Python path and of the mismatches, the indexes and the options of each
query that mismatches, the rebases (in the process only: `zoe serve` does
not tell them), the commits of the writer and the seconds from the flip to
the first native answer after it. It never holds a value of a query. A
production log stays on the runner of its site, and only the report leaves
it.

A LOG is a file of the log, or a directory: its files `catalog-*.jsonl`,
at any depth, such as the `probe/` of the runs of B5 with `--catalog-log`.
STORE is a ZOE store, or a directory with `store/zoe`, such as a fixture of
B5. The leg directories go to `target/bench/catalog/replay/<started>/`.
"""

from __future__ import annotations

import dataclasses
import json
import subprocess
import time
from pathlib import Path

from benchcommon import servers

from plonebench import stack
from plonebench.catalog import log as querylog

WORK = stack.REPO / "target" / "bench" / "catalog" / "replay"
SCRIPT = Path(__file__).resolve().parent / "replay_zope.py"
LEGS = ("inprocess", "serve")


@dataclasses.dataclass(frozen=True)
class Settings:
    """The settings of a replay."""

    legs: tuple[str, ...] = LEGS
    # The keys of the mirror: low, so the run rebases and queries outside
    # the window.
    rebase_rows: int = 50
    window_tids: int = 20
    window_seconds: int = 30
    # The seconds between two commits of the writer.
    writer_interval: float = 0.02
    # Each `pinned_every`th search runs on the pinned Connection, which
    # begins again after `pinned_sync` of its searches.
    pinned_every: int = 10
    pinned_sync: int = 5
    # The times that the replay runs the log, and the most searches.
    rounds: int = 1
    limit: int | None = None
    flip: bool = True
    seed: int = 292
    ready_timeout: int = 600


def log_files(paths) -> list[Path]:
    """The files of the logs `paths`: each file, and the files
    `catalog-*.jsonl` under each directory."""
    out = []
    for path in map(Path, paths):
        if path.is_dir():
            out.extend(sorted(path.rglob("catalog-*.jsonl")))
        else:
            out.append(path)
    return out


def store_of(path: Path) -> Path:
    """The ZOE store of `path`: `path/store/zoe` when it holds one."""
    nested = Path(path) / "store" / "zoe"
    return nested if nested.is_dir() else Path(path)


def zope_conf(home: Path, storage: str) -> str:
    """The zope.conf of a leg: the zoeclient scenario, the backend `catalog`
    in the mode `compare`, and the storage section `storage`."""
    instancehome = (
        stack.PLONE / stack.STACKS["zoe-native"].scenario / "parts" / "instance"
    )
    return f"""%import zoe
%define INSTANCEHOME {instancehome}
instancehome $INSTANCEHOME
%define CLIENTHOME {home / "var"}
clienthome $CLIENTHOME
debug-mode off
security-policy-implementation C
verbose-security off
default-zpublisher-encoding utf-8
<environment>
    CHAMELEON_CACHE {home / "var" / "cache"}
    ZOE_PLONE_BACKENDS catalog
    ZOE_PLONE_CATALOG_MODE compare
</environment>
<zodb_db main>
    cache-size 30000
{storage}
    mount-point /
</zodb_db>
"""


def zoe_section(store: Path, settings: Settings) -> str:
    return f"""    <zoe>
      path {store}
      catalog-rebase-rows {settings.rebase_rows}
      catalog-window-tids {settings.window_tids}
      catalog-window-seconds {settings.window_seconds}
    </zoe>"""


def client_section(address: str, home: Path) -> str:
    return f"""    <zoeclient>
      server {address}
      storage 1
      native true
      blob-dir {home / "blobcache"}
      shared-blob-dir off
    </zoeclient>"""


def serve_args(settings: Settings) -> tuple[str, ...]:
    return (
        "--catalog-rebase-rows",
        str(settings.rebase_rows),
        "--catalog-window-tids",
        str(settings.window_tids),
        "--catalog-window-seconds",
        str(settings.window_seconds),
    )


def _run_leg(leg: str, log: Path, store: Path, home: Path, settings: Settings):
    """Run the replay of one leg in `home`. Return its report."""
    for sub in ("var/cache", "log"):
        (home / sub).mkdir(parents=True, exist_ok=True)
    copy = home / "store"
    stack.copy_store(store, copy)
    leg_settings = dataclasses.asdict(settings) | {"leg": leg}
    (home / "settings.json").write_text(json.dumps(leg_settings, indent=1))
    out = home / "report.json"

    def run(section: str) -> None:
        conf = home / "zope.conf"
        conf.write_text(zope_conf(home, section))
        command = [
            str(stack.interpreter(stack.STACKS["zoe-native"])),
            str(stack.INZOPE / "run.py"),
            str(conf),
            str(SCRIPT),
            str(log),
            str(home / "settings.json"),
            str(out),
        ]
        with (
            open(home / "log" / "replay.out", "w") as stdout,
            open(home / "log" / "replay.err", "w") as stderr,
        ):
            code = subprocess.run(
                command,
                env=stack.clean_environment(),
                cwd=home,
                stdout=stdout,
                stderr=stderr,
                check=False,
            ).returncode
        if code != 0:
            tail = "\n".join(
                (home / "log" / "replay.err").read_text().splitlines()[-30:]
            )
            raise RuntimeError(f"the replay of {leg} failed with {code}:\n{tail}")

    if leg == "inprocess":
        run(zoe_section(copy, settings))
    else:
        with servers.zoe_serve(home, serve_args(settings)) as address:
            run(client_section(address, home))
    return json.loads(out.read_text())


def replay(
    logs, store, settings: Settings | None = None, work: Path | None = None
) -> dict:
    """Replay the logs `logs` on copies of the ZOE store `store`, in each
    leg of `settings`. Return the report: the legs, by name. A line of
    another version of the format raises `LogError` before any copy."""
    settings = settings or Settings()
    files = log_files(logs)
    if not files:
        raise querylog.LogError("no file of a query log")
    lines = querylog.read_all(files)
    if not lines:
        raise querylog.LogError("the logs hold no search")
    source = store_of(Path(store))
    if not (source / "zoe.redb").exists() and not any(source.glob("*.redb")):
        raise FileNotFoundError(f"{source} holds no ZOE store")
    started = time.strftime("%Y%m%dT%H%M%S")
    root = Path(work) if work is not None else WORK / started
    root.mkdir(parents=True, exist_ok=True)
    merged = root / "queries.jsonl"
    with open(merged, "w") as out:
        for line in lines:
            out.write(json.dumps(line, sort_keys=True) + "\n")
    report = {"lines": len(lines), "logs": len(files), "legs": {}}
    for leg in settings.legs:
        if leg not in LEGS:
            raise ValueError(f"no leg {leg!r}: use {', '.join(LEGS)}")
        report["legs"][leg] = _run_leg(leg, merged, source, root / leg, settings)
    (root / "report.json").write_text(json.dumps(report, indent=1, sort_keys=True))
    report["directory"] = str(root)
    return report


def failed(report: dict) -> bool:
    """Tell if a leg of the report found a mismatch."""
    return any(leg["mismatches"] for leg in report["legs"].values())


def format_report(report: dict) -> str:
    """The report as text: no value of a query."""
    out = [f"replay: {report['lines']} searches from {report['logs']} log files"]
    for name, leg in report["legs"].items():
        out.append(
            f"replay {name}: {leg['searches']} searches on {leg['entries']} entries,"
            f" {leg['replayed']} replayed, {leg['skipped']} not replayable,"
            f" {leg['pinned']} on the pinned snapshot, {leg['changed']} in a"
            f" transaction that changed the catalog, in {leg['seconds']} s"
        )
        searches = max(1, sum(leg["queries"].values()))
        for path, n in sorted(leg["queries"].items()):
            out.append(f"replay {name}: path {path}: {n} ({100 * n / searches:.1f} %)")
        for reason, side, n in sorted(leg["fallbacks"], key=lambda row: -row[2]):
            out.append(f"replay {name}: reason {reason} ({side}): {n}")
        for key in ("compared", "cache_differences", "mismatches"):
            out.append(f"replay {name}: {key}: {leg[key]}")
        for shape in leg["mismatched"]:
            out.append(f"replay {name}: mismatch: {shape}")
        for error, n in sorted(leg["errors"].items()):
            out.append(f"replay {name}: error {error}: {n}")
        writer = leg["writer"]
        out.append(
            f"replay {name}: writer: {writer['commits']} commits ({writer['edits']}"
            f" edits, {writer['creates']} creates, {writer['transitions']}"
            f" transitions), {writer['conflicts']} conflicts, {writer['errors']} errors"
        )
        rebases = leg["rebases"]
        out.append(
            f"replay {name}: rebases: "
            + ("not told by zoe serve" if rebases is None else str(rebases))
        )
        flip = leg["flip"]
        out.append(
            f"replay {name}: flip of a BooleanIndex: "
            + (
                f"first native answer after it in {flip['seconds_to_native']} s"
                f" ({flip['index_entries']} entries)"
                if flip["committed"] and flip["seconds_to_native"] is not None
                else "no native answer after it"
                if flip["committed"]
                else "not committed"
            )
        )
    return "\n".join(out)
