"""End-to-end tests of the build of a fixture, on demand.

- `xlarge-mini`, the shape of `xlarge` at about 1/100, builds end to end:
  the site on ZEO, its 56 indexes and 53 metadata columns, a pack of the
  FileStorage, `zoe convert` of the packed file, and equal digests of the
  two stores. The FileStorage holds no history of its build: its records
  are its live objects, and zoe convert copies only these.
- `small` keeps the stock catalog of Plone 6.1: no generic index or
  metadata column.
- The digest walks a store larger than the object cache, and keeps the
  cache near its target size. The test runs digest.py on the ZEO store of
  `small` with a cache of 1,000 objects.

They build two Plone sites, for about 5 minutes on the reference machine,
and take the CPUs of the machine. Run nothing else beside them. They need
`just plone zeo build`, `just plone zoeclient build` and `cargo build
--release -p zoe-server --bin zoe` (or `ZOE_BINARY`):

    ZOE_BENCH_FIXTURE_BUILD=1 just bench plone test \
        bench/plone/tests/test_fixture_build.py

The servers listen on the ports of the defaults plus 400.
`ZOE_BENCH_FIXTURE_PORT_OFFSET` gives another offset. The fixtures of the
tests are `test-xlarge-mini-<digest>` and `test-small-<digest>` under
`target/bench/plone/fixtures/`. Each run builds them again.
"""

import dataclasses
import json
import os
import re
import shutil
import subprocess
from pathlib import Path

import pytest
from ZODB.FileStorage import FileStorage
from ZODB.serialize import referencesf
from ZODB.utils import z64

from plonebench import corpus, fixture, stack

pytestmark = pytest.mark.skipif(
    os.environ.get("ZOE_BENCH_FIXTURE_BUILD") != "1",
    reason="builds Plone sites for about 5 minutes: set ZOE_BENCH_FIXTURE_BUILD=1",
)

SCRIPTS = Path(__file__).resolve().parent / "scripts"
# The target size of the object cache of the digest of `small`.
CACHE = 1_000

# The catalog of a Plone 6.1 site with plone.app.contenttypes,
# plonetheme.barceloneta and plone.restapi.
STOCK_INDEXES = {
    "Creator",
    "Date",
    "Description",
    "SearchableText",
    "Subject",
    "Title",
    "Type",
    "UID",
    "allowedRolesAndUsers",
    "cmf_uid",
    "created",
    "effective",
    "effectiveRange",
    "end",
    "exclude_from_nav",
    "expires",
    "getId",
    "getObjPositionInParent",
    "getRawRelatedItems",
    "id",
    "in_reply_to",
    "is_default_page",
    "is_folderish",
    "modified",
    "object_provides",
    "path",
    "portal_type",
    "review_state",
    "sortable_title",
    "start",
    "sync_uid",
}
STOCK_COLUMNS = {
    "CreationDate",
    "Creator",
    "Date",
    "Description",
    "EffectiveDate",
    "ExpirationDate",
    "ModificationDate",
    "Subject",
    "Title",
    "Type",
    "UID",
    "cmf_uid",
    "created",
    "effective",
    "end",
    "exclude_from_nav",
    "expires",
    "getIcon",
    "getId",
    "getObjSize",
    "getRemoteUrl",
    "id",
    "image_scales",
    "is_folderish",
    "listCreators",
    "location",
    "mime_type",
    "modified",
    "portal_type",
    "review_state",
    "start",
    "sync_uid",
}
GENERIC_KINDS = {"FieldIndex", "KeywordIndex", "DateIndex", "BooleanIndex"}


def settings() -> stack.Settings:
    offset = int(os.environ.get("ZOE_BENCH_FIXTURE_PORT_OFFSET", "400"))
    defaults = stack.Settings()
    return dataclasses.replace(
        defaults,
        base_port=defaults.base_port + offset,
        server_port=defaults.server_port + offset,
        admin_port=defaults.admin_port + offset,
    )


def build(preset: str) -> tuple[corpus.Plan, Path]:
    """Build the fixture of `preset` again, under a name of the tests."""
    spec = corpus.PRESETS[preset]
    name = "test-" + fixture.default_name(spec, preset)
    target = fixture.prepare(spec, name, settings(), force=True)
    return corpus.plan_site(spec), target


def copy_data_fs(target: Path, directory: Path) -> Path:
    """Copy the Data.fs of a fixture, so that no check opens the fixture."""
    copy = directory / "Data.fs"
    shutil.copyfile(target / "store" / "filestorage" / "Data.fs", copy)
    return copy


@pytest.fixture(scope="module")
def mini():
    return build("xlarge-mini")


@pytest.fixture(scope="module")
def small():
    return build("small")


@pytest.fixture(scope="module")
def data_fs(mini, tmp_path_factory) -> Path:
    return copy_data_fs(mini[1], tmp_path_factory.mktemp("filestorage"))


def catalog_of(data_fs: Path) -> dict:
    """Return the indexes and the metadata columns of the catalog."""
    result = subprocess.run(
        [
            str(stack.interpreter(stack.STACKS["zeo"])),
            str(SCRIPTS / "catalog_counts.py"),
            str(data_fs),
        ],
        capture_output=True,
        text=True,
        env=stack.clean_environment(),
        cwd=data_fs.parent,
    )
    assert result.returncode == 0, result.stderr[-3000:]
    return json.loads(result.stdout.strip().splitlines()[-1])


def records_and_live_objects(path: Path) -> tuple[int, int, int]:
    """Return the data records of the FileStorage, its oids, and the oids
    that the root reaches."""
    storage = FileStorage(str(path), read_only=True)
    try:
        records = sum(1 for transaction in storage.iterator() for _ in transaction)
        oids = len(storage)
        seen = {z64}
        todo = [z64]
        while todo:
            data, _ = storage.load(todo.pop())
            for oid in referencesf(data):
                if oid not in seen:
                    seen.add(oid)
                    todo.append(oid)
        return records, oids, len(seen)
    finally:
        storage.close()


def test_the_mini_fixture_is_equivalent_on_both_stores(mini):
    plan, target = mini
    equivalence = json.loads((target / "equivalence.json").read_text())
    assert equivalence["equivalent"] is True
    assert equivalence["objects"] == len(plan.items)
    assert equivalence["catalog_entries"] == len(plan.items) + 1
    manifest = json.loads((target / "manifest.json").read_text())
    assert len(manifest["items"]) == len(plan.items)


def test_the_filestorage_of_the_fixture_holds_no_history(mini, data_fs):
    _, target = mini
    records, oids, live = records_and_live_objects(data_fs)
    assert records == oids == live, (records, oids, live)
    assert not (target / "store" / "filestorage" / "Data.fs.old").exists()
    # The report of zoe convert counts the records of the packed file.
    report = (target / "convert.txt").read_text()
    assert re.search(r"^\s*problems\s+0$", report, re.M), report
    copied = re.search(r"^\s*records\s+(\d+)$", report, re.M)
    assert copied and int(copied.group(1)) == records, report


def test_the_catalog_has_56_indexes_and_53_metadata_columns(mini, data_fs):
    plan, _ = mini
    catalog = catalog_of(data_fs)
    indexes = catalog["indexes"]
    assert len(indexes) == 56
    assert len(catalog["metadata"]) == 53
    assert STOCK_INDEXES <= set(indexes)
    assert STOCK_COLUMNS <= set(catalog["metadata"])
    generic = {
        name: index for name, index in indexes.items() if name not in STOCK_INDEXES
    }
    assert {index["kind"] for index in generic.values()} == GENERIC_KINDS
    # The build sets the attribute of each generic index on each item.
    for name, index in generic.items():
        assert index["entries"] >= 0.9 * len(plan.items), (name, index)
    assert catalog["size"] == len(plan.items) + 1


def test_small_keeps_the_stock_catalog(small, tmp_path):
    plan, target = small
    catalog = catalog_of(copy_data_fs(target, tmp_path))
    assert set(catalog["indexes"]) == STOCK_INDEXES
    assert set(catalog["metadata"]) == STOCK_COLUMNS
    assert catalog["size"] == len(plan.items) + 1


def test_the_digest_keeps_the_object_cache_near_its_target(small, tmp_path):
    _, source = small
    config = stack.STACKS["zeo"]
    run = tmp_path / "run"
    store = fixture.restore(source, config, run)
    zeo = stack.Stack(
        config, dataclasses.replace(settings(), zodb_cache_objects=CACHE), run, store
    )
    home = run / "probe"
    conf = zeo.prepare_home(home, "probe")
    zeo.start_server()
    try:
        result = subprocess.run(
            [
                str(stack.interpreter(config)),
                str(stack.INZOPE / "run.py"),
                str(conf),
                str(SCRIPTS / "cache_probe.py"),
                str(stack.INZOPE / "digest.py"),
                str(tmp_path / "digest.json"),
                str(tmp_path / "probe.json"),
            ],
            capture_output=True,
            text=True,
            env=stack.clean_environment(),
            cwd=home,
        )
    finally:
        zeo.stop()
    assert result.returncode == 0, result.stderr[-3000:]
    probe = json.loads((tmp_path / "probe.json").read_text())
    digest = json.loads((tmp_path / "digest.json").read_text())
    reference = json.loads((source / "digest-zeo.json").read_text())
    # The digest still reads the whole site.
    assert digest["counts"] == reference["counts"]
    assert digest["catalog"]["size"] == reference["catalog"]["size"]
    # The store is larger than the cache.
    assert probe["target"] == CACHE
    assert probe["objects"] >= 10 * CACHE, probe
    assert probe["peak"] <= 2 * CACHE, probe
