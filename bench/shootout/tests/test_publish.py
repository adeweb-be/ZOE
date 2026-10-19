"""Acceptance test of `just bench publish` and of the index of version 2.

ADR 0054 gives every family of results one layout and one index in a
checkout of the results repository:

    index.json
    results/shootout/<machine>/<python>/<storage>/<started>-<commit>.json
    results/native/<machine>/<python>/<started>-<commit>.json
    results/client/<machine>/<python>/<started>-<commit>.json
    results/plone/<machine>/<python>/<fixture>/<started>-<commit>.json

`python -m shootout publish [--main REF] SRC... CHECKOUT`, which
`just bench publish` runs:

- knows the family of each document and validates it against the schema
  of its family;
- refuses a quick run, a run of a dirty tree, a run with no record of its
  tree, a commit that is not on REF, and a B5 summary with a run whose
  verification did not pass. It then prints one line for each reason,
  which names the document, exits with 1 and changes nothing in CHECKOUT;
- else copies the bytes of each document to its path in the layout and
  rewrites `index.json` of version 2 from the whole tree, oldest first;
- exits with 2 when CHECKOUT is not a directory, with a message that
  names it, and creates nothing.

`python -m shootout index DIR`, which `just bench index` runs, writes the
index of version 2 from a tree of either version. A result in the layout
of version 1 (`results/<machine>/<python>/<storage>/...`) is a document of
the family `shootout`, and it keeps its path.

The sample documents are in `tests/data/publish`: one of each family,
valid against its schema, with numbers about a synthetic corpus and no
host name, user name or path. Their commit is the first commit of the
repository, which every branch holds, so the tests give `--main HEAD`.
The test runs no benchmark.
"""

import json
import pathlib
import subprocess
import sys

import jsonschema
import pytest

BENCH = pathlib.Path(__file__).parents[2]
SCHEMAS = BENCH / "schema"
DATA = pathlib.Path(__file__).parent / "data" / "publish"
FAMILIES = ("shootout", "native", "client", "plone")
SCHEMA_OF = {
    "shootout": "results.schema.json",
    "native": "native.schema.json",
    "client": "client.schema.json",
    "plone": "plone.schema.json",
}
COMMIT = "d939b3f580637ba1f457c9041a12b2f26b0a5273"
MAIN = ("--main", "HEAD")

# The entry of index.json of version 2 for each sample document, in the
# order of the index: oldest first.
EXPECTED = {
    "shootout": {
        "family": "shootout",
        "path": "results/shootout/reference/cp313/zeo/"
        "20260101T000000Z-d939b3f58063.json",
        "machine": "reference",
        "python": "cp313",
        "stacks": ["zeo"],
        "fixture": None,
        "scenarios": ["add", "read_cold"],
        "started": "2026-01-01T00:00:00Z",
        "commit": COMMIT,
    },
    "native": {
        "family": "native",
        "path": "results/native/reference/cp314t/20260101T010000Z-d939b3f58063.json",
        "machine": "reference",
        "python": "cp314t",
        "stacks": ["zeo", "zoe-native"],
        "fixture": None,
        "scenarios": ["add", "read_hot"],
        "started": "2026-01-01T01:00:00Z",
        "commit": COMMIT,
    },
    "client": {
        "family": "client",
        "path": "results/client/reference/cp313/20260101T020000Z-d939b3f58063.json",
        "machine": "reference",
        "python": "cp313",
        "stacks": ["ZEO.ClientStorage", "zoe.zeo.ClientStorage"],
        "fixture": None,
        "scenarios": ["cold", "commit"],
        "started": "2026-01-01T02:00:00Z",
        "commit": COMMIT,
    },
    "plone": {
        "family": "plone",
        "path": "results/plone/reference/cp313/small-ba01ff255313/"
        "20260101T030000Z-d939b3f58063.json",
        "machine": "reference",
        "python": "cp313",
        "stacks": ["zeo", "zoe-native-qi-kv"],
        "fixture": "small-ba01ff255313",
        "scenarios": ["edit"],
        "started": "2026-01-01T03:00:00Z",
        "commit": COMMIT,
    },
}

# A result of zodbshootout in the layout of version 1.
V1_PATH = "results/reference/cp313/zeo/20260101T000000Z-d939b3f58063.json"


@pytest.fixture
def checkout(tmp_path):
    """A checkout of the results repository with no result yet: a README
    and an index of version 1."""
    root = tmp_path / "checkout"
    root.mkdir()
    (root / "README.md").write_text("# Benchmark results\n")
    (root / "index.json").write_text(
        json.dumps({"index_version": 1, "results": []}, indent=2) + "\n"
    )
    return root


def sample(family):
    return json.loads((DATA / f"{family}.json").read_text())


def originals():
    return {family: (DATA / f"{family}.json").read_bytes() for family in FAMILIES}


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


def validate(document, family):
    schema_path = SCHEMAS / SCHEMA_OF[family]
    assert schema_path.exists(), f"{schema_path} is missing"
    jsonschema.validate(document, json.loads(schema_path.read_text()))


def write_v1_result(root):
    """Write a result of zodbshootout of today, without a record of its
    tree, in the layout of version 1, and the index of version 1."""
    document = sample("shootout")
    del document["run"]["dirty"]
    path = root / V1_PATH
    path.parent.mkdir(parents=True)
    path.write_text(json.dumps(document, indent=2) + "\n")
    entry = {
        "path": V1_PATH,
        "machine": "reference",
        "python": "cp313",
        "storage": "zeo",
        "started": "2026-01-01T00:00:00Z",
        "commit": COMMIT,
    }
    (root / "index.json").write_text(
        json.dumps({"index_version": 1, "results": [entry]}, indent=2) + "\n"
    )


def test_a_publish_of_each_family_fills_the_layout_and_the_index(checkout):
    before = originals()

    done = publish(DATA, checkout)

    assert done.returncode == 0, output(done)
    assert documents(checkout) == sorted(e["path"] for e in EXPECTED.values())
    for family, entry in EXPECTED.items():
        path = checkout / entry["path"]
        # publish copies the bytes of the document.
        assert path.read_bytes() == before[family], family
        validate(json.loads(path.read_text()), family)
    index = index_of(checkout)
    assert index["index_version"] == 2
    assert index["results"] == [EXPECTED[family] for family in FAMILIES]
    # publish changes nothing outside CHECKOUT.
    assert originals() == before


def test_publish_takes_files_and_adds_to_the_index(checkout):
    first = publish(DATA / "shootout.json", checkout)
    assert first.returncode == 0, output(first)
    assert index_of(checkout)["results"] == [EXPECTED["shootout"]]

    rest = [DATA / f"{family}.json" for family in FAMILIES[1:]]
    second = publish(*rest, checkout)
    assert second.returncode == 0, output(second)
    assert index_of(checkout)["results"] == [EXPECTED[family] for family in FAMILIES]

    # A document that is already in the checkout changes nothing.
    state = snapshot(checkout)
    again = publish(DATA / "plone.json", checkout)
    assert again.returncode == 0, output(again)
    assert snapshot(checkout) == state


def quick(document):
    document["run"]["quick"] = True
    document["run"]["set"] = "quick"


def dirty(document):
    document["run"]["dirty"] = True


def unrecorded(document):
    del document["run"]["dirty"]


def failed(document):
    document["runs"][1]["verification"] = "fail"
    document["runs"][1]["violations"] = 2


def off_main(document):
    document["run"]["commit"] = "f" * 40


# A family, a change that makes its sample not publishable, and the words of
# the reason.
REFUSALS = {
    "quick": ("shootout", quick, ("a quick run",)),
    "dirty-shootout": ("shootout", dirty, ("a run of a dirty tree",)),
    "dirty-native": ("native", dirty, ("a run of a dirty tree",)),
    "dirty-client": ("client", dirty, ("a run of a dirty tree",)),
    "dirty-plone": ("plone", dirty, ("a run of a dirty tree",)),
    "unrecorded-tree": ("shootout", unrecorded, ("no record of a clean tree",)),
    "failed-verification": (
        "plone",
        failed,
        (
            "the verification of run",
            "20260101T030500Z-edit-zoe-native-qi-kv-r0",
            "fail",
        ),
    ),
    "not-on-main": ("client", off_main, ("commit ffffffffffff is not on HEAD",)),
}


@pytest.mark.parametrize("case", sorted(REFUSALS))
def test_a_document_that_is_not_publishable_is_refused(case, checkout, tmp_path):
    family, change, words = REFUSALS[case]
    document = sample(family)
    change(document)
    # Another start, so the document does not go to the path of a sample.
    document["run"]["started"] = "2026-02-01T00:00:00Z"
    source = tmp_path / "source"
    source.mkdir()
    # The name holds no name of a sample, such as native.json.
    refused = source / f"{case}-refused.json"
    refused.write_text(json.dumps(document, indent=2) + "\n")
    before = snapshot(checkout)

    # The samples are publishable, and they go with the refused document.
    done = publish(DATA, refused, checkout)

    assert done.returncode == 1, output(done)
    lines = [line for line in done.stderr.splitlines() if refused.name in line]
    assert any(all(word in line for word in words) for line in lines), output(done)
    for name in FAMILIES:
        assert f"{name}.json" not in done.stderr, output(done)
    assert snapshot(checkout) == before


def test_publish_into_a_checkout_that_does_not_exist_is_a_usage_error(tmp_path):
    missing = tmp_path / "missing"

    done = publish(DATA, missing)

    assert done.returncode == 2, output(done)
    assert any(
        str(missing) in line and "not a directory" in line
        for line in done.stderr.splitlines()
    ), output(done)
    assert not missing.exists()


def test_index_reads_a_tree_of_version_1(checkout):
    write_v1_result(checkout)

    done = shootout("index", checkout)

    assert done.returncode == 0, output(done)
    assert index_of(checkout) == {
        "index_version": 2,
        "results": [{**EXPECTED["shootout"], "path": V1_PATH}],
    }


def test_publish_keeps_the_results_of_version_1(checkout):
    write_v1_result(checkout)
    old = (checkout / V1_PATH).read_bytes()

    done = publish(DATA, checkout)

    assert done.returncode == 0, output(done)
    assert (checkout / V1_PATH).read_bytes() == old
    assert index_of(checkout)["results"] == [
        {**EXPECTED["shootout"], "path": V1_PATH},
        *(EXPECTED[family] for family in FAMILIES),
    ]
