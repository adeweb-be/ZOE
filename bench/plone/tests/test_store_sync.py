"""A run copies the store of the fixture in full and syncs the copy to the
disk before its server starts. So the kernel writes no page of the copy
during the warmup or the steps, where it would compete with the syncs of
the commits.

    just bench plone test bench/plone/tests/test_store_sync.py
"""

import json
import os
import subprocess
import sys

import pytest
from manifests import write_like_the_build

from plonebench import corpus, fixture, runner, stack

BLOCK = 4096
FILES = (
    "filestorage/Data.fs",
    "filestorage/Data.fs.index",
    "blobstorage/0x00/0x01/0x0000000000000001.blob",
    "zoe/zoe.redb",
    "zoe/blobs/0x00/0x01/0x0000000000000001.blob",
)


def make_store(directory):
    for name in FILES:
        path = directory / "store" / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(os.urandom(BLOCK))
    return directory


def files(path):
    return sorted(str(p.relative_to(path)) for p in path.rglob("*") if p.is_file())


def parts(name):
    return ("filestorage", "blobstorage") if name == "zeo" else ("zoe",)


@pytest.mark.parametrize("name", ["zeo", "zoe-native"])
def test_a_restore_syncs_the_whole_copy(tmp_path, monkeypatch, name):
    source = make_store(tmp_path / "fixture")
    synced = []
    monkeypatch.setattr(stack, "sync_files", lambda path: synced.append(files(path)))
    record = {}
    store = fixture.restore(source, stack.STACKS[name], tmp_path / "run", record)
    copied = [f for f in sorted(FILES) if f.startswith(parts(name))]
    assert store == tmp_path / "run" / "store"
    assert files(store) == copied
    for path in copied:
        assert (store / path).read_bytes() == (source / "store" / path).read_bytes()
    # One sync, after the copy of every file.
    assert synced == [copied]
    assert record["bytes"] == BLOCK * len(copied)
    assert record["copy_seconds"] >= 0 and record["sync_seconds"] >= 0


def test_sync_files_waits_for_the_disk(tmp_path):
    (tmp_path / "file").write_bytes(os.urandom(BLOCK))
    stack.sync_files(tmp_path)


def test_a_run_syncs_its_copy_before_the_server_starts(tmp_path, monkeypatch):
    source = make_store(tmp_path / "fixture")
    write_like_the_build(corpus.plan_site(corpus.PRESETS["small"]), source)
    (source / "equivalence.json").write_text(json.dumps({"equivalent": True}))
    events = []
    copy = stack.copy_store

    def copy_store(origin, destination):
        events.append(f"copy {origin.name}")
        copy(origin, destination)

    def start_server(self, timeout=120.0):
        events.append("server")
        # A process for the sampler to watch.
        self.processes["server"] = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(60)"]
        )

    def start_instances(self, timeout=300.0):
        raise RuntimeError("no instances in this test")

    monkeypatch.setattr(fixture, "fixture_dir", lambda name: source)
    monkeypatch.setattr(stack, "missing_prerequisites", lambda config: [])
    monkeypatch.setattr(stack, "copy_store", copy_store)
    monkeypatch.setattr(stack, "sync_files", lambda path: events.append("sync"))
    monkeypatch.setattr(stack.Stack, "start_server", start_server)
    monkeypatch.setattr(stack.Stack, "start_instances", start_instances)
    options = runner.RunOptions(
        stack="zeo",
        scenario="edit",
        fixture="test",
        steps=[(1, 1.0)],
        out=tmp_path / "run",
    )
    with pytest.raises(RuntimeError, match="no instances"):
        runner.run(options)
    assert events == ["copy filestorage", "copy blobstorage", "sync", "server"]
    result = json.loads((tmp_path / "run" / "result.json").read_text())
    assert sorted(result["restore"]) == ["bytes", "copy_seconds", "sync_seconds"]
    # The run wrote the targets file of the fixture before it started.
    assert (source / "targets.sqlite").exists()
