"""The copy of the store for a run writes what the last run changed, not
the whole store.

`fixture.restore(FIXTURE_DIR, CONFIG, RUN_DIR)` gives the store of a run
at `RUN_DIR/store`, equal to the store of the fixture, file for file and
byte for byte. At the end of a run, where the runner removes the store
today, it calls `fixture.release(FIXTURE_DIR, CONFIG, STORE)`. Then the
next restore of the same fixture and stack writes at most a few MB more
than the bytes that the run changed. A store that a run keeps
(`--keep-store`, or a failed run) stays where it is, and the next restore
still gives the fixture.

On ext4 a copy of the store of `xlarge` writes 6 GB for `zeo` and 11 GB
for a stack of `zoe serve`, at about 100 MB/s on the reference machine.

The bytes that the restore writes are the `wchar` of `/proc/self/io`,
which counts the children of the process too. So the check of the bytes
runs on Linux only.

    just bench plone test bench/plone/tests/test_store_copy.py
"""

import os
import sys
from pathlib import Path

import pytest

from plonebench import fixture, stack

MIB = 2**20
# The large file of each store, and the files of its blob directory.
LARGE = 24 * MIB
BLOBS = 64
BLOB = 16_384
# What a run changes: bytes at the end of the large file, and one page in
# its middle.
APPENDED = MIB
PAGE = 65_536
# The most bytes that a restore may write after a run.
SLACK = 4 * MIB
STACKS = ["zeo", "zoe-native"]


def write(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)


def make_fixture(directory: Path) -> Path:
    """A fixture directory with a store of each server and no site."""
    store = directory / "store"
    write(store / "filestorage" / "Data.fs", os.urandom(LARGE))
    write(store / "filestorage" / "Data.fs.index", os.urandom(PAGE))
    write(store / "zoe" / "zoe.redb", os.urandom(LARGE))
    write(store / "blobstorage" / ".layout", b"bushy")
    for n in range(BLOBS):
        name = f"0x{n:016x}"
        write(
            store / "blobstorage" / "0x00" / f"0x{n:02x}" / f"{name}.blob",
            os.urandom(BLOB),
        )
        write(
            store / "zoe" / "blobs" / "0x00" / f"0x{n:02x}" / f"{name}.blob",
            os.urandom(BLOB),
        )
    return directory


def tree(path: Path) -> dict[str, bytes]:
    return {
        str(p.relative_to(path)): p.read_bytes()
        for p in sorted(path.rglob("*"))
        if p.is_file()
    }


def parts(config: stack.StackConfig) -> list[str]:
    return ["filestorage", "blobstorage"] if config.server == "zeo" else ["zoe"]


def assert_is_the_fixture(store: Path, source: Path, config) -> None:
    for part in parts(config):
        assert tree(store / part) == tree(source / "store" / part), part


def large_file(store: Path, config) -> Path:
    if config.server == "zeo":
        return store / "filestorage" / "Data.fs"
    return store / "zoe" / "zoe.redb"


def run_on(store: Path, config) -> None:
    """Change the store as a run does: append to the large file, write a
    page in its middle, add files and remove one."""
    large = large_file(store, config)
    with open(large, "r+b") as file:
        file.seek(LARGE // 2)
        file.write(os.urandom(PAGE))
        file.seek(0, os.SEEK_END)
        file.write(os.urandom(APPENDED))
    (large.parent / (large.name + ".lock")).write_bytes(b"1234")
    blobs = store / ("blobstorage" if config.server == "zeo" else "zoe/blobs")
    write(blobs / "0x00" / "0xff" / "new.blob", os.urandom(BLOB))
    (blobs / "0x00" / "0x00" / f"0x{0:016x}.blob").unlink()


def written() -> int:
    with open("/proc/self/io") as file:
        for line in file:
            if line.startswith("wchar:"):
                return int(line.split()[1])
    raise AssertionError("no wchar in /proc/self/io")


@pytest.fixture
def source(tmp_path) -> Path:
    return make_fixture(tmp_path / "fixture")


@pytest.mark.skipif(sys.platform != "linux", reason="counts the bytes in /proc/self/io")
@pytest.mark.parametrize("name", STACKS)
def test_a_restore_after_a_run_writes_what_the_run_changed(source, tmp_path, name):
    config = stack.STACKS[name]
    first = fixture.restore(source, config, tmp_path / "run-1")
    assert first == tmp_path / "run-1" / "store"
    assert_is_the_fixture(first, source, config)
    run_on(first, config)
    fixture.release(source, config, first)

    before = written()
    second = fixture.restore(source, config, tmp_path / "run-2")
    cost = written() - before
    assert second == tmp_path / "run-2" / "store"
    assert_is_the_fixture(second, source, config)
    assert cost <= APPENDED + PAGE + SLACK, f"the restore wrote {cost} bytes"


@pytest.mark.parametrize("name", STACKS)
def test_a_restore_after_a_kept_store_gives_the_fixture(source, tmp_path, name):
    config = stack.STACKS[name]
    kept = fixture.restore(source, config, tmp_path / "run-1")
    run_on(kept, config)
    changed = tree(kept)
    # No release: the run kept its store, or failed.
    second = fixture.restore(source, config, tmp_path / "run-2")
    assert_is_the_fixture(second, source, config)
    assert tree(kept) == changed
