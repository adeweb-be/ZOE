"""Acceptance tests of the query log of the catalog and of its replay
(ADR 0059, decision 6).

- The probe writes the log of a fixed set of searches: one line for each
  call of `Catalog.search`, in the version of the format, with the query
  as ZCatalog got it, its sort, the class of each index, whether the
  search ran in a transaction that changed the catalog, and its counts.
- The replay of that log on a small ZOE store, with a writer that commits
  during the replay and with low `catalog-rebase-rows` and mirror window,
  in-process and through `zoe serve`, gives no mismatch, the counts by path
  and by reason, a rebase and a query outside the window.
- A line of another version of the format is refused.
- The report holds no value of a query: a marker value in a query of the
  log is in no output of the replay.

The site is a Plone site of a few folders and Documents in a new
`zoe.Storage`, which a Zope process of the zoeclient scenario builds. The
tests need `just plone zoeclient build` and the `zoe` binary in release
(`cargo build --release -p zoe-server --bin zoe`, or `ZOE_BINARY`), and
take a few minutes:

    just bench plone test bench/plone/tests/test_catalog_log.py
"""

import json
import subprocess
import textwrap
from pathlib import Path

import pytest
from benchcommon import servers

from plonebench import stack
from plonebench.catalog import log, replay

INTERPRETER = stack.interpreter(stack.STACKS["zoe-native"])

pytestmark = pytest.mark.skipif(
    not INTERPRETER.exists() or not servers.zoe_path().is_file(),
    reason="needs `just plone zoeclient build` and the zoe binary in release",
)

# A value that only one query of the log holds.
MARKER = "marker-4b1d27"

# The change sets of the mirror window of the replay.
WINDOW = 2

# The fixed set of searches of the site, through `searchResults`.
SITE_SCRIPT = textwrap.dedent(
    """
    import json
    import sys

    import transaction
    from AccessControl.SecurityManagement import newSecurityManager
    from AccessControl.SpecialUsers import system
    from DateTime import DateTime
    from Products.CMFPlone.factory import addPloneSite
    from zope.component.hooks import setSite

    probe, out, marker = sys.argv[1:]
    sys.path.insert(0, probe)
    import plonebench_catalog

    newSecurityManager(None, system)
    addPloneSite(
        app,
        "Plone",
        setup_content=False,
        extension_ids=("plone.app.contenttypes:default",),
    )
    transaction.commit()
    site = app["Plone"]
    setSite(site)
    workflow = site.portal_workflow
    for f in range(3):
        site.invokeFactory("Folder", f"f-{f}", title=f"Folder {f}")
        folder = site[f"f-{f}"]
        for i in range(8):
            folder.invokeFactory("Document", f"d-{i}", title=f"Document {f} {i}")
            doc = folder[f"d-{i}"]
            doc.setSubject(("alpha", "beta")[: 1 + i % 2])
            doc.setEffectiveDate(DateTime("2026/01/01 UTC") - i)
            if i % 2:
                workflow.doActionFor(doc, "publish")
            doc.reindexObject()
    transaction.commit()

    plonebench_catalog.install(out)
    catalog = site.portal_catalog
    queries = [
        {"portal_type": "Document"},
        {"portal_type": "Document", "Subject": marker},
        {"Subject": ["alpha", "beta"], "sort_on": "sortable_title"},
        {"review_state": "published", "sort_on": "modified", "sort_order": "reverse"},
        {"path": {"query": "/Plone/f-1", "depth": 1}, "sort_on": "getId"},
        {"effectiveRange": DateTime("2026/01/01 UTC"), "portal_type": "Document"},
        {"is_folderish": True},
        {
            "portal_type": ["Document", "Folder"],
            "sort_on": ["portal_type", "getId"],
            "sort_limit": 10,
        },
        {"portal_type": "Document", "sort_on": "getId", "b_start": 2, "b_size": 3},
        {"SearchableText": "Document"},
    ]
    searches = 0
    for query in queries:
        catalog.unrestrictedSearchResults(**query)
        searches += 1
    catalog.searchResults(portal_type="Document")
    searches += 1
    # A search in a transaction that changed the catalog.
    site["f-0"]["d-0"].setTitle("Changed")
    site["f-0"]["d-0"].reindexObject()
    catalog.unrestrictedSearchResults(portal_type="Document", sort_on="sortable_title")
    searches += 1
    transaction.abort()
    print(json.dumps({"searches": searches}))
    """
)


def zope_conf(home, store):
    instancehome = stack.PLONE / stack.STACKS["zoe-native"].scenario
    return f"""%import zoe
%define INSTANCEHOME {instancehome / "parts" / "instance"}
instancehome $INSTANCEHOME
%define CLIENTHOME {home / "var"}
clienthome $CLIENTHOME
debug-mode off
security-policy-implementation C
<environment>
    CHAMELEON_CACHE {home / "var" / "cache"}
</environment>
<zodb_db main>
    <zoe>
      path {store}
    </zoe>
    mount-point /
</zodb_db>
"""


@pytest.fixture(scope="module")
def site(tmp_path_factory):
    """A small Plone site in a new ZOE store, and the log of the fixed set
    of searches on it. Return the store, the log and the searches."""
    home = tmp_path_factory.mktemp("site")
    (home / "var" / "cache").mkdir(parents=True)
    store = home / "store"
    conf = home / "zope.conf"
    conf.write_text(zope_conf(home, store))
    script = home / "site.py"
    script.write_text(SITE_SCRIPT)
    out = home / "catalog-site.jsonl"
    result = subprocess.run(
        [
            str(INTERPRETER),
            str(stack.INZOPE / "run.py"),
            str(conf),
            str(script),
            str(stack.PROBE),
            str(out),
            MARKER,
        ],
        env=stack.clean_environment(),
        cwd=home,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr[-5000:]
    searches = json.loads(result.stdout.strip().splitlines()[-1])["searches"]
    return store, out, searches


@pytest.fixture(scope="module")
def replayed(site, tmp_path_factory):
    """The report of the replay of the log on the store, in both legs."""
    store, out, _ = site
    settings = replay.Settings(
        rebase_rows=2,
        window_tids=WINDOW,
        window_seconds=30,
        writer_interval=0.01,
        pinned_every=3,
        pinned_sync=4,
        rounds=8,
    )
    report = replay.replay([out], store, settings, tmp_path_factory.mktemp("replay"))
    return report, replay.format_report(report)


def test_the_probe_writes_one_line_for_each_search(site):
    _, out, searches = site
    lines = log.read(out)
    assert len(lines) == searches
    assert all(line["v"] == log.VERSION for line in lines)
    assert all(line["catalog"] == ["", "Plone", "portal_catalog"] for line in lines)
    assert all(line["replayable"] for line in lines)
    # The query as ZCatalog got it: searchResults adds the roles of the
    # user and the effective range.
    member = [line for line in lines if "allowedRolesAndUsers" in line["indexes"]]
    assert member and member[0]["indexes"]["effectiveRange"] == "DateRangeIndex"
    sorts = [line["sort"] for line in lines if line["sort"]]
    assert "sortable_title" in sorts and ["portal_type", "getId"] in sorts
    assert sum(line["changed"] for line in lines) == 1
    assert MARKER in out.read_text()
    found = log.inventory(lines)
    assert found["calls"] == searches
    types = {row["type"] for row in found["index_types"]}
    assert {"FieldIndex", "KeywordIndex", "ExtendedPathIndex", "ZCTextIndex"} <= types
    assert MARKER not in json.dumps(found) + log.format_inventory(found)


@pytest.mark.parametrize("leg", replay.LEGS)
def test_the_replay_with_commits_finds_no_mismatch(replayed, leg):
    report, _ = replayed
    found = report["legs"][leg]
    assert found["errors"] == {}
    assert found["mismatches"] == 0, found["mismatched"]
    assert found["replayed"] == found["searches"]
    assert found["queries"]["native"] > 0
    assert found["compared"] == found["queries"]["native"]
    reasons = {(reason, side): n for reason, side, n in found["fallbacks"]}
    # ZCTextIndex is no index of the mirror.
    assert reasons[("index", "client")] > 0
    assert reasons[("outside_window", "server")] >= 1
    assert found["writer"]["commits"] > WINDOW
    assert found["flip"]["committed"]
    if leg == "inprocess":
        assert found["rebases"] >= 1
    else:
        assert found["rebases"] is None


def test_a_line_of_another_version_is_refused(site, tmp_path):
    _, out, _ = site
    other = tmp_path / "catalog-other.jsonl"
    first = json.loads(out.read_text().splitlines()[0])
    first["v"] = log.VERSION + 1
    other.write_text(out.read_text() + json.dumps(first) + "\n")
    with pytest.raises(log.LogError, match="version"):
        log.read(other)
    store = site[0]
    with pytest.raises(log.LogError, match="version"):
        replay.replay([other], store, replay.Settings(), tmp_path / "work")
    # The replay refuses the log before it copies the store.
    assert not (tmp_path / "work").exists()


def test_the_report_holds_no_value_of_a_query(replayed):
    report, text = replayed
    on_disk = (Path(report["directory"]) / "report.json").read_text()
    for output in (text, json.dumps(report), on_disk):
        assert MARKER not in output
        assert "/Plone" not in output
