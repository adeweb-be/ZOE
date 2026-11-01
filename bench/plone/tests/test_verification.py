"""The verification finds lost updates, phantoms and refused writes that
committed, and accepts both results of an ambiguous write."""

import json

from plonebench import corpus, verification, workload
from plonebench.ledger import AMBIGUOUS, FAILED, OK, Record

MANIFEST = {
    "items": [
        {"path": "s/doc", "versions": 1, "workflow_history": 1},
        {"path": "bench/hot/doc-000", "versions": 1, "workflow_history": 1},
    ]
}


def rec(op, kind, outcome, start, end, target="s/doc", user=0, **values):
    return Record(
        op=op,
        kind=kind,
        user=user,
        role="r",
        account="a",
        instance=0,
        step=0,
        start=start,
        end=end,
        outcome=outcome,
        target=target,
        values=values,
    )


def observed_object(**changes):
    obj = {
        "exists": True,
        "title": "Edit aa ab ac ad x",
        "description": "d",
        "state": "published",
        "modified": "m",
        "uid": "u",
        "versions": 2,
        "version_titles": None,
        "workflow_history": 1,
        "anonymous_view": True,
        "history_id": 1,
        "catalog": [
            {
                "Title": "Edit aa ab ac ad x",
                "review_state": "published",
                "modified": "m",
                "anonymous": True,
            }
        ],
    }
    obj.update(changes)
    return obj


def observed(objects, **extra):
    data = {
        "objects": objects,
        "extra": {},
        "duplicate_uids": {},
        "duplicate_history_ids": {},
        "tokens": {},
        "content_objects": 10,
        "catalog_entries": 10,
        "catalog_duplicates": 0,
        "catalog_orphans": [],
        "uncatalogued": [],
    }
    data.update(extra)
    return data


def test_the_last_acknowledged_edit_wins():
    records = [
        rec("1", "edit", OK, 0, 1, title="Edit ca cb cc cd x", description="d"),
        rec("2", "edit", OK, 2, 3, title="Edit aa ab ac ad x", description="d"),
    ]
    obj = observed_object(versions=3)
    verdict = verification.judge(records, MANIFEST, observed({"s/doc": obj}))
    assert verdict["verdict"] == "pass", verdict


def test_a_lost_update_fails():
    records = [
        rec("1", "edit", OK, 0, 1, title="Edit aa ab ac ad x", description="d"),
        rec("2", "edit", OK, 2, 3, title="Edit da db dc dd x", description="d"),
    ]
    obj = observed_object(versions=3)
    verdict = verification.judge(records, MANIFEST, observed({"s/doc": obj}))
    assert verdict["checks"]["edits"]["violations"] == 1


def test_an_ambiguous_later_write_may_have_committed():
    records = [
        rec("1", "edit", OK, 0, 1, title="Edit ca cb cc cd x", description="d"),
        rec("2", "edit", AMBIGUOUS, 2, 3, title="Edit aa ab ac ad x", description="d"),
    ]
    for versions in (2, 3):
        obj = observed_object(versions=versions)
        verdict = verification.judge(records, MANIFEST, observed({"s/doc": obj}))
        assert verdict["checks"]["edits"]["violations"] == 0
        assert verdict["checks"]["versions"]["violations"] == 0


def test_a_failed_edit_adds_no_version():
    records = [
        rec("1", "edit", OK, 0, 1, title="Edit aa ab ac ad x", description="d"),
        rec("2", "edit", FAILED, 2, 3, title="Edit da db dc dd x", description="d"),
    ]
    obj = observed_object(versions=3)
    verdict = verification.judge(records, MANIFEST, observed({"s/doc": obj}))
    assert verdict["checks"]["versions"]["violations"] == 1


def test_concurrent_writers_accept_any_write_that_could_be_last():
    path = "bench/hot/doc-000"
    records = [
        rec(
            "1",
            "patch-title",
            OK,
            0.0,
            1.0,
            target=path,
            user=1,
            title="Patch aa ab ac ad x",
        ),
        rec(
            "2",
            "patch-title",
            OK,
            0.5,
            1.5,
            target=path,
            user=2,
            title="Patch ba bb bc bd x",
        ),
    ]
    for title in ("Patch aa ab ac ad x", "Patch ba bb bc bd x"):
        obj = observed_object(title=title, versions=3)
        obj["catalog"][0]["Title"] = title
        verdict = verification.judge(records, MANIFEST, observed({path: obj}))
        assert verdict["checks"]["edits"]["violations"] == 0, title


def test_a_write_acknowledged_before_another_started_cannot_be_last():
    path = "bench/hot/doc-000"
    records = [
        rec(
            "1",
            "patch-title",
            OK,
            0.0,
            1.0,
            target=path,
            user=1,
            title="Patch aa ab ac ad x",
        ),
        rec(
            "2",
            "patch-title",
            OK,
            2.0,
            3.0,
            target=path,
            user=2,
            title="Patch ba bb bc bd x",
        ),
    ]
    obj = observed_object(title="Patch aa ab ac ad x", versions=3)
    obj["catalog"][0]["Title"] = "Patch aa ab ac ad x"
    verdict = verification.judge(records, MANIFEST, observed({path: obj}))
    assert verdict["checks"]["edits"]["violations"] == 1


def test_phantoms_and_refused_creations():
    records = [
        rec(
            "c1",
            "create",
            OK,
            0,
            1,
            target="s/new-1",
            title="New ea eb ec ed x",
            folder="s",
        ),
        rec(
            "c2",
            "create",
            FAILED,
            0,
            1,
            target="s",
            title="New fa fb fc fd x",
            folder="s",
        ),
    ]
    objects = {
        "s/new-1": observed_object(
            title="New ea eb ec ed x",
            versions=1,
            catalog=[
                {
                    "Title": "New ea eb ec ed x",
                    "review_state": "published",
                    "modified": "m",
                    "anonymous": True,
                }
            ],
        )
    }
    extra = {
        "s/new-1": {"title": "New ea eb ec ed x", "type": "Document"},
        "s/new-2": {"title": "New fa fb fc fd x", "type": "Document"},
        "s/stray": {"title": "Something", "type": "Document"},
    }
    verdict = verification.judge(records, MANIFEST, observed(objects, extra=extra))
    assert verdict["checks"]["created"]["violations"] == 0
    assert verdict["checks"]["failed_writes"]["violations"] == 1
    assert verdict["checks"]["phantoms"]["violations"] == 1


def test_stale_catalog_and_permissions():
    obj = observed_object(state="private", anonymous_view=True)
    verdict = verification.judge([], MANIFEST, observed({"s/doc": obj}))
    assert verdict["checks"]["permissions"]["violations"] == 1
    assert verdict["checks"]["catalog"]["violations"] == 1


def test_search_tokens():
    records = [
        rec("1", "edit", OK, 0, 1, title="Edit ca cb cc cd x", description="d"),
        rec("2", "edit", OK, 2, 3, title="Edit aa ab ac ad x", description="d"),
    ]
    obj = observed_object(versions=3)
    good = {"aa ab ac ad": ["s/doc"], "ca cb cc cd": []}
    verdict = verification.judge(
        records, MANIFEST, observed({"s/doc": obj}, tokens=good)
    )
    assert verdict["checks"]["search"]["violations"] == 0
    stale = {"aa ab ac ad": ["s/doc"], "ca cb cc cd": ["s/doc"]}
    verdict = verification.judge(
        records, MANIFEST, observed({"s/doc": obj}, tokens=stale)
    )
    assert verdict["checks"]["search"]["violations"] == 1


def test_the_request_names_the_targets_and_tokens():
    records = [
        rec("1", "edit", OK, 0, 1, title="Edit aa ab ac ad x", description="d"),
        rec("2", "create", FAILED, 0, 1, target="s", title="New fa fb fc fd x"),
    ]
    wanted = verification.request(records, MANIFEST)
    assert wanted["paths"] == ["s/doc"]
    assert wanted["tokens"] == ["aa ab ac ad"]


# The bounded checks: what the request names, and the checks of the
# length of the catalog, of the entries next to the writes and of the
# sample.


def site_manifest(count=300, **extra):
    """A manifest of a Folder `s` and `count - 1` Documents in it."""
    items = [
        {
            "path": "s",
            "portal_type": "Folder",
            "title": "S",
            "state": "published",
            "uid": "uid-s",
            "versions": 0,
            "workflow_history": 2,
        }
    ]
    for n in range(count - 1):
        items.append(
            {
                "path": f"s/doc-{n:03d}",
                "portal_type": "Document",
                "title": f"Title {n}",
                "state": "published",
                "uid": f"uid-{n}",
                "versions": 1,
                "workflow_history": 2,
            }
        )
    manifest = {"spec": {"seed": 3}, "items": items, "catalog_size": count + 1}
    manifest.update(extra)
    return manifest


def test_the_request_names_the_containers_the_neighbours_and_a_sample():
    manifest = site_manifest()
    records = [
        rec("1", "edit", OK, 0, 1, target="s/doc-149", title="Edit aa ab ac ad x"),
        rec(
            "2", "create", OK, 0, 1, target="s/new", folder="s", title="New ba bb bc bd"
        ),
        rec("3", "create", FAILED, 0, 1, target="t", title="New ca cb cc cd"),
    ]
    wanted = verification.request(records, manifest)
    assert wanted["paths"] == ["s/doc-149", "s/new"]
    assert wanted["containers"] == ["s", "t"]
    # s/doc-149 is item 150 of the manifest, and s is item 0.
    assert wanted["neighbours"] == [[0, 65], [86, 215]]
    sample = wanted["sample"]
    assert len(sample) == verification.SAMPLE == len(set(sample))
    order = [item["path"] for item in manifest["items"]]
    assert sample == [path for path in order if path in sample]
    # The same fixture gives the same sample; another seed another one.
    assert verification.request([], manifest)["sample"] == sample
    other = site_manifest(spec={"seed": 4})
    assert verification.request([], other)["sample"] != sample


def test_the_neighbours_are_merged_and_bounded():
    assert verification.neighbour_ranges([], 10) == []
    assert verification.neighbour_ranges([5], 10, 2) == [[3, 8]]
    assert verification.neighbour_ranges([0, 9], 10, 2) == [[0, 3], [7, 10]]
    assert verification.neighbour_ranges([4, 2, 6], 10, 1) == [[1, 8]]


def bounded(objects=None, **extra):
    data = observed(objects or {}, catalog_entries=301)
    data.update(extra)
    return data


def test_the_length_of_the_catalog_counts_the_new_objects():
    manifest = site_manifest()
    records = [
        rec(
            "c",
            "create",
            OK,
            0,
            1,
            target="s/new",
            folder="s",
            title="New ea eb ec ed x",
        )
    ]
    new = observed_object(title="New ea eb ec ed x", versions=1)
    new["catalog"][0]["Title"] = "New ea eb ec ed x"
    extra = {"s/new": {"title": "New ea eb ec ed x", "type": "Document"}}
    verdict = verification.judge(
        records, manifest, bounded({"s/new": new}, extra=extra, catalog_entries=302)
    )
    assert verdict["verdict"] == "pass", verdict
    # An entry lost anywhere in the site, or one more.
    for length in (301, 303):
        verdict = verification.judge(
            records,
            manifest,
            bounded({"s/new": new}, extra=extra, catalog_entries=length),
        )
        assert verdict["checks"]["catalog"]["violations"] == 1, length
        assert (
            f"holds {length} entries, not 302"
            in verdict["checks"]["catalog"]["examples"][0]
        )


def test_an_entry_next_to_a_write_has_the_values_of_the_manifest():
    manifest = site_manifest()
    records = [
        rec("1", "edit", OK, 0, 1, target="s/doc-001", title="Edit aa ab ac ad x")
    ]
    obj = observed_object(versions=2)
    entries = {
        "s/doc-000": {"Title": "Title 0", "review_state": "published"},
        # The run wrote this one: the manifest does not tell its title.
        "s/doc-001": {"Title": "Edit aa ab ac ad x", "review_state": "published"},
    }
    data = bounded({"s/doc-001": obj}, entries=entries)
    assert verification.judge(records, manifest, data)["verdict"] == "pass"
    stale = dict(entries, **{"s/doc-002": {"Title": "Old", "review_state": "private"}})
    verdict = verification.judge(
        records, manifest, bounded({"s/doc-001": obj}, entries=stale)
    )
    assert verdict["checks"]["catalog"]["violations"] == 1
    assert verdict["checks"]["catalog"]["examples"] == [
        "s/doc-002: Title, review_state of the manifest"
    ]


def test_the_sample_has_the_values_of_the_manifest():
    manifest = site_manifest()
    seen = observed_object(
        title="Title 5", versions=1, workflow_history=2, state="published"
    )
    seen["catalog"][0]["Title"] = "Title 5"
    data = bounded({"s/doc-005": seen}, sample=["s/doc-005"])
    verdict = verification.judge([], manifest, data)
    assert verdict["verdict"] == "pass", verdict
    assert verdict["checks"]["sample"]["checked"] == 1
    lost = dict(seen, versions=2)
    verdict = verification.judge(
        [], manifest, bounded({"s/doc-005": lost}, sample=["s/doc-005"])
    )
    assert verdict["checks"]["sample"]["examples"] == [
        "s/doc-005: versions of the manifest"
    ]
    missing = bounded({"s/doc-005": {"exists": False}}, sample=["s/doc-005"])
    verdict = verification.judge([], manifest, missing)
    assert verdict["checks"]["sample"]["examples"] == ["s/doc-005: missing"]


def test_the_sample_takes_no_workflow_history_of_an_item_without_a_workflow():
    # The manifest gives a File the workflow history of its container.
    manifest = site_manifest()
    manifest["items"][6].update(portal_type="File", state=None, workflow_history=2)
    seen = observed_object(
        title="Title 5", versions=1, workflow_history=0, state=None, anonymous_view=True
    )
    seen["catalog"][0].update(Title="Title 5", review_state=None)
    verdict = verification.judge(
        [], manifest, bounded({"s/doc-005": seen}, sample=["s/doc-005"])
    )
    assert verdict["checks"]["sample"]["violations"] == 0, verdict["checks"]
    assert verdict["checks"]["sample"]["checked"] == 1


def test_a_uid_twice_in_the_manifest_fails():
    manifest = site_manifest()
    manifest["items"][2]["uid"] = manifest["items"][1]["uid"]
    verdict = verification.judge([], manifest, bounded())
    assert verdict["checks"]["unique_ids"]["violations"] == 1


def test_the_judge_reads_a_targets_file_as_the_manifest(tmp_path):
    manifest = site_manifest(
        site="Plone",
        users={},
        search_terms=[],
        subjects=[],
    )
    manifest["spec"] = corpus.PRESETS["small"].to_json()
    for item in manifest["items"]:
        item.update(pool="browse", section=0, size=0)
    (tmp_path / "manifest.json").write_text(json.dumps(manifest))
    records = [
        rec("1", "edit", OK, 0, 1, target="s/doc-001", title="Edit aa ab ac ad x")
    ]
    seen = observed_object(versions=2)
    entries = {"s/doc-002": {"Title": "Old", "review_state": "published"}}
    data = bounded({"s/doc-001": seen}, entries=entries)
    with workload.TargetsFile.open(workload.write_targets(tmp_path)) as targets:
        assert verification.request(records, targets) == verification.request(
            records, manifest
        )
        from_file = verification.judge(records, targets, data)
    assert from_file == verification.judge(records, manifest, data)
    assert from_file["checks"]["catalog"]["violations"] == 1
