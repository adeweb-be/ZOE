"""The fixture: one synthetic site, built once, in a FileStorage and in a
ZOE store with the same content.

`prepare` builds the site on a ZEO server with a new FileStorage, converts
the `Data.fs` with `zoe convert`, then reads the logical digest of the site
through each store and compares them. The fixture lives under
`target/bench/plone/fixtures/<name>`:

    spec.json           the SiteSpec
    credentials.json    the password of the accounts (mode 0600)
    manifest.json       the plan, with UIDs, states and versions
    store/filestorage/Data.fs, store/blobstorage/   the ZEO store
    store/zoe/          the ZOE store (zoe.redb and blobs/)
    convert.txt         the report of zoe convert
    digest-zeo.json, digest-zoe.json, equivalence.json

A run never opens these stores: it copies them first (`restore`).
"""

from __future__ import annotations

import json
import os
import secrets
import shutil
import subprocess
import time
from pathlib import Path

from plonebench import corpus, stack


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
    zeo = stack.Stack(stack.STACKS["zeo"], settings, target / "build", target / "store")
    started = time.time()
    zeo.start_server()
    try:
        print("plonebench: building the site on ZEO", flush=True)
        zeo.run_script(
            "build_site.py",
            str(target / "spec.json"),
            str(credentials),
            str(target / "manifest.json"),
            name="builder",
        )
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
    equivalence["store_bytes"] = {
        "filestorage": _size(target / "store" / "filestorage"),
        "blobstorage": _size(target / "store" / "blobstorage"),
        "zoe": _size(target / "store" / "zoe"),
    }
    (target / "equivalence.json").write_text(json.dumps(equivalence, indent=1))
    shutil.rmtree(target / "build", ignore_errors=True)
    verdict = "equivalent" if equivalence["equivalent"] else "NOT equivalent"
    print(f"plonebench: fixture {name}: {equivalence['objects']} objects, {verdict}")
    if not equivalence["equivalent"]:
        raise RuntimeError(f"the stores differ: {equivalence['differences']}")
    return target


def _size(path: Path) -> int:
    return sum(p.stat().st_size for p in path.rglob("*") if p.is_file())


def restore(fixture: Path, config: stack.StackConfig, run: Path) -> Path:
    """Copy the store of the stack from the fixture into the run."""
    store = run / "store"
    if store.exists():
        shutil.rmtree(store)
    if config.server == "zeo":
        stack.copy_store(fixture / "store" / "filestorage", store / "filestorage")
        stack.copy_store(fixture / "store" / "blobstorage", store / "blobstorage")
    else:
        stack.copy_store(fixture / "store" / "zoe", store / "zoe")
    return store
