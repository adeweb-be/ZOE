"""The plans of a catalog fixture for `zoe-bench catalog`, inside Zope.

    interpreter inzope/run.py ZOPE_CONF plans_zope.py SETTINGS LOG OUT

`plonebench.catalog.plans` runs it in the interpreter of the zoeclient
scenario, on the ZOE store of a catalog fixture in the process, with the
backend `catalog` of zoe.plone. SETTINGS is a JSON file: the fixture, its
query set, and the knobs below. LOG is a query log in the merged form of
`plonebench.catalog.log`, maybe empty. OUT receives the plans file, the
msgpack form that `zoe-bench catalog` reads (`crates/zoe-bench/src/
catalog/plans.rs`).

The script:

1. replaces the ZCTextIndex indexes `SearchableText`, `Title` and
   `Description` of the catalog with `ZoeTextIndex` indexes of the same
   ids, as the profile step `zoe.plone:text` makes them: the enhanced mode,
   the language of the site and, for `SearchableText`, the boosts. The
   fixture has no content object, so it does not reindex.
2. stages the texts of each entry of the plan through `index_object` of
   each `ZoeTextIndex`, which writes the text deltas with the write path of
   the store, in commits of `batch` entries. The registration key of the
   catalog comes with the backend, at the open of the database.
3. makes the plans with the plan builder of the backend
   (`zoe.plone.catalog.plan_of`), from the arguments that ZCatalog gives
   `Catalog.search`: the intersections of the query set; each text term of
   the query set, but for the band `rare`, in five variants (alone, with
   the filter of a listing, with a selective filter, with a sort on
   `effective`, with the filter and the sort), and the two filters alone;
   the path queries; and each distinct line of LOG, with the reason of the
   client when it has no plan. With `expect`, each plan gets the rids,
   `len()` and `actual_result_count` of the stock search.
4. makes the writes: renames of leaf folders that keep the rid of each
   entry, so `paths`, `uids` and the path index change, until `rename_rows`
   rows changed. They run on a `DemoStorage` over the store and never reach
   the store: the plans file gets the records of each transaction.
5. writes the settings of `zoe.text` of both modes and a sample of the live
   texts.

Steps 1 and 2 run once: a store that holds the `ZoeTextIndex` indexes
keeps its texts.
"""

import json
import sys
import time

import msgpack
import transaction
import zoe.kv
from AccessControl.SecurityManagement import newSecurityManager
from AccessControl.SpecialUsers import system
from Acquisition import aq_base
from Products.ZCatalog.Catalog import Catalog
from ZODB.DB import DB
from ZODB.DemoStorage import DemoStorage
from zoe.plone import catalog as backend
from zoe.plone import textindex
from zoe.storage import Unavailable
from zope.component.hooks import setSite
from ZTUtils.Lazy import LazyCat

from plonebench.catalog import log as querylog
from plonebench.catalog import plan

settings_path, log_path, out_path = sys.argv[1:]
settings = json.loads(open(settings_path).read())
lines = querylog.read(log_path) if log_path else []
spec = plan.CatalogSpec.from_json(settings["spec"])
entries = plan.Entries(spec)
db = app._p_jar.db()  # noqa: F821
storage = db.storage
LANGUAGE = "en"
LEXICON = "plone_lexicon"


def say(text):
    print(f"plans: {text}", flush=True)


class Extra:
    """The extra record of a new index."""


class Doc:
    """What a `ZoeTextIndex` reads of one entry: its three texts and its
    language."""

    def __init__(self, values):
        self.SearchableText = values["SearchableText"]
        self.Title = values["Title"]
        self.Description = values["Description"]
        self.Language = LANGUAGE


class Stub:
    """What the path index reads of one entry: its physical path."""

    def __init__(self, path):
        self._physical = tuple(path.split("/"))

    def getPhysicalPath(self):
        return self._physical


def text_indexes(tool):
    """Give the catalog the three `ZoeTextIndex` indexes. Return True when
    they are new: the texts are not staged yet."""
    inner = tool._catalog
    if all(
        isinstance(aq_base(inner.getIndex(name)), textindex.ZoeTextIndex)
        for name in textindex.TEXT_INDEXES
    ):
        return False
    for name in textindex.TEXT_INDEXES:
        extra = Extra()
        extra.indexed_attrs = name
        extra.lexicon_id = LEXICON
        extra.mode = "enhanced"
        extra.language = LANGUAGE
        if name == "SearchableText":
            extra.boosts = ",".join(f"{k}:{v}" for k, v in textindex.BOOSTS.items())
        index = textindex.ZoeTextIndex(name, extra=extra, caller=tool)
        inner.delIndex(name)
        inner.addIndex(name, index)
    transaction.commit()
    say("the ZoeTextIndex indexes replace the ZCTextIndex indexes")
    return True


def stage_texts(tool):
    """Stage the texts of each entry of the plan, in commits of `batch`
    entries."""
    inner = tool._catalog
    indexes = [inner.getIndex(name) for name in textindex.TEXT_INDEXES]
    batch = settings["batch"]
    started = time.time()
    done = 0
    for values in entries:
        rid = inner.uids.get(values["_path"])
        if rid is None:
            raise SystemExit(f"plans: no rid for an entry of the plan, after {done}")
        doc = Doc(values)
        for index in indexes:
            index.index_object(rid, doc)
        done += 1
        if done % batch == 0 or done == spec.entries:
            transaction.commit()
            app._p_jar.cacheMinimize()  # noqa: F821
            if done % (batch * 20) == 0 or done == spec.entries:
                seconds = time.time() - started
                say(f"{done}/{spec.entries} entries staged in {seconds:.0f} s")


def capture(tool):
    """Return `run(query)` and `call(arguments)`: each makes the plan of a
    search and, with `expect`, the answer of the stock search."""
    inner = tool._catalog
    stock = backend._installed.get("stock") or Catalog.search
    want = settings["expect"]

    def call(query, sort_index, reverse, limit, merge):
        planned, reason = backend.plan_of(
            inner, query, sort_index, reverse, limit, merge
        )
        expect = None
        if want and planned is not None:
            result = stock(inner, query, sort_index, reverse, limit, merge)
            rids = [r[1] if isinstance(r, tuple) else r for r in backend._rids(result)]
            expect = {
                "r": b"".join(int(r).to_bytes(4, "big", signed=True) for r in rids),
                "n": len(result),
                "a": result.actual_result_count,
            }
        return planned, reason, expect

    def run(query):
        seen = []
        original = Catalog.search

        def search(self, q, sort_index=None, reverse=False, limit=None, merge=True):
            # The search gives no result: only its arguments count.
            seen.append(call(q, sort_index, reverse, limit, merge))
            return LazyCat([])

        Catalog.search = search
        try:
            tool.unrestrictedSearchResults(**query)
        finally:
            Catalog.search = original
        if len(seen) != 1:
            raise SystemExit(f"plans: a query made {len(seen)} searches")
        return seen[0]

    return run, call


def item(name, scenario, made, **extra):
    planned, reason, expect = made
    out = {"name": name, "scenario": scenario, "plan": planned, "expect": expect}
    if planned is None:
        out["reason"] = reason or "?"
    out.update(extra)
    return out


def text_queries(run, queries):
    """The text terms of the query set but the band `rare`, in five
    variants, and the two filters alone."""
    now = plan.iso(entries.now)
    listing = {
        "portal_type": "Document",
        "review_state": plan.PUBLISHED,
        "allowedRolesAndUsers": ["Anonymous"],
        "effectiveRange": now,
    }
    selective = {"Subject": entries.words[57], "portal_type": "News Item"}
    page = {"b_start": 0, "b_size": 20}
    sort = {"sort_on": "effective", "sort_order": "descending", "sort_limit": 20}
    variants = (
        ("alone", page),
        ("listing", {**listing, **page}),
        ("selective", {**selective, **page}),
        ("sort", {**sort, **page}),
        ("listing_sort", {**listing, **sort, **page}),
    )
    out = [
        item("listing", "filter", run(listing)),
        item("selective", "filter", run(selective)),
    ]
    for q in queries:
        if q["kind"] != "text" or q["band"] == "rare":
            continue
        for variant, rest in variants:
            out.append(
                item(
                    f"{q['name']}-{variant}",
                    "text",
                    run({**q["query"], **rest}),
                    band=q["band"],
                    variant=variant,
                )
            )
    return out


def leaf_folders(inner):
    """The paths of the leaf folders of the plan, in order: the Folders of
    depth 3 under a unit."""
    out = []
    for path in inner.uids.keys(min=f"/{plan.SITE_ID}/section-"):
        parts = path.split("/")
        if (
            len(parts) == 5
            and parts[3].startswith("folder-")
            and parts[4].startswith("folder-")
        ):
            out.append(path)
    return out


def path_queries(run, leaves):
    """Path queries around the first leaf folder that the writes rename."""
    leaf = leaves[0]
    parent = leaf.rsplit("/", 1)[0]
    unit = parent.rsplit("/", 1)[0]
    queries = (
        (
            "unit",
            {
                "path": {"query": unit, "depth": -1},
                "sort_on": "sortable_title",
                "sort_limit": 50,
            },
        ),
        (
            "children",
            {"path": {"query": parent, "depth": 1}, "sort_on": "sortable_title"},
        ),
        ("navtree", {"path": {"query": leaf, "navtree": True, "depth": 1}}),
        ("top", {"path": {"query": f"/{plan.SITE_ID}", "depth": 1}}),
    )
    return [item(name, "path", run(query)) for name, query in queries]


def log_queries(call, inner):
    """The distinct lines of the log, each with the lines that it stands
    for."""
    out = {}
    for line in lines:
        if not line.get("replayable", True):
            continue
        key = json.dumps(
            [
                line["query"],
                line.get("sort"),
                line.get("reverse"),
                line.get("limit"),
                line.get("merge", True),
            ],
            sort_keys=True,
        )
        if key in out:
            out[key]["weight"] += 1
            continue
        query = querylog.decode(line["query"])
        sort = line.get("sort")
        try:
            if sort is None:
                sort_index = None
            elif isinstance(sort, str):
                sort_index = inner.getIndex(sort)
            else:
                sort_index = [inner.getIndex(name) for name in sort]
            made = call(
                query,
                sort_index,
                querylog.decode(line.get("reverse", False)),
                line.get("limit"),
                line.get("merge", True),
            )
        except Exception as e:
            made = (None, f"error:{type(e).__name__}", None)
        out[key] = item(f"log-{len(out)}", "mix", made, weight=1)
    return list(out.values())


def renames(leaves):
    """The writes: renames of leaf folders that keep the rids, on a
    `DemoStorage` over the store, in transactions of `rename_batch`
    folders. Return the records and the rows of each transaction."""
    # The close of the other database leaves the store open.
    demo = DemoStorage(base=storage, close_base_on_close=False)
    other = DB(demo)
    tm = transaction.TransactionManager()
    connection = other.open(transaction_manager=tm)
    try:
        tool = connection.root()["Application"][plan.SITE_ID].portal_catalog
        inner = tool._catalog
        path_index = inner.getIndex("path")
        target = settings["rename_rows"]
        rows = []
        done = 0
        pending = 0
        moved = 0
        for leaf in leaves:
            if done >= target:
                break
            renamed = f"{leaf}-moved"
            items = [
                (p, r)
                for p, r in inner.uids.items(min=leaf, max=leaf + "/\uffff")
                if p == leaf or p.startswith(leaf + "/")
            ]
            for old, rid in items:
                new = renamed + old[len(leaf) :]
                inner.paths[rid] = new
                del inner.uids[old]
                inner.uids[new] = rid
                path_index.index_object(rid, Stub(new))
            done += len(items)
            pending += len(items)
            moved += 1
            if moved % settings["rename_batch"] == 0:
                tm.commit()
                rows.append(pending)
                pending = 0
        if pending:
            tm.commit()
            rows.append(pending)
        writes = []
        for txn, count in zip(demo.changes.iterator(), rows, strict=True):
            records = [[r.oid, r.data] for r in txn if r.data is not None]
            writes.append({"rows": count, "records": records})
        return writes
    finally:
        tm.abort()
        connection.close()
        other.close()


def wait_text(run, query, timeout):
    """Wait until the text index of the store answers `query`."""
    started = time.monotonic()
    while True:
        try:
            run(query)
            return
        except Unavailable:
            if time.monotonic() - started > timeout:
                raise
            time.sleep(0.5)


def main():
    newSecurityManager(None, system)
    tm = transaction.manager
    site = app[plan.SITE_ID]  # noqa: F821
    setSite(site)
    tool = site.portal_catalog
    if text_indexes(tool):
        stage_texts(tool)
    inner = tool._catalog
    oid = aq_base(inner)._p_oid
    run, call = capture(tool)
    queries = settings["queries"]
    if settings["expect"]:
        first = next(q for q in queries if q["kind"] == "text")
        wait_text(run, first["query"], settings["text_timeout"])
    out = [
        item(q["name"], "intersect", run(q["query"]))
        for q in queries
        if q["kind"] == "intersection"
    ]
    out += text_queries(run, queries)
    leaves = leaf_folders(inner)
    out += path_queries(run, leaves)
    out += log_queries(call, inner)
    say(
        f"{len(out)} queries, {sum(1 for q in out if q['plan'] is None)} without a plan"
    )
    texts = []
    for values in entries:
        if len(texts) >= settings["sample"]:
            break
        if values["portal_type"] in plan.WITH_TEXT:
            texts.append(
                [
                    "SearchableText",
                    inner.uids[values["_path"]],
                    LANGUAGE,
                    values["SearchableText"],
                ]
            )
    space = zoe.kv.Namespace(app._p_jar, "durable", textindex.NAMESPACE)  # noqa: F821
    enhanced = space.get(oid)
    if enhanced is None:
        raise SystemExit("plans: the store holds no settings of the text indexes")
    compat = json.loads(enhanced)
    for each in compat["indexes"].values():
        each["mode"] = "compat"
    compat = json.dumps(compat, sort_keys=True).encode("utf-8")
    tm.abort()
    writes = renames(leaves)
    say(f"{len(writes)} writes, {sum(w['rows'] for w in writes)} rows")
    document = {
        "version": 1,
        "catalog": oid,
        "fixture": {
            "name": settings["name"],
            "preset": settings["preset"],
            "counts": settings["counts"],
        },
        "modes": {"enhanced": enhanced, "compat": compat},
        "queries": out,
        "writes": writes,
        "texts": texts,
    }
    with open(out_path, "wb") as f:
        msgpack.pack(document, f, use_bin_type=True)
    say(f"written: {out_path}")


main()
transaction.abort()
db.close()
