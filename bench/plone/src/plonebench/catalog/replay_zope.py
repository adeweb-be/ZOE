"""The replay of a query log inside Zope (ADR 0059, decision 6).

    interpreter inzope/run.py ZOPE_CONF replay_zope.py LOG SETTINGS OUT

`plonebench.catalog.replay` runs it in the interpreter of the zoeclient
scenario, on a copy of a ZOE store: in-process on `zoe.Storage`, or through
`zoe serve` with the native client. The zope.conf installs the backend
`catalog` of zoe.plone in the mode `compare`, so each search runs the stock
search and asks the mirror of the store, compares the answers and counts.

The reader replays each line of the log with `Catalog.search`, as the
probe recorded it, on one of two Connections: the fresh one begins a new
transaction before each search, so it sees the last commit; each
`pinned_every`th search that the hook can plan runs on the pinned one, which
keeps its snapshot for `pinned_sync` of its searches, so its later searches
fall outside the mirror window. A line that ran in a transaction that
changed the catalog runs on the fresh Connection after a change of one
Document, without a commit, so the search sends a transaction overlay.

The writer commits saves of the shapes of B5 to the same store during the
replay, from a Connection of its own: an edit of a Document (its title and
its subjects), a new Document in a folder, and a workflow transition. At
the middle of the replay, it commits one flip of a BooleanIndex
(`_invert_index`): the pickle of the index object changes, the mirror goes
stale and builds again. A thread then asks the mirror at the snapshot just
after that commit until it answers, and measures the seconds from the
commit to that answer. The saves search the catalog too, in a transaction
that changed it, and these searches count with the others.

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
    """The catalog tool as `connection` sees it. Its site becomes the site
    of the thread, as in a request: a sort on `getObjPositionInParent` and
    the content of Plone look up their components there."""
    from Acquisition import aq_parent
    from zope.component.hooks import setSite

    tool = connection.root()["Application"].unrestrictedTraverse(TOOL)
    setSite(aq_parent(tool))
    return tool


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
        self.oid = aq_base(tool_of(self.connection)._catalog)._p_oid
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
                    threading.Thread(
                        target=self.measure_flip, name="flip", daemon=True
                    ).start()
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
        from Testing.makerequest import makerequest

        tool_of(self.connection)
        # The workflow tool reads the request of its context.
        root = makerequest(self.connection.root()["Application"])
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

    def measure_flip(self):
        """Ask the mirror at the snapshot just after the flip until it
        answers: the seconds of the build again, and the reasons until
        then."""
        plan = {"c": [], "s": [], "l": None, "bs": 0, "bz": None, "cols": None}
        plan["rn"] = 0
        start = p64(self.flip["tid"] + 1)
        reasons = {}
        deadline = time.monotonic() + 600
        while time.monotonic() < deadline:
            reply = storage.catalog_query(self.oid, start, plan, None)
            if "f" not in reply:
                self.flip["seconds"] = round(time.monotonic() - self.flip["at"], 3)
                break
            reasons[reply["f"]] = reasons.get(reply["f"], 0) + 1
            time.sleep(0.01)
        self.flip["reasons"] = reasons

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


def arguments(catalog, line):
    """The arguments of `Catalog.search` of one line, on `catalog`."""
    query = querylog.decode(line["query"])
    sort = line.get("sort")
    if sort is None:
        sort_index = None
    elif isinstance(sort, str):
        sort_index = catalog.getIndex(sort)
    else:
        sort_index = [catalog.getIndex(name) for name in sort]
    reverse = querylog.decode(line.get("reverse", False))
    return query, sort_index, reverse, line.get("limit"), line.get("merge", True)


def served(tool, line):
    """Tell if the hook makes a plan for the line: the mirror can serve
    it."""
    try:
        plan, _ = backend.plan_of(tool._catalog, *arguments(tool._catalog, line))
    except Exception:
        return False
    return plan is not None


def run_search(tool, line):
    """Replay one line on the catalog tool `tool`."""
    catalog = tool._catalog
    return len(catalog.search(*arguments(catalog, line)))


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
    servable = 0
    rounds = settings.get("rounds", 1)
    plan = [line for _ in range(rounds) for line in lines]
    if settings.get("limit"):
        plan = plan[: settings["limit"]]
    flip_at = len(plan) // 2 if settings.get("flip", True) else None
    counts = {"replayed": 0, "skipped": 0, "pinned": 0, "changed": 0}
    errors = {}
    # A build after a stale commit starts the count of the rebases of the
    # mirror again, so the reader adds up what it sees.
    rebases = {"seen": before["rebases"] if before else 0, "total": 0}

    def count_rebases():
        found = status(oid)
        if found is None or found.get("rebases") is None:
            return
        now = found["rebases"]
        rebases["total"] += now - rebases["seen"] if now >= rebases["seen"] else now
        rebases["seen"] = now

    started = time.monotonic()
    for i, line in enumerate(plan):
        if i == flip_at:
            writer.flip_due.set()
        if not line.get("replayable", True):
            counts["skipped"] += 1
            continue
        # Each `pinned_every`th search that the mirror can serve runs on
        # the pinned snapshot.
        every = settings["pinned_every"]
        on_pinned = False
        if every and not line.get("changed") and served(tool_of(fresh), line):
            servable += 1
            on_pinned = servable % every == 0
        fresh_tm.abort()
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
        count_rebases()
    seconds = time.monotonic() - started
    writer.stop.set()
    writer.join(30)
    pinned_tm.abort()
    pinned.close()
    fresh.close()
    after = status(oid)
    count_rebases()
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
        "rebases": rebases["total"] if after is not None else None,
        "mirror": (
            {key: after.get(key) for key in ("state", "sets", "rebases", "bytes")}
            if after is not None
            else None
        ),
        "flip": {
            "committed": bool(writer.flip.get("committed")),
            "index_entries": writer.flip.get("index_entries"),
            "seconds_to_native": writer.flip.get("seconds"),
            "reasons_until_native": writer.flip.get("reasons"),
        },
    }
    with open(out_path, "w") as out:
        json.dump(report, out, indent=1, sort_keys=True)


main()
