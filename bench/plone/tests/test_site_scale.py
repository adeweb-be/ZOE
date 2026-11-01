"""Locust and the verification on a site of 250,000 items.

What Locust needs comes from the files of a fixture as they are today:
`workload.write_targets(FIXTURE_DIR)` reads `manifest.json` (with the
spec that it holds) and writes the file or files that Locust reads beside
it. It changes no file of the fixture and plans no site, so a fixture
that exists (`small`, `medium`, `xlarge`) needs no new build.
`fixture.prepare` calls it after the build, and the runner calls it
before Locust starts when the fixture does not have its files yet. Locust
then reads them and builds no plan of the site.

- `write_targets` takes less than 30 s on the manifest of `xlarge`, whose
  plan alone takes about 60 s, and leaves `manifest.json` and `spec.json`
  as they are.
- Locust starts and makes the 64 users of each scenario in less than 10 s
  and less than 500 MB, on the manifest of `xlarge`.
- A call of the task of a user of `browse` costs at most 1 ms on that
  manifest: a pick does not scan the list of the items.
- The request of the verification does not grow with the site: for a
  ledger of 1,000 operations it is less than 1 MB, and it names each
  target of the ledger.

Each scenario runs in its own process (`tests/scripts/locust_probe.py`),
as Locust does, with no server: a recorder takes the place of the
requests. The module plans `xlarge` once, for about a minute and 1 GB,
and writes its manifest of about 100 MB to a temporary directory.

    just bench plone test bench/plone/tests/test_site_scale.py
"""

import hashlib
import json
import random
import time
from pathlib import Path

import pytest
from manifests import probe, write_like_the_build

from plonebench import corpus, verification, workload
from plonebench.ledger import OK, Record

USERS = 64
# The limits of the start of Locust and of the setup of its users.
SECONDS = 10.0
MEMORY = 500 * 2**20
BROWSE_CALLS = 16
BROWSE_SECONDS = 0.001
LEDGER = 1_000
REQUEST_BYTES = 1_000_000
WRITE_SECONDS = 30.0
# The files of a fixture that the build writes today.
FILES = ("manifest.json", "spec.json", "credentials.json")


def digests(directory: Path) -> dict[str, str]:
    return {
        name: hashlib.sha256((directory / name).read_bytes()).hexdigest()
        for name in FILES
    }


@pytest.fixture(scope="module")
def written(tmp_path_factory) -> tuple[Path, dict, float, dict]:
    """A directory with the files of a fixture of `xlarge` as the build
    writes them today, then what `write_targets` writes beside them."""
    directory = tmp_path_factory.mktemp("xlarge")
    write_like_the_build(corpus.plan_site(corpus.PRESETS["xlarge"]), directory)
    before = digests(directory)
    started = time.perf_counter()
    workload.write_targets(directory)
    seconds = time.perf_counter() - started
    return directory, before, seconds, digests(directory)


@pytest.fixture(scope="module")
def site(written) -> Path:
    return written[0]


def test_the_files_for_locust_come_from_the_files_of_a_fixture(written):
    directory, before, seconds, after = written
    assert after == before, "write_targets changed a file of the fixture"
    assert seconds < WRITE_SECONDS, f"write_targets took {seconds:.1f} s"


@pytest.mark.parametrize("scenario", sorted(workload.SCENARIOS))
def test_locust_starts_and_sets_up_64_users_on_250_000_items(site, scenario, tmp_path):
    measured = probe(site, scenario, USERS, 2, tmp_path / "probe.json")
    seconds = measured["start_seconds"] + measured["setup_seconds"]
    assert seconds < SECONDS, measured
    assert measured["maxrss_bytes"] < MEMORY, measured
    # Every user works: each call of a task makes at least one operation.
    assert measured["operations"] >= measured["work_calls"], measured


def test_a_browse_pick_does_not_scan_the_items(site, tmp_path):
    measured = probe(site, "browse", USERS, BROWSE_CALLS, tmp_path / "probe.json")
    assert measured["work_calls"] == USERS * BROWSE_CALLS
    per_call = measured["work_seconds"] / measured["work_calls"]
    assert per_call <= BROWSE_SECONDS, measured


def test_the_request_of_the_verification_does_not_grow_with_the_site(site):
    manifest = json.loads((site / "manifest.json").read_text())
    rng = random.Random(1)
    documents = [
        item
        for item in manifest["items"]
        if item["portal_type"] == "Document" and item["pool"] == corpus.EDIT
    ]
    folders = [item for item in manifest["items"] if item["portal_type"] == "Folder"]
    words = manifest["subjects"]
    records = []
    for n in range(LEDGER):
        code = " ".join(rng.choice(words) for _ in range(4))
        if n % 10:
            target = rng.choice(documents)["path"]
            kind = "edit"
            values = {"title": f"Edit {code} x", "description": "Edited"}
        else:
            folder = rng.choice(folders)["path"]
            target = f"{folder}/new-{n:04d}"
            kind = "create"
            values = {"title": f"New {code} x", "folder": folder}
        records.append(
            Record(
                op=f"{n:032x}",
                kind=kind,
                user=n % USERS,
                role=kind,
                account="editor-000",
                instance=0,
                step=0,
                start=float(n),
                end=n + 0.5,
                outcome=OK,
                target=target,
                values=values,
            )
        )
    wanted = verification.request(records, manifest)
    assert {r.target for r in records} <= set(wanted["paths"])
    size = len(json.dumps(wanted))
    assert size < REQUEST_BYTES, f"the request holds {size} bytes"
