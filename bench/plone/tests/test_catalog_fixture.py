"""End-to-end tests of the build of the catalog fixture, on demand.

`catalog-mini` builds twice with one seed, each time in a temporary
directory: the site and its catalog of 10,000 entries in a FileStorage, the
pack, `zoe convert`, and the digest of the catalog through each store.

- The two builds give the same digest.
- The entries of each index and of each column, read through ZCatalog,
  are the counts of the plan; the catalog has the 56 indexes and the 53
  metadata columns of `xlarge`.
- `len(catalog)` is 10,000.
- The ZOE store and the `Data.fs` give the same digest.
- Each query of the query set gives, through ZCatalog, the count of hits
  that the plan gives.

They need `just plone zeo build`, `just plone zoeclient build` and `cargo
build --release -p zoe-server --bin zoe` (or `ZOE_BINARY`), and take a few
minutes:

    ZOE_BENCH_FIXTURE_BUILD=1 just bench plone test \
        bench/plone/tests/test_catalog_fixture.py
"""

import json
import os

import pytest

from plonebench.catalog import fixture, plan

pytestmark = pytest.mark.skipif(
    os.environ.get("ZOE_BENCH_FIXTURE_BUILD") != "1",
    reason="builds the catalog fixture twice: set ZOE_BENCH_FIXTURE_BUILD=1",
)

PRESET = "catalog-mini"
SPEC = plan.PRESETS[PRESET]
KINDS = ("filestorage", "zoe")


@pytest.fixture(scope="module")
def builds(tmp_path_factory):
    """Two builds of `catalog-mini` with one seed, in temporary directories."""
    return [
        fixture.prepare(SPEC, tmp_path_factory.mktemp(f"build-{n}") / PRESET, PRESET)
        for n in range(2)
    ]


@pytest.fixture(scope="module")
def tally():
    """The counts of the plan, computed outside Zope."""
    return plan.tally(SPEC)


def digest(target, kind):
    """The digest of the catalog, read through ZCatalog on the store `kind`."""
    return json.loads((target / f"digest-{kind}.json").read_text())


def manifest(target):
    return json.loads((target / "manifest.json").read_text())


def test_two_builds_with_one_seed_give_the_same_digest(builds):
    first, second = builds
    assert manifest(first)["digest"] == manifest(second)["digest"]
    for kind in KINDS:
        assert digest(first, kind) == digest(second, kind)


def test_each_index_and_each_column_holds_the_entries_of_the_plan(builds, tally):
    counts = tally.counts()
    assert len(counts["indexes"]) == 56
    assert len(counts["columns"]) == 53
    for target in builds:
        assert manifest(target)["counts"] == counts
        for kind in KINDS:
            read = digest(target, kind)
            indexes = {name: item["entries"] for name, item in read["indexes"].items()}
            columns = {name: item["entries"] for name, item in read["columns"].items()}
            assert indexes == counts["indexes"]
            assert columns == counts["columns"]


def test_the_catalog_holds_10000_entries(builds):
    for target in builds:
        for kind in KINDS:
            assert digest(target, kind)["size"] == 10_000


def test_the_zoe_store_and_the_data_fs_give_the_same_digest(builds):
    for target in builds:
        assert digest(target, "zoe") == digest(target, "filestorage")
        assert manifest(target)["equivalent"] is True


def test_each_query_gives_the_count_of_the_plan(builds, tally):
    query_set = tally.query_set()
    expected = {query["name"]: query["count"] for query in query_set}
    kinds = {query["kind"] for query in query_set}
    assert kinds == {"intersection", "text"}
    for target in builds:
        assert manifest(target)["queries"] == query_set
        for kind in KINDS:
            assert digest(target, kind)["queries"] == expected
