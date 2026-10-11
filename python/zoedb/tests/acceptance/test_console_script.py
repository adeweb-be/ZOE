"""Acceptance test of the console script `zoe` of the zoedb wheel.

The wheel installs a console script named `zoe`. It runs the same code as the
`zoe` binary: `zoe convert` turns a Data.fs into a store and back, `zoe pack`
packs a store as of a number of days ago or reports what it would remove,
every option has a `ZOE_*` environment twin, and the exit codes are 0 on
success, 1 on a failure and 2 on a usage error.

The test converts the Data.fs fixture of zoe-convert and reads the result back
with ZODB's FileStorage. Each test imports ZODB itself, so that the module
collects before ZODB is installed.
"""

import importlib.metadata
import os
import subprocess
import sysconfig
from pathlib import Path

import zoe

REPO = Path(__file__).resolve().parents[4]
FIXTURE = REPO / "crates" / "zoe-convert" / "tests" / "data-fs"
SUBCOMMANDS = ["convert", "pack"]
# The classes of the records that a pack of the fixture removes.
PACKED_CLASSES = ["persistent.mapping.PersistentMapping", "ZODB.blob.Blob"]


def script():
    """Return the path of the console script in the environment."""
    return Path(sysconfig.get_path("scripts")) / "zoe"


def run(*args, env=None):
    """Run the console script. Only the variables of env reach it as ZOE_*."""
    environ = {k: v for k, v in os.environ.items() if not k.startswith("ZOE_")}
    environ.update(env or {})
    return subprocess.run(
        [str(script()), *map(str, args)],
        env=environ,
        capture_output=True,
        text=True,
        check=False,
    )


def check(result, code):
    """Check the exit code, and show the output when it is not the one expected."""
    assert result.returncode == code, result.stdout + result.stderr


def convert_fixture(store):
    """Convert the fixture into a store with the console script."""
    check(
        run(
            "convert",
            "--from-data-fs",
            FIXTURE / "Data.fs",
            "--data-fs-blob-dir",
            FIXTURE / "blobs-bushy",
            "--path",
            store,
        ),
        0,
    )


def to_data_fs(store, out):
    """Convert a store to `out/Data.fs` and `out/blobs`. Return their paths."""
    out.mkdir(parents=True, exist_ok=True)
    data_fs, blobs = out / "Data.fs", out / "blobs"
    check(
        run(
            "convert",
            "--to-data-fs",
            data_fs,
            "--data-fs-blob-dir",
            blobs,
            "--path",
            store,
        ),
        0,
    )
    return data_fs, blobs


def contents(data_fs, blobs):
    """Return every transaction of a FileStorage with its records and blobs.

    The status of a transaction is left out: the fixture holds packed
    transactions, and the converter writes every transaction as complete.
    """
    from ZODB.FileStorage import FileStorage
    from ZODB.POSException import POSKeyError

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
                records.append((record.oid, record.data, blob))
            records.sort(key=lambda record: record[0])
            metadata = (txn.user, txn.description, txn.extension_bytes)
            txns.append((txn.tid, metadata, records))
    finally:
        storage.close()
    return txns


def test_the_wheel_installs_the_console_script():
    scripts = importlib.metadata.distribution("zoedb").entry_points
    assert [ep.name for ep in scripts if ep.group == "console_scripts"] == ["zoe"]
    assert script().is_file()


def test_help_lists_the_subcommands():
    result = run("--help")
    check(result, 0)
    for subcommand in SUBCOMMANDS:
        assert subcommand in result.stdout
        check(run(subcommand, "--help"), 0)


def test_version_is_the_version_of_the_wheel():
    result = run("--version")
    check(result, 0)
    assert zoe.__version__ in result.stdout


def test_a_usage_error_exits_with_2(tmp_path):
    store = tmp_path / "zoe"
    for args in [
        [],
        ["no-such-command"],
        ["pack", "--path", store, "--days", "many"],
        ["convert", "--path", store],
    ]:
        check(run(*args), 2)
    assert not store.exists()


def test_convert_round_trips_the_fixture_through_a_store(tmp_path):
    from ZODB import DB
    from ZODB.FileStorage import FileStorage
    from ZODB.scripts import fstest

    source = contents(FIXTURE / "Data.fs", FIXTURE / "blobs-bushy")
    store = tmp_path / "zoe"
    convert_fixture(store)
    data_fs, blobs = to_data_fs(store, tmp_path / "out")

    fstest.check(str(data_fs))
    assert contents(data_fs, blobs) == source

    db = DB(FileStorage(str(data_fs), blob_dir=str(blobs), read_only=True))
    try:
        with db.transaction() as connection:
            root = connection.root()
            assert root["folder"]["doc-1"]["title"] == "first title, changed"
            with root["file"].open() as f:
                assert f.read() == b"third blob revision\n" * 50
    finally:
        db.close()


def test_convert_takes_each_option_from_its_environment_twin(tmp_path):
    store = tmp_path / "zoe"
    env = {
        "ZOE_FROM_DATA_FS": str(FIXTURE / "Data.fs"),
        "ZOE_DATA_FS_BLOB_DIR": str(FIXTURE / "blobs-bushy"),
        "ZOE_PATH": str(store),
    }
    check(run("convert", env=env), 0)

    flags, _ = to_data_fs(store, tmp_path / "flags")
    twins = tmp_path / "twins"
    twins.mkdir()
    env = {
        "ZOE_TO_DATA_FS": str(twins / "Data.fs"),
        "ZOE_DATA_FS_BLOB_DIR": str(twins / "blobs"),
        "ZOE_PATH": str(store),
    }
    check(run("convert", env=env), 0)
    assert (twins / "Data.fs").read_bytes() == flags.read_bytes()


def test_a_file_that_is_not_a_data_fs_exits_with_1(tmp_path):
    path = tmp_path / "Data.fs"
    path.write_bytes(b"this is not a FileStorage file")
    result = run("convert", "--from-data-fs", path, "--path", tmp_path / "zoe")
    check(result, 1)
    assert result.stderr


def test_a_dry_run_reports_by_class_and_removes_nothing(tmp_path):
    store = tmp_path / "zoe"
    convert_fixture(store)
    before = contents(*to_data_fs(store, tmp_path / "before"))

    # Every transaction of the fixture is older than 7 days.
    result = run("pack", "--path", store, "--days", "7", "--dry-run")
    check(result, 0)
    for name in PACKED_CLASSES:
        assert name in result.stdout
    assert contents(*to_data_fs(store, tmp_path / "after")) == before


def test_pack_keeps_one_revision_of_each_live_object(tmp_path):
    from ZODB import DB
    from ZODB.FileStorage import FileStorage

    store = tmp_path / "zoe"
    convert_fixture(store)
    check(run("pack", "--path", store, "--days", "7"), 0)
    data_fs, blobs = to_data_fs(store, tmp_path / "out")

    records = [
        record for _, _, records in contents(data_fs, blobs) for record in records
    ]
    oids = [oid for oid, _, _ in records]
    assert len(oids) == len(set(oids)), "a pack kept two revisions of an object"
    # The fixture writes the oids 0 to 13. The document that it deletes (10)
    # and the object whose creation it undoes (13) are garbage.
    assert {int.from_bytes(oid, "big") for oid in oids} == set(range(13)) - {10}
    assert all(data is not None for _, data, _ in records)

    db = DB(FileStorage(str(data_fs), blob_dir=str(blobs), read_only=True))
    try:
        with db.transaction() as connection:
            root = connection.root()
            assert sorted(root["folder"]) == ["doc-1"]
            assert root["counter"]["value"] == 1
            with root["file"].open() as f:
                assert f.read() == b"third blob revision\n" * 50
    finally:
        db.close()


def test_pack_takes_each_option_from_its_environment_twin(tmp_path):
    store = tmp_path / "zoe"
    convert_fixture(store)
    before = contents(*to_data_fs(store, tmp_path / "before"))

    env = {"ZOE_PATH": str(store), "ZOE_DAYS": "7", "ZOE_DRY_RUN": "true"}
    result = run("pack", env=env)
    check(result, 0)
    for name in PACKED_CLASSES:
        assert name in result.stdout
    assert contents(*to_data_fs(store, tmp_path / "dry")) == before

    env["ZOE_DRY_RUN"] = "false"
    check(run("pack", env=env), 0)
    after = contents(*to_data_fs(store, tmp_path / "packed"))
    assert sum(len(records) for _, _, records in after) < sum(
        len(records) for _, _, records in before
    )


def test_pack_on_a_missing_store_exits_with_1(tmp_path):
    missing = tmp_path / "missing"
    result = run("pack", "--path", missing, "--days", "7")
    check(result, 1)
    assert result.stderr
    assert not missing.exists()
