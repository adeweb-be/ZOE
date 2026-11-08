"""The replay of a query log inside Zope (ADR 0059, decision 6).

    interpreter inzope/run.py ZOPE_CONF replay_zope.py LOG SETTINGS OUT

`plonebench.catalog.replay` runs it in the interpreter of the zoeclient
scenario, on a copy of a ZOE store: in-process on `zoe.Storage`, or through
`zoe serve` with the native client. The zope.conf installs the backend
`catalog` of zoe.plone in the mode `compare`, so each search runs the stock
search and asks the mirror of the store, compares the answers and counts.

The reader replays each line of the log with `Catalog.search`, as the
probe recorded it, on one of two Connections: the fresh one begins a new
transaction before each search, so it sees the last commit; the pinned one
keeps its snapshot for `pinned_sync` of its searches, so its later searches
fall outside the mirror window. A line that ran in a transaction that
changed the catalog runs on the fresh Connection after a change of one
Document, without a commit, so the search sends a transaction overlay.

The writer commits saves of the shapes of B5 to the same store during the
replay, from a Connection of its own: an edit of a Document (its title and
its subjects), a new Document in a folder, and a workflow transition. At
the middle of the replay, it commits one flip of a BooleanIndex
(`_invert_index`): the pickle of the index object changes, the mirror goes
stale and builds again. The reader measures the seconds from that commit
to the first native answer at a snapshot after it.

OUT receives a JSON report: the counts by path, by reason and side of the
Python path, of the comparisons, of the mismatches with the indexes and
the options of each, of the differences of the cache of the request, of
the writer, the rebases, and the flip. It holds no value of a query.
"""

import json
import random
import sys
import threading
import time
import traceback

import transaction
from Acquisition import aq_base
from ZODB.POSException import ConflictError
from ZODB.utils import p64, u64
from zoe.plone import catalog as backend

from plonebench.catalog import log as querylog

log_path, settings_path, out_path = sys.argv[1:]
settings = json.loads(open(settings_path).read())
lines = querylog.read(log_path)
# inzope/run.py gives the application as `app`.
db = app._p_jar.db()  # noqa: F821
storage = db.storage
rnd = random.Random(settings["seed"])


def system_user():
    from AccessControl.SecurityManagement import newSecurityManager
    from AccessControl.SpecialUsers import system

    newSecurityManager(None, system)


def site_path():
    """The path of the catalog tool of the log: the path of the first line
    that has one."""
    for line in lines:
        if line.get("catalog"):
            return "/".join(line["catalog"])
    raise SystemExit("replay: the log names no catalog")


TOOL = site_path()


def tool_of(connection):
    return connection.root()["Application"].unrestrictedTraverse(TOOL)


def start_of(connection):
    jar_storage = getattr(connection, "_normal_storage", None) or connection._storage
    return jar_storage._start


def wait_ready(oid, timeout):
    """Wait until the mirror of the store answers a query on the catalog
    `oid`. Return the seconds of the wait."""
    plan = {"c": [], "s": [], "l": None, "bs": 0, "bz": None, "cols": None, "rn": 0}
    started = time.monotonic()
    while True:
        start = p64(u64(storage.lastTransaction()) + 1)
        reply = storage.catalog_query(oid, start, plan, None)
        if "f" not in reply:
            return time.monotonic() - started
        if time.monotonic() - started > timeout:
            raise SystemExit(f"replay: the mirror is not ready: {reply['f']}")
        time.sleep(0.2)


def status(oid):
    """The state of the mirror in the process, or None through zoe serve."""
    read = getattr(storage, "_catalog_status", None)
    return read(oid) if read is not None else None


class Writer(threading.Thread):
    """Commit saves of the shapes of B5 until `stop` is set."""

    def __init__(self, interval):
        super().__init__(name="writer", daemon=True)
        self.interval = interval
        self.stop = threading.Event()
        self.flip_due = threading.Event()
        self.counts = {"commits": 0, "conflicts": 0, "edits": 0, "creates": 0}
        self.counts.update(transitions=0, errors=0)
        self.flip = {"committed": False}
        self.lock = threading.Lock()
        self.tm = transaction.TransactionManager()
        self.connection = db.open(transaction_manager=self.tm)
        self.documents, self.folders = self.targets()
        self.tm.abort()

    def commits(self):
        with self.lock:
            return self.counts["commits"]

    def targets(self):
        catalog = tool_of(self.connection)
        documents = [
            b.getPath()
            for b in catalog.unrestrictedSearchResults(portal_type="Document")
        ]
        folders = [
            b.getPath() for b in catalog.unrestrictedSearchResults(portal_type="Folder")
        ]
        rnd.shuffle(documents)
        rnd.shuffle(folders)
        return documents[:500], folders[:50]

    def run(self):
        system_user()
        documents, folders = self.documents, self.folders
        n = 0
        while not self.stop.is_set():
            n += 1
            try:
                if self.flip_due.is_set() and not self.flip["committed"]:
                    self.do_flip()
                else:
                    self.save(n, documents, folders)
                self.tm.commit()
                with self.lock:
                    self.counts["commits"] += 1
                if self.flip.get("pending"):
                    self.flip.update(
                        committed=True,
                        pending=False,
                        tid=u64(db.lastTransaction()),
                        at=time.monotonic(),
                    )
            except ConflictError:
                self.tm.abort()
                with self.lock:
                    self.counts["conflicts"] += 1
            except Exception:
                self.tm.abort()
                with self.lock:
                    self.counts["errors"] += 1
                if self.counts["errors"] < 5:
                    traceback.print_exc()
            self.stop.wait(self.interval)
        self.tm.abort()
        self.connection.close()

    def save(self, n, documents, folders):
        root = self.connection.root()["Application"]
        kind = rnd.random()
        if kind < 0.6 and documents:
            obj = root.unrestrictedTraverse(rnd.choice(documents))
            obj.setTitle(f"Replay edit {n}")
            obj.setSubject(tuple(rnd.sample(("alpha", "beta", "gamma", "delta"), 2)))
            obj.reindexObject()
            key = "edits"
        elif kind < 0.85 and folders:
            folder = root.unrestrictedTraverse(rnd.choice(folders))
            name = f"replay-{n}"
            folder.invokeFactory("Document", name, title=f"Replay new {n}")
            documents.append("/".join(folder[name].getPhysicalPath()))
            key = "creates"
        elif documents:
            obj = root.unrestrictedTraverse(rnd.choice(documents))
            workflow = tool_of(self.connection).portal_workflow
            actions = [
                a["id"]
                for a in workflow.listActionInfos(object=obj)
                if a["id"] in ("publish", "retract", "submit", "reject")
            ]
            if not actions:
                return
            workflow.doActionFor(obj, rnd.choice(sorted(actions)))
            key = "transitions"
        else:
            return
        with self.lock:
            self.counts[key] += 1

    def do_flip(self):
        """Invert the first BooleanIndex of the catalog, as ZCatalog does
        when the indexed value holds 60 % of the entries."""
        catalog = tool_of(self.connection)._catalog
        for name in sorted(catalog.indexes):
            index = aq_base(catalog.getIndex(name))
            if type(index).__name__ == "BooleanIndex":
                index._invert_index()
                self.flip.update(index_entries=len(catalog), pending=True)
                return
        self.flip.update(committed=True, missing=True)


def run_search(tool, line):
    """Replay one line on the catalog tool `tool`."""
    catalog = tool._catalog
    query = querylog.decode(line["query"])
    sort = line.get("sort")
    if sort is None:
        sort_index = None
    elif isinstance(sort, str):
        sort_index = catalog.getIndex(sort)
    else:
        sort_index = [catalog.getIndex(name) for name in sort]
    reverse = querylog.decode(line.get("reverse", False))
    result = catalog.search(
        query, sort_index, reverse, line.get("limit"), line.get("merge", True)
    )
    return len(result)


def main():
    system_user()
    first = db.open(transaction_manager=transaction.TransactionManager())
    oid = aq_base(tool_of(first)._catalog)._p_oid
    entries = len(tool_of(first)._catalog)
    first.close()
    ready = wait_ready(oid, settings.get("ready_timeout", 600))
    before = status(oid)
    writer = Writer(settings["writer_interval"])
    fresh_tm = transaction.TransactionManager()
    fresh = db.open(transaction_manager=fresh_tm)
    pinned_tm = transaction.TransactionManager()
    pinned = db.open(transaction_manager=pinned_tm)
    documents = writer.documents[:50]
    # The counts hold the searches of the log only.
    backend._counts.clear()
    backend._mismatched.clear()
    writer.start()
    pinned_at = writer.commits()
    pinned_uses = 0
    rounds = settings.get("rounds", 1)
    plan = [line for _ in range(rounds) for line in lines]
    if settings.get("limit"):
        plan = plan[: settings["limit"]]
    flip_at = len(plan) // 2 if settings.get("flip", True) else None
    counts = {"replayed": 0, "skipped": 0, "pinned": 0, "changed": 0}
    errors = {}
    flip = {}
    started = time.monotonic()
    for i, line in enumerate(plan):
        if i == flip_at:
            writer.flip_due.set()
        if not line.get("replayable", True):
            counts["skipped"] += 1
            continue
        every = settings["pinned_every"]
        on_pinned = every and i % every == every - 1 and not line.get("changed")
        if on_pinned:
            if pinned_uses >= settings["pinned_sync"]:
                pinned_tm.begin()
                pinned_at = writer.commits()
                pinned_uses = 0
            # The writer commits past the window of the pinned snapshot.
            deadline = time.monotonic() + 5
            while (
                writer.commits() - pinned_at <= settings["window_tids"]
                and time.monotonic() < deadline
            ):
                time.sleep(0.01)
            pinned_uses += 1
            connection = pinned
            counts["pinned"] += 1
        else:
            fresh_tm.begin()
            connection = fresh
        snapshot = u64(start_of(connection))
        native = backend._counts_now()["queries"].get("native", 0)
        try:
            tool = tool_of(connection)
            if line.get("changed") and documents:
                obj = connection.root()["Application"].unrestrictedTraverse(
                    rnd.choice(documents)
                )
                obj.setTitle("Replay change")
                obj.reindexObject()
                counts["changed"] += 1
            run_search(tool, line)
            counts["replayed"] += 1
        except Exception as error:
            name = f"{type(error).__module__}.{type(error).__qualname__}"
            errors[name] = errors.get(name, 0) + 1
        finally:
            if connection is fresh:
                fresh_tm.abort()
        done = writer.flip
        if done.get("committed") and "tid" in done and "seconds" not in flip:
            went_native = backend._counts_now()["queries"].get("native", 0) > native
            if connection is fresh and went_native and snapshot > done["tid"]:
                flip["seconds"] = round(time.monotonic() - done["at"], 3)
    seconds = time.monotonic() - started
    writer.stop.set()
    writer.join(30)
    pinned_tm.abort()
    pinned.close()
    fresh.close()
    after = status(oid)
    found = backend._counts_now()
    report = {
        "leg": settings["leg"],
        "entries": entries,
        "searches": len(plan),
        "ready_seconds": round(ready, 3),
        "seconds": round(seconds, 3),
        **counts,
        "errors": errors,
        "queries": found["queries"],
        "fallbacks": [
            [reason, side, n]
            for (reason, side), n in sorted(found["fallbacks"].items())
        ],
        "compared": found["compared"],
        "mismatches": found["mismatches"],
        "cache_differences": found["cache_differences"],
        "mismatched": list(backend._mismatched),
        "writer": writer.counts,
        "rebases": (
            after["rebases"] - (before["rebases"] if before else 0)
            if after is not None
            else None
        ),
        "flip": {
            "committed": bool(writer.flip.get("committed")),
            "index_entries": writer.flip.get("index_entries"),
            "seconds_to_native": flip.get("seconds"),
        },
    }
    with open(out_path, "w") as out:
        json.dump(report, out, indent=1, sort_keys=True)


main()
