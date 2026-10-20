"""B5 runs on `xlarge-mini`, the shape of `xlarge` at about 1/100, on demand.

- A run of `mixed` with 4 Zope instances, steps of 5 s, ends with the
  verification `pass`, on `zeo` and on `zoe-native-qi-kv` (with the
  profile `zoe.plone:default`, as the showcase runs it).
- After a run of `edit` on `zeo`, the verification passes. On a copy of
  the store of that run, it fails when an edited Document lost its last
  edit, when the catalog misses an item that the run did not touch, and
  when the catalog misses a neighbour of an edited Document while the
  length of the catalog stays the same (`tests/scripts/damage.py`).
- The observer of the verification reads only a part of the site: the
  content objects that it loads are fewer than half the items of the
  site (`tests/scripts/load_probe.py`). It reads the objects that the
  ledger names, their containers and a fixed sample, and the rest from
  the catalog.

The fixture is `xlarge-mini-<digest>` under `target/bench/plone/fixtures/`.
The module builds it when it is missing, for about 4 minutes. The runs
use a copy of it, `test-today-xlarge-mini-<digest>`, that holds only the
files that a fixture holds today: so they show that a fixture that exists
runs with no new build, and that the runner writes what Locust needs
when it is missing. The runs
take about 10 minutes and the CPUs of the machine: run nothing else
beside them. They need `just plone zeo build`, `just plone zoeclient
build` and the release `zoe` (`cargo build --release -p zoe-server --bin
zoe`, or `ZOE_BINARY`):

    ZOE_BENCH_B5_MINI=1 just bench plone test bench/plone/tests/test_b5_mini.py

The servers listen on the ports of the defaults plus 500.
`ZOE_BENCH_B5_PORT_OFFSET` gives another offset. The directories of the
runs stay under the temporary directory of pytest.
"""

import dataclasses
import json
import os
import shutil
import subprocess
from pathlib import Path

import pytest

from plonebench import corpus, fixture, ledger, runner, stack

pytestmark = pytest.mark.skipif(
    os.environ.get("ZOE_BENCH_B5_MINI") != "1",
    reason="runs B5 on xlarge-mini for about 15 minutes: set ZOE_BENCH_B5_MINI=1",
)

SCRIPTS = Path(__file__).resolve().parent / "scripts"
PRESET = "xlarge-mini"
STEPS = [(4, 5.0), (16, 5.0)]
INSTANCES = 4
PROFILES = {"zoe-native-qi-kv": "zoe.plone:default"}
# The files of a fixture that `prepare` writes today, beside its stores.
TODAY = (
    "spec.json",
    "credentials.json",
    "manifest.json",
    "convert.txt",
    "digest-zeo.json",
    "digest-zoe.json",
    "equivalence.json",
)


def settings() -> stack.Settings:
    offset = int(os.environ.get("ZOE_BENCH_B5_PORT_OFFSET", "500"))
    defaults = stack.Settings()
    return dataclasses.replace(
        defaults,
        instances=INSTANCES,
        # The layout of the capacity set: one physical core for each
        # instance.
        server_cpus="0",
        zope_cpus=("1", "3", "6", "8"),
        locust_cpus="12,13",
        base_port=defaults.base_port + offset,
        server_port=defaults.server_port + offset,
        admin_port=defaults.admin_port + offset,
    )


@pytest.fixture(scope="module")
def mini() -> Path:
    """A copy of the fixture with only the files of a fixture of today."""
    spec = corpus.PRESETS[PRESET]
    name = fixture.default_name(spec, PRESET)
    source = fixture.prepare(spec, name, settings())
    target = fixture.fixture_dir("test-today-" + name)
    if target.exists():
        shutil.rmtree(target)
    target.mkdir(parents=True)
    for file in TODAY:
        shutil.copy2(source / file, target / file)
    shutil.copytree(source / "store", target / "store")
    return target


def run(fixture_dir: Path, name: str, scenario: str, out: Path, keep=False) -> dict:
    options = runner.RunOptions(
        stack=name,
        scenario=scenario,
        fixture=fixture_dir.name,
        steps=STEPS,
        warmup=5.0,
        trim=1.0,
        keep_store=keep,
        settings=settings(),
        out=out,
        profile=PROFILES.get(name),
    )
    runner.run(options)
    return json.loads((out / "result.json").read_text())


@pytest.mark.parametrize("name", ["zeo", "zoe-native-qi-kv"])
def test_a_run_with_4_instances_ends_with_verification_pass(mini, name, tmp_path):
    result = run(mini, name, "mixed", tmp_path / "run")
    assert result["stack"]["settings"]["instances"] == INSTANCES
    verification = result["verification"]
    assert verification["verdict"] == "pass", verification
    checks = verification["checks"]
    for check in ("edits", "created", "workflow", "catalog"):
        assert checks[check]["checked"] > 0, (check, checks[check])


@pytest.fixture(scope="module")
def edit_run(mini, tmp_path_factory) -> tuple[Path, dict]:
    out = tmp_path_factory.mktemp("edit") / "run"
    return out, run(mini, "zeo", "edit", out, keep=True)


def manifest_of(fixture_dir: Path) -> dict:
    return json.loads((fixture_dir / "manifest.json").read_text())


def edited(run_dir: Path) -> list[str]:
    """The Documents of acknowledged edits, the last edit first."""
    records = ledger.read([str(run_dir / "ledger" / "run.jsonl")])
    done = sorted(
        (r for r in records if r.kind == "edit" and r.outcome == ledger.OK),
        key=lambda r: r.end,
        reverse=True,
    )
    return list(dict.fromkeys(r.target for r in done))


def verify_damaged(mini, edit_run, tmp_path, mode: str, *args: str) -> dict:
    """Damage a copy of the store of the edit run, then verify it."""
    source, _ = edit_run
    config = stack.STACKS["zeo"]
    store = tmp_path / "store"
    for part in ("filestorage", "blobstorage"):
        stack.copy_store(source / "store" / part, store / part)
    # The verification of the copy reads the ledger of the edit run.
    run_dir = tmp_path / "run"
    shutil.copytree(source / "ledger", run_dir / "ledger")
    st = stack.Stack(config, settings(), run_dir, store)
    st.start_server()
    try:
        home = run_dir / "damage"
        conf = st.prepare_home(home, "damage")
        done = subprocess.run(
            [
                str(stack.interpreter(config)),
                str(stack.INZOPE / "run.py"),
                str(conf),
                str(SCRIPTS / "damage.py"),
                mode,
                *args,
            ],
            capture_output=True,
            text=True,
            env=stack.clean_environment(),
            cwd=home,
        )
        assert done.returncode == 0, done.stderr[-3000:]
        return runner._verify(st, run_dir, mini)
    finally:
        st.stop()


def test_the_verification_passes_after_an_edit_run(edit_run):
    run_dir, result = edit_run
    verification = result["verification"]
    assert verification["verdict"] == "pass", verification
    assert verification["checks"]["edits"]["checked"] > 0
    assert edited(run_dir)


def test_the_verification_fails_when_an_edited_item_lost_its_edit(
    mini, edit_run, tmp_path
):
    run_dir, _ = edit_run
    target = edited(run_dir)[0]
    items = {item["path"]: item for item in manifest_of(mini)["items"]}
    verdict = verify_damaged(
        mini, edit_run, tmp_path, "title", target, items[target]["title"]
    )
    assert verdict["verdict"] == "fail", verdict
    assert verdict["checks"]["edits"]["violations"] >= 1, verdict["checks"]


def test_the_verification_fails_when_the_catalog_misses_an_item(
    mini, edit_run, tmp_path
):
    # The edit scenario never touches a News Item, nor anything in its
    # folder.
    news = [
        item["path"]
        for item in manifest_of(mini)["items"]
        if item["portal_type"] == "News Item"
    ]
    verdict = verify_damaged(mini, edit_run, tmp_path, "uncatalog", news[-1])
    assert verdict["verdict"] == "fail", verdict
    assert verdict["checks"]["catalog"]["violations"] >= 1, verdict["checks"]


def test_the_verification_fails_when_a_neighbour_leaves_the_catalog(
    mini, edit_run, tmp_path
):
    run_dir, _ = edit_run
    touched = set(edited(run_dir))
    files = {}
    for item in manifest_of(mini)["items"]:
        if item["portal_type"] == "File":
            files.setdefault(item["path"].rpartition("/")[0], []).append(item["path"])
    # A File beside an edited Document: no operation of the run names it.
    neighbour = next(
        files[parent][0]
        for parent in (path.rpartition("/")[0] for path in edited(run_dir))
        if files.get(parent)
    )
    assert neighbour not in touched
    verdict = verify_damaged(
        mini, edit_run, tmp_path, "uncatalog-keep-count", neighbour
    )
    assert verdict["verdict"] == "fail", verdict
    assert verdict["checks"]["catalog"]["violations"] >= 1, verdict["checks"]


def test_the_observer_reads_a_part_of_the_site(mini, edit_run, tmp_path):
    source, _ = edit_run
    config = stack.STACKS["zeo"]
    store = tmp_path / "store"
    for part in ("filestorage", "blobstorage"):
        stack.copy_store(source / "store" / part, store / part)
    st = stack.Stack(config, settings(), tmp_path / "run", store)
    st.start_server()
    try:
        home = tmp_path / "run" / "probe"
        conf = st.prepare_home(home, "probe")
        done = subprocess.run(
            [
                str(stack.interpreter(config)),
                str(stack.INZOPE / "run.py"),
                str(conf),
                str(SCRIPTS / "load_probe.py"),
                str(stack.INZOPE / "observe.py"),
                str(tmp_path / "probe.json"),
                # The request that the runner wrote for the verification
                # of the run.
                str(source / "verify-request.json"),
                str(tmp_path / "observed.json"),
            ],
            capture_output=True,
            text=True,
            env=stack.clean_environment(),
            cwd=home,
        )
    finally:
        st.stop()
    assert done.returncode == 0, done.stderr[-3000:]
    probe = json.loads((tmp_path / "probe.json").read_text())
    items = len(manifest_of(mini)["items"])
    assert 0 < probe["content_objects"] < items / 2, (probe, items)
