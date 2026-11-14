"""Acceptance test of the plans of `zoe-bench catalog`: the plans that the
harness makes from a log of `catalog-mini` replay through `zoe serve` to
the rids and the counts of the Python path on the same store.

A Zope process records a log of searches on a copy of `catalog-mini` with
the probe of B5: the intersections of its query set, its text terms, path
queries, a batch, and searches that the client sends to the Python path.
`plonebench.catalog.plans` makes the plans file from that log, with the
answer of the stock search of each plan (`--expect`). `zoe-bench catalog
--check` starts `zoe serve` on the store, sends each plan with
`catalog_query` at the last commit, and compares the rids, `len()` and
`actual_result_count` of each reply with the answer of the stock search.

The test uses the fixture `catalog-mini` of `just bench catalog prepare
--preset catalog-mini` when it exists, else it builds one. It needs `just
plone zeo build`, `just plone zoeclient build`, and `zoe` and `zoe-bench`
in release (`cargo build --release -p zoe-server --bin zoe -p zoe-bench
--bin zoe-bench`, or `ZOE_BINARY` and `ZOE_BENCH_BINARY`), and takes some
minutes:

    just bench plone test bench/plone/tests/test_catalog_plans.py
"""

import json
import shutil
import subprocess
import textwrap

import pytest
from benchcommon import servers

from plonebench import stack
from plonebench.catalog import fixture, plan, plans

INTERPRETER = stack.interpreter(stack.STACKS["zoe-native"])
PRESET = "catalog-mini"

pytestmark = pytest.mark.skipif(
    not INTERPRETER.exists()
    or not stack.interpreter(stack.STACKS["zeo"]).exists()
    or not servers.zoe_path().is_file()
    or not plans.zoe_bench_binary().is_file(),
    reason="needs `just plone zeo build`, `just plone zoeclient build`, and zoe"
    " and zoe-bench in release",
)

# The searches of the log, after the query set of the fixture.
LOG_SCRIPT = textwrap.dedent(
    """
    import json
    import sys

    from AccessControl.SecurityManagement import newSecurityManager
    from AccessControl.SpecialUsers import system
    from DateTime import DateTime
    from zope.component.hooks import setSite

    probe, out, queries = sys.argv[1:]
    sys.path.insert(0, probe)
    import plonebench_catalog

    newSecurityManager(None, system)
    site = app["Plone"]
    setSite(site)
    catalog = site.portal_catalog
    plonebench_catalog.install(out)
    searches = 0
    for query in json.loads(open(queries).read()):
        if "start" in query:
            # DateRecurringIndex takes a DateTime, not a str.
            query["start"]["query"] = DateTime(query["start"]["query"])
        catalog.unrestrictedSearchResults(**query)
        searches += 1
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


def log_queries(manifest):
    """The searches of the log: the query set of the fixture, then other
    shapes of Plone, some of which the client sends to the Python path."""
    entries = plan.Entries(plan.CatalogSpec.from_json(manifest["spec"]))
    now = plan.iso(entries.now)
    out = [q["query"] for q in manifest["queries"]]
    unit = f"/{plan.SITE_ID}/section-000"
    out += [
        {"path": {"query": unit, "depth": 1}, "sort_on": "sortable_title"},
        {"path": {"query": unit, "depth": -1}, "portal_type": "Document"},
        {
            "portal_type": "News Item",
            "review_state": plan.PUBLISHED,
            "effectiveRange": now,
            "sort_on": "effective",
            "sort_order": "descending",
            "b_start": 5,
            "b_size": 10,
        },
        {"Subject": entries.words[60], "sort_on": "modified", "sort_limit": 5},
        {"SearchableText": entries.words[10], "portal_type": "Document"},
        # DateRecurringIndex: the client sends it to the Python path.
        {"start": {"query": now, "range": "min"}},
    ]
    return out


@pytest.fixture(scope="module")
def mini(tmp_path_factory):
    """A copy of the fixture `catalog-mini` without plans, and its log."""
    spec = plan.PRESETS[PRESET]
    existing = fixture.fixture_dir(fixture.default_name(spec, PRESET))
    target = tmp_path_factory.mktemp("fixture") / PRESET
    if (existing / "manifest.json").exists():
        shutil.copytree(
            existing,
            target,
            ignore=shutil.ignore_patterns("plans", plans.PLANS, "zoe.text-*"),
        )
    else:
        fixture.prepare(spec, target, PRESET)
    manifest = json.loads((target / "manifest.json").read_text())
    home = tmp_path_factory.mktemp("log")
    (home / "var" / "cache").mkdir(parents=True)
    store = home / "store"
    stack.copy_store(target / "store" / "zoe", store)
    conf = home / "zope.conf"
    conf.write_text(zope_conf(home, store))
    script = home / "log.py"
    script.write_text(LOG_SCRIPT)
    queries = home / "queries.json"
    queries.write_text(json.dumps(log_queries(manifest)))
    out = home / "catalog-mini.jsonl"
    result = subprocess.run(
        [
            str(INTERPRETER),
            str(stack.INZOPE / "run.py"),
            str(conf),
            str(script),
            str(stack.PROBE),
            str(out),
            str(queries),
        ],
        env=stack.clean_environment(),
        cwd=home,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr[-5000:]
    return target, out


@pytest.fixture(scope="module")
def checked(mini):
    """The plans file of the log, and the outcome of `zoe-bench catalog
    --check` on the store of the fixture."""
    target, log = mini
    made = plans.make(target, [str(log)], expect=True)
    result = subprocess.run(
        [
            str(plans.zoe_bench_binary()),
            "catalog",
            "--zoe",
            str(servers.zoe_binary()),
            "--store",
            str(target / "store" / "zoe"),
            "--plans",
            str(made),
            "--check",
        ],
        env=stack.clean_environment(),
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.stdout, result.stderr[-5000:]
    return made, json.loads(result.stdout), result.returncode


def test_the_plans_replay_to_the_answers_of_the_python_path(checked):
    _, report, code = checked
    assert report["mismatches"] == [], report["mismatches"]
    assert report["errors"] == {}
    assert report["checked"] > 0
    assert report["matched"] == report["checked"] - sum(report["refusals"].values())
    assert code == 0


def test_the_plans_cover_each_scenario(checked):
    made, report, _ = checked
    assert made.name == plans.PLANS
    for scenario in ("intersect", "text", "filter", "path", "mix"):
        assert report["scenarios"].get(scenario, 0) > 0, report["scenarios"]
    # The text terms of the query set but the band rare, in five variants.
    manifest = json.loads((made.parent / "manifest.json").read_text())
    terms = [
        q for q in manifest["queries"] if q["kind"] == "text" and q["band"] != "rare"
    ]
    assert terms
    assert report["scenarios"]["text"] == 5 * len(terms)


def test_a_line_that_takes_the_python_path_keeps_its_reason(checked):
    _, report, _ = checked
    assert report["client_reasons"].get("index", 0) >= 1, report["client_reasons"]


def test_the_store_keeps_the_text_index_of_each_mode(checked):
    made, _, _ = checked
    store = made.parent / "store"
    assert any((store / "zoe" / "text").iterdir())
    assert any((store / "zoe.text-compat").iterdir())
