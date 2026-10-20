"""Unit tests of the shape of the units and of the pack of a fixture.

- `split` gives whole parts that add up to the total, inside their bounds.
- The fields of the units, of the catalog and of the pack are in the JSON
  of a spec only when they are set.
- The generic attributes of an item are a function of the seed and the
  path.
- `fixture._pack` leaves the live records of a FileStorage, and no old
  file of a pack.
"""

import json
import random

import pytest
import transaction
from persistent.mapping import PersistentMapping
from ZODB import DB
from ZODB.blob import Blob
from ZODB.FileStorage import FileStorage
from ZODB.serialize import referencesf
from ZODB.utils import z64

from plonebench import corpus, fixture


@pytest.mark.parametrize(
    "total, weights, least, most",
    [
        (100, [1.0, 2.0, 3.0], 0, 0),
        (10, [1.0] * 7, 1, 0),
        (1_000, [1.0, 1.0, 50.0, 0.1], 1, 400),
        (34_000, corpus.quantile_weights(random.Random(1), 240, 1.3), 1, 1_450),
    ],
)
def test_split_gives_exact_parts_inside_their_bounds(total, weights, least, most):
    parts = corpus.split(total, weights, least, most)
    assert len(parts) == len(weights)
    assert sum(parts) == total
    assert min(parts) >= least
    if most:
        assert max(parts) <= most


def test_split_follows_the_weights():
    assert corpus.split(10, [1.0, 1.0, 3.0]) == [2, 2, 6]
    # The excess of a capped part goes to the others.
    assert corpus.split(10, [1.0, 1.0, 8.0], most=4) == [3, 3, 4]


def test_split_refuses_a_total_out_of_the_bounds():
    with pytest.raises(ValueError):
        corpus.split(2, [1.0, 1.0, 1.0], least=1)
    with pytest.raises(ValueError):
        corpus.split(10, [1.0, 1.0], most=4)


def test_the_quantile_weights_do_not_depend_on_the_seed():
    a = corpus.quantile_weights(random.Random(1), 50, 1.3)
    b = corpus.quantile_weights(random.Random(2), 50, 1.3)
    assert a != b
    assert sorted(a) == sorted(b)


def test_a_spec_of_the_tree_has_no_field_of_the_units():
    data = corpus.PRESETS["small"].to_json()
    assert not set(corpus.OPTIONAL_FIELDS) & set(data)
    assert not corpus.PRESETS["small"].units


def test_a_spec_of_the_units_has_its_fields():
    spec = corpus.PRESETS["xlarge"]
    data = spec.to_json()
    assert spec.units
    assert data["leaf_folders"] == 7_200
    assert data["pack_every"] == 50_000
    # The summary of a campaign holds the spec: numbers and lists only.
    for value in json.loads(json.dumps(data)).values():
        values = value if isinstance(value, list) else [value]
        assert all(type(v) in (int, float) for v in values), value


def test_the_shared_folder_follows_the_shape():
    assert corpus.shared_folder(corpus.PRESETS["small"]) == "bench/shared"
    assert corpus.shared_folder(corpus.PRESETS["xlarge-mini"]) == (
        "bench/shared/folder-00"
    )


def test_the_generic_attributes_are_a_function_of_the_path():
    plan = corpus.plan_site(corpus.PRESETS["xlarge-mini"])
    indexes = dict(corpus.catalog_indexes(plan.spec))
    assert len(indexes) == 25
    assert len(corpus.catalog_columns(plan.spec)) == 21
    path = plan.by_type("Document")[0].path
    values = corpus.catalog_values(plan, path)
    assert values == corpus.catalog_values(plan, path)
    assert values != corpus.catalog_values(plan, plan.by_type("File")[0].path)
    assert set(values) == set(indexes)
    for name, value in values.items():
        kind = indexes[name]
        if kind == "KeywordIndex":
            assert 1 <= len(value) <= 3
        elif kind == "DateIndex":
            assert 0 <= value <= 3650
        elif kind == "BooleanIndex":
            assert isinstance(value, bool)
    assert corpus.catalog_indexes(corpus.PRESETS["small"]) == []


def records_oids_and_live(path) -> tuple[int, int, int]:
    storage = FileStorage(str(path), read_only=True)
    try:
        records = sum(1 for txn in storage.iterator() for _ in txn)
        seen = {z64}
        todo = [z64]
        while todo:
            data, _ = storage.load(todo.pop())
            for oid in referencesf(data):
                if oid not in seen:
                    seen.add(oid)
                    todo.append(oid)
        return records, len(storage), len(seen)
    finally:
        storage.close()


def test_the_pack_leaves_the_live_records_and_no_old_file(tmp_path):
    store = tmp_path / "store"
    (store / "filestorage").mkdir(parents=True)
    data_fs = store / "filestorage" / "Data.fs"
    db = DB(FileStorage(str(data_fs), blob_dir=str(store / "blobstorage")))
    connection = db.open()
    root = connection.root()
    root["kept"] = PersistentMapping()
    root["dropped"] = PersistentMapping()
    root["blob"] = Blob(b"first")
    transaction.commit()
    for n in range(3):
        root["kept"]["n"] = n
        transaction.commit()
    with root["blob"].open("w") as file:
        file.write(b"second")
    del root["dropped"]
    transaction.commit()
    connection.close()
    db.close()
    # The files that a pack through ZEO keeps.
    (store / "filestorage" / "Data.fs.old").write_bytes(b"old")
    (store / "blobstorage.old").mkdir()

    fixture._pack(store)

    records, oids, live = records_oids_and_live(data_fs)
    # The root, the kept mapping and the blob.
    assert records == oids == live == 3
    assert not (store / "filestorage" / "Data.fs.old").exists()
    assert not (store / "blobstorage.old").exists()
    db = DB(FileStorage(str(data_fs), blob_dir=str(store / "blobstorage")))
    try:
        root = db.open().root()
        assert root["kept"]["n"] == 2
        with root["blob"].open() as file:
            assert file.read() == b"second"
    finally:
        db.close()
