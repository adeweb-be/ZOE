"""Unit tests of `publish` and of the layout: the reasons and the usage
errors that the acceptance test of `just bench publish` does not reach.

Each test publishes into a checkout under tmp_path. The test runs no
benchmark.
"""

import json
import pathlib

import pytest

from shootout import layout, publish

DATA = pathlib.Path(__file__).parent / "data" / "publish"


@pytest.fixture
def checkout(tmp_path):
    root = tmp_path / "checkout"
    root.mkdir()
    return root


def sample(family):
    return json.loads((DATA / f"{family}.json").read_text())


def write(path, document):
    path.write_text(json.dumps(document, indent=2) + "\n")
    return str(path)


def run(capsys, *sources, checkout, main="HEAD"):
    code = publish.publish([str(s) for s in sources], str(checkout), main)
    return code, capsys.readouterr()


def test_a_file_that_is_not_json_is_refused(tmp_path, checkout, capsys):
    source = tmp_path / "broken.json"
    source.write_text("{")

    code, out = run(capsys, source, checkout=checkout)

    assert code == 1
    assert f"publish: {source}: is not a JSON document" in out.err
    assert "publish: refused 1 of 1 documents" in out.err
    assert list(checkout.iterdir()) == []


@pytest.mark.parametrize(
    "document",
    [{"run": {}}, {**sample("shootout"), "rows": []}, ["a", "list"]],
    ids=["no-key", "two-keys", "a-list"],
)
def test_a_document_of_no_known_family_is_refused(document, tmp_path, checkout, capsys):
    source = write(tmp_path / "other.json", document)

    code, out = run(capsys, source, checkout=checkout)

    assert code == 1
    assert f"publish: {source}: not a document of a known family" in out.err


def test_a_document_that_is_not_valid_names_its_schema(tmp_path, checkout, capsys):
    document = sample("native")
    del document["rule"]
    source = write(tmp_path / "invalid.json", document)

    code, out = run(capsys, source, checkout=checkout)

    assert code == 1
    assert f"publish: {source}: not valid against native.schema.json: " in out.err
    assert "'rule' is a required property" in out.err


def test_a_document_without_a_commit_is_refused(tmp_path, checkout, capsys):
    document = sample("shootout")
    document["run"]["commit"] = None
    source = write(tmp_path / "no-commit.json", document)

    code, out = run(capsys, source, checkout=checkout)

    assert code == 1
    assert f"publish: {source}: no commit" in out.err


def test_each_reason_of_a_document_has_its_line(tmp_path, checkout, capsys):
    document = sample("shootout")
    document["run"]["quick"] = True
    document["run"]["dirty"] = None
    source = write(tmp_path / "two-reasons.json", document)

    code, out = run(capsys, source, checkout=checkout)

    assert code == 1
    assert f"publish: {source}: a quick run" in out.err
    assert f"publish: {source}: no record of a clean tree" in out.err


def test_a_path_with_other_content_is_refused(tmp_path, checkout, capsys):
    target = checkout / layout.published_path(sample("client"))
    target.parent.mkdir(parents=True)
    target.write_text("{}\n")

    code, out = run(capsys, DATA / "client.json", checkout=checkout)

    assert code == 1
    assert f"exists in {checkout} with other content" in out.err
    assert target.read_text() == "{}\n"
    assert not (checkout / "index.json").exists()


def test_two_documents_for_one_path_are_refused(tmp_path, checkout, capsys):
    document = sample("native")
    document["run"]["seconds"] = 1.0
    source = write(tmp_path / "same-path.json", document)

    code, out = run(capsys, DATA / "native.json", source, checkout=checkout)

    assert code == 1
    assert f"publish: {source}: another document goes to results/native/" in out.err
    assert list(checkout.iterdir()) == []


def test_the_same_document_twice_is_published_once(checkout, capsys):
    code, out = run(capsys, DATA / "native.json", DATA, checkout=checkout)

    assert code == 0, out.err
    index = json.loads((checkout / "index.json").read_text())
    assert [entry["family"] for entry in index["results"]] == [
        "shootout",
        "native",
        "client",
        "plone",
    ]


def test_a_part_of_the_path_that_is_not_a_name_is_refused(tmp_path, checkout, capsys):
    # The schema of zodbshootout takes any name of an implementation.
    document = sample("shootout")
    document["python"]["implementation"] = "../up"
    source = tmp_path / "up.json"
    write(source, document)

    assert layout.published_path(document) is None
    code, out = run(capsys, source, checkout=checkout)

    assert code == 1
    assert "its path in the layout holds a part that is not a name" in out.err
    assert list(checkout.iterdir()) == []


@pytest.mark.parametrize(
    ("make", "words"),
    [
        (lambda tmp: tmp / "missing.json", "does not exist"),
        (lambda tmp: (tmp / "empty").mkdir() or tmp / "empty", "holds no document"),
    ],
    ids=["missing", "empty"],
)
def test_a_source_without_documents_is_a_usage_error(
    make, words, tmp_path, checkout, capsys
):
    source = make(tmp_path)

    code, out = run(capsys, DATA, source, checkout=checkout)

    assert code == 2
    assert f"publish: {source} {words}" in out.err
    assert list(checkout.iterdir()) == []


def test_a_branch_that_git_cannot_read_is_a_usage_error(checkout, capsys):
    code, out = run(capsys, DATA, checkout=checkout, main="no-such-branch")

    assert code == 2
    assert "publish: no-such-branch is not a commit of the ZOE repository" in out.err
    assert list(checkout.iterdir()) == []


def test_index_refuses_a_document_of_another_family(checkout):
    path = checkout / "results" / "native" / "reference" / "cp313" / "x.json"
    path.parent.mkdir(parents=True)
    write(path, sample("client"))

    with pytest.raises(ValueError, match="not a document of the family native"):
        layout.write_index(checkout)


def test_index_of_the_tree_of_the_harness_leaves_quick_runs_out(checkout):
    full = sample("shootout")
    quick = sample("shootout")
    quick["run"].update(quick=True, set="quick")
    for document in (full, quick):
        path = layout.result_path(checkout, document)
        path.parent.mkdir(parents=True, exist_ok=True)
        write(path, document)

    layout.write_index(checkout)

    index = json.loads((checkout / "index.json").read_text())
    assert [entry["path"] for entry in index["results"]] == [
        "results/reference/cp313/zeo/20260101T000000Z-d939b3f58063.json"
    ]
