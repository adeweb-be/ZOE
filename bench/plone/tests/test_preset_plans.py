"""The presets `small`, `medium` and `large` keep their plans, item for
item and word for word, and their specs. The name of a fixture hashes its
spec, so the fixtures of these presets keep their names too.

The digests below are those of the plans that the fixtures of these names
hold. A change of a digest is a change of the content of a fixture.
"""

import hashlib
import json

import pytest

from plonebench import corpus, fixture

# The fields of an item of the plan that the fixtures of these presets hold.
FIELDS = (
    "path",
    "portal_type",
    "title",
    "description",
    "text",
    "subjects",
    "state",
    "pool",
    "size",
    "section",
    "children",
)

# The name of the fixture, the count of items and the digest of the plan.
PLANS = {
    "small": (
        "small-ba01ff255313",
        685,
        "df821ff96e4a332dae78a030382b8466e6fe04586cc3fcd4bde21bebdd0dc800",
    ),
    "medium": (
        "medium-8ecaa36fc3c4",
        4865,
        "d0abd7a9e4669eafdd79a18e9d51345f520d5b47270e85845ab84adcf57c528f",
    ),
    "large": (
        "large-5a21ee14e7df",
        27609,
        "0e91af756c9beaae7c504e38dba37bcb0dcf4eda454dc5cea81eb1da6b48e58a",
    ),
}

# The spec.json of a fixture of `small`.
SMALL_SPEC_JSON = {
    "seed": 1,
    "sections": 4,
    "depth": 2,
    "fanout": 3,
    "documents": 8,
    "news": 2,
    "files": 1,
    "file_sizes": [16384, 131072, 1048576],
    "paragraphs": [3, 8],
    "words": [40, 120],
    "vocabulary": 2000,
    "private": 0.2,
    "pending": 0.1,
    "editors": 48,
    "members": 32,
    "hot": 4,
    "shared": 48,
}


def plan_digest(plan: corpus.Plan) -> str:
    digest = hashlib.sha256(json.dumps(plan.words).encode())
    for item in plan.items:
        values = [getattr(item, name) for name in FIELDS]
        values = [list(v) if isinstance(v, tuple) else v for v in values]
        digest.update(json.dumps(values).encode())
        digest.update(b"\n")
    return digest.hexdigest()


@pytest.mark.parametrize("preset", sorted(PLANS))
def test_the_plan_of_the_preset_does_not_change(preset):
    name, count, digest = PLANS[preset]
    spec = corpus.PRESETS[preset]
    assert fixture.default_name(spec, preset) == name
    plan = corpus.plan_site(spec)
    assert len(plan.items) == count
    assert plan_digest(plan) == digest


def test_the_spec_of_small_does_not_change():
    spec = corpus.PRESETS["small"]
    assert json.loads(json.dumps(spec.to_json())) == SMALL_SPEC_JSON
    assert corpus.SiteSpec.from_json(SMALL_SPEC_JSON) == spec


@pytest.mark.parametrize("preset", sorted(corpus.PRESETS))
def test_the_spec_of_each_preset_reads_back_from_its_file(preset):
    spec = corpus.PRESETS[preset]
    data = json.loads(json.dumps(spec.to_json()))
    assert corpus.SiteSpec.from_json(data) == spec
    assert corpus.SiteSpec.from_json(data).digest() == spec.digest()
