"""Acceptance test: `zoe verify` finds what ZODB's own tools find.

The console script `zoe verify --path STORE --format json` runs without a
Python interpreter behind it: it reads the references with zoe-pickle. On
the same store, its findings match what ZODB finds through Python:

- the dangling references: each persistent reference of a current record,
  as `ZODB.serialize.referencesf` reads it, that names an oid without a
  current record;
- the classes: the class of each current record, as
  `ZODB.utils.get_pickle_metadata` reads it, with its count. The classes
  that Python cannot import are the ones that zodbverify reports.

The store is synthetic: a site of persistent mappings, a dangling
reference, an object of a class that no module provides, and a deleted
object.
"""

import collections
import importlib
import io
import json
import pickle
import subprocess
import sysconfig
from pathlib import Path

import zoe

ZERO = bytes(8)


def p64(n):
    return n.to_bytes(8, "big")


class Ref:
    """A persistent reference to a PersistentMapping, in a hand-made state."""

    def __init__(self, oid):
        self.oid = oid


class _Pickler(pickle.Pickler):
    def persistent_id(self, obj):
        from persistent.mapping import PersistentMapping

        if isinstance(obj, Ref):
            return (p64(obj.oid), PersistentMapping)
        return None


def record(module, name, state):
    """A ZODB record of the class module.name with state.

    The class pickle is written by hand, so the class need not exist.
    """
    out = io.BytesIO()
    out.write(b"\x80\x03c" + module.encode() + b"\n" + name.encode() + b"\nq\x00.")
    _Pickler(out, 3).dump(state)
    return out.getvalue()


def build(path):
    from ZODB.Connection import TransactionMetaData

    def commit(stores, deletions=()):
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        for oid, serial, data in stores:
            storage.store(p64(oid), serial, data, "", txn)
        for oid, serial in deletions:
            storage.deleteObject(p64(oid), serial, txn)
        storage.tpc_vote(txn)
        return storage.tpc_finish(txn)

    storage = zoe.Storage(str(path))
    try:
        mapping = ("persistent.mapping", "PersistentMapping")
        first = commit(
            [
                (0, ZERO, record(*mapping, {"data": {"a": Ref(1), "b": Ref(2)}})),
                (1, ZERO, record(*mapping, {"data": {"lost": Ref(40)}})),
                (2, ZERO, record("no.such.module", "Thing", {"x": 1})),
                (3, ZERO, record(*mapping, {"data": {}})),
            ]
        )
        commit([], deletions=[(3, first)])
    finally:
        storage.close()


def expected(path):
    """The findings of ZODB's own code on the store."""
    from ZODB.serialize import referencesf
    from ZODB.utils import get_pickle_metadata, u64

    storage = zoe.Storage(str(path), read_only=True)
    try:
        current = {}
        next_oid = None
        while True:
            oid, _, data, next_oid = storage.record_iternext(next_oid)
            current[u64(oid)] = data
            if next_oid is None:
                break
    finally:
        storage.close()

    dangling = set()
    classes = collections.Counter()
    for oid, data in current.items():
        for ref in referencesf(data):
            if u64(ref) not in current:
                dangling.add((oid, u64(ref)))
        module, name = get_pickle_metadata(data)
        classes[f"{module}.{name}"] += 1
    return dangling, dict(classes)


def importable(qualified):
    module, _, name = qualified.rpartition(".")
    try:
        return hasattr(importlib.import_module(module), name)
    except ImportError:
        return False


def run_verify(path, *args):
    script = Path(sysconfig.get_path("scripts")) / "zoe"
    result = subprocess.run(
        [str(script), "verify", "--path", str(path), "--format", "json", *args],
        capture_output=True,
        text=True,
        check=False,
    )
    return result.returncode, json.loads(result.stdout)


def test_the_findings_match_what_zodb_finds(tmp_path):
    path = tmp_path / "store"
    build(path)
    dangling, classes = expected(path)
    assert dangling == {(1, 40)}

    code, report = run_verify(path)
    assert code == 1
    found = {(item["oid"], item["missing"]) for item in report["references"]}
    assert found == dangling
    assert report["classes"] == classes
    assert report["checksums"] == []
    assert report["blobs"] == []

    # The classes that Python cannot import are those outside the list of
    # the classes that it can.
    known = tmp_path / "classes.txt"
    known.write_text("".join(f"{c}\n" for c in classes if importable(c)))
    code, report = run_verify(path, "--classes", str(known))
    assert sorted(report["unknown_classes"]) == sorted(
        c for c in classes if not importable(c)
    )
    assert report["unknown_classes"] == ["no.such.module.Thing"]
