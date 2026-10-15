"""The verification finds lost updates, phantoms and refused writes that
committed, and accepts both results of an ambiguous write."""

from plonebench import verification
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
        "title": "Edit aaaaaaaa x",
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
                "Title": "Edit aaaaaaaa x",
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
        rec("1", "edit", OK, 0, 1, title="Edit 11111111 x", description="d"),
        rec("2", "edit", OK, 2, 3, title="Edit aaaaaaaa x", description="d"),
    ]
    obj = observed_object(versions=3)
    verdict = verification.judge(records, MANIFEST, observed({"s/doc": obj}))
    assert verdict["verdict"] == "pass", verdict


def test_a_lost_update_fails():
    records = [
        rec("1", "edit", OK, 0, 1, title="Edit aaaaaaaa x", description="d"),
        rec("2", "edit", OK, 2, 3, title="Edit 22222222 x", description="d"),
    ]
    obj = observed_object(versions=3)
    verdict = verification.judge(records, MANIFEST, observed({"s/doc": obj}))
    assert verdict["checks"]["edits"]["violations"] == 1


def test_an_ambiguous_later_write_may_have_committed():
    records = [
        rec("1", "edit", OK, 0, 1, title="Edit 11111111 x", description="d"),
        rec("2", "edit", AMBIGUOUS, 2, 3, title="Edit aaaaaaaa x", description="d"),
    ]
    for versions in (2, 3):
        obj = observed_object(versions=versions)
        verdict = verification.judge(records, MANIFEST, observed({"s/doc": obj}))
        assert verdict["checks"]["edits"]["violations"] == 0
        assert verdict["checks"]["versions"]["violations"] == 0


def test_a_failed_edit_adds_no_version():
    records = [
        rec("1", "edit", OK, 0, 1, title="Edit aaaaaaaa x", description="d"),
        rec("2", "edit", FAILED, 2, 3, title="Edit 22222222 x", description="d"),
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
            title="Patch aaaaaaaa x",
        ),
        rec(
            "2",
            "patch-title",
            OK,
            0.5,
            1.5,
            target=path,
            user=2,
            title="Patch bbbbbbbb x",
        ),
    ]
    for title in ("Patch aaaaaaaa x", "Patch bbbbbbbb x"):
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
            title="Patch aaaaaaaa x",
        ),
        rec(
            "2",
            "patch-title",
            OK,
            2.0,
            3.0,
            target=path,
            user=2,
            title="Patch bbbbbbbb x",
        ),
    ]
    obj = observed_object(title="Patch aaaaaaaa x", versions=3)
    obj["catalog"][0]["Title"] = "Patch aaaaaaaa x"
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
            title="New 111111111111 x",
            folder="s",
        ),
        rec(
            "c2",
            "create",
            FAILED,
            0,
            1,
            target="s",
            title="New 222222222222 x",
            folder="s",
        ),
    ]
    objects = {
        "s/new-1": observed_object(
            title="New 111111111111 x",
            versions=1,
            catalog=[
                {
                    "Title": "New 111111111111 x",
                    "review_state": "published",
                    "modified": "m",
                    "anonymous": True,
                }
            ],
        )
    }
    extra = {
        "s/new-1": {"title": "New 111111111111 x", "type": "Document"},
        "s/new-2": {"title": "New 222222222222 x", "type": "Document"},
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
        rec("1", "edit", OK, 0, 1, title="Edit 11111111 x", description="d"),
        rec("2", "edit", OK, 2, 3, title="Edit aaaaaaaa x", description="d"),
    ]
    obj = observed_object(versions=3)
    good = {"aaaaaaaa": ["s/doc"], "11111111": []}
    verdict = verification.judge(
        records, MANIFEST, observed({"s/doc": obj}, tokens=good)
    )
    assert verdict["checks"]["search"]["violations"] == 0
    stale = {"aaaaaaaa": ["s/doc"], "11111111": ["s/doc"]}
    verdict = verification.judge(
        records, MANIFEST, observed({"s/doc": obj}, tokens=stale)
    )
    assert verdict["checks"]["search"]["violations"] == 1


def test_the_request_names_the_targets_and_tokens():
    records = [
        rec("1", "edit", OK, 0, 1, title="Edit aaaaaaaa x", description="d"),
        rec("2", "create", FAILED, 0, 1, target="s", title="New 222222222222 x"),
    ]
    wanted = verification.request(records, MANIFEST)
    assert wanted["paths"] == ["s/doc"]
    assert wanted["tokens"] == ["aaaaaaaa"]
