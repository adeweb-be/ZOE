"""The fixture: one synthetic site, built once, in a FileStorage and in a
ZOE store with the same content.

`prepare` builds the site on a ZEO server with a new FileStorage, converts
the `Data.fs` with `zoe convert`, then reads the logical digest of the site
through each store and compares them. When `pack_every` of the spec is not
0, the build packs the FileStorage through ZEO after each `pack_every`
items, and `prepare` removes the old files that each pack keeps
(`Data.fs.old` and `blobstorage.old`). Then `prepare` stops the server and
packs the FileStorage once more, before the digest and the conversion. So
the fixture holds no history of its build, and the history of the build
does not fill the disk. The fixture lives under
`target/bench/plone/fixtures/<name>`:

    spec.json           the SiteSpec
    credentials.json    the password of the accounts (mode 0600)
    manifest.json       the plan, with UIDs, states and versions
    targets.sqlite      what Locust and the verification read, from the
                        manifest (`workload.write_targets`)
    store/filestorage/Data.fs, store/blobstorage/   the ZEO store
    store/zoe/          the ZOE store (zoe.redb and blobs/)
    convert.txt         the report of zoe convert

`zoe convert` writes the records with the default compression of the store at
the commit that ran it: raw since ADR 0041, zstd before. `equivalence.json`
records that commit under `converted_at`. `zoe serve` reads every codec, so
an older fixture still runs, but its records stay compressed: prepare it again
with `--force` to measure raw records.
    digest-zeo.json, digest-zoe.json, equivalence.json

A run never opens these stores: it copies them first (`restore`), and
syncs the copy to the disk before its server starts.
"""

from __future__ import annotations

import json
import os
import secrets
import shutil
import subprocess
import time
from pathlib import Path

from plonebench import corpus, environment, stack, workload

# The start of the line that build_site.py prints after each pack.
PACKED = "build_site: packed"


def fixture_dir(name: str) -> Path:
    return stack.WORK / "fixtures" / name


def default_name(spec: corpus.SiteSpec, preset: str | None) -> str:
    return f"{preset or 'custom'}-{spec.digest()}"


def compare(left: dict, right: dict) -> dict:
    """Compare two digests. Return the differences, at most 20."""
    differences = []
    for key in ("counts", "users", "groups"):
        if left.get(key) != right.get(key):
            differences.append(f"{key} differ")
    if left["catalog"] != right["catalog"]:
        differences.append("catalog differs")
    a, b = left["objects"], right["objects"]
    for path in sorted(set(a) | set(b)):
        if a.get(path) != b.get(path):
            differences.append(f"object {path} differs")
            if len(differences) >= 20:
                break
    return {
        "equivalent": not differences,
        "objects": len(a),
        "catalog_entries": left["catalog"]["size"],
        "differences": differences,
    }


def prepare(
    spec: corpus.SiteSpec, name: str, settings: stack.Settings, force: bool = False
) -> Path:
    target = fixture_dir(name)
    if (target / "equivalence.json").exists() and not force:
        print(f"plonebench: the fixture {name} exists; --force builds it again")
        return target
    if target.exists():
        shutil.rmtree(target)
    (target / "store" / "filestorage").mkdir(parents=True)
    (target / "spec.json").write_text(json.dumps(spec.to_json(), indent=1))
    credentials = target / "credentials.json"
    with open(os.open(credentials, os.O_WRONLY | os.O_CREAT, 0o600), "w") as file:
        json.dump({"password": secrets.token_urlsafe(18)}, file)

    # Build on ZEO.
    store = target / "store"
    zeo = stack.Stack(stack.STACKS["zeo"], settings, target / "build", store)
    started = time.time()
    zeo.start_server()
    try:
        print("plonebench: building the site on ZEO", flush=True)
        _build(zeo, target, credentials)
        if spec.pack_every:
            zeo.stop()
            _pack(store)
            zeo.start_server()
        zeo.run_script("digest.py", str(target / "digest-zeo.json"), name="digest")
    finally:
        zeo.stop()
    built = time.time() - started

    # Convert the Data.fs to a ZOE store.
    print("plonebench: converting with zoe convert", flush=True)
    result = subprocess.run(
        [
            str(stack.zoe_binary()),
            "convert",
            "--from-data-fs",
            str(target / "store" / "filestorage" / "Data.fs"),
            "--data-fs-blob-dir",
            str(target / "store" / "blobstorage"),
            "--path",
            str(target / "store" / "zoe"),
        ],
        capture_output=True,
        text=True,
        env=stack.clean_environment(),
    )
    (target / "convert.txt").write_text(result.stdout + result.stderr)
    if result.returncode != 0:
        raise RuntimeError(f"zoe convert failed:\n{result.stdout}{result.stderr}")

    # Read the digest through zoe serve, on a copy, so the fixture stays
    # as the conversion wrote it.
    copy = target / "check"
    stack.copy_store(target / "store" / "zoe", copy / "store" / "zoe")
    zoe = stack.Stack(
        stack.STACKS["zoe-native"], settings, copy / "run", copy / "store"
    )
    zoe.start_server()
    try:
        zoe.run_script("digest.py", str(target / "digest-zoe.json"), name="digest")
    finally:
        zoe.stop()
    shutil.rmtree(copy)

    left = json.loads((target / "digest-zeo.json").read_text())
    right = json.loads((target / "digest-zoe.json").read_text())
    equivalence = compare(left, right)
    equivalence["build_seconds"] = round(built, 1)
    equivalence["converted_at"] = environment.git("rev-parse", "HEAD")
    equivalence["store_bytes"] = {
        "filestorage": _size(target / "store" / "filestorage"),
        "blobstorage": _size(target / "store" / "blobstorage"),
        "zoe": _size(target / "store" / "zoe"),
    }
    (target / "equivalence.json").write_text(json.dumps(equivalence, indent=1))
    shutil.rmtree(target / "build", ignore_errors=True)
    workload.write_targets(target)
    verdict = "equivalent" if equivalence["equivalent"] else "NOT equivalent"
    print(f"plonebench: fixture {name}: {equivalence['objects']} objects, {verdict}")
    if not equivalence["equivalent"]:
        raise RuntimeError(f"the stores differ: {equivalence['differences']}")
    return target


def _build(zeo: stack.Stack, target: Path, credentials: Path) -> None:
    """Run build_site.py in a client of the server of `zeo`, as
    `Stack.run_script` does. After each pack of the build, remove the old
    files of the pack."""
    home = zeo.run / "builder"
    conf = zeo.prepare_home(home, "builder")
    command = [
        str(stack.interpreter(zeo.config)),
        str(stack.INZOPE / "run.py"),
        str(conf),
        "build_site.py",
        str(target / "spec.json"),
        str(credentials),
        str(target / "manifest.json"),
    ]
    out = home / "log" / "build_site.py.out"
    err = home / "log" / "build_site.py.err"
    # The log of stdout gets each line at once, so a long build shows its
    # progress there.
    with open(out, "w", buffering=1) as stdout, open(err, "w") as stderr:
        process = subprocess.Popen(
            command,
            env=stack.clean_environment(),
            cwd=home,
            stdout=subprocess.PIPE,
            stderr=stderr,
            text=True,
        )
        try:
            for line in process.stdout:
                stdout.write(line)
                if line.startswith(PACKED):
                    _remove_old(zeo.store)
                    print(f"plonebench: {line.strip()}", flush=True)
        except BaseException:
            # The caller stops the server next. Stop the build first, so
            # that it does not write to a server that is gone.
            process.kill()
            process.wait()
            raise
        code = process.wait()
    if code != 0:
        tail = "\n".join(err.read_text().splitlines()[-30:])
        raise RuntimeError(f"build_site.py failed with {code}:\n{tail}")


def _remove_old(store: Path) -> None:
    """Remove the files that a pack of the FileStorage of `store` keeps:
    the file before the pack, and the blobs that the pack removed."""
    (store / "filestorage" / "Data.fs.old").unlink(missing_ok=True)
    shutil.rmtree(store / "blobstorage.old", ignore_errors=True)


def _pack(store: Path) -> None:
    """Pack the FileStorage of `store` to now, with no server on it. The
    pack keeps no old file."""
    from ZODB.FileStorage import FileStorage
    from ZODB.serialize import referencesf

    data_fs = store / "filestorage" / "Data.fs"
    _remove_old(store)
    before = data_fs.stat().st_size
    started = time.time()
    storage = FileStorage(
        str(data_fs), blob_dir=str(store / "blobstorage"), pack_keep_old=False
    )
    try:
        storage.pack(time.time(), referencesf)
    finally:
        storage.close()
    print(
        f"plonebench: packed Data.fs from {before / 1e9:.2f} GB to "
        f"{data_fs.stat().st_size / 1e9:.2f} GB in {time.time() - started:.1f} s",
        flush=True,
    )


def _size(path: Path) -> int:
    return sum(p.stat().st_size for p in path.rglob("*") if p.is_file())


def restore(
    fixture: Path, config: stack.StackConfig, run: Path, record: dict | None = None
) -> Path:
    """Copy the store of the stack from the fixture into the run, then
    sync the copy to the disk. So the kernel writes no page of the copy
    during the warmup or the steps, where it would compete with the syncs
    of the commits. When `record` is a dict, it gets the bytes of the copy
    and the seconds of the copy and of the sync."""
    store = run / "store"
    if store.exists():
        shutil.rmtree(store)
    started = time.time()
    if config.server == "zeo":
        stack.copy_store(fixture / "store" / "filestorage", store / "filestorage")
        stack.copy_store(fixture / "store" / "blobstorage", store / "blobstorage")
    else:
        stack.copy_store(fixture / "store" / "zoe", store / "zoe")
    copied = time.time()
    stack.sync_files(store)
    if record is not None:
        record["bytes"] = _size(store)
        record["copy_seconds"] = copied - started
        record["sync_seconds"] = time.time() - copied
    return store
