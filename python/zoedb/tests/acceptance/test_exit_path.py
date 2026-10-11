"""Acceptance test of the exit path: ZODB opens what the Rust converter writes.

The test writes a synthetic ``Data.fs`` and its blobs with ZODB. The Rust test
``data_fs_round_trips_through_zoe`` of zoe-convert converts them to a ZOE store
and back to ``out/Data.fs`` and ``out/blobs``. ZODB's FileStorage must open the
result, read every transaction, record and blob of the source from it, find no
format error, and commit on top of it.
"""

import os
import shutil
import subprocess
from pathlib import Path

import transaction
import ZODB
from BTrees.OOBTree import OOBTree
from persistent.mapping import PersistentMapping
from ZODB.blob import Blob
from ZODB.FileStorage import FileStorage
from ZODB.POSException import POSKeyError
from ZODB.scripts import fstest

REPO = Path(__file__).resolve().parents[4]
TREE = {f"key-{i:03}": f"value {i}" for i in range(100)}


def set_metadata(connection, user, **extension):
    """Set the user and the extension data of the transaction of a connection."""
    txn = connection.transaction_manager.get()
    txn.user = user
    for key, value in extension.items():
        txn.extension[key] = value


def write_source(data_fs, blobs):
    """Write a store with a BTree, a blob in two revisions and an undo."""
    db = ZODB.DB(FileStorage(str(data_fs), blob_dir=str(blobs)))
    try:
        with db.transaction("create content") as connection:
            set_metadata(connection, "someone", source="exit path test")
            root = connection.root()
            root["tree"] = tree = OOBTree()
            tree.update(TREE)
            root["document"] = PersistentMapping(title="first")
            root["file"] = Blob(b"first blob revision\n" * 100)
        with db.transaction("edit") as connection:
            set_metadata(connection, "editor")
            root = connection.root()
            root["document"]["title"] = "second"
            with root["file"].open("w") as f:
                f.write(b"second blob revision\n" * 100)
        with db.transaction("add an object to undo") as connection:
            connection.root()["temporary"] = PersistentMapping()
        db.undo(db.undoLog(0, 1)[0]["id"])
        transaction.commit()
    finally:
        db.close()


def convert(work):
    """Run the Rust converter on ``work/Data.fs`` and ``work/blobs``."""
    cargo = shutil.which("cargo")
    assert cargo, "the exit-path test runs the Rust converter with cargo"
    command = [
        cargo,
        "test",
        "--locked",
        "--quiet",
        "--package",
        "zoe-convert",
        "--test",
        "to_data_fs",
        "--",
        "--exact",
        "data_fs_round_trips_through_zoe",
    ]
    env = dict(os.environ, ZOE_EXIT_PATH_DIR=str(work))
    result = subprocess.run(
        command, cwd=REPO, env=env, capture_output=True, text=True, check=False
    )
    assert result.returncode == 0, result.stdout + result.stderr


def contents(data_fs, blobs):
    """Return every transaction of a FileStorage, with its records and blobs."""
    storage = FileStorage(str(data_fs), blob_dir=str(blobs), read_only=True)
    txns = []
    try:
        for txn in storage.iterator():
            records = []
            for record in txn:
                try:
                    with open(storage.loadBlob(record.oid, record.tid), "rb") as f:
                        blob = f.read()
                except POSKeyError:
                    blob = None
                records.append((record.oid, record.tid, record.data, blob))
            records.sort(key=lambda record: record[0])
            metadata = (txn.user, txn.description, txn.extension_bytes)
            txns.append((txn.tid, txn.status, metadata, records))
    finally:
        storage.close()
    return txns


def test_zodb_opens_the_data_fs_that_the_converter_writes(tmp_path):
    write_source(tmp_path / "Data.fs", tmp_path / "blobs")
    source = contents(tmp_path / "Data.fs", tmp_path / "blobs")

    convert(tmp_path)

    out = tmp_path / "out"
    data_fs, blobs = out / "Data.fs", out / "blobs"
    fstest.check(str(data_fs))
    assert contents(data_fs, blobs) == source
    # The undo wrote a record without data for the undone creation.
    assert any(record[2] is None for _, _, _, records in source for record in records)

    db = ZODB.DB(FileStorage(str(data_fs), blob_dir=str(blobs)))
    try:
        with db.transaction("written on the converted file") as connection:
            root = connection.root()
            assert root["document"]["title"] == "second"
            assert dict(root["tree"].items()) == TREE
            with root["file"].open() as f:
                assert f.read() == b"second blob revision\n" * 100
            assert "temporary" not in root
            root["after"] = PersistentMapping(note="after the exit path")
    finally:
        db.close()

    # The file opens again with the new transaction, and stays well formed.
    fstest.check(str(data_fs))
    after = contents(data_fs, blobs)
    assert after[: len(source)] == source
    assert len(after) == len(source) + 1
