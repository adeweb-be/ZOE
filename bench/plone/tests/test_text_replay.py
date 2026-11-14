"""Acceptance tests of the replay of the text queries in the mode `compat`
of `ZoeTextIndex` (ADR 0060, section 11, decision 8).

- A Zope process of the zoeclient scenario records a log of text searches
  and the texts of its documents, as `just plone zoe catalog text` records
  them in the suites: the layer option of `zoe.plone.testing` replaces the
  ZCTextIndex indexes of a new Plone site with `ZoeTextIndex` indexes in
  the mode `compat`, and the probe of the harness writes the query log.
- The replay of that log on a small ZOE store that holds those texts, with
  a writer that commits during the replay, in-process and through `zoe
  serve`, gives no mismatch against ZCTextIndex, and the counts by mode
  (`fresh`, `pinned`, `staged`) and by path (`catalog_query`,
  `text_query`).
- The report holds no text of a query: a marker word in a query and in a
  text of the log is in no output of the replay.

The tests need `just plone zoeclient build` and the `zoe` binary in
release (`cargo build --release -p zoe-server --bin zoe`, or `ZOE_BINARY`),
and take a few minutes:

    just bench plone test bench/plone/tests/test_text_replay.py
"""

import json
import subprocess
import textwrap
from pathlib import Path

import pytest
from benchcommon import servers

from plonebench import stack
from plonebench.catalog import text_replay

INTERPRETER = stack.interpreter(stack.STACKS["zoe-native"])

pytestmark = pytest.mark.skipif(
    not INTERPRETER.exists() or not servers.zoe_path().is_file(),
    reason="needs `just plone zoeclient build` and the zoe binary in release",
)

# A word that only one query and one text of the log hold.
MARKER = "markerword"

# The tids of the mirror window of the replay.
WINDOW = 3

# The site, its documents and the text searches of the log.
SITE_SCRIPT = textwrap.dedent(
    """
    import json
    import sys

    import transaction
    from AccessControl.SecurityManagement import newSecurityManager
    from AccessControl.SpecialUsers import system
    from Products.CMFPlone import factory
    from zope.component.hooks import setSite

    probe, directory, marker = sys.argv[1:]
    sys.path.insert(0, probe)
    from zoe.plone import testing

    # The patch of `replace_text_indexes` replaces `factory.addPloneSite`,
    # so the script reads it from the module after the patch.
    testing.replace_text_indexes("compat")
    testing.record_texts(directory)
    newSecurityManager(None, system)
    factory.addPloneSite(
        app,
        "Plone",
        setup_content=False,
        extension_ids=("plone.app.contenttypes:default",),
    )
    transaction.commit()
    site = app["Plone"]
    setSite(site)
    index = site.portal_catalog._catalog.getIndex("SearchableText")
    assert index.getSettings()["mode"] == "compat", index.getSettings()
    words = ["kalo", "mite", "ruso", "vena", "Café", "dabu", "Ærø", "日本語"]
    for i in range(24):
        text = " ".join(words[(i * k) % len(words)] for k in range(1, 2 + i % 5))
        if i == 7:
            text += " " + marker
        site.invokeFactory(
            "Document",
            f"d-{i}",
            title=f"{words[i % len(words)]} {words[(i + 3) % len(words)]}",
            description=text if i % 3 else "",
        )
        site[f"d-{i}"].setSubject((words[i % 4],))
        site[f"d-{i}"].reindexObject()
    transaction.commit()
    catalog = site.portal_catalog
    queries = [
        {"SearchableText": "kalo"},
        {"SearchableText": "kalo mite"},
        {"SearchableText": "kalo OR ruso"},
        {"SearchableText": "kalo AND NOT vena"},
        {"SearchableText": '"mite ruso"'},
        {"SearchableText": "ka*"},
        {"SearchableText": "cafe"},
        {"SearchableText": "日本"},
        {"SearchableText": marker},
        {"SearchableText": "kalo", "b_start": 1, "b_size": 3},
        {"Title": "vena"},
        {"Title": "d?bu"},
        {"Description": "mite"},
        {"SearchableText": "kalo", "portal_type": "Document"},
    ]
    for query in queries:
        catalog.unrestrictedSearchResults(**query)
    # A search in a transaction that changed the catalog.
    site["d-0"].setTitle("ruso vena")
    site["d-0"].reindexObject()
    catalog.unrestrictedSearchResults(SearchableText="ruso")
    transaction.abort()
    print(json.dumps({"searches": len(queries) + 1}))
    """
)


@pytest.fixture(scope="module")
def recorded(tmp_path_factory):
    """The directory of the log and of the texts of a small site."""
    home = tmp_path_factory.mktemp("site")
    (home / "var" / "cache").mkdir(parents=True)
    store = home / "store"
    conf = home / "zope.conf"
    conf.write_text(
        text_replay.zope_conf(home, f"    <zoe>\n      path {store}\n    </zoe>")
    )
    script = home / "site.py"
    script.write_text(SITE_SCRIPT)
    directory = home / "catalog-text"
    result = subprocess.run(
        [
            str(INTERPRETER),
            str(stack.INZOPE / "run.py"),
            str(conf),
            str(script),
            str(stack.PROBE),
            str(directory),
            MARKER,
        ],
        env=stack.clean_environment(),
        cwd=home,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr[-5000:]
    return directory


@pytest.fixture(scope="module")
def replayed(recorded, tmp_path_factory):
    """The report of the replay of the log, in both legs."""
    settings = text_replay.Settings(
        window_tids=WINDOW,
        window_seconds=30,
        writer_interval=0.01,
        pinned_every=4,
        pinned_sync=3,
        staged_every=3,
        rounds=6,
    )
    report = text_replay.replay_text(
        [recorded], settings, tmp_path_factory.mktemp("replay")
    )
    return report, text_replay.format_report(report)


def test_the_suites_record_text_searches_and_texts(recorded):
    lines = [
        json.loads(line)
        for path in recorded.glob("catalog-*.jsonl")
        for line in path.read_text().splitlines()
    ]
    found = text_replay.searches(lines)
    assert len(found) >= 15
    assert {s["index"] for s in found} == {"SearchableText", "Title", "Description"}
    assert any(s["changed"] for s in found)
    texts = text_replay.texts([recorded])
    assert all(texts[name] for name in text_replay.INDEXES)
    assert any(MARKER in t for t in texts["SearchableText"])


@pytest.mark.parametrize("leg", text_replay.LEGS)
def test_the_replay_with_commits_finds_no_mismatch(replayed, leg):
    report, _ = replayed
    found = report["legs"][leg]
    assert text_replay.problems(found) == []
    assert found["mismatches"] == 0, found["mismatched"]
    assert found["errors"] == {}
    for mode in text_replay.MODES:
        for path in text_replay.PATHS:
            assert found["by_mode_path"].get(f"{mode} {path}", 0) > 0, (mode, path)
    assert found["compared"] == 2 * found["searches"]
    # The hook answered natively, and the pinned searches outside the
    # mirror window took the Python path.
    assert found["queries"]["native"] > 0
    reasons = {(reason, side): n for reason, side, n in found["fallbacks"]}
    assert reasons.get(("outside_window", "server"), 0) >= 1
    assert found["writer"]["commits"] > WINDOW
    assert found["writer"]["errors"] == 0


def test_an_unsupported_search_or_a_failed_change_fails_a_leg(replayed):
    report, _ = replayed
    leg = json.loads(json.dumps(report["legs"]["inprocess"]))
    leg["fallbacks"].append(["unsupported", "client", 1])
    assert text_replay.problems(leg)
    leg = json.loads(json.dumps(report["legs"]["inprocess"]))
    leg["change_errors"] = 1
    assert text_replay.problems(leg)


def test_the_report_holds_no_text_of_a_query(replayed):
    report, text = replayed
    on_disk = (Path(report["directory"]) / "report.json").read_text()
    for output in (text, json.dumps(report), on_disk):
        assert MARKER not in output
        assert "kalo" not in output
