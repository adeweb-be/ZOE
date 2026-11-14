"""The plans of a catalog fixture for `zoe-bench catalog`, and the run of
`zoe-bench catalog` on a copy of the fixture.

    python -m plonebench.catalog plans [--fixture NAME] [--log LOG...] [--expect]
    python -m plonebench.catalog run [--fixture NAME] [--scenario S,...] [OPTIONS]

`plans` runs `plans_zope.py` once in a Zope process of the zoeclient
scenario, on the ZOE store of the fixture, with the backend `catalog` of
zoe.plone: the fixture gets its `ZoeTextIndex` indexes, the text deltas of
its entries and the registration key of its catalog, and the plans file
`plans.msgpack` beside its manifest. Then `zoe catalog rebuild-text` builds
the text index of each mode, and `zoe-bench catalog --set-mode` keeps the
index of the mode `compat` beside the store, so a run changes the mode
with no rebuild. A store of an older format moves to the format of the
binary with `zoe migrate` first. The fixture then holds:

    plans.msgpack       the plans file
    store/zoe/          the store, its texts, its text index in the mode
                        enhanced
    store/zoe.text-compat/   the text index in the mode compat
    plans/              the Zope home and the logs of the step

`run` copies the store and the kept index to `target/bench/catalog/work`,
runs `zoe-bench catalog` on the copy with the plans file, writes the
result to `target/bench/catalog/<started>.json` and the Markdown report to
`target/bench/catalog/<started>.md`, and removes the copy. The result
records the commit of the repository and whether its tree differs.
"""

from __future__ import annotations

import json
import shutil
import subprocess
import time
from pathlib import Path

from benchcommon import provenance

from plonebench import fixture as b5fixture
from plonebench import stack
from plonebench.catalog import fixture, plan
from plonebench.catalog import log as querylog

PLANS = "plans.msgpack"
SCRIPT = Path(__file__).resolve().parent / "plans_zope.py"
WORK = fixture.WORK
# The scenarios of `zoe-bench catalog`; the first four are the default set.
SCENARIOS = ("cold", "intersect", "mix", "text", "text-rebuild")
# The entries of each commit of the texts, the folders of each write, and
# the live texts of the sample.
BATCH = 5_000
RENAME_BATCH = 20
SAMPLE = 2_000
# The rows that the writes change on `catalog-1m`: past the default
# `catalog-rebase-rows` of the mirror, 20,000.
RENAME_ROWS_1M = 24_000


def zoe_bench_binary() -> Path:
    """The `zoe-bench` binary: `ZOE_BENCH_BINARY`, else the release build of
    the repository."""
    import os

    if binary := os.environ.get("ZOE_BENCH_BINARY"):
        return Path(binary)
    target = stack.REPO / (os.environ.get("CARGO_TARGET_DIR") or "target")
    return target / "release" / "zoe-bench"


def default_fixture() -> str:
    """The name of the fixture of `catalog-1m`."""
    spec = plan.PRESETS["catalog-1m"]
    return fixture.default_name(spec, "catalog-1m")


def rename_rows(entries: int) -> int:
    """The rows that the writes change for a fixture of `entries`."""
    return RENAME_ROWS_1M if entries >= 1_000_000 else max(50, entries // 40)


def preset_of(manifest: dict) -> str:
    """The preset whose spec equals the spec of the fixture, else
    `custom`: a fixture of a preset with another seed is `custom`."""
    for name, spec in plan.PRESETS.items():
        if spec.to_json() == plan.CatalogSpec.from_json(manifest["spec"]).to_json():
            return name
    return "custom"


def zope_conf(home: Path, store: Path, catalog_memory: str | None) -> str:
    """The zope.conf of the step: the zoeclient scenario on the store in
    the process, with the backend `catalog` in the mode `on`."""
    instancehome = (
        stack.PLONE / stack.STACKS["zoe-native"].scenario / "parts" / "instance"
    )
    memory = f"\n      catalog-memory {catalog_memory}" if catalog_memory else ""
    return f"""%import zoe
%define INSTANCEHOME {instancehome}
instancehome $INSTANCEHOME
%define CLIENTHOME {home / "var"}
clienthome $CLIENTHOME
debug-mode off
security-policy-implementation C
verbose-security off
default-zpublisher-encoding utf-8
<environment>
    CHAMELEON_CACHE {home / "var" / "cache"}
    ZOE_PLONE_BACKENDS catalog
    ZOE_PLONE_CATALOG_MODE on
</environment>
<zodb_db main>
    cache-size 100000
    <zoe>
      path {store}{memory}
    </zoe>
    mount-point /
</zodb_db>
"""


def _run(command: list[str], log: Path, what: str) -> None:
    """Run `command`, its output to `log`. Raise RuntimeError with its tail
    when it fails."""
    with open(log, "w") as out:
        code = subprocess.run(
            command,
            env=stack.clean_environment(),
            stdout=out,
            stderr=subprocess.STDOUT,
            check=False,
        ).returncode
    if code != 0:
        tail = "\n".join(log.read_text().splitlines()[-30:])
        raise RuntimeError(f"{what} failed with {code}:\n{tail}")


def make(
    target: Path,
    logs: list[str] | None = None,
    expect: bool = False,
    catalog_memory: str | None = None,
    rows: int | None = None,
) -> Path:
    """Make the plans file of the fixture in `target`, from the logs `logs`.
    With `expect`, each plan holds the answer of the stock search. Return
    the path of the plans file."""
    manifest = json.loads((target / "manifest.json").read_text())
    store = target / "store" / "zoe"
    b5fixture.migrate(store)
    home = target / "plans"
    for sub in ("var/cache", "log"):
        (home / sub).mkdir(parents=True, exist_ok=True)
    merged = home / "queries.jsonl"
    with open(merged, "w") as out:
        if logs:
            from plonebench.catalog import replay

            for line in querylog.read_all(replay.log_files(logs)):
                out.write(json.dumps(line, sort_keys=True) + "\n")
    spec = plan.CatalogSpec.from_json(manifest["spec"])
    settings = {
        "name": target.name,
        "preset": preset_of(manifest),
        "counts": manifest["counts"],
        "spec": manifest["spec"],
        "queries": manifest["queries"],
        "expect": expect,
        "batch": BATCH,
        "rename_rows": rows if rows is not None else rename_rows(spec.entries),
        "rename_batch": RENAME_BATCH,
        "sample": SAMPLE,
        "text_timeout": 600,
    }
    (home / "settings.json").write_text(json.dumps(settings, indent=1))
    conf = home / "zope.conf"
    conf.write_text(zope_conf(home, store, catalog_memory))
    out = home / PLANS
    started = time.time()
    _run(
        [
            str(stack.interpreter(stack.STACKS["zoe-native"])),
            str(stack.INZOPE / "run.py"),
            str(conf),
            str(SCRIPT),
            str(home / "settings.json"),
            str(merged),
            str(out),
        ],
        home / "log" / "plans.txt",
        "plans_zope.py",
    )
    print(f"plonebench: plans made in {time.time() - started:.0f} s", flush=True)
    text_indexes(store, out, home / "log")
    shutil.move(out, target / PLANS)
    return target / PLANS


def text_indexes(store: Path, plans: Path, logs: Path) -> None:
    """Build the text index of the store in each mode with `zoe catalog
    rebuild-text`, and keep the index of the mode `compat` beside the
    store: the store ends in the mode `enhanced`."""
    zoe = str(stack.zoe_binary())
    bench = [str(zoe_bench_binary()), "catalog", "--zoe", zoe, "--store", str(store)]
    bench += ["--plans", str(plans)]
    for mode in ("enhanced", "compat"):
        started = time.time()
        _run(bench + ["--set-mode", mode], logs / f"set-mode-{mode}.txt", "set-mode")
        _run(
            [zoe, "catalog", "rebuild-text", str(store)],
            logs / f"rebuild-text-{mode}.txt",
            "zoe catalog rebuild-text",
        )
        print(
            f"plonebench: the text index in the mode {mode} in"
            f" {time.time() - started:.0f} s",
            flush=True,
        )
    _run(bench + ["--set-mode", "enhanced"], logs / "set-mode.txt", "set-mode")


def run(name: str | None, args: list[str]) -> Path:
    """Run `zoe-bench catalog` with `args` on a copy of the fixture `name`.
    Return the path of the result."""
    target = fixture.fixture_dir(name or default_fixture())
    plans = target / PLANS
    if not plans.is_file():
        raise FileNotFoundError(
            f"{plans} is missing: run"
            f" `just bench catalog plans --fixture {target.name}`"
        )
    work = WORK / "work"
    shutil.rmtree(work, ignore_errors=True)
    store = work / "zoe"
    print(f"plonebench: copying the store of {target.name}", flush=True)
    stack.copy_store(target / "store" / "zoe", store)
    for kept in (target / "store").glob("zoe.text-*"):
        stack.copy_store(kept, work / kept.name)
    started = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    out = WORK / f"{started}.json"
    report = WORK / f"{started}.md"
    dirty = provenance.dirty()
    env = stack.clean_environment(
        ZOE_BENCH_COMMIT=provenance.commit() or "",
        ZOE_BENCH_DIRTY="" if dirty is None else str(dirty).lower(),
    )
    if dirty is None:
        env.pop("ZOE_BENCH_DIRTY")
    command = [
        str(zoe_bench_binary()),
        "catalog",
        "--zoe",
        str(stack.zoe_binary()),
        "--store",
        str(store),
        "--plans",
        str(plans),
        "--markdown",
        str(report),
        *args,
    ]
    try:
        done = subprocess.run(command, env=env, stdout=subprocess.PIPE, check=False)
        if done.returncode != 0:
            raise RuntimeError(f"zoe-bench catalog exited with {done.returncode}")
        out.write_bytes(done.stdout)
    finally:
        shutil.rmtree(work, ignore_errors=True)
    print(report.read_text())
    print(f"plonebench: the result is in {out}")
    return out
