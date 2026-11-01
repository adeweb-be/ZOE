"""Locust does the same operations on `small` and `medium` as before the
build wrote its file for Locust.

For each scenario, 64 users start and run 8 calls of their task with no
server (`tests/scripts/locust_probe.py`), on the fixture files of the
preset and on what `workload.write_targets` writes beside them. The
sha256 of the accounts of the users and of their operations, each with
its targets, is stored below: its first 16 hexadecimal digits. The code
gave it before the file for Locust existed. So a run on `small` or on
`medium` does the same operations, in the same order, on the same items,
as before.

    just bench plone test bench/plone/tests/test_locust_operations.py
"""

from pathlib import Path

import pytest
from manifests import probe, write_like_the_build

from plonebench import corpus, workload

USERS = 64
CALLS = 8
DIGESTS = {
    "small": {
        "browse": "6e712e3689bb4d5f",
        "catalog": "a027cad76c36faeb",
        "contention-fields": "010ada56978b29fe",
        "contention-folder": "697d05c3382fdb45",
        "contention-same": "c95b73775009f183",
        "create-shared": "e3e4d2aa6d2bec09",
        "create-spread": "45bf6e70e861b9e5",
        "edit": "5a508a557f81dea1",
        "mixed": "068c9d7895684394",
        "mixed-background": "ac6ffd674b020f21",
        "warmup": "38824b0f0529dcc5",
        "workflow": "f78d8eef96a478e7",
    },
    "medium": {
        "browse": "5457771e92015187",
        "catalog": "db510ca6cbe0c3c6",
        "contention-fields": "4bc532fb6eb0cb30",
        "contention-folder": "de0e22a41c2045a7",
        "contention-same": "8db3a922b9c63f58",
        "create-shared": "be22d7d72e6d0966",
        "create-spread": "e10b5c40731eb568",
        "edit": "35e617bbeb8064ff",
        "mixed": "3bd8e57ae2afc7e1",
        "mixed-background": "71f4b732e2a7cca8",
        "warmup": "dd66037d7ab4f184",
        "workflow": "e1b48ecd1b283eac",
    },
}


@pytest.fixture(scope="module", params=sorted(DIGESTS))
def site(request, tmp_path_factory) -> tuple[str, Path]:
    directory = tmp_path_factory.mktemp(request.param)
    write_like_the_build(corpus.plan_site(corpus.PRESETS[request.param]), directory)
    workload.write_targets(directory)
    return request.param, directory


def test_every_scenario_has_a_digest():
    for digests in DIGESTS.values():
        assert sorted(digests) == sorted(workload.SCENARIOS)


@pytest.mark.parametrize("scenario", sorted(workload.SCENARIOS))
def test_the_operations_of_the_preset_do_not_change(site, scenario, tmp_path):
    preset, directory = site
    measured = probe(directory, scenario, USERS, CALLS, tmp_path / "probe.json")
    assert measured["digest"][:16] == DIGESTS[preset][scenario], measured
