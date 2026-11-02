"""Acceptance test of the `<zoe>` stanza and the `zoe://` URI.

`%import zoe` and a `<zoe>` section open a `zoe.Storage`, and so does a
`zoe://` URI through zodburi. Every key of the stanza is in kebab-case, has
a `ZOE_*` environment twin, and has the same name in the query of the URI.

Each test imports ZODB, ZConfig and zodburi itself, so that the module
collects before these packages are installed.
"""

import contextlib
import io
import os
import pickle
import re
import tempfile
import threading
import time
import urllib.parse

import pytest
from support import TIMEOUT, ZERO, record
from support import commit_one as commit

import zoe

pytestmark = pytest.mark.usefixtures("no_zoe_environment")

# The keys of the stanza: the keys of the example in the specification,
# read-only, as in <filestorage> and <zeoclient>, and compression, as the
# option of zoe serve.
KEYS = {
    "path",
    "blob-dir",
    "backend",
    "lock-timeout",
    "early-locks",
    "early-lock-classes",
    "read-only",
    "pack-gc",
    "compression",
    "structural-merges",
    "structural-merge-class",
    "posting-dicts",
    "resolution-log",
    "native-reader",
}
# The ways to give a value to a key.
SOURCES = ["stanza", "environment", "uri"]
# A schema that holds one storage section of any type.
SCHEMA = """
<schema>
  <import package="ZODB"/>
  <import package="zoe"/>
  <section type="ZODB.storage" name="*" attribute="storage"/>
</schema>
"""
# The largest tid. loadBefore(oid, MAX_TID) loads the current record.
MAX_TID = (2**63 - 1).to_bytes(8, "big")


def twin(key):
    """Return the name of the environment twin of a stanza key."""
    return "ZOE_" + key.upper().replace("-", "_")


def stanza(path, settings):
    """Return a configuration with one `<zoe>` section."""
    lines = [f"  path {path}"] if path is not None else []
    lines += [f"  {key} {value}" for key, value in settings.items()]
    return "%import zoe\n<zoe>\n" + "".join(line + "\n" for line in lines) + "</zoe>\n"


def open_storage(source, path, settings, monkeypatch):
    """Open a storage in path. Give it settings through one source."""
    import ZODB.config

    if source == "uri":
        import zodburi

        query = urllib.parse.urlencode(settings)
        uri = f"zoe://{path}" + (f"?{query}" if query else "")
        factory, _ = zodburi.resolve_uri(uri)
        return factory()
    if source == "environment":
        for key, value in settings.items():
            monkeypatch.setenv(twin(key), value)
        settings = {}
    return ZODB.config.storageFromString(stanza(path, settings))


@contextlib.contextmanager
def closing(storage):
    """Close storage at the end."""
    try:
        yield storage
    finally:
        storage.close()


def commit_blob(storage, data):
    """Commit one blob in one transaction. Return its oid and tid."""
    from ZODB.Connection import TransactionMetaData

    fd, path = tempfile.mkstemp(dir=storage.temporaryDirectory())
    with os.fdopen(fd, "wb") as file:
        file.write(data)
    blob = pickle.dumps((("ZODB.blob", "Blob"), None), 3) + pickle.dumps(None, 3)
    oid = storage.new_oid()
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    try:
        storage.storeBlob(oid, ZERO, blob, path, "", txn)
        storage.tpc_vote(txn)
        return oid, storage.tpc_finish(txn)
    except Exception:
        storage.tpc_abort(txn)
        raise


def lock_wait(storage):
    """Make a vote wait for a lock that another transaction holds.

    Return the time that the vote waited, and the error that ended it.
    """
    from ZODB.Connection import TransactionMetaData
    from ZODB.POSException import ConflictError

    oid = storage.new_oid()
    serial = commit(storage, oid, ZERO, record(0))
    holder = TransactionMetaData()
    storage.tpc_begin(holder)
    outcome = {}
    try:
        storage.store(oid, serial, record(1), "", holder)
        storage.tpc_vote(holder)

        def contend():
            txn = TransactionMetaData()
            storage.tpc_begin(txn)
            try:
                storage.store(oid, serial, record(2), "", txn)
                start = time.monotonic()
                try:
                    storage.tpc_vote(txn)
                except ConflictError as error:
                    outcome["error"] = error
                outcome["waited"] = time.monotonic() - start
            finally:
                storage.tpc_abort(txn)

        thread = threading.Thread(target=contend, daemon=True)
        thread.start()
        thread.join(TIMEOUT)
        assert not thread.is_alive(), "the vote did not end"
    finally:
        storage.tpc_abort(holder)
    return outcome.get("waited"), outcome.get("error")


def test_the_stanza_opens_a_zoe_storage(tmp_path):
    import ZODB.config

    storage = ZODB.config.storageFromString(stanza(tmp_path / "store", {}))
    with closing(storage):
        assert isinstance(storage, zoe.Storage)
        assert storage.isReadOnly() is False


def test_a_database_with_the_stanza_commits_and_reloads(tmp_path):
    import ZODB.config

    text = "%import zoe\n<zodb>\n  <zoe>\n    path {}\n  </zoe>\n</zodb>\n"
    text = text.format(tmp_path / "store")

    db = ZODB.config.databaseFromString(text)
    try:
        assert isinstance(db.storage, zoe.Storage)
        with db.transaction() as conn:
            conn.root()["value"] = 1
    finally:
        db.close()

    db = ZODB.config.databaseFromString(text)
    try:
        with db.transaction() as conn:
            assert conn.root()["value"] == 1
    finally:
        db.close()


def test_the_uri_opens_a_zoe_storage_and_a_database(tmp_path):
    import ZODB
    import zodburi

    uri = f"zoe://{tmp_path / 'store'}?database_name=main&connection_cache_size=500"
    factory, dbkw = zodburi.resolve_uri(uri)
    assert dbkw["database_name"] == "main"
    assert dbkw["cache_size"] == 500

    storage = factory()
    assert isinstance(storage, zoe.Storage)
    db = ZODB.DB(storage, **dbkw)
    try:
        with db.transaction() as conn:
            conn.root()["value"] = 1
    finally:
        db.close()

    factory, dbkw = zodburi.resolve_uri(uri)
    db = ZODB.DB(factory(), **dbkw)
    try:
        with db.transaction() as conn:
            assert conn.root()["value"] == 1
    finally:
        db.close()


def test_the_stanza_has_the_named_keys_in_kebab_case():
    import ZConfig

    schema = ZConfig.loadSchemaFile(io.StringIO(SCHEMA))
    keys = {key for key, _ in schema.gettype("zoe")}
    assert keys == KEYS
    assert all(re.fullmatch(r"[a-z]+(-[a-z]+)*", key) for key in keys)


def test_path_has_an_environment_twin(tmp_path, monkeypatch):
    import ZODB.config

    monkeypatch.setenv(twin("path"), str(tmp_path / "store"))
    storage = ZODB.config.storageFromString(stanza(None, {}))
    with closing(storage):
        assert isinstance(storage, zoe.Storage)
        oid = storage.new_oid()
        tid = commit(storage, oid, ZERO, record(1))

    with closing(zoe.Storage(str(tmp_path / "store"))) as storage:
        assert tuple(storage.loadBefore(oid, MAX_TID))[:2] == (record(1), tid)


@pytest.mark.parametrize("source", SOURCES)
def test_blob_dir_sets_the_blob_directory(tmp_path, monkeypatch, source):
    blobs = tmp_path / "blobs"
    settings = {"blob-dir": str(blobs)}
    with closing(
        open_storage(source, tmp_path / "store", settings, monkeypatch)
    ) as storage:
        oid, tid = commit_blob(storage, b"synthetic blob")
        path = os.path.realpath(storage.loadBlob(oid, tid))

    assert path.startswith(os.path.realpath(blobs) + os.sep)
    with open(path, "rb") as file:
        assert file.read() == b"synthetic blob"


@pytest.mark.parametrize("source", SOURCES)
def test_lock_timeout_sets_the_longest_lock_wait(tmp_path, monkeypatch, source):
    from ZODB.POSException import ConflictError

    settings = {"lock-timeout": "1s"}
    with closing(
        open_storage(source, tmp_path / "store", settings, monkeypatch)
    ) as storage:
        waited, error = lock_wait(storage)

    # The default lock timeout is much longer than the one that the test sets.
    assert isinstance(error, ConflictError)
    assert 0.5 <= waited < 5


@pytest.mark.parametrize("source", SOURCES)
def test_compression_sets_the_compression_of_new_records(tmp_path, monkeypatch, source):
    # zstd makes a run of one byte almost nothing. Stored raw, it takes its
    # whole size in the database file.
    data = b"x" * (4 << 20)
    sizes = {}
    for value in ("none", "zstd:3"):
        store = tmp_path / value.replace(":", "-")
        settings = {"compression": value}
        with closing(open_storage(source, store, settings, monkeypatch)) as storage:
            commit(storage, storage.new_oid(), ZERO, data)
        sizes[value] = (store / "zoe.redb").stat().st_size
    assert sizes["none"] >= len(data)
    assert sizes["zstd:3"] < len(data) // 2, sizes

    settings = {"compression": "lz4"}
    with pytest.raises(ValueError):
        open_storage(source, tmp_path / "other", settings, monkeypatch).close()


@pytest.mark.parametrize("source", SOURCES)
def test_read_only_refuses_tpc_begin(tmp_path, monkeypatch, source):
    from ZODB.Connection import TransactionMetaData
    from ZODB.POSException import ReadOnlyError

    with closing(zoe.Storage(str(tmp_path / "store"))) as storage:
        commit(storage, storage.new_oid(), ZERO, record(1))

    settings = {"read-only": "true"}
    with closing(
        open_storage(source, tmp_path / "store", settings, monkeypatch)
    ) as storage:
        assert storage.isReadOnly() is True
        with pytest.raises(ReadOnlyError):
            storage.tpc_begin(TransactionMetaData())


@pytest.mark.parametrize("source", SOURCES)
def test_backend_takes_redb_and_refuses_others(tmp_path, monkeypatch, source):
    settings = {"backend": "redb"}
    with closing(
        open_storage(source, tmp_path / "store", settings, monkeypatch)
    ) as storage:
        assert isinstance(storage, zoe.Storage)

    settings = {"backend": "no-such-backend"}
    with pytest.raises(ValueError):
        open_storage(source, tmp_path / "other", settings, monkeypatch).close()


@pytest.mark.parametrize("source", SOURCES)
def test_early_locks_opens_when_off_and_on_and_refuses_a_bad_value(
    tmp_path, monkeypatch, source
):
    for value in ("off", "on"):
        settings = {"early-locks": value}
        with closing(
            open_storage(source, tmp_path / value, settings, monkeypatch)
        ) as storage:
            assert isinstance(storage, zoe.Storage)

    # A value that is not a boolean fails the open.
    settings = {"early-locks": "maybe"}
    with pytest.raises(ValueError):
        open_storage(source, tmp_path / "maybe", settings, monkeypatch).close()


@pytest.mark.parametrize("key", ["lock_timeout", "no-such-key"])
def test_the_uri_refuses_a_key_that_the_stanza_does_not_have(tmp_path, key):
    import zodburi

    # The URI without a query opens a storage, so the refusal below comes
    # from the key and not from a missing resolver.
    uri = f"zoe://{tmp_path / 'store'}"
    factory, _ = zodburi.resolve_uri(uri)
    factory().close()

    with pytest.raises((KeyError, ValueError)):
        factory, _ = zodburi.resolve_uri(f"{uri}?{key}=1s")
        factory().close()
