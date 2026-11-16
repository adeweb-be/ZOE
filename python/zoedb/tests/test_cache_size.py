"""Acceptance test of the page cache of an in-process store.

`zoe.Storage(path, cache_size=BYTES)` and the key `cache-size` of `<zoe>`
set the most bytes that the page cache of the store holds, as
`--cache-size` of `zoe serve` does. Without them, the store keeps the
default of the store.
"""

import pytest
import ZODB.config
import zodburi
from support import ZERO, commit_one, record

import zoe

pytestmark = pytest.mark.usefixtures("no_zoe_environment")

# The largest tid. loadBefore(oid, MAX_TID) loads the current record.
MAX_TID = (2**63 - 1).to_bytes(8, "big")
SMALL = 16 * 2**20


def stanza(*lines):
    """Return a configuration with one `<zoe>` section of lines."""
    body = "".join(f"  {line}\n" for line in lines)
    return f"%import zoe\n<zoe>\n{body}</zoe>\n"


def open_stanza(*lines):
    return ZODB.config.storageFromString(stanza(*lines))


def write_and_read(storage):
    """Commit one record to storage, then read it back."""
    oid = storage.new_oid()
    tid = commit_one(storage, oid, ZERO, record(1))
    assert tuple(storage.loadBefore(oid, MAX_TID))[:2] == (record(1), tid)
    return oid, tid


def test_the_keyword_sets_the_page_cache(tmp_path):
    storage = zoe.Storage(str(tmp_path / "store"), cache_size=SMALL)
    try:
        write_and_read(storage)
    finally:
        storage.close()


def test_the_key_of_the_stanza_sets_the_page_cache(tmp_path):
    storage = open_stanza(f"path {tmp_path / 'store'}", "cache-size 16MB")
    try:
        assert isinstance(storage, zoe.Storage)
        write_and_read(storage)
    finally:
        storage.close()


def test_the_stanza_the_twin_and_the_uri_give_the_bytes(tmp_path, monkeypatch):
    monkeypatch.setattr(zoe, "Storage", lambda path, **options: options)
    options = open_stanza(f"path {tmp_path}", "cache-size 16MB")
    assert options["cache_size"] == SMALL
    monkeypatch.setenv("ZOE_CACHE_SIZE", "256MB")
    options = open_stanza(f"path {tmp_path}")
    assert options["cache_size"] == 256 * 2**20
    # A key of the section wins over its twin.
    options = open_stanza(f"path {tmp_path}", "cache-size 16MB")
    assert options["cache_size"] == SMALL
    monkeypatch.delenv("ZOE_CACHE_SIZE")
    factory, _ = zodburi.resolve_uri(f"zoe://{tmp_path}?cache-size=16MB")
    assert factory()["cache_size"] == SMALL
    # Without the key and the twin, the default of the store stays.
    options = open_stanza(f"path {tmp_path}")
    assert options["cache_size"] is None


def test_a_bad_value_raises_the_error_of_a_bad_catalog_memory(tmp_path):
    with pytest.raises(ValueError) as catalog:
        open_stanza(f"path {tmp_path / 'store'}", "catalog-memory lots")
    with pytest.raises(ValueError) as cache:
        open_stanza(f"path {tmp_path / 'store'}", "cache-size lots")
    assert type(cache.value) is type(catalog.value)
    assert "cache-size" in str(cache.value)
    assert "lots" in str(cache.value)


def test_without_the_key_the_store_opens_as_before(tmp_path):
    path = tmp_path / "store"
    storage = open_stanza(f"path {path}")
    try:
        oid, tid = write_and_read(storage)
    finally:
        storage.close()
    # A store opens with a small cache and keeps its records.
    storage = zoe.Storage(str(path), cache_size=SMALL)
    try:
        assert tuple(storage.loadBefore(oid, MAX_TID))[:2] == (record(1), tid)
    finally:
        storage.close()
    storage = zoe.Storage(str(path))
    try:
        assert tuple(storage.loadBefore(oid, MAX_TID))[:2] == (record(1), tid)
    finally:
        storage.close()
