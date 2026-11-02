"""The catalog fixture: built once, in a FileStorage and in a ZOE store with
the same catalog.

`prepare` runs `inzope/build_catalog.py` in a Zope process of the zeo
scenario on a new FileStorage, packs the `Data.fs`, converts it with
`zoe convert`, then reads the digest of the catalog through each store
(`inzope/catalog_digest.py`, read-only, with no configuration of Zope) and
refuses the fixture when the two digests differ, or when a count or a hit
of a query differs from the plan. The fixture lives under
`target/bench/catalog/fixtures/<name>`:

    spec.json           the CatalogSpec
    plan.json           the counts and the query set of the plan, from the
                        build
    store/filestorage/Data.fs     the packed FileStorage
    store/zoe/          the ZOE store
    convert.txt         the report of zoe convert
    digest-filestorage.json, digest-zoe.json
    manifest.json       the counts by index and column, the query set, the
                        digest, the commit, the bytes and the seconds of
                        each phase; written last
    build/log/          the logs of the build

No run opens these stores in place: a reader copies them first, or opens
them read-only.
"""

from __future__ import annotations

import json
import shutil
import subprocess
import time
from pathlib import Path

from plonebench import environment, stack
from plonebench.catalog import plan

WORK = stack.REPO / "target" / "bench" / "catalog"
# The objects in the cache of the connection of the build. The build
# minimises the cache after each commit.
BUILD_CACHE = 200_000
# The build process gives each set the same order, so KeywordIndex keeps
# the keywords of an entry in the same order in each build.
HASH_SEED = "0"


def fixture_dir(name: str) -> Path:
    return WORK / "fixtures" / name


def default_name(spec: plan.CatalogSpec, preset: str | None) -> str:
    return f"{preset or 'custom'}-{spec.digest()}"


def zope_conf(home: Path, data_fs: Path) -> str:
    """The zope.conf of the build: the zeo scenario on a FileStorage of its
    own."""
    instancehome = stack.PLONE / stack.STACKS["zeo"].scenario / "parts" / "instance"
    return f"""%define INSTANCEHOME {instancehome}
instancehome $INSTANCEHOME
%define CLIENTHOME {home / "var"}
clienthome $CLIENTHOME
debug-mode off
security-policy-implementation C
verbose-security off
default-zpublisher-encoding utf-8
<environment>
    CHAMELEON_CACHE {home / "var" / "cache"}
</environment>
<zodb_db main>
    cache-size {BUILD_CACHE}
    <filestorage>
      path {data_fs}
    </filestorage>
    mount-point /
</zodb_db>
"""


def compare(
    spec: plan.CatalogSpec, planned: dict, left: dict, right: dict
) -> list[str]:
    """Compare the digests of the two stores, and each with the plan.
    Return the differences, at most 20."""
    differences = []
    for key in sorted(set(left) | set(right)):
        if left.get(key) != right.get(key):
            differences.append(f"the stores differ in {key}")
    counts = planned["counts"]
    if left["size"] != spec.entries:
        differences.append(f"the catalog holds {left['size']} entries")
    for part in ("indexes", "columns"):
        read = {name: item["entries"] for name, item in left[part].items()}
        if read != counts[part]:
            names = sorted(set(read) | set(counts[part]))
            for name in names:
                if read.get(name) != counts[part].get(name):
                    differences.append(
                        f"{part} {name}: {read.get(name)} entries, the plan "
                        f"gives {counts[part].get(name)}"
                    )
    for query in planned["queries"]:
        hits = left["queries"].get(query["name"])
        if hits != query["count"]:
            differences.append(
                f"query {query['name']}: {hits} hits, the plan gives {query['count']}"
            )
    return differences[:20]


def prepare(
    spec: plan.CatalogSpec,
    target: Path,
    preset: str | None = None,
    force: bool = False,
) -> Path:
    """Build the fixture of `spec` in the directory `target`. Return
    `target`. An existing fixture stays, unless `force`."""
    if (target / "manifest.json").exists() and not force:
        print(f"plonebench: the fixture {target.name} exists; --force builds it again")
        return target
    missing = [
        message
        for name in ("zeo", "zoe-native")
        for message in stack.missing_prerequisites(stack.STACKS[name])
    ]
    if missing:
        raise RuntimeError("missing: " + "; ".join(missing))
    if target.exists():
        shutil.rmtree(target)
    store = target / "store"
    data_fs = store / "filestorage" / "Data.fs"
    data_fs.parent.mkdir(parents=True)
    (target / "spec.json").write_text(json.dumps(spec.to_json(), indent=1))
    started = time.time()

    print(f"plonebench: building {spec.entries} catalog entries", flush=True)
    _build(target, data_fs)
    planned = json.loads((target / "plan.json").read_text())
    seconds = dict(planned["seconds"])
    unpacked = data_fs.stat().st_size

    phase = time.time()
    _pack(data_fs)
    seconds["pack"] = round(time.time() - phase, 1)

    phase = time.time()
    print("plonebench: converting with zoe convert", flush=True)
    result = subprocess.run(
        [
            str(stack.zoe_binary()),
            "convert",
            "--from-data-fs",
            str(data_fs),
            "--path",
            str(store / "zoe"),
        ],
        capture_output=True,
        text=True,
        env=stack.clean_environment(),
    )
    (target / "convert.txt").write_text(result.stdout + result.stderr)
    if result.returncode != 0:
        raise RuntimeError(f"zoe convert failed:\n{result.stdout}{result.stderr}")
    seconds["convert"] = round(time.time() - phase, 1)

    digests = {}
    for kind, name, path in (
        ("filestorage", "zeo", data_fs),
        ("zoe", "zoe-native", store / "zoe"),
    ):
        phase = time.time()
        print(
            f"plonebench: reading the digest of the catalog through {kind}", flush=True
        )
        out = target / f"digest-{kind}.json"
        _digest(stack.STACKS[name], kind, path, out, target / "plan.json")
        digests[kind] = json.loads(out.read_text())
        seconds[f"digest_{kind}"] = round(time.time() - phase, 1)
    seconds["total"] = round(time.time() - started, 1)

    differences = compare(spec, planned, digests["filestorage"], digests["zoe"])
    manifest = {
        "name": target.name,
        "preset": preset,
        "spec": spec.to_json(),
        "shape": vars(plan.shape(spec)),
        "site": plan.SITE_ID,
        "counts": planned["counts"],
        "queries": planned["queries"],
        "digest": digests["filestorage"]["digest"],
        "equivalent": not differences,
        "differences": differences,
        "commit": environment.git("rev-parse", "HEAD"),
        "dirty": environment.dirty(),
        "seconds": seconds,
        "bytes": {
            "data_fs_before_pack": unpacked,
            "data_fs": data_fs.stat().st_size,
            "zoe": _size(store / "zoe"),
        },
    }
    (target / "manifest.json").write_text(json.dumps(manifest, indent=1))
    shutil.rmtree(target / "build" / "var", ignore_errors=True)
    if differences:
        (target / "manifest.json").rename(target / "refused.json")
        raise RuntimeError(f"the fixture is refused: {differences}")
    print(
        f"plonebench: fixture {target.name}: {spec.entries} entries, digest "
        f"{manifest['digest']}, {seconds['total']} s",
        flush=True,
    )
    return target


def _build(target: Path, data_fs: Path) -> None:
    """Run build_catalog.py in a Zope process on the FileStorage `data_fs`.
    The log of stdout gets each line at once, and each line of progress
    goes to stdout too."""
    config = stack.STACKS["zeo"]
    home = target / "build"
    for sub in ("var/cache", "log"):
        (home / sub).mkdir(parents=True, exist_ok=True)
    conf = home / "zope.conf"
    conf.write_text(zope_conf(home, data_fs))
    command = [
        str(stack.interpreter(config)),
        str(stack.INZOPE / "run.py"),
        str(conf),
        "build_catalog.py",
        str(target / "spec.json"),
        str(target / "plan.json"),
    ]
    out = home / "log" / "build_catalog.py.out"
    err = home / "log" / "build_catalog.py.err"
    with open(out, "w", buffering=1) as stdout, open(err, "w") as stderr:
        process = subprocess.Popen(
            command,
            env=stack.clean_environment(PYTHONHASHSEED=HASH_SEED),
            cwd=home,
            stdout=subprocess.PIPE,
            stderr=stderr,
            text=True,
        )
        try:
            for line in process.stdout:
                stdout.write(line)
                print(f"plonebench: {line.strip()}", flush=True)
        except BaseException:
            process.kill()
            process.wait()
            raise
        code = process.wait()
    if code != 0:
        tail = "\n".join(err.read_text().splitlines()[-30:])
        raise RuntimeError(f"build_catalog.py failed with {code}:\n{tail}")


def _pack(data_fs: Path) -> None:
    """Pack the FileStorage `data_fs` to now. The pack keeps no old file."""
    from ZODB.FileStorage import FileStorage
    from ZODB.serialize import referencesf

    before = data_fs.stat().st_size
    started = time.time()
    storage = FileStorage(str(data_fs), pack_keep_old=False)
    try:
        storage.pack(time.time(), referencesf)
    finally:
        storage.close()
    print(
        f"plonebench: packed Data.fs from {before / 1e9:.2f} GB to "
        f"{data_fs.stat().st_size / 1e9:.2f} GB in {time.time() - started:.1f} s",
        flush=True,
    )


def _digest(
    config: stack.StackConfig, kind: str, path: Path, out: Path, queries: Path
) -> None:
    """Run catalog_digest.py with the interpreter of `config` on the store
    at `path`."""
    result = subprocess.run(
        [
            str(stack.interpreter(config)),
            str(stack.INZOPE / "catalog_digest.py"),
            kind,
            str(path),
            str(out),
            str(queries),
        ],
        capture_output=True,
        text=True,
        env=stack.clean_environment(),
        cwd=out.parent,
    )
    log = out.parent / "build" / "log" / f"digest-{kind}.txt"
    log.write_text(result.stdout + result.stderr)
    if result.returncode != 0:
        tail = "\n".join(result.stderr.splitlines()[-30:])
        raise RuntimeError(f"catalog_digest.py on {kind} failed:\n{tail}")


def _size(path: Path) -> int:
    return sum(p.stat().st_size for p in path.rglob("*") if p.is_file())


def fixtures() -> list[str]:
    """The names of the fixtures under WORK, with their size and digest."""
    lines = []
    for path in sorted((WORK / "fixtures").glob("*/manifest.json")):
        data = json.loads(path.read_text())
        size = data["counts"]["size"]
        lines.append(f"{path.parent.name}: {size} entries, digest {data['digest']}")
    return lines
