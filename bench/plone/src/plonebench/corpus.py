"""The plan of the synthetic site: a pure function of a `SiteSpec`.

This module imports only the standard library. The scripts that run in
Zope import it to build the site, and Locust imports it to pick its
targets. So both read the same plan from the same spec.

The text is made of generated words, never of a natural language. The
words follow a Zipf distribution, so a search term has a known frequency.
"""

from __future__ import annotations

import dataclasses
import hashlib
import json
import random
from dataclasses import dataclass, field

# The workflow of Documents, News Items and Folders in a Plone 6.1 site.
PUBLISHED = "published"
PRIVATE = "private"
PENDING = "pending"

# The pools of Documents. A pool decides which scenario may change a
# Document, so the scenarios that do not collide on purpose never share a
# Document. Browsing reads every pool but WORKFLOW, whose visibility
# changes during a run.
BROWSE = "browse"
EDIT = "edit"
WORKFLOW = "workflow"

# The folder of the benchmark under the site root, and its folders.
BENCH = "bench"
INBOX = "inbox"
HOT = "hot"
SHARED = "shared"
IMPORT = "import"

# The users. Each simulated user logs in with an account of its own.
EDITOR = "editor"
MEMBER = "member"
IMPORTER = "importer"
MAINTAINER = "maintainer"


@dataclass(frozen=True)
class SiteSpec:
    """The shape of the synthetic site. Every count is configurable."""

    seed: int = 1
    # The top-level folders, and the tree under each one: `depth` levels of
    # `fanout` folders.
    sections: int = 4
    depth: int = 2
    fanout: int = 3
    # Items in each folder of the tree.
    documents: int = 8
    news: int = 2
    files: int = 1
    # The sizes of the files, in bytes. Each file takes one at random.
    file_sizes: tuple[int, ...] = (16_384, 131_072, 1_048_576)
    # Paragraphs in the text of a Document or News Item, and words in one.
    paragraphs: tuple[int, int] = (3, 8)
    words: tuple[int, int] = (40, 120)
    vocabulary: int = 2_000
    # The share of Documents in each state, after the build. The rest is
    # published.
    private: float = 0.2
    pending: float = 0.1
    # Accounts: editors (one section each, through a group with local
    # roles) and members (no local role).
    editors: int = 48
    members: int = 32
    # Documents under bench/hot and bench/shared, for controlled
    # contention.
    hot: int = 4
    shared: int = 48

    def to_json(self) -> dict:
        return dataclasses.asdict(self)

    @classmethod
    def from_json(cls, data: dict) -> SiteSpec:
        values = dict(data)
        for key in ("file_sizes", "paragraphs", "words"):
            if key in values:
                values[key] = tuple(values[key])
        return cls(**values)

    def digest(self) -> str:
        """Return a short hash of the spec, for the name of a fixture."""
        text = json.dumps(self.to_json(), sort_keys=True)
        return hashlib.sha256(text.encode()).hexdigest()[:12]


PRESETS = {
    # A site that builds in a few minutes on a laptop.
    "small": SiteSpec(),
    # About ten times the items of `small`.
    "medium": SiteSpec(sections=8, depth=3, fanout=3, documents=10, news=3, files=1),
    # About a hundred times the items of `small`.
    "large": SiteSpec(
        sections=12, depth=3, fanout=4, documents=20, news=4, files=2, editors=96
    ),
}


@dataclass
class Item:
    """One object of the plan."""

    path: str
    portal_type: str
    title: str
    description: str = ""
    text: str = ""
    subjects: tuple[str, ...] = ()
    state: str = PUBLISHED
    pool: str = BROWSE
    size: int = 0
    section: int = -1
    children: list[str] = field(default_factory=list)


@dataclass
class Plan:
    spec: SiteSpec
    items: list[Item]
    words: list[str]

    def by_type(self, portal_type: str) -> list[Item]:
        return [item for item in self.items if item.portal_type == portal_type]

    def counts(self) -> dict[str, int]:
        counts: dict[str, int] = {}
        for item in self.items:
            counts[item.portal_type] = counts.get(item.portal_type, 0) + 1
        return counts


_CONSONANTS = "bcdfghjklmnprstvz"
_VOWELS = "aeiou"


def make_words(rng: random.Random, count: int) -> list[str]:
    """Return `count` distinct generated words of 2 to 4 syllables."""
    words: list[str] = []
    seen: set[str] = set()
    while len(words) < count:
        syllables = rng.randint(2, 4)
        word = "".join(
            rng.choice(_CONSONANTS) + rng.choice(_VOWELS) for _ in range(syllables)
        )
        if word not in seen:
            seen.add(word)
            words.append(word)
    return words


def zipf_weights(count: int, exponent: float = 1.0) -> list[float]:
    return [1.0 / (rank**exponent) for rank in range(1, count + 1)]


class Text:
    """A generator of sentences from a Zipf vocabulary."""

    def __init__(self, rng: random.Random, words: list[str]):
        self.rng = rng
        self.words = words
        self.cumulative = _cumulative(zipf_weights(len(words)))

    def word(self) -> str:
        return self.rng.choices(self.words, cum_weights=self.cumulative)[0]

    def phrase(self, low: int, high: int) -> str:
        return " ".join(self.word() for _ in range(self.rng.randint(low, high)))

    def title(self) -> str:
        return self.phrase(2, 5).capitalize()

    def paragraphs(self, spec: SiteSpec) -> str:
        count = self.rng.randint(*spec.paragraphs)
        return "".join(
            f"<p>{self.phrase(*spec.words).capitalize()}.</p>" for _ in range(count)
        )


def _cumulative(weights: list[float]) -> list[float]:
    total = 0.0
    result = []
    for weight in weights:
        total += weight
        result.append(total)
    return result


def search_terms(plan: Plan, count: int = 40) -> list[str]:
    """Return words of the vocabulary at ranks that give mid-size results.

    The most frequent words match almost every item. The rare words match
    none. The ranks between give result sets that need a few pages.
    """
    words = plan.words
    start = min(10, len(words) - 1)
    step = max(1, (len(words) // 4 - start) // count)
    return [words[i] for i in range(start, start + step * count, step)][:count]


def plan_site(spec: SiteSpec) -> Plan:
    """Return the plan of the site of `spec`. The same spec gives the same
    plan, item for item."""
    rng = random.Random(spec.seed)
    words = make_words(rng, spec.vocabulary)
    text = Text(rng, words)
    subjects = words[50:90]
    items: list[Item] = []
    counter = {"document": 0}

    def add(item: Item, parent: Item | None) -> Item:
        items.append(item)
        if parent is not None:
            parent.children.append(item.path)
        return item

    def state_and_pool() -> tuple[str, str]:
        index = counter["document"]
        counter["document"] += 1
        # Every fifth Document belongs to the workflow pool, and two of
        # five to the edit pool.
        pool = {4: WORKFLOW, 1: EDIT, 2: EDIT}.get(index % 5, BROWSE)
        draw = rng.random()
        if pool == WORKFLOW:
            # Half published, half private, so that a transition always
            # exists in both directions.
            return (PUBLISHED if index % 2 else PRIVATE), pool
        if draw < spec.private:
            return PRIVATE, pool
        if draw < spec.private + spec.pending:
            return PENDING, pool
        return PUBLISHED, pool

    def fill(folder: Item, section: int) -> None:
        for n in range(spec.documents):
            state, pool = state_and_pool()
            add(
                Item(
                    path=f"{folder.path}/doc-{n:03d}",
                    portal_type="Document",
                    title=text.title(),
                    description=text.phrase(8, 20).capitalize() + ".",
                    text=text.paragraphs(spec),
                    subjects=tuple(
                        sorted(set(rng.sample(subjects, rng.randint(0, 3))))
                    ),
                    state=state,
                    pool=pool,
                    section=section,
                ),
                folder,
            )
        for n in range(spec.news):
            add(
                Item(
                    path=f"{folder.path}/news-{n:03d}",
                    portal_type="News Item",
                    title=text.title(),
                    description=text.phrase(8, 20).capitalize() + ".",
                    text=text.paragraphs(spec),
                    subjects=tuple(
                        sorted(set(rng.sample(subjects, rng.randint(0, 3))))
                    ),
                    section=section,
                ),
                folder,
            )
        for n in range(spec.files):
            add(
                Item(
                    path=f"{folder.path}/file-{n:03d}",
                    portal_type="File",
                    title=text.title(),
                    description=text.phrase(4, 10).capitalize() + ".",
                    size=rng.choice(spec.file_sizes),
                    section=section,
                ),
                folder,
            )

    def tree(parent: Item, level: int, section: int) -> None:
        fill(parent, section)
        if level >= spec.depth:
            return
        for n in range(spec.fanout):
            child = add(
                Item(
                    path=f"{parent.path}/folder-{n:02d}",
                    portal_type="Folder",
                    title=text.title(),
                    description=text.phrase(4, 10).capitalize() + ".",
                    section=section,
                ),
                parent,
            )
            tree(child, level + 1, section)

    for s in range(spec.sections):
        section = add(
            Item(
                path=f"section-{s:02d}",
                portal_type="Folder",
                title=f"Section {s:02d} " + text.title(),
                description=text.phrase(4, 10).capitalize() + ".",
                section=s,
            ),
            None,
        )
        tree(section, 0, s)
        add(
            Item(
                path=f"{section.path}/recent",
                portal_type="Collection",
                title="Recent " + text.title(),
                section=s,
            ),
            section,
        )

    bench = add(Item(path=BENCH, portal_type="Folder", title="Bench"), None)
    for name in (INBOX, HOT, SHARED, IMPORT):
        add(
            Item(path=f"{BENCH}/{name}", portal_type="Folder", title=name.capitalize()),
            bench,
        )
    for name, count in ((HOT, spec.hot), (SHARED, spec.shared)):
        folder = next(item for item in items if item.path == f"{BENCH}/{name}")
        for n in range(count):
            add(
                Item(
                    path=f"{BENCH}/{name}/doc-{n:03d}",
                    portal_type="Document",
                    title=text.title(),
                    description=text.phrase(8, 20).capitalize() + ".",
                    text=text.paragraphs(spec),
                    pool=name,
                ),
                folder,
            )
    return Plan(spec=spec, items=items, words=words)


def file_bytes(spec: SiteSpec, path: str, size: int) -> bytes:
    """Return the deterministic content of the file at `path`."""
    seed = hashlib.sha256(f"{spec.seed}:{path}".encode()).digest()
    rng = random.Random(seed)
    return rng.randbytes(size)


def code_words(plan: Plan) -> list[str]:
    """Return the words of the operation codes: words of the vocabulary
    that the text of the site already holds, so a code adds no new word
    to the lexicons of the text indexes."""
    return plan.words[100:1100]


def op_code(op: str, words: list[str]) -> str:
    """Return the code of the operation `op` (hex): four words of `words`.
    A title of the benchmark starts with a prefix and this code, so the
    verification finds the operation of a title, and a search for the
    four words finds the object."""
    return " ".join(words[int(op[i : i + 6], 16) % len(words)] for i in range(0, 24, 6))


def user_ids(spec: SiteSpec) -> dict[str, list[str]]:
    return {
        EDITOR: [f"{EDITOR}-{n:03d}" for n in range(spec.editors)],
        MEMBER: [f"{MEMBER}-{n:03d}" for n in range(spec.members)],
        IMPORTER: [IMPORTER],
        MAINTAINER: [MAINTAINER],
    }


def editor_section(spec: SiteSpec, editor_index: int) -> int:
    return editor_index % spec.sections
