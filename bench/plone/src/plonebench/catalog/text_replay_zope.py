"""The replay of the text queries of the Plone suites inside Zope (ADR 0060,
section 11, decision 8).

    interpreter inzope/run.py ZOPE_CONF text_replay_zope.py build CORPUS OUT
    interpreter inzope/run.py ZOPE_CONF text_replay_zope.py replay CORPUS SETTINGS OUT

`plonebench.catalog.text_replay` runs it in the interpreter of the
zoeclient scenario, with the backend `catalog` of zoe.plone in the mode
`on`. CORPUS is the JSON of the replay: the texts of each index and the
text searches of the log.

`build` makes a Plone site `Plone` in a new ZOE store. Its catalog gets,
for `SearchableText`, `Title` and `Description`, a `ZoeTextIndex` in the
mode `compat` on the same attribute and the lexicon `plone_lexicon`
(`zoe_<id>`), beside the ZCTextIndex of the site. Each text of an index is
an object of its own, catalogued in both indexes of that index. OUT gets
the number of the documents.

`replay` waits until the mirror and the text index of the store answer,
then runs each search on the ZCTextIndex with the stock search, and on the
`ZoeTextIndex` through `Catalog.search` (`catalog_query`, with the hook)
and through the stock search (`text_query`). It compares each answer of
the `ZoeTextIndex` with the answer of the ZCTextIndex: the rid, the score
and the normalized score of each brain, `len()` and `actual_result_count`,
or the name of the error. A search runs in one of three modes: `fresh`, on
a Connection that begins again before it; `pinned`, on a Connection that
keeps its snapshot for `pinned_sync` of its searches while the writer
commits past the mirror window; `staged`, on the fresh Connection after a
change of the text of a document that it does not commit. A writer thread
commits changes of the texts, new documents and removed documents from a
Connection of its own during the replay.

OUT receives a JSON report: the counts by mode and by path, the
comparisons, the mismatches with their mode, path, index and line, the
paths and the reasons of the hook, the overlays past the bound, the errors
by class, the changes of the reader that failed, and the writer. It holds
no value of a query and no text.
"""

import json
import random
import sys
import threading
import time
import traceback

import transaction
from Acquisition import aq_base
from Products.ZCatalog.Catalog import Catalog
from ZODB.POSException import ConflictError
from ZODB.utils import p64, u64
from zoe.plone import catalog as backend
from zoe.plone import textindex

from plonebench.catalog import log as querylog

INDEXES = ("SearchableText", "Title", "Description")
ZOE = "zoe_{}"
SITE = "Plone"
LEXICON = "plone_lexicon"
BATCH = 500

# inzope/run.py gives the application as `app`.
db = app._p_jar.db()  # noqa: F821


def system_user():
    from AccessControl.SecurityManagement import newSecurityManager
    from AccessControl.SpecialUsers import system

    newSecurityManager(None, system)


class Doc:
    """A document of the replay: the text of one index."""

    def __init__(self, name, text):
        setattr(self, name, text)


class _Extra:
    """The extra record of an index."""


def uid_of(name, n):
    return f"/{SITE}/replay-text/{name}/{n}"


def tool_of(connection):
    from Acquisition import aq_parent
    from zope.component.hooks import setSite

    tool = connection.root()["Application"][SITE].portal_catalog
    setSite(aq_parent(tool))
    return tool


def build(corpus_path, out_path):
    from Products.CMFPlone.factory import addPloneSite

    corpus = json.loads(open(corpus_path).read())
    system_user()
    root = app  # noqa: F821
    addPloneSite(
        root,
        SITE,
        setup_content=False,
        extension_ids=("plone.app.contenttypes:default",),
    )
    transaction.commit()
    site = root[SITE]
    pc = site.portal_catalog
    for name in INDEXES:
        extra = _Extra()
        extra.indexed_attrs = name
        extra.lexicon_id = LEXICON
        extra.mode = "compat"
        index = textindex.ZoeTextIndex(ZOE.format(name), extra=extra, caller=pc)
        pc._catalog.addIndex(ZOE.format(name), index)
    # The entries of the site go into the new indexes too, so both indexes
    # of each id hold the same documents.
    textindex._reindex(pc, [ZOE.format(name) for name in INDEXES])
    transaction.commit()
    documents = 0
    for name in INDEXES:
        for n, text in enumerate(corpus["texts"].get(name, [])):
            pc._catalog.catalogObject(
                Doc(name, text), uid_of(name, n), idxs=[name, ZOE.format(name)]
            )
            documents += 1
            if documents % BATCH == 0:
                transaction.commit()
    transaction.commit()
    with open(out_path, "w") as out:
        json.dump({"documents": documents}, out)


def answer(catalog, query, stock):
    """The rid, the score and the normalized score of each brain of the
    search `query`, `len()` and `actual_result_count`; or the name of the
    error. `stock` runs the stock search, else the search of the hook."""
    search = backend._installed["stock"] if stock else Catalog.search
    try:
        result = search(catalog, query)
        found = [
            (b.getRID(), b.data_record_score_, b.data_record_normalized_score_)
            for b in result
        ]
    except ConflictError:
        raise
    except Exception as e:
        return f"{type(e).__module__}.{type(e).__qualname__}"
    return [found, len(result), getattr(result, "actual_result_count", len(result))]


class Writer(threading.Thread):
    """Commit changes of the texts until `stop` is set."""

    def __init__(self, corpus, interval, rnd):
        super().__init__(name="writer", daemon=True)
        self.texts = corpus["texts"]
        self.interval = interval
        self.rnd = rnd
        self.stop = threading.Event()
        self.counts = {"commits": 0, "conflicts": 0, "errors": 0}
        self.counts.update(edits=0, creates=0, removes=0)
        self.lock = threading.Lock()
        self.tm = transaction.TransactionManager()
        self.connection = db.open(transaction_manager=self.tm)
        self.next = {name: len(self.texts.get(name, [])) for name in INDEXES}

    def commits(self):
        with self.lock:
            return self.counts["commits"]

    def run(self):
        system_user()
        while not self.stop.is_set():
            try:
                key = self.change()
                self.tm.commit()
                with self.lock:
                    self.counts["commits"] += 1
                    self.counts[key] += 1
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

    def change(self):
        pc = tool_of(self.connection)._catalog
        names = [n for n in INDEXES if self.texts.get(n)]
        name = self.rnd.choice(names)
        texts = self.texts[name]
        roll = self.rnd.random()
        idxs = [name, ZOE.format(name)]
        if roll < 0.7:
            # Another text, or two texts of the corpus as one.
            n = self.rnd.randrange(self.next[name])
            text = self.rnd.choice(texts)
            if self.rnd.random() < 0.3:
                text = text + " " + self.rnd.choice(texts)
            pc.catalogObject(Doc(name, text), uid_of(name, n), idxs=idxs)
            return "edits"
        if roll < 0.85:
            n = self.next[name]
            self.next[name] += 1
            pc.catalogObject(
                Doc(name, self.rnd.choice(texts)), uid_of(name, n), idxs=idxs
            )
            return "creates"
        n = self.rnd.randrange(self.next[name])
        if uid_of(name, n) in pc.uids:
            pc.uncatalogObject(uid_of(name, n))
        return "removes"


def wait_ready(oid, timeout):
    """Wait until the mirror and the text index answer a search on the
    catalog `oid`."""
    started = time.monotonic()
    plan = {"c": [], "s": [], "l": None, "bs": 0, "bz": None, "cols": None, "rn": 0}
    connection = db.open(transaction_manager=transaction.TransactionManager())
    try:
        pc = tool_of(connection)._catalog
        while True:
            start = p64(u64(db.storage.lastTransaction()) + 1)
            reply = db.storage.catalog_query(oid, start, plan, None)
            if "f" not in reply:
                try:
                    pc.getIndex(ZOE.format("SearchableText")).query_index(
                        _Record(["replay"])
                    )
                    return time.monotonic() - started
                except Exception:
                    pass
            if time.monotonic() - started > timeout:
                raise SystemExit("replay text: the store does not answer")
            time.sleep(0.2)
    finally:
        connection.close()


class _Record:
    """A record of a query: its keys."""

    def __init__(self, keys):
        self.keys = keys


def replay(corpus_path, settings_path, out_path):
    corpus = json.loads(open(corpus_path).read())
    settings = json.loads(open(settings_path).read())
    rnd = random.Random(settings["seed"])
    system_user()
    first = db.open(transaction_manager=transaction.TransactionManager())
    oid = aq_base(tool_of(first)._catalog)._p_oid
    first.close()
    ready = wait_ready(oid, settings.get("ready_timeout", 600))
    writer = Writer(corpus, settings["writer_interval"], random.Random(rnd.random()))
    fresh_tm = transaction.TransactionManager()
    fresh = db.open(transaction_manager=fresh_tm)
    pinned_tm = transaction.TransactionManager()
    pinned = db.open(transaction_manager=pinned_tm)
    backend._counts.clear()
    plan = [s for _ in range(settings.get("rounds", 1)) for s in corpus["searches"]]
    if settings.get("limit"):
        plan = plan[: settings["limit"]]
    by_mode_path = {}
    mismatched = []
    errors = {}
    counts = {"compared": 0, "mismatches": 0, "change_errors": 0}
    pinned_uses = 0
    pinned_at = 0
    writer.start()
    started = time.monotonic()
    for i, search in enumerate(plan):
        if settings["pinned_every"] and i % settings["pinned_every"] == 1:
            mode = "pinned"
        elif search.get("changed") or (
            settings["staged_every"] and i % settings["staged_every"] == 0
        ):
            mode = "staged"
        else:
            mode = "fresh"
        name = search["index"]
        try:
            value = querylog.decode(search["value"])
        except querylog.LogError:
            continue
        query = {name: value}
        for option in ("b_start", "b_size"):
            if option in search:
                query[option] = search[option]
        zoe_query = dict(query)
        zoe_query[ZOE.format(name)] = zoe_query.pop(name)
        fresh_tm.abort()
        if mode == "pinned":
            if pinned_uses >= settings["pinned_sync"]:
                pinned_tm.begin()
                pinned_at = writer.commits()
                pinned_uses = 0
            # The writer commits past the mirror window of the snapshot.
            deadline = time.monotonic() + 5
            while (
                writer.commits() - pinned_at <= settings["window_tids"]
                and time.monotonic() < deadline
            ):
                time.sleep(0.01)
            pinned_uses += 1
            connection = pinned
        else:
            fresh_tm.begin()
            connection = fresh
        try:
            pc = tool_of(connection)._catalog
            if mode == "staged":
                try:
                    texts = corpus["texts"][name]
                    n = rnd.randrange(len(texts))
                    pc.catalogObject(
                        Doc(name, rnd.choice(texts)),
                        uid_of(name, n),
                        idxs=[name, ZOE.format(name)],
                    )
                except ConflictError:
                    raise
                except Exception:
                    counts["change_errors"] += 1
            expected = answer(pc, query, stock=True)
            for path, stock in (("catalog_query", False), ("text_query", True)):
                found = answer(pc, zoe_query, stock=stock)
                key = f"{mode} {path}"
                by_mode_path[key] = by_mode_path.get(key, 0) + 1
                counts["compared"] += 1
                if found != expected:
                    counts["mismatches"] += 1
                    mismatched.append(
                        {
                            "mode": mode,
                            "path": path,
                            "index": name,
                            "line": search["line"],
                        }
                    )
        except Exception as e:
            key = f"{type(e).__module__}.{type(e).__qualname__}"
            errors[key] = errors.get(key, 0) + 1
            if sum(errors.values()) < 5:
                traceback.print_exc()
        finally:
            if connection is fresh:
                fresh_tm.abort()
    seconds = time.monotonic() - started
    writer.stop.set()
    writer.join(30)
    pinned_tm.abort()
    pinned.close()
    fresh.close()
    found = backend._counts_now()
    report = {
        "leg": settings["leg"],
        "searches": len(plan),
        "ready_seconds": round(ready, 3),
        "seconds": round(seconds, 3),
        **counts,
        "by_mode_path": by_mode_path,
        "mismatched": mismatched[:1000],
        "errors": errors,
        "queries": found["queries"],
        "fallbacks": [
            [reason, side, n]
            for (reason, side), n in sorted(found["fallbacks"].items())
        ],
        "overlay_bound": found["overlay_bound"],
        "writer": writer.counts,
    }
    with open(out_path, "w") as out:
        json.dump(report, out, indent=1, sort_keys=True)


def main():
    mode, *args = sys.argv[1:]
    if mode == "build":
        build(*args)
    elif mode == "replay":
        replay(*args)
    else:
        raise SystemExit(f"text_replay_zope: no mode {mode!r}")


main()
