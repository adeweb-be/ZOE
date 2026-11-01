"""The targets file of a fixture gives what its manifest gives.

`workload.write_targets` writes it from `manifest.json`, and
`workload.TargetsFile` reads it: the lists of Locust, the Documents of
each editor, and the items of the manifest by index, by path and by
container, as `workload.Targets` gives them from the manifest in memory.

    just bench plone test bench/plone/tests/test_targets_file.py
"""

import json

import pytest
from manifests import write_like_the_build

from plonebench import corpus, workload

USERS = 64


@pytest.fixture(scope="module", params=["small", "xlarge-mini"])
def fixture_dir(request, tmp_path_factory):
    directory = tmp_path_factory.mktemp(request.param)
    write_like_the_build(corpus.plan_site(corpus.PRESETS[request.param]), directory)
    workload.write_targets(directory)
    return directory


@pytest.fixture
def both(fixture_dir):
    manifest = json.loads((fixture_dir / "manifest.json").read_text())
    with workload.TargetsFile.open(fixture_dir / workload.TARGETS_FILE) as targets:
        yield workload.Targets(manifest), targets


def fields(item: dict) -> dict:
    return {key: item.get(key) for key in workload.ITEM_FIELDS}


def test_the_words_of_the_spec_are_those_of_the_plan():
    for name in ("small", "medium", "xlarge-mini"):
        spec = corpus.PRESETS[name]
        assert corpus.site_words(spec) == corpus.plan_site(spec).words, name


def test_the_browsable_lists_are_those_of_the_manifest(both):
    targets, file = both
    browsable = targets.browsable()
    folders = [i for i in browsable if i["portal_type"] == "Folder"]
    docs = [i for i in browsable if i["portal_type"] in ("Document", "News Item")]
    files = [i for i in browsable if i["portal_type"] == "File"]
    assert (file.folders, file.docs, file.files) == (
        len(folders),
        len(docs),
        len(files),
    )
    assert [file.folder(n)["path"] for n in range(file.folders)] == [
        i["path"] for i in folders
    ]
    assert [file.file(n) for n in range(file.files)] == [
        {"path": i["path"], "size": i["size"]} for i in files
    ]
    for n, folder in enumerate(folders):
        below = [
            d
            for d, doc in enumerate(docs)
            if doc["path"].startswith(folder["path"] + "/")
        ]
        picked = file.folder(n)
        assert list(range(picked["start"], picked["stop"])) == below, folder["path"]
    for d, doc in enumerate(docs):
        parent = doc["path"].rpartition("/")[0]
        expected = next((n for n, i in enumerate(folders) if i["path"] == parent), None)
        assert file.doc(d) == {
            "path": doc["path"],
            "portal_type": doc["portal_type"],
            "parent": expected,
        }


def test_the_targets_of_the_roles_are_those_of_the_manifest(both):
    targets, file = both
    manifest = targets.manifest
    assert file.site == manifest["site"]
    assert file.users == manifest["users"]
    assert file.search_terms == manifest["search_terms"]
    assert file.subjects == manifest["subjects"]
    assert file.code_words == corpus.code_words(corpus.plan_site(targets.spec).words)
    assert file.collections == [
        {"path": i["path"]} for i in targets.items(portal_type="Collection")
    ]
    for rank in range(USERS):
        account, section = file.editor(rank)
        assert (account, section) == targets.editor(rank)
        for pool in (corpus.EDIT, corpus.WORKFLOW):
            assert file.own_docs(rank, pool, USERS) == [
                fields(i) for i in targets.own_docs(rank, pool, USERS)
            ]
        assert file.section_docs(section, corpus.EDIT) == [
            fields(i) for i in targets.section_docs(section, corpus.EDIT)
        ]
        assert file.section_folders(section) == targets.section_folders(section)
        assert file.shared_doc(rank) == targets.shared_doc(rank)
        assert file.pair_doc(rank) == targets.pair_doc(rank)
        assert file.hot_doc(rank) == targets.hot_doc(rank)


def test_the_items_are_those_of_the_manifest(both):
    targets, file = both
    items = targets.manifest["items"]
    assert len(file) == len(targets) == len(items)
    assert file.seed == targets.seed
    assert file.catalog_size == targets.catalog_size == len(items) + 1
    assert file.duplicate_uids == targets.duplicate_uids == []
    assert file.paths(0, len(items)) == targets.paths(0, len(items))
    assert file.paths(5, 9) == [i["path"] for i in items[5:9]]
    for n, item in enumerate(items):
        assert file.index(item["path"]) == targets.index(item["path"]) == n
    for n in (0, 1, len(items) // 2, len(items) - 1):
        assert file.item(items[n]["path"]) == fields(items[n])
    assert file.index("no/such/item") is None
    assert file.item("no/such/item") is None
    paths = [i["path"] for i in items[::3]] + ["no/such/item"]
    assert file.items_by_path(paths) == {
        path: fields(item) for path, item in targets.items_by_path(paths).items()
    }
    containers = {""} | {i["path"].rpartition("/")[0] for i in items}
    for path in sorted(containers):
        assert file.children(path) == targets.children(path), path
    assert file.children("no/such/item") == []


def test_a_uid_twice_in_the_manifest_is_listed(tmp_path):
    directory = write_like_the_build(
        corpus.plan_site(corpus.PRESETS["small"]), tmp_path
    )
    manifest = json.loads((directory / "manifest.json").read_text())
    manifest["items"][7]["uid"] = manifest["items"][3]["uid"]
    (directory / "manifest.json").write_text(json.dumps(manifest))
    with workload.TargetsFile.open(workload.write_targets(directory)) as file:
        assert file.duplicate_uids == [manifest["items"][3]["uid"]]
    assert workload.Targets(manifest).duplicate_uids == file.duplicate_uids


def test_ensure_targets_writes_the_file_once_for_each_manifest(tmp_path):
    directory = write_like_the_build(
        corpus.plan_site(corpus.PRESETS["small"]), tmp_path
    )
    path = workload.ensure_targets(directory)
    assert path == directory / workload.TARGETS_FILE
    written = path.stat().st_ino
    assert workload.ensure_targets(directory) == path
    assert path.stat().st_ino == written
    # Another build of the fixture writes another manifest.
    manifest = json.loads((directory / "manifest.json").read_text())
    manifest["catalog_size"] += 1
    (directory / "manifest.json").write_text(json.dumps(manifest))
    workload.ensure_targets(directory)
    with workload.TargetsFile.open(path) as file:
        assert file.catalog_size == manifest["catalog_size"]


def test_the_documents_below_a_folder_are_one_slice(tmp_path):
    directory = write_like_the_build(
        corpus.plan_site(corpus.PRESETS["small"]), tmp_path
    )
    manifest = json.loads((directory / "manifest.json").read_text())
    items = manifest["items"]
    # A Document of section-00 after the items of the other sections.
    moved = next(
        n
        for n, item in enumerate(items)
        if item["path"].startswith("section-00/")
        and item["portal_type"] == "Document"
        and item["pool"] == corpus.BROWSE
        and item["state"] == corpus.PUBLISHED
    )
    items.insert(len(items) - 60, items.pop(moved))
    (directory / "manifest.json").write_text(json.dumps(manifest))
    with pytest.raises(ValueError, match="one slice"):
        workload.write_targets(directory)
    assert sorted(p.name for p in directory.iterdir()) == [
        "credentials.json",
        "manifest.json",
        "spec.json",
    ]
