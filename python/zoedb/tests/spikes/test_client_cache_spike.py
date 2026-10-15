"""Acceptance test of the spike on the layout of the client cache.

The spike chooses one layout of the client cache for three stages: in
memory in one process, a persistent file, and one file that the client
processes of a node map together. The layout keeps the semantics of
`ZEO.cache.ClientCache`. It writes the finding `docs/spikes/client-cache.md`
and an ADR draft.

This test checks the finding:

- the sections `## Layout`, `## Hit cost`, `## Crash safety`,
  `## Several processes`, `## Verification`, `## Clear` and
  `## Card for the shared cache`, none of them empty;
- `## Hit cost` gives the cost of a hit of the prototype and of a hit of
  `ZEO.cache` on the same machine, in µs;
- `## Card for the shared cache` holds a card in the task-card template of
  the working-methods document, for F38;
- the ADR draft `docs/adr/NNNN-the-client-cache-file.md` exists with
  `Status: proposed`. The number is the next free one when the spike
  merges.

The test also checks that the sections answer the questions of the spike:
eviction and compression in the layout, a checksum on each entry, the
lock of the writers, the invalidations and a process that lags.

`just test` runs it.
"""

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]
FINDING = ROOT / "docs" / "spikes" / "client-cache.md"
ADR_NAME = "the-client-cache-file.md"
SECTIONS = [
    "## Layout",
    "## Hit cost",
    "## Crash safety",
    "## Several processes",
    "## Verification",
    "## Clear",
    "## Card for the shared cache",
]
# The fields of the task-card template, and the requirement of a card.
CARD_FIELDS = [
    "red zone:",
    "Requirement:",
    "Goal:",
    "Acceptance tests (do not edit):",
    "Scope fence:",
    "Out of scope:",
    "Invariants:",
    "Spike or build:",
    "Budget:",
    "Done when:",
]


def section(text, heading):
    """Return the body of the section `heading`, up to the next section."""
    assert heading in text, f"no section {heading!r}"
    return text.split(heading, 1)[1].split("\n## ", 1)[0]


def test_the_finding_has_every_section():
    text = FINDING.read_text()
    missing = [heading for heading in SECTIONS if heading not in text]
    assert not missing, missing
    for heading in SECTIONS:
        assert section(text, heading).strip(), f"the section {heading} is empty"


def test_the_hit_cost_compares_the_prototype_and_zeo_cache_in_microseconds():
    lines = section(FINDING.read_text(), "## Hit cost").splitlines()
    # The micro sign and the Greek mu look the same.
    assert any("µs" in line or "μs" in line for line in lines), "no unit µs"
    prototype = [line for line in lines if "prototype" in line.lower()]
    zeo = [line for line in lines if "ZEO.cache" in line]
    assert any(re.search(r"\d", line) for line in prototype), "no hit of the prototype"
    assert any(re.search(r"\d", line) for line in zeo), "no hit of ZEO.cache"


def test_the_card_for_the_shared_cache_follows_the_template():
    card = section(FINDING.read_text(), "## Card for the shared cache")
    missing = [field for field in CARD_FIELDS if field not in card]
    assert not missing, missing
    assert "F38" in card


def test_the_sections_answer_the_questions_of_the_spike():
    text = FINDING.read_text()
    layout = section(text, "## Layout").lower()
    assert "evict" in layout
    assert "compress" in layout
    assert "checksum" in section(text, "## Crash safety").lower()
    several = section(text, "## Several processes").lower()
    assert "lock" in several
    assert "invalidation" in several
    assert "lag" in several
    assert "clear" in section(text, "## Clear").lower()


def test_the_adr_draft_is_proposed():
    drafts = sorted((ROOT / "docs" / "adr").glob(f"[0-9][0-9][0-9][0-9]-{ADR_NAME}"))
    assert len(drafts) == 1, drafts
    # The draft takes a number after the ADRs that exist: 0032 or later.
    assert int(drafts[0].name[:4]) >= 32, drafts[0].name
    text = drafts[0].read_text()
    assert re.search(r"^Status: proposed", text, re.MULTILINE), "not proposed"
