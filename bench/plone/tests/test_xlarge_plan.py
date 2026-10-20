"""The preset `xlarge`: a site of about 250,000 items in the shape of a
real Plone site, with the stock types of Plone 6.1. The preset
`xlarge-mini` has the same shape at about 1/100, for the tests: fewer
units, and Folders of depth 3 of the same size.

The shape (a decision of the human):

    depth 1   240 Folders: the units of the site
    depth 2   750 Folders, about 3 in each unit
    depth 3   7,200 Folders, about 30 in each unit; 34,000 News Items,
              in one Folder of depth 2 of each unit
    depth 4   177,000 Documents and 30,500 Files, in the Folders of depth 3

A Folder of depth 3 holds about 25 Documents and about 4 Files. The units
differ in size: the children of a container (the site or a Folder) have a
90th percentile near 32, a 99th percentile near 110 and a maximum near
1,450, and about 4 containers hold more than 1,000 children. Files are
16 KB each. The states are those of the other presets: 20 % private,
10 % pending. Each editor of `xlarge` gets a unit of its own for the edit
pools. The folders of `bench`, which the contention scenarios use, are not
part of the table. `contention-folder` uses one Folder of depth 3.

The plan of `xlarge` takes about a minute and about 1 GB.
"""

import math
from collections import Counter, defaultdict
from dataclasses import dataclass
from statistics import mean

import pytest

from plonebench import corpus, workload

# The counts of the table by type and depth, outside `bench`.
TABLES = {
    "xlarge": {
        ("Folder", 1): 240,
        ("Folder", 2): 750,
        ("Folder", 3): 7_200,
        ("News Item", 3): 34_000,
        ("Document", 4): 177_000,
        ("File", 4): 30_500,
    },
    "xlarge-mini": {
        ("Folder", 1): 8,
        ("Folder", 2): 25,
        ("Folder", 3): 72,
        ("News Item", 3): 340,
        ("Document", 4): 1_770,
        ("File", 4): 305,
    },
}
# The count of all items, and its tolerance.
TOTALS = {"xlarge": (250_000, 0.01), "xlarge-mini": (2_500, 0.05)}
# The sizes of the containers. "Near" is within 10 %.
SIZES = {
    "xlarge": {"p90": 32, "p99": 110, "max": 1_450, "over_1000": (3, 5)},
    "xlarge-mini": {"p90": 32},
}
NEAR = 0.10
# The users of the largest step of the capacity set.
USERS = 64
STOCK_TYPES = {"Folder", "Document", "News Item", "File", "Collection"}


@dataclass
class Site:
    preset: str
    plan: corpus.Plan
    targets: workload.Targets


def depth(path: str) -> int:
    return path.count("/") + 1


def parent(path: str) -> str:
    return path.rpartition("/")[0]


def in_bench(path: str) -> bool:
    return path == corpus.BENCH or path.startswith(corpus.BENCH + "/")


def unit_of(path: str) -> str:
    return path.split("/", 1)[0]


def percentile(values, q: float):
    """The nearest-rank percentile."""
    ordered = sorted(values)
    return ordered[math.ceil(q * len(ordered)) - 1]


def manifest_of(plan: corpus.Plan) -> dict:
    spec = plan.spec
    return {
        "spec": spec.to_json(),
        "site": "Plone",
        "items": [
            {
                "path": item.path,
                "portal_type": item.portal_type,
                "title": item.title,
                "state": item.state
                if item.portal_type in ("Document", "News Item")
                else corpus.PUBLISHED,
                "pool": item.pool,
                "section": item.section,
                "size": item.size,
            }
            for item in plan.items
        ],
        "users": corpus.user_ids(spec),
    }


@pytest.fixture(scope="module", params=sorted(TABLES))
def site(request) -> Site:
    plan = corpus.plan_site(corpus.PRESETS[request.param])
    return Site(request.param, plan, workload.Targets(manifest_of(plan)))


def units(plan: corpus.Plan) -> list[corpus.Item]:
    return [
        item
        for item in plan.items
        if item.portal_type == "Folder"
        and depth(item.path) == 1
        and not in_bench(item.path)
    ]


def test_the_site_has_its_count_of_items(site):
    total, tolerance = TOTALS[site.preset]
    assert abs(len(site.plan.items) - total) <= tolerance * total


def test_the_site_has_the_counts_of_the_table(site):
    items = [item for item in site.plan.items if not in_bench(item.path)]
    counts = Counter((item.portal_type, depth(item.path)) for item in items)
    table = TABLES[site.preset]
    for key, expected in table.items():
        assert abs(counts[key] - expected) <= 0.02 * expected, (key, counts[key])
    # The types of the table lie only at the depths of the table.
    for portal_type in {name for name, _ in table}:
        total = sum(n for (name, _), n in table.items() if name == portal_type)
        elsewhere = sum(
            n
            for (name, level), n in counts.items()
            if name == portal_type and (name, level) not in table
        )
        assert elsewhere <= 0.02 * total, (portal_type, elsewhere)
    assert {item.portal_type for item in site.plan.items} <= STOCK_TYPES


def test_a_folder_of_depth_3_holds_about_25_documents_and_4_files(site):
    children = defaultdict(Counter)
    for item in site.plan.items:
        children[parent(item.path)][item.portal_type] += 1
    leaves = [
        item.path
        for item in site.plan.items
        if item.portal_type == "Folder"
        and depth(item.path) == 3
        and not in_bench(item.path)
    ]
    documents = [children[path]["Document"] for path in leaves]
    files = [children[path]["File"] for path in leaves]
    assert 22.5 <= mean(documents) <= 27.5
    assert 3.6 <= mean(files) <= 4.4
    assert min(documents) >= 1
    assert all(children[path]["Folder"] == 0 for path in leaves)


def test_the_news_items_of_a_unit_lie_in_one_folder_of_depth_2(site):
    folders = defaultdict(set)
    for item in site.plan.items:
        if item.portal_type == "News Item" and not in_bench(item.path):
            folders[unit_of(item.path)].add(parent(item.path))
    for unit in units(site.plan):
        assert len(folders[unit.path]) == 1, unit.path
        (folder,) = folders[unit.path]
        assert depth(folder) == 2


def test_the_containers_have_the_sizes_of_the_real_site(site):
    containers = {"": 0}
    containers.update(
        (item.path, 0) for item in site.plan.items if item.portal_type == "Folder"
    )
    for item in site.plan.items:
        containers[parent(item.path)] += 1
    sizes = list(containers.values())
    expected = SIZES[site.preset]
    for key, q in (("p90", 0.90), ("p99", 0.99)):
        if key in expected:
            value = percentile(sizes, q)
            assert abs(value - expected[key]) <= NEAR * expected[key], (key, value)
    if "max" in expected:
        assert abs(max(sizes) - expected["max"]) <= NEAR * expected["max"], max(sizes)
    if "over_1000" in expected:
        low, high = expected["over_1000"]
        assert low <= sum(size > 1_000 for size in sizes) <= high


def test_the_units_differ_in_size(site):
    sizes = Counter(
        unit_of(item.path) for item in site.plan.items if not in_bench(item.path)
    )
    values = [sizes[unit.path] for unit in units(site.plan)]
    assert max(values) >= 5 * percentile(values, 0.5)


def test_the_files_are_16_kb(site):
    files = site.plan.by_type("File")
    assert files and all(item.size == 16_384 for item in files)


def test_the_states_and_the_pools_are_those_of_the_other_presets(site):
    spec = site.plan.spec
    assert (spec.private, spec.pending) == (0.2, 0.1)
    documents = [
        item
        for item in site.plan.items
        if item.portal_type == "Document" and not in_bench(item.path)
    ]
    pools = Counter(item.pool for item in documents)
    assert set(pools) == {corpus.BROWSE, corpus.EDIT, corpus.WORKFLOW}
    for pool, share in (
        (corpus.BROWSE, 0.4),
        (corpus.EDIT, 0.4),
        (corpus.WORKFLOW, 0.2),
    ):
        assert abs(pools[pool] / len(documents) - share) <= 0.01, pool
    drawn = [item for item in documents if item.pool != corpus.WORKFLOW]
    for state, share in ((corpus.PRIVATE, 0.2), (corpus.PENDING, 0.1)):
        observed = sum(item.state == state for item in drawn) / len(drawn)
        assert abs(observed - share) <= 4 * math.sqrt(share * (1 - share) / len(drawn))


def test_each_editor_of_xlarge_has_a_unit_of_its_own(site):
    if site.preset != "xlarge":
        pytest.skip("xlarge-mini has fewer units than editors")
    editors = site.targets.manifest["users"][corpus.EDITOR]
    assert len(editors) >= USERS
    sections = [site.targets.editor(rank)[1] for rank in range(len(editors))]
    assert len(set(sections)) == len(editors)
    assert set(sections) <= {unit.section for unit in units(site.plan)}


def test_the_editors_never_share_a_document(site):
    unit_of_section = {unit.section: unit.path for unit in units(site.plan)}
    for pool in (corpus.EDIT, corpus.WORKFLOW):
        owned = []
        for rank in range(USERS):
            _, section = site.targets.editor(rank)
            docs = site.targets.own_docs(rank, pool, USERS)
            assert docs, (pool, rank)
            for doc in docs:
                assert doc["portal_type"] == "Document" and doc["pool"] == pool
                assert unit_of(doc["path"]) == unit_of_section[section]
            owned.append({doc["path"] for doc in docs})
        assert len(set().union(*owned)) == sum(len(paths) for paths in owned), pool


def test_the_contention_targets_exist(site):
    by_path = {item.path: item for item in site.plan.items}
    targets = site.targets
    shared = {targets.shared_doc(rank) for rank in range(USERS)}
    shared |= {targets.pair_doc(rank)[0] for rank in range(USERS)}
    for path in shared:
        assert by_path[path].portal_type == "Document", path
        assert by_path[path].pool == corpus.SHARED, path
    folders = {parent(path) for path in shared}
    # contention-folder edits Documents of one Folder of depth 3.
    assert len(folders) == 1
    (folder,) = folders
    assert by_path[folder].portal_type == "Folder" and depth(folder) == 3
    for rank in range(USERS):
        hot = by_path[targets.hot_doc(rank)]
        assert hot.portal_type == "Document" and hot.pool == corpus.HOT
    for name in (corpus.INBOX, corpus.IMPORT):
        assert by_path[f"{corpus.BENCH}/{name}"].portal_type == "Folder"
    assert targets.items(portal_type="Collection")
    browsable = {item["portal_type"] for item in targets.browsable()}
    assert {"Folder", "Document", "News Item", "File"} <= browsable
