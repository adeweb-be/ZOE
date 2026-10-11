"""Unit tests of the `<zoe>` section and the `zoe://` URI.

The acceptance test covers each key through the section, its twin and the
URI. These tests cover the rest: the datatypes of the keys, which source
wins, the empty values, the missing path, the errors and the form of the
URI.
"""

import io
import os

import pytest
import ZConfig
import ZODB.config
import zodburi

from zoe import config

SCHEMA = """
<schema>
  <import package="ZODB"/>
  <import package="zoe"/>
  <section type="ZODB.storage" name="*" attribute="storage"/>
</schema>
"""


@pytest.fixture(autouse=True)
def no_zoe_environment(monkeypatch):
    """Remove each ZOE_* variable, so that only the test sets them."""
    for name in list(os.environ):
        if name.startswith("ZOE_"):
            monkeypatch.delenv(name)


def stanza(*lines):
    """Return a configuration with one `<zoe>` section of lines."""
    body = "".join(f"  {line}\n" for line in lines)
    return f"%import zoe\n<zoe>\n{body}</zoe>\n"


def open_stanza(*lines):
    return ZODB.config.storageFromString(stanza(*lines))


def blob_directory(storage):
    """Return the real path of the blob directory of storage."""
    return os.path.dirname(os.path.realpath(storage.temporaryDirectory()))


def test_the_section_and_the_uri_convert_each_key_the_same_way():
    schema = ZConfig.loadSchemaFile(io.StringIO(SCHEMA))
    datatypes = {key: info.datatype for key, info in schema.gettype("zoe")}
    assert datatypes.keys() == config.KEYS.keys()
    for key, datatype in datatypes.items():
        assert datatype is config.KEYS[key], key


def test_a_twin_is_zoe_and_the_key_in_snake_case():
    assert config.twin("path") == "ZOE_PATH"
    assert config.twin("blob-dir") == "ZOE_BLOB_DIR"
    assert config.twin("lock-timeout") == "ZOE_LOCK_TIMEOUT"


def test_lock_timeout_takes_whole_seconds():
    seconds = config.KEYS["lock-timeout"]
    assert seconds("10") == 10
    assert seconds("10s") == 10
    assert seconds("2m") == 120
    with pytest.raises(ValueError):
        seconds("1.5s")


def test_a_key_of_the_section_wins_over_its_twin(tmp_path, monkeypatch):
    monkeypatch.setenv("ZOE_PATH", str(tmp_path / "other"))
    monkeypatch.setenv("ZOE_READ_ONLY", "true")
    storage = open_stanza(f"path {tmp_path / 'store'}", "read-only false")
    try:
        assert storage.isReadOnly() is False
        assert storage.getName() == os.path.realpath(tmp_path / "store")
    finally:
        storage.close()
    assert not (tmp_path / "other").exists()


def test_a_twin_sets_a_missing_key(tmp_path, monkeypatch):
    open_stanza(f"path {tmp_path / 'store'}").close()

    monkeypatch.setenv("ZOE_READ_ONLY", "true")
    storage = open_stanza(f"path {tmp_path / 'store'}")
    try:
        assert storage.isReadOnly() is True
    finally:
        storage.close()


def test_an_empty_twin_counts_as_not_set(tmp_path, monkeypatch):
    monkeypatch.setenv("ZOE_READ_ONLY", "")
    monkeypatch.setenv("ZOE_BLOB_DIR", "")
    storage = open_stanza(f"path {tmp_path / 'store'}")
    try:
        assert storage.isReadOnly() is False
        assert blob_directory(storage) == os.path.realpath(tmp_path / "store" / "blobs")
    finally:
        storage.close()


def test_a_missing_path_raises_value_error(monkeypatch):
    with pytest.raises(ValueError, match="ZOE_PATH"):
        open_stanza()
    monkeypatch.setenv("ZOE_PATH", "")
    with pytest.raises(ValueError, match="ZOE_PATH"):
        open_stanza()


def test_a_bad_twin_names_the_variable(tmp_path, monkeypatch):
    monkeypatch.setenv("ZOE_LOCK_TIMEOUT", "1.5s")
    with pytest.raises(ValueError, match="ZOE_LOCK_TIMEOUT='1.5s'"):
        open_stanza(f"path {tmp_path / 'store'}")


def test_a_bad_value_in_the_section_raises_value_error(tmp_path):
    for line in ("lock-timeout 1.5s", "read-only maybe", "backend fdb"):
        with pytest.raises(ValueError):
            open_stanza(f"path {tmp_path / 'store'}", line)


def test_the_uri_does_not_read_the_twins(tmp_path, monkeypatch):
    monkeypatch.setenv("ZOE_READ_ONLY", "true")
    monkeypatch.setenv("ZOE_BACKEND", "no-such-backend")
    factory, _ = zodburi.resolve_uri(f"zoe://{tmp_path / 'store'}")
    storage = factory()
    try:
        assert storage.isReadOnly() is False
    finally:
        storage.close()


def test_the_uri_decodes_the_path_and_the_query(tmp_path):
    path = tmp_path / "a store"
    blobs = tmp_path / "the blobs"
    uri = f"zoe://{str(path).replace(' ', '%20')}?blob-dir={blobs}".replace(" ", "+")
    factory, _ = zodburi.resolve_uri(uri)
    storage = factory()
    try:
        assert storage.getName() == os.path.realpath(path)
        assert blob_directory(storage) == os.path.realpath(blobs)
    finally:
        storage.close()


def test_a_bad_value_in_the_uri_names_the_key(tmp_path):
    with pytest.raises(ValueError, match="lock-timeout='soon'"):
        zodburi.resolve_uri(f"zoe://{tmp_path}?lock-timeout=soon")


@pytest.mark.parametrize(
    "uri",
    [
        "zoe://host/store",
        "zoe://store",
        "zoe:///store#fragment",
        "zoe:store",
        "zoe://",
    ],
)
def test_the_uri_takes_only_an_absolute_path(uri):
    with pytest.raises(ValueError, match="zoe:///absolute/path"):
        zodburi.resolve_uri(uri)


def test_the_uri_does_not_take_path_in_the_query(tmp_path):
    with pytest.raises(KeyError):
        zodburi.resolve_uri(f"zoe://{tmp_path}?path={tmp_path}")
