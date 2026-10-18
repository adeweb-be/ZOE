"""Unit tests of `zoe.plone.registry_rows` that need no Plone: the keys, the
marker, the pickler, the atomic scan and the cache (ADR 0050)."""

import gc

import persistent
import pytest
from ZODB.broken import Broken, PersistentBroken, find_global, persistentBroken
from ZODB.POSException import ConflictError

from zoe.plone import registry_rows
from zoe.plone.registry_rows import (
    FIELD,
    MARKER,
    VALUE,
    Cache,
    decode_marker,
    dumps,
    encode_marker,
    loads,
    namespace_name,
    record_key,
    scan_rows,
)

OID = b"\x00\x00\x00\x00\x00\x00\x00\x2a"


class Field(persistent.Persistent):
    """A persistent object that the tests take as a field."""

    def __init__(self, title):
        self.title = title


class Other(persistent.Persistent):
    """A persistent object that is not a field."""


def is_field(obj):
    return isinstance(obj, Field)


def test_the_name_of_a_namespace_holds_the_id():
    registry_id = registry_rows.new_id()
    assert len(registry_id) == 32
    int(registry_id, 16)
    assert namespace_name(registry_id) == f"zoe.plone.registry:{registry_id}"
    assert registry_rows.new_id() != registry_id


def test_the_keys_of_a_record():
    assert record_key(FIELD, "plone.site_title") == b"fplone.site_title"
    assert record_key(VALUE, "plone.site_title") == b"vplone.site_title"
    assert record_key(VALUE, "a" * 1023) == b"v" + b"a" * 1023
    with pytest.raises(ValueError, match="at most 1023 bytes"):
        record_key(VALUE, "a" * 1024)
    # The marker key is shorter than each key of a record.
    assert MARKER == b"m"


def test_the_marker_holds_the_layout_the_oid_and_the_counter():
    data = encode_marker(OID, 7)
    assert len(data) == 17
    assert data[0] == 1
    assert decode_marker(data) == (OID, 7)
    assert decode_marker(None) is None
    # Another layout, or another length, is no marker.
    assert decode_marker(b"\x02" + data[1:]) is None
    assert decode_marker(data[:-1]) is None
    with pytest.raises(ValueError):
        encode_marker(b"\x00", 1)


def test_a_value_pickles_by_value():
    value = {"a": (1, 2.5, b"x"), "b": frozenset({"y"}), "c": [None, True]}
    data = dumps(value, is_field)
    assert data[:2] == b"\x80\x03"
    assert loads(data) == value


def test_a_field_pickles_by_value_and_loads_with_no_connection():
    field = Field("Title")
    field.value_type = Field("Inner")
    loaded = loads(dumps(field, is_field))
    assert type(loaded) is Field
    assert loaded.title == "Title"
    assert loaded.value_type.title == "Inner"
    assert loaded._p_jar is None
    assert loaded._p_oid is None


def test_a_persistent_object_that_is_no_field_is_refused():
    with pytest.raises(TypeError, match="no persistent object but a field"):
        dumps({"a": Other()}, is_field)
    with pytest.raises(TypeError):
        dumps(Field("x"), lambda obj: False)


def test_a_broken_field_keeps_its_class_and_its_state():
    cls = persistentBroken(find_global("zoe.no_such_module", "GoneField"))
    broken = cls.__new__(cls)
    broken.__setstate__({"title": "Gone"})
    assert isinstance(broken, PersistentBroken)
    data = dumps(broken, is_field)
    loaded = loads(data)
    # The class is still gone: the read gives a broken object of that class.
    assert isinstance(loaded, Broken)
    assert type(loaded).__module__ == "zoe.no_such_module"
    assert type(loaded).__name__ == "GoneField"
    assert loaded.__Broken_state__ == {"title": "Gone"}
    # A broken object pickles again as itself.
    assert loads(dumps(loaded, is_field)).__Broken_state__ == {"title": "Gone"}


def test_a_class_that_is_gone_reads_as_broken():
    data = dumps(Field("x"), is_field).replace(
        b"test_zoe_plone_registry_rows", b"test_zoe_plone_registry_gone"
    )
    assert isinstance(loads(data), Broken)


class FakeStore:
    """A store of durable namespaces with pages and a generation."""

    def __init__(self, rows, page=2):
        self.rows = dict(rows)
        self.page = page
        self.generation = 1
        self.scans = 0
        # Called before each page: a commit during the scan.
        self.on_page = None

    def kv_generation(self, name):
        return self.generation.to_bytes(8, "big")

    def kv_scan(self, flavour, name, start, stop, limit):
        assert flavour == "durable"
        if self.on_page is not None:
            self.on_page(self)
        if start is None:
            self.scans += 1
        keys = sorted(key for key in self.rows if start is None or key >= start)
        return [[key, self.rows[key]] for key in keys[: min(limit, self.page)]]


def test_a_scan_reads_every_page_and_labels_with_the_marker():
    rows = {b"fa": b"1", b"va": b"2", b"vb": b"3", MARKER: encode_marker(OID, 4)}
    store = FakeStore(rows)
    found, label = scan_rows(store, "ns")
    assert found == rows
    assert label == 4
    assert scan_rows(FakeStore({b"va": b"1"}), "ns") == ({b"va": b"1"}, 0)


def test_a_torn_scan_scans_again():
    store = FakeStore({b"va": b"1", b"vb": b"2", b"vc": b"3"})
    commits = []

    def commit_once(store):
        if not commits:
            commits.append(1)
            store.rows[b"vd"] = b"4"
            store.generation += 1

    store.on_page = commit_once
    found, _label = scan_rows(store, "ns")
    assert store.scans == 2
    assert found[b"vd"] == b"4"


def test_a_scan_that_is_always_torn_raises_a_conflict():
    store = FakeStore({b"va": b"1", b"vb": b"2", b"vc": b"3"})

    def commit(store):
        store.generation += 1

    store.on_page = commit
    with pytest.raises(ConflictError):
        scan_rows(store, "ns")
    assert store.scans == registry_rows.SCAN_TRIES


class Storage:
    """A storage of the cache: any object with weak references."""


def test_the_cache_serves_a_counter_at_or_below_its_label():
    cache = Cache()
    storage = Storage()
    scans = []

    def scan(label):
        def run():
            scans.append(label)
            return {b"v": str(label).encode()}, label

        return run

    assert cache.rows(storage, "ns", 3, scan(3)) == {b"v": b"3"}
    assert cache.rows(storage, "ns", 2, scan(9)) == {b"v": b"3"}
    assert cache.rows(storage, "ns", 3, scan(9)) == {b"v": b"3"}
    assert scans == [3]
    # A registry with a higher counter scans again.
    assert cache.rows(storage, "ns", 4, scan(5)) == {b"v": b"5"}
    assert cache.label(storage, "ns") == 5
    assert scans == [3, 5]


def test_the_cache_keeps_only_a_higher_label():
    cache = Cache()
    storage = Storage()
    cache.rows(storage, "ns", 1, lambda: ({b"v": b"5"}, 5))
    # A scan with a lower label, from a registry ahead of its rows, gives
    # its rows to its caller, and the cache keeps the higher label.
    assert cache.rows(storage, "ns", 6, lambda: ({b"v": b"4"}, 4)) == {b"v": b"4"}
    assert cache.label(storage, "ns") == 5
    cache.rows(storage, "ns", 6, lambda: ({b"v": b"6"}, 6))
    assert cache.label(storage, "ns") == 6


def test_the_cache_keeps_sixteen_namespaces_for_each_storage():
    cache = Cache()
    storage = Storage()
    for n in range(17):
        cache.rows(storage, f"ns{n}", 1, lambda: ({}, 1))
    names = cache.names(storage)
    assert len(names) == 16
    assert "ns0" not in names
    # A read moves a namespace to the end.
    cache.rows(storage, "ns1", 1, lambda: pytest.fail("no scan"))
    cache.rows(storage, "ns17", 1, lambda: ({}, 1))
    assert "ns1" in cache.names(storage)
    assert "ns2" not in cache.names(storage)


def test_the_cache_holds_a_storage_through_a_weak_reference():
    cache = Cache()
    storage = Storage()
    cache.rows(storage, "ns", 1, lambda: ({}, 1))
    other = Storage()
    cache.rows(other, "ns", 1, lambda: ({}, 1))
    assert cache.label(other, "ns") == 1
    del storage
    gc.collect()
    assert len(cache._entries) == 1
    cache.clear()
    assert cache.label(other, "ns") is None
