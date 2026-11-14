"""Acceptance test of the family `catalog` of the results repository.

ADR 0054, the amendment "the family catalog", adds a fifth family for the
results of `zoe-bench catalog`, which `just bench catalog` writes. Its
documents are valid against `bench/schema/catalog.schema.json`, and the
key `catalog` at their top level names the family. Its layout is:

    results/catalog/<machine>/<fixture>/<started>-<commit>.json

with no <python> part. Its entry in `index.json` of version 2 has
`python` null, `stacks` `["zoe"]` and `fixture` the name of the fixture.

`python -m shootout publish`, which `just bench publish` runs, takes a
catalog document as it takes the others, with one more rule: the document
is of a full run, on the preset `catalog-1m`, with each scenario of the
default set (`intersect`, `text`, `mix` and `cold`). It may also hold
`text-rebuild`. The replies `f` and `e` that a run counts do not stop a
publish. A refused document prints one line for each reason, which names
the document, and the checkout does not change.

The sample is `tests/data/catalog/catalog.json`: a full run, valid
against the schema, with numbers about a synthetic corpus and no host
name, user name, path or address. It is not in `tests/data/publish`,
which holds one document of each of the four other families. Its commit
is the first commit of the repository, which every branch holds, so the
tests give `--main HEAD`. The test runs no benchmark.
"""

import json
import pathlib
import subprocess
import sys

import jsonschema
import pytest

BENCH = pathlib.Path(__file__).parents[2]
SCHEMA = BENCH / "schema" / "catalog.schema.json"
DATA = pathlib.Path(__file__).parent / "data"
SAMPLE = DATA / "catalog" / "catalog.json"
# One document of each of the four other families.
OTHERS = DATA / "publish"
COMMIT = "d939b3f580637ba1f457c9041a12b2f26b0a5273"
MAIN = ("--main", "HEAD")
PATH = (
    "results/catalog/reference/catalog-1m-0c51e2a4f7d9/"
    "20260101T040000Z-d939b3f58063.json"
)
EXPECTED = {
    "family": "catalog",
    "path": PATH,
    "machine": "reference",
    "python": None,
    "stacks": ["zoe"],
    "fixture": "catalog-1m-0c51e2a4f7d9",
    "scenarios": ["cold", "intersect", "mix", "text"],
    "started": "2026-01-01T04:00:00Z",
    "commit": COMMIT,
}


@pytest.fixture
def checkout(tmp_path):
    """A checkout of the results repository with no result yet."""
    root = tmp_path / "checkout"
    root.mkdir()
    (root / "README.md").write_text("# Benchmark results\n")
    (root / "index.json").write_text(
        json.dumps({"index_version": 1, "results": []}, indent=2) + "\n"
    )
    return root


def sample():
    return json.loads(SAMPLE.read_text())


def shootout(*args):
    return subprocess.run(
        [sys.executable, "-m", "shootout", *map(str, args)],
        capture_output=True,
        text=True,
        check=False,
    )


def publish(*args):
    return shootout("publish", *MAIN, *args)


def output(done):
    return done.stdout + done.stderr


def snapshot(root):
    """Every path under root, with the bytes of each file."""
    return {
        path.relative_to(root).as_posix(): path.read_bytes() if path.is_file() else None
        for path in sorted(root.rglob("*"))
    }


def documents(root):
    return sorted(
        path.relative_to(root).as_posix()
        for path in (root / "results").rglob("*")
        if path.is_file()
    )


def index_of(root):
    return json.loads((root / "index.json").read_text())


def write(directory, name, document):
    path = directory / name
    path.write_text(json.dumps(document, indent=2) + "\n")
    return path


def test_the_sample_is_valid_and_counts_refusals_and_errors():
    document = sample()
    jsonschema.validate(document, json.loads(SCHEMA.read_text()))
    assert "catalog" in document
    for key in ("storage", "cells", "rows", "fixture"):
        assert key not in document, key
    # The replies f and e of the run are numbers of the document.
    cells = [
        cell for scenario in document["scenarios"] for cell in scenario.get("cells", [])
    ]
    assert any(cell["refusals"] for cell in cells)
    assert any(scenario.get("refusals") for scenario in document["scenarios"])


def test_a_catalog_run_publishes_at_its_path_and_in_the_index(checkout):
    before = SAMPLE.read_bytes()

    done = publish(SAMPLE, checkout)

    assert done.returncode == 0, output(done)
    assert documents(checkout) == [PATH]
    assert (checkout / PATH).read_bytes() == before
    index = index_of(checkout)
    assert index == {"index_version": 2, "results": [EXPECTED]}
    # `just bench index` gives the same index from the tree.
    again = shootout("index", checkout)
    assert again.returncode == 0, output(again)
    assert index_of(checkout) == index
    assert SAMPLE.read_bytes() == before


def test_a_run_with_text_rebuild_beside_the_default_set_publishes(checkout, tmp_path):
    document = sample()
    document["run"]["started"] = "2026-02-01T00:00:00Z"
    rebuild = {
        "name": "text-rebuild",
        "mode": "enhanced",
        "rebuild_seconds": 512.4,
        "peak_rss_at_rebuild_bytes": 2885681152,
        "mirror_ready_seconds": 40.9,
        "rss_bytes": 1811939328,
        "peak_rss_bytes": 2885681152,
        "mirror_bytes": 1409286144.0,
        "build_seconds": 40.9,
    }
    document["scenarios"].append(rebuild)
    source = tmp_path / "source"
    source.mkdir()
    path = write(source, "with-rebuild.json", document)

    done = publish(path, checkout)

    assert done.returncode == 0, output(done)
    [entry] = index_of(checkout)["results"]
    assert entry["scenarios"] == ["cold", "intersect", "mix", "text", "text-rebuild"]
    assert entry["path"].startswith("results/catalog/reference/catalog-1m-")


def dirty(document):
    document["run"]["dirty"] = True


def unrecorded(document):
    document["run"]["dirty"] = None


def off_main(document):
    document["run"]["commit"] = "f" * 40


def only(*names):
    def change(document):
        document["scenarios"] = [
            scenario for scenario in document["scenarios"] if scenario["name"] in names
        ]

    return change


def text_rebuild_alone(document):
    document["scenarios"] = [
        {
            "name": "text-rebuild",
            "mode": "enhanced",
            "rebuild_seconds": 512.4,
            "peak_rss_at_rebuild_bytes": 2885681152,
            "mirror_ready_seconds": 40.9,
            "rss_bytes": 1811939328,
            "peak_rss_bytes": 2885681152,
            "mirror_bytes": 1409286144.0,
            "build_seconds": 40.9,
        }
    ]


def mini(document):
    document["catalog"]["preset"] = "catalog-mini"
    document["catalog"]["name"] = "catalog-mini-5e0b7a913c22"


def invalid(document):
    del document["server"]


def with_a_path(document):
    document["server"]["store"] = "/srv/zoe/store"


# A change that makes the sample not publishable, and the words of the
# reason.
REFUSALS = {
    "dirty": (dirty, ("a run of a dirty tree",)),
    "unrecorded-tree": (unrecorded, ("no record of a clean tree",)),
    "not-on-main": (off_main, ("commit ffffffffffff is not on HEAD",)),
    "one-scenario": (only("text"), ("a partial run", "intersect", "mix", "cold")),
    "three-scenarios": (only("cold", "intersect", "mix"), ("a partial run", "text")),
    "text-rebuild-alone": (text_rebuild_alone, ("a partial run", "intersect")),
    "catalog-mini": (mini, ("a partial run", "catalog-mini", "catalog-1m")),
    "schema": (invalid, ("not valid against catalog.schema.json", "server")),
    "path-in-the-document": (with_a_path, ("not valid against catalog.schema.json",)),
}


@pytest.mark.parametrize("case", sorted(REFUSALS))
def test_a_catalog_run_that_is_not_publishable_is_refused(case, checkout, tmp_path):
    change, words = REFUSALS[case]
    document = sample()
    change(document)
    # Another start, so the document does not go to the path of the sample.
    document["run"]["started"] = "2026-02-01T00:00:00Z"
    source = tmp_path / "source"
    source.mkdir()
    refused = write(source, f"{case}-refused.json", document)
    before = snapshot(checkout)

    # The sample is publishable, and it goes with the refused document.
    done = publish(SAMPLE, refused, checkout)

    assert done.returncode == 1, output(done)
    lines = [line for line in done.stderr.splitlines() if refused.name in line]
    assert any(all(word in line for word in words) for line in lines), output(done)
    assert not any(str(SAMPLE) in line for line in done.stderr.splitlines()), output(
        done
    )
    assert snapshot(checkout) == before


def test_the_four_other_families_publish_as_before(tmp_path):
    roots = {}
    for name, sources in (
        ("without", (OTHERS,)),
        ("with", (OTHERS, SAMPLE)),
    ):
        root = tmp_path / name
        root.mkdir()
        done = publish(*sources, root)
        assert done.returncode == 0, output(done)
        roots[name] = root

    without = index_of(roots["without"])["results"]
    with_catalog = index_of(roots["with"])["results"]
    assert {entry["family"] for entry in without} == {
        "shootout",
        "native",
        "client",
        "plone",
    }
    assert [e for e in with_catalog if e["family"] != "catalog"] == without
    assert [e for e in with_catalog if e["family"] == "catalog"] == [EXPECTED]
    for path in documents(roots["without"]):
        assert (roots["with"] / path).read_bytes() == (
            roots["without"] / path
        ).read_bytes()
    assert set(documents(roots["with"])) == {*documents(roots["without"]), PATH}
