"""The replay of the text queries of the Plone suites in the mode `compat`
of `ZoeTextIndex`, against ZCTextIndex on the same documents, with commits
during the replay (ADR 0060, section 11, decision 8).

    python -m plonebench.catalog replay --text DIR... [--legs inprocess,serve]

DIR holds what `just plone zoe catalog text` writes in
`target/plone/zoe/var/catalog-text`: the query log of the probe of the
harness (`catalog-*.jsonl`, at any depth) and the texts that each
`ZoeTextIndex` staged in the suites (`texts-*.jsonl`), one JSON line
`{"i": index, "t": text}` each.

`replay_text` reads the logs first and refuses a line of another version
of the format. It keeps the text searches: for each line, each index of
the query whose class is `ZCTextIndex` or `ZoeTextIndex`, with its value
and the `b_start` and `b_size` of the query. It keeps each distinct text of
`SearchableText`, `Title` and `Description` once. Then a Zope process of
the zoeclient scenario (`text_replay_zope.py build`) makes a ZOE store
that holds the documents of the suites: a Plone site whose catalog holds,
for each of the three indexes, an object for each of its texts in the
ZCTextIndex of the site and in a `ZoeTextIndex` in the mode `compat` of the
same attribute (`zoe_<id>`). For each leg, the replay copies that store
and runs `text_replay_zope.py replay` on the copy, with the backend
`catalog` in the mode `on`:

- `inprocess`: the store is a `zoe.Storage` of the process;
- `serve`: `zoe serve` serves the copy, and the process is its client
  through `<zoeclient>` with the native client, so the mirror and the text
  index are in the server and each query goes over the wire.

Each search runs on the ZCTextIndex with the stock search, then on the
`ZoeTextIndex` through `Catalog.search` with the hook of the backend
(`catalog_query`, or the Python path when the hook refuses), and through
the stock search (`text_query`). Each answer must equal the answer of the
ZCTextIndex: the rids, the order, `len()`, `actual_result_count`, the
scores and the normalized scores, or the same error. The searches run in
three modes: `fresh`, at the last commit; `pinned`, on a Connection that
keeps its snapshot while the writer commits past the mirror window; and
`staged`, in a transaction that changed a document and did not commit, so
the `ZoeTextIndex` sends its staged texts and the hook its transaction
overlay. A writer commits changes of the texts, new documents and removed
documents during the replay.

The report gives, for each leg, the counts by mode and by path, the
comparisons and the mismatches (the mode, the path, the index and the
family of each, and the line of the log), the paths and the reasons of
the hook, the overlays past the bound, and the writer. It never holds a
value of a query or a text. A leg fails on a mismatch, on an error of the
replay, on a change of the reader or a commit of the writer that failed,
and on a search that the hook sent to the Python path with `unsupported`:
on a ZOE store, that means a call failed.
"""

from __future__ import annotations

import dataclasses
import json
import subprocess
import time
from pathlib import Path

from benchcommon import servers

from plonebench import fixture, stack
from plonebench.catalog import log as querylog
from plonebench.catalog import replay

WORK = stack.REPO / "target" / "bench" / "catalog" / "replay-text"
SCRIPT = Path(__file__).resolve().parent / "text_replay_zope.py"
LEGS = replay.LEGS

#: The classes of the text indexes of a line of the log.
TEXT_CLASSES = ("ZCTextIndex", "ZoeTextIndex")
#: The text indexes of the replay.
INDEXES = ("SearchableText", "Title", "Description")
#: The modes of a search.
MODES = ("fresh", "pinned", "staged")
#: The paths of a search on the `ZoeTextIndex`.
PATHS = ("catalog_query", "text_query")


@dataclasses.dataclass(frozen=True)
class Settings:
    """The settings of a replay of the text queries."""

    legs: tuple[str, ...] = LEGS
    # The tids of the mirror window: low, so the pinned searches fall
    # outside it.
    window_tids: int = 20
    window_seconds: int = 30
    # The seconds between two commits of the writer.
    writer_interval: float = 0.02
    # Each `pinned_every`th search runs on the pinned Connection, which
    # begins again after `pinned_sync` of its searches; each
    # `staged_every`th, and each search of a line that ran in a
    # transaction that changed the catalog, runs in a transaction that
    # changed a document.
    pinned_every: int = 7
    pinned_sync: int = 5
    staged_every: int = 5
    rounds: int = 1
    limit: int | None = None
    seed: int = 297
    ready_timeout: int = 600


def files(paths, pattern: str) -> list[Path]:
    """The files `pattern` under each directory of `paths`, and each file
    of `paths` whose name matches it."""
    out = []
    for path in map(Path, paths):
        if path.is_dir():
            out.extend(sorted(path.rglob(pattern)))
        elif path.match(pattern):
            out.append(path)
    return out


def searches(lines: list[dict]) -> list[dict]:
    """The text searches of the lines of a log: for each line and each of
    its text indexes, the index, its value in the typed form, `b_start`,
    `b_size`, whether the line ran in a transaction that changed the
    catalog, and the number of the line. A line that the format does not
    carry gives none."""
    out = []
    for number, line in enumerate(lines, 1):
        if not line.get("replayable", True):
            continue
        query = line.get("query")
        if not isinstance(query, dict) or query.get("$") != "dict":
            continue
        items = {}
        for key, value in query["v"]:
            if isinstance(key, str):
                items[key] = value
        for name, cls in sorted((line.get("indexes") or {}).items()):
            if cls not in TEXT_CLASSES or name not in INDEXES or name not in items:
                continue
            batch = {}
            for option in ("b_start", "b_size"):
                if isinstance(items.get(option), int):
                    batch[option] = items[option]
            out.append(
                {
                    "index": name,
                    "value": items[name],
                    **batch,
                    "changed": bool(line.get("changed")),
                    "line": number,
                }
            )
    return out


def texts(paths) -> dict[str, list[str]]:
    """The distinct texts of each index of `INDEXES` in the files
    `texts-*.jsonl` of `paths`, in the order of their first line."""
    seen = {name: {} for name in INDEXES}
    for path in files(paths, "texts-*.jsonl"):
        with open(path) as f:
            for number, text in enumerate(f, 1):
                if not text.strip():
                    continue
                try:
                    item = json.loads(text)
                except ValueError:
                    raise querylog.LogError(
                        f"line {number} of {path.name} is not JSON"
                    ) from None
                name, value = item.get("i"), item.get("t")
                if name in seen and isinstance(value, str):
                    seen[name].setdefault(value, None)
    return {name: list(found) for name, found in seen.items()}


def zope_conf(home: Path, storage: str) -> str:
    """The zope.conf of the zoeclient scenario with the backend `catalog` in
    the mode `on`, and the storage section `storage`."""
    return replay.zope_conf(home, storage).replace(
        "ZOE_PLONE_CATALOG_MODE compare", "ZOE_PLONE_CATALOG_MODE on"
    )


def zoe_section(store: Path, settings: Settings) -> str:
    return f"""    <zoe>
      path {store}
      catalog-window-tids {settings.window_tids}
      catalog-window-seconds {settings.window_seconds}
    </zoe>"""


def serve_args(settings: Settings) -> tuple[str, ...]:
    return (
        "--catalog-window-tids",
        str(settings.window_tids),
        "--catalog-window-seconds",
        str(settings.window_seconds),
    )


def _run_zope(home: Path, storage: str, args: list[str], what: str) -> None:
    """Run `text_replay_zope.py` with `args` in a Zope process of the
    zoeclient scenario on the storage section `storage`."""
    for sub in ("var/cache", "log"):
        (home / sub).mkdir(parents=True, exist_ok=True)
    conf = home / "zope.conf"
    conf.write_text(zope_conf(home, storage))
    command = [
        str(stack.interpreter(stack.STACKS["zoe-native"])),
        str(stack.INZOPE / "run.py"),
        str(conf),
        str(SCRIPT),
        *args,
    ]
    with (
        open(home / "log" / f"{what}.out", "w") as stdout,
        open(home / "log" / f"{what}.err", "w") as stderr,
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
        tail = "\n".join((home / "log" / f"{what}.err").read_text().splitlines()[-30:])
        raise RuntimeError(f"the {what} failed with {code}:\n{tail}")


def _run_leg(leg: str, corpus: Path, store: Path, home: Path, settings: Settings):
    """Run the replay of one leg in `home` on a copy of `store`. Return its
    report."""
    copy = home / "store"
    stack.copy_store(store, copy)
    fixture.migrate(copy)
    leg_settings = dataclasses.asdict(settings) | {"leg": leg}
    (home / "settings.json").write_text(json.dumps(leg_settings, indent=1))
    out = home / "report.json"
    args = ["replay", str(corpus), str(home / "settings.json"), str(out)]
    if leg == "inprocess":
        _run_zope(home, zoe_section(copy, settings), args, "replay")
    else:
        with servers.zoe_serve(home, serve_args(settings)) as address:
            _run_zope(home, replay.client_section(address, home), args, "replay")
    return json.loads(out.read_text())


def replay_text(paths, settings: Settings | None = None, work: Path | None = None):
    """Replay the text queries of the logs under `paths` on the texts under
    `paths`, in each leg of `settings`. Return the report: the legs, by
    name. A line of another version of the format raises `LogError` before
    any build."""
    settings = settings or Settings()
    logs = files(paths, "catalog-*.jsonl")
    if not logs:
        raise querylog.LogError("no file of a query log")
    found = searches(querylog.read_all(logs))
    if not found:
        raise querylog.LogError("the logs hold no text search")
    corpus_texts = texts(paths)
    if not any(corpus_texts.values()):
        raise querylog.LogError("no file of texts")
    for leg in settings.legs:
        if leg not in LEGS:
            raise ValueError(f"no leg {leg!r}: use {', '.join(LEGS)}")
    started = time.strftime("%Y%m%dT%H%M%S")
    root = Path(work) if work is not None else WORK / started
    root.mkdir(parents=True, exist_ok=True)
    corpus = root / "corpus.json"
    corpus.write_text(json.dumps({"texts": corpus_texts, "searches": found}))
    build = root / "build"
    store = build / "store"
    _run_zope(
        build,
        zoe_section(store, settings),
        ["build", str(corpus), str(build / "built.json")],
        "build",
    )
    built = json.loads((build / "built.json").read_text())
    report = {
        "logs": len(logs),
        "searches": len(found),
        "documents": built["documents"],
        "legs": {},
    }
    for leg in settings.legs:
        report["legs"][leg] = _run_leg(leg, corpus, store, root / leg, settings)
    (root / "report.json").write_text(json.dumps(report, indent=1, sort_keys=True))
    report["directory"] = str(root)
    return report


def problems(leg: dict) -> list[str]:
    """Why a leg fails."""
    out = []
    if leg["mismatches"]:
        out.append(f"{leg['mismatches']} mismatches")
    if leg["errors"]:
        out.append(f"{sum(leg['errors'].values())} errors of the replay")
    if leg["change_errors"]:
        out.append(f"{leg['change_errors']} changes of the reader that failed")
    if leg["writer"]["errors"]:
        out.append(f"{leg['writer']['errors']} commits of the writer that failed")
    unsupported = sum(
        n
        for reason, side, n in leg["fallbacks"]
        if reason == "unsupported" and side == "client"
    )
    if unsupported:
        out.append(f"{unsupported} searches that the hook sent to `unsupported`")
    if not leg["compared"]:
        out.append("no search was compared")
    return out


def failed(report: dict) -> bool:
    """Tell if a leg of the report fails."""
    return any(problems(leg) for leg in report["legs"].values())


def format_report(report: dict) -> str:
    """The report as text: no value of a query and no text."""
    out = [
        f"replay text: {report['searches']} text searches from {report['logs']}"
        f" log files, {report['documents']} documents"
    ]
    for name, leg in report["legs"].items():
        out.append(
            f"replay text {name}: {leg['searches']} searches, compared"
            f" {leg['compared']}, mismatches {leg['mismatches']}, in"
            f" {leg['seconds']} s"
        )
        for mode in MODES:
            for path in PATHS:
                n = leg["by_mode_path"].get(f"{mode} {path}", 0)
                out.append(f"replay text {name}: mode {mode}, path {path}: {n}")
        for path, n in sorted(leg["queries"].items()):
            out.append(f"replay text {name}: hook path {path}: {n}")
        for reason, side, n in sorted(leg["fallbacks"], key=lambda row: -row[2]):
            out.append(f"replay text {name}: reason {reason} ({side}): {n}")
        out.append(
            f"replay text {name}: overlay past the bound: {leg['overlay_bound']}"
        )
        for found in leg["mismatched"][:100]:
            out.append(
                f"replay text {name}: mismatch: mode {found['mode']}, path"
                f" {found['path']}, index {found['index']}, line {found['line']}"
            )
        for error, n in sorted(leg["errors"].items()):
            out.append(f"replay text {name}: error of the replay {error}: {n}")
        writer = leg["writer"]
        out.append(
            f"replay text {name}: writer: {writer['commits']} commits"
            f" ({writer['edits']} edits, {writer['creates']} creates,"
            f" {writer['removes']} removes), {writer['conflicts']} conflicts,"
            f" {writer['errors']} errors; changes of the reader that failed:"
            f" {leg['change_errors']}"
        )
        for problem in problems(leg):
            out.append(f"replay text {name}: FAILS: {problem}")
    return "\n".join(out)
