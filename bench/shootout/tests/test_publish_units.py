"""Unit tests of `publish` and of the layout: the reasons and the usage
errors that the acceptance test of `just bench publish` does not reach.

Each test publishes into a checkout under tmp_path. The test runs no
benchmark.
"""

import json
import pathlib
import subprocess

import pytest

from shootout import environment, layout, publish

DATA = pathlib.Path(__file__).parent / "data" / "publish"


def _has_git() -> bool:
    try:
        done = subprocess.run(
            ["git", "rev-parse", "--verify", "--quiet", "HEAD"],
            cwd=publish.REPOSITORY,
            capture_output=True,
            check=False,
        )
    except OSError:
        return False
    return done.returncode == 0


# publish checks the commit of a document with git. A tree with no .git,
# such as the container of the Dagger function bench, cannot.
needs_git = pytest.mark.skipif(not _has_git(), reason="the tree has no git history")


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


@needs_git
def test_a_file_that_is_not_json_is_refused(tmp_path, checkout, capsys):
    source = tmp_path / "broken.json"
    source.write_text("{")

    code, out = run(capsys, source, checkout=checkout)

    assert code == 1
    assert f"publish: {source}: is not a JSON document" in out.err
    assert "publish: refused 1 of 1 documents" in out.err
    assert list(checkout.iterdir()) == []


@needs_git
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


@needs_git
def test_a_document_that_is_not_valid_names_its_schema(tmp_path, checkout, capsys):
    document = sample("native")
    del document["rule"]
    source = write(tmp_path / "invalid.json", document)

    code, out = run(capsys, source, checkout=checkout)

    assert code == 1
    assert f"publish: {source}: not valid against native.schema.json: " in out.err
    assert "'rule' is a required property" in out.err


@needs_git
def test_a_document_without_a_commit_is_refused(tmp_path, checkout, capsys):
    document = sample("shootout")
    document["run"]["commit"] = None
    source = write(tmp_path / "no-commit.json", document)

    code, out = run(capsys, source, checkout=checkout)

    assert code == 1
    assert f"publish: {source}: no commit" in out.err


@needs_git
def test_each_reason_of_a_document_has_its_line(tmp_path, checkout, capsys):
    document = sample("shootout")
    document["run"]["quick"] = True
    document["run"]["dirty"] = None
    source = write(tmp_path / "two-reasons.json", document)

    code, out = run(capsys, source, checkout=checkout)

    assert code == 1
    assert f"publish: {source}: a quick run" in out.err
    assert f"publish: {source}: no record of a clean tree" in out.err


@needs_git
def test_a_path_with_other_content_is_refused(tmp_path, checkout, capsys):
    target = checkout / layout.published_path(sample("client"))
    target.parent.mkdir(parents=True)
    target.write_text("{}\n")

    code, out = run(capsys, DATA / "client.json", checkout=checkout)

    assert code == 1
    assert f"exists in {checkout} with other content" in out.err
    assert target.read_text() == "{}\n"
    assert not (checkout / "index.json").exists()


@needs_git
def test_two_documents_for_one_path_are_refused(tmp_path, checkout, capsys):
    document = sample("native")
    document["run"]["seconds"] = 1.0
    source = write(tmp_path / "same-path.json", document)

    code, out = run(capsys, DATA / "native.json", source, checkout=checkout)

    assert code == 1
    assert f"publish: {source}: another document goes to results/native/" in out.err
    assert list(checkout.iterdir()) == []


@needs_git
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


@needs_git
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


def snapshot(root):
    """Every path under root: the bytes of a file, the target of a link."""
    found = {}
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root).as_posix()
        if path.is_symlink():
            found[relative] = ("link", str(path.readlink()))
        elif path.is_file():
            found[relative] = path.read_bytes()
        else:
            found[relative] = "directory"
    return found


@needs_git
@pytest.mark.parametrize(
    ("place", "words"),
    [
        (
            lambda root: (root / "results" / "notes.json").write_text("{not json"),
            "results/notes.json is not a JSON document",
        ),
        (
            lambda root: write(
                root / "results" / "native" / "client.json", sample("client")
            ),
            "results/native/client.json is not a document of the family native",
        ),
        (
            lambda root: write(root / "results" / "x" / "y" / "z.json", {"run": 1}),
            "results/x/y/z.json is not a document of the family shootout",
        ),
    ],
    ids=["not-json", "other-family", "not-a-document"],
)
def test_a_file_that_the_index_cannot_read_stops_the_publish(
    place, words, checkout, capsys
):
    (checkout / "results" / "native").mkdir(parents=True)
    (checkout / "results" / "x" / "y").mkdir(parents=True)
    place(checkout)
    before = snapshot(checkout)

    code, out = run(capsys, DATA / "shootout.json", checkout=checkout)

    assert code == 1
    assert f"publish: {checkout}: {words}" in out.err
    assert f"{checkout} cannot take the documents; nothing changed" in out.err
    assert snapshot(checkout) == before


@needs_git
def test_a_target_that_is_a_directory_is_refused(checkout, capsys):
    target = checkout / layout.published_path(sample("shootout"))
    target.mkdir(parents=True)
    before = snapshot(checkout)

    code, out = run(
        capsys, DATA / "client.json", DATA / "shootout.json", checkout=checkout
    )

    assert code == 1
    assert "exists in" in out.err and "and is not a file" in out.err
    # The client document, which is publishable, is not written either.
    assert snapshot(checkout) == before


@needs_git
def test_a_part_of_a_target_that_is_a_file_is_refused(checkout, capsys):
    (checkout / "results").mkdir()
    (checkout / "results" / "client").write_text("a file\n")
    before = snapshot(checkout)

    code, out = run(capsys, DATA / "client.json", checkout=checkout)

    assert code == 1
    assert "passes through a file in" in out.err
    assert snapshot(checkout) == before


@needs_git
def test_a_symbolic_link_in_the_checkout_is_refused(tmp_path, checkout, capsys):
    outside = tmp_path / "outside"
    outside.mkdir()
    (checkout / "results").mkdir()
    (checkout / "results" / "shootout").symlink_to(outside)
    before = snapshot(checkout)

    code, out = run(capsys, DATA / "shootout.json", checkout=checkout)

    assert code == 1
    assert "passes through a symbolic link in" in out.err
    assert snapshot(checkout) == before
    assert list(outside.iterdir()) == []


@needs_git
def test_an_index_that_is_a_symbolic_link_is_refused(tmp_path, checkout, capsys):
    outside = tmp_path / "index.json"
    outside.write_text("{}\n")
    (checkout / "index.json").symlink_to(outside)

    code, out = run(capsys, DATA / "native.json", checkout=checkout)

    assert code == 1
    assert "index.json passes through a symbolic link in" in out.err
    assert outside.read_text() == "{}\n"
    assert not (checkout / "results").exists()


@needs_git
def test_a_document_that_is_a_symbolic_link_stops_the_publish(
    tmp_path, checkout, capsys
):
    path = checkout / "results" / "shootout" / "reference" / "cp313" / "zeo"
    path.mkdir(parents=True)
    (path / "old.json").symlink_to(DATA / "shootout.json")
    before = snapshot(checkout)

    code, out = run(capsys, DATA / "client.json", checkout=checkout)

    assert code == 1
    assert "old.json is a symbolic link" in out.err
    assert snapshot(checkout) == before


def git(repository, *args):
    subprocess.run(["git", *args], cwd=repository, check=True, capture_output=True)


@pytest.fixture
def page(tmp_path, monkeypatch):
    """A repository with a page under bench/dashboard, which publish
    copies."""
    repository = tmp_path / "repository"
    dashboard = repository / "bench" / "dashboard"
    (dashboard / "app").mkdir(parents=True)
    (dashboard / "__pycache__").mkdir()
    (dashboard / "index.html").write_text("<html></html>\n")
    (dashboard / "app" / "main.js").write_text("// the page\n")
    (dashboard / ".nojekyll").write_text("")
    (dashboard / "__pycache__" / "x.pyc").write_bytes(b"\0")
    secret = tmp_path / "secret.txt"
    secret.write_text("outside the repository\n")
    (dashboard / "link.txt").symlink_to(secret)
    git(repository, "init", "-q")
    git(repository, "add", "-f", "bench")
    (dashboard / "untracked.txt").write_text("not in git\n")
    monkeypatch.setattr(publish, "DASHBOARD", dashboard)
    return dashboard


@needs_git
def test_the_page_is_the_files_that_git_tracks(page, checkout, capsys):
    code, out = run(capsys, DATA / "client.json", checkout=checkout)

    assert code == 0, out.err
    copied = {
        path.relative_to(checkout).as_posix()
        for path in checkout.rglob("*")
        if path.is_file() and not path.is_relative_to(checkout / "results")
    }
    assert copied == {"index.html", "app/main.js", "index.json"}
    assert (checkout / "app" / "main.js").read_text() == "// the page\n"


@needs_git
@pytest.mark.parametrize("name", ["results/shootout/x.json", "index.json"])
def test_a_page_that_goes_to_a_published_path_stops_the_publish(
    name, page, checkout, capsys
):
    path = page / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("{}\n")
    git(page.parents[1], "add", "-f", "bench")

    code, out = run(capsys, DATA / "client.json", checkout=checkout)

    assert code == 1
    assert f"publish: bench/dashboard/{name}: goes to {name}" in out.err
    assert list(checkout.iterdir()) == []


def test_a_name_with_a_newline_is_not_a_part_of_a_path():
    document = sample("client")
    document["machine"]["name"] = "reference\n"

    assert layout.published_path(document) is None


@needs_git
def test_a_commit_that_is_not_the_tree_has_no_record_of_a_clean_tree(monkeypatch):
    head = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=publish.REPOSITORY,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()

    monkeypatch.setenv("ZOE_BENCH_COMMIT", "0" * 40)
    assert environment.dirty() is None
    monkeypatch.setenv("ZOE_BENCH_COMMIT", head[:12])
    assert environment.dirty() in (True, False)
    monkeypatch.delenv("ZOE_BENCH_COMMIT")
    assert environment.dirty() in (True, False)
