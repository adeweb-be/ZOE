"""Unit tests of `zoe.plone.indexing` that need no Plone: the keys, the
values, the coalescing of a batch and the cookie (ADR 0047)."""

import pytest

from zoe.plone import indexing
from zoe.plone.indexing import (
    Entry,
    coalesce,
    decode,
    encode,
    highest,
    key_time,
    make_key,
)


def test_a_key_holds_its_time_and_sorts_by_time():
    first = make_key(1_000_000, b"\xff" * 8, 7)
    second = make_key(1_000_001, b"\x00" * 8, 0)
    assert len(first) == 20
    assert key_time(first) == 1.0
    assert first < second
    # The operations of one processing keep their order.
    assert make_key(5, b"a" * 8, 1) < make_key(5, b"a" * 8, 2)


def test_the_name_of_a_queue_holds_the_oid_of_the_catalog():
    oid = b"\x00\x00\x00\x00\x00\x00\x01\x02"
    assert indexing.queue_name(oid) == "zoe.plone.indexing:0000000000000102"
    assert indexing.failed_name(oid) == "zoe.plone.indexing.failed:0000000000000102"


def test_a_value_is_json():
    data = encode("reindex", "/plone/doc", ("Title", "SearchableText"), 1)
    assert data == (
        b'{"idxs":["Title","SearchableText"],"metadata":true,'
        b'"op":"reindex","path":"/plone/doc"}'
    )
    assert decode(data) == {
        "op": "reindex",
        "path": "/plone/doc",
        "idxs": ["Title", "SearchableText"],
        "metadata": True,
    }
    failed = decode(encode("unindex", "/plone/doc", (), True, 2, "ValueError: x"))
    assert failed["failures"] == 2
    assert failed["error"] == "ValueError: x"


def pairs(*operations):
    return [
        (make_key(n, b"\x00" * 8, 0), encode(op, path, idxs, metadata))
        for n, (op, path, idxs, metadata) in enumerate(operations)
    ]


def test_coalesce_unites_the_indexes_of_a_path():
    entries = coalesce(
        pairs(
            ("reindex", "/p/a", ["review_state"], True),
            ("reindex", "/p/a", ["cmf_uid"], False),
            ("reindex", "/p/b", ["Title"], False),
        )
    )
    assert list(entries) == ["/p/a", "/p/b"]
    a = entries["/p/a"]
    assert not a.full
    assert a.idxs == {"review_state", "cmf_uid"}
    assert a.metadata
    assert len(a.keys) == 2
    assert not entries["/p/b"].metadata


@pytest.mark.parametrize(
    "operation",
    [
        ("reindex", "/p/a", [], False),
        ("index", "/p/a", [], False),
        ("unindex", "/p/a", [], False),
    ],
)
def test_an_index_an_unindex_or_no_index_means_all_indexes(operation):
    (entry,) = coalesce(
        pairs(("reindex", "/p/a", ["Title"], False), operation)
    ).values()
    assert entry.full
    assert entry.metadata == (operation[0] != "reindex")


def test_a_security_reindex_marks_its_entry():
    entries = coalesce(
        pairs(
            ("reindex", "/p/f", [], True),
            ("reindex", "/p/f", ["allowedRolesAndUsers"], False),
            ("reindex", "/p/g", ["review_state", "allowedRolesAndUsers"], False),
        )
    )
    # A full reindex keeps the mark of its security reindex.
    assert entries["/p/f"].full and entries["/p/f"].security
    assert not entries["/p/g"].security


def test_highest_keeps_the_top_of_each_subtree():
    assert highest(["/p/a/b", "/p/a", "/p/ab", "/p/c/d", "/p/c/d/e"]) == [
        "/p/a",
        "/p/ab",
        "/p/c/d",
    ]


def test_an_entry_starts_empty():
    entry = Entry()
    assert (entry.keys, entry.full, entry.idxs, entry.metadata) == (
        [],
        False,
        set(),
        False,
    )


def test_the_summary_of_the_delays():
    assert indexing._summary([]) == {"count": 0, "p50": None, "p95": None, "max": None}
    delays = [float(n) for n in range(100)]
    assert indexing._summary(delays) == {
        "count": 100,
        "p50": 50.0,
        "p95": 95.0,
        "max": 99.0,
    }


def test_the_cookie_holds_the_oid_the_key_the_database_and_the_path():
    oid = b"\x00" * 7 + b"\x09"
    key = make_key(1, b"\x01" * 8, 3)
    name = b"second-db".hex()
    value = f"{oid.hex()}-{key.hex()}-{name}-/plone/a%20b-c"
    assert indexing._parse_cookie(value) == (oid, key, "second-db", "/plone/a b-c")
    assert indexing._parse_cookie(f"{oid.hex()}-{key.hex()}-{name}-") == (
        oid,
        key,
        "second-db",
        "/",
    )
    for bad in (
        "",
        "x",
        f"{oid.hex()}-00-{name}-/",
        f"zz-{key.hex()}-{name}-/",
        f"{oid.hex()}-{key.hex()}--/",
        f"{oid.hex()}-{key.hex()}-/plone",
        None,
    ):
        assert indexing._parse_cookie(bad) is None


class Request(dict):
    """A request with the `get` of a mapping."""


def test_a_cookie_that_does_not_parse_expires_with_its_path():
    assert indexing._cookie_path("x-y-/plone", Request()) == "/plone"
    request = Request(PATH_INFO="/plone/folder/doc")
    assert indexing._cookie_path("garbage", request) == "/plone"
    assert indexing._cookie_path(None, Request()) == "/"


@pytest.mark.parametrize(
    "value, error",
    [
        (b"not json", "not JSON"),
        (b"[]", "not a JSON object"),
        (b'{"op": "drop", "path": "/p", "idxs": [], "metadata": true}', "operation"),
        (b'{"op": "reindex", "path": "p", "idxs": [], "metadata": true}', "path"),
        (b'{"op": "reindex", "path": "/p", "idxs": [1], "metadata": true}', "indexes"),
        (b'{"op": "reindex", "path": "/p", "idxs": [], "metadata": 1}', "metadata"),
        (b"\xff", "not JSON"),
    ],
)
def test_parse_refuses_a_value_that_is_not_an_operation(value, error):
    with pytest.raises(ValueError, match=error):
        indexing.parse(value)


def test_parse_takes_an_operation():
    value = encode("unindex", "/plone/doc", (), True)
    assert indexing.parse(value)["op"] == "unindex"


def test_installed_is_false_without_install():
    assert not indexing.installed()
