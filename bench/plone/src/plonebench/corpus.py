"""The plan of the synthetic site: a pure function of a `SiteSpec`.

This module imports only the standard library. The scripts that run in
Zope import it to build the site, and Locust imports it to pick its
targets. So both read the same plan from the same spec.

The text is made of generated words, never of a natural language. The
words follow a Zipf distribution, so a search term has a known frequency.

A spec gives one of two shapes. The tree: each section holds `depth`
levels of `fanout` Folders, and each Folder holds the same items. The
units, when `leaf_folders` is not 0: the shape of a real site, with units
of different sizes, as `plan_site` says.
"""

from __future__ import annotations

import dataclasses
import hashlib
import json
import math
import random
from dataclasses import dataclass, field
from statistics import NormalDist

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
    # `fanout` folders. The units below change this.
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
    # The units. When `leaf_folders` is not 0, the sections are the units
    # of the site, and `depth` and `fanout` do not apply. A unit holds
    # Folders of depth 2, and these hold the Folders of depth 3, the
    # leaves. `unit_folders` counts the Folders of depth 2, `leaf_folders`
    # the leaves. `documents` and `files` then count the Documents and the
    # Files of the whole site, in the leaves. `news` counts the News Items
    # of the whole site: each unit holds its own in one Folder of depth 2,
    # and no Folder holds more than `largest`.
    unit_folders: int = 0
    leaf_folders: int = 0
    largest: int = 0
    # The spread of the sizes of the units and of the leaves: the standard
    # deviation of the log of a size.
    unit_spread: float = 0.0
    leaf_spread: float = 0.0
    # The generic indexes of the catalog, by kind, on attributes that the
    # build sets on each item, and the metadata columns of the first of
    # these attributes.
    field_indexes: int = 0
    keyword_indexes: int = 0
    date_indexes: int = 0
    boolean_indexes: int = 0
    metadata_columns: int = 0
    # The pack. When it is not 0, the build packs the FileStorage through
    # ZEO after each `pack_every` items, and the fixture packs it once
    # more after the build. So the fixture holds no history of its build.
    pack_every: int = 0

    @property
    def units(self) -> bool:
        """True when the spec gives the shape of the units."""
        return self.leaf_folders > 0

    def to_json(self) -> dict:
        """Return the fields of the spec. A field of `OPTIONAL_FIELDS` is
        absent while it holds its default. So the JSON of a spec of the
        tree holds the fields of the tree only, and its digest and the name
        of its fixture do not depend on the other fields."""
        data = dataclasses.asdict(self)
        for spec_field in dataclasses.fields(self):
            name = spec_field.name
            if name in OPTIONAL_FIELDS and data[name] == spec_field.default:
                del data[name]
        return data

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


# The fields that `to_json` leaves out while they hold their default.
OPTIONAL_FIELDS = (
    "unit_folders",
    "leaf_folders",
    "largest",
    "unit_spread",
    "leaf_spread",
    "field_indexes",
    "keyword_indexes",
    "date_indexes",
    "boolean_indexes",
    "metadata_columns",
    "pack_every",
)

# A site of about 250,000 items in the shape of a real Plone site, with
# the stock types of Plone 6.1. The shape is a decision of the human:
#
#     depth 1   240 Folders: the units
#     depth 2   750 Folders, about 3 in each unit
#     depth 3   7,200 Folders, about 30 in each unit; 34,000 News Items,
#               in one Folder of depth 2 of each unit
#     depth 4   177,000 Documents and 30,500 Files, in the Folders of depth 3
#
# A Folder of depth 3 holds about 25 Documents and 4 Files. The catalog
# has 25 generic indexes and 21 generic metadata columns beside the stock
# ones. Each of the 96 editors gets a unit of its own. The fixture is
# packed.
XLARGE = SiteSpec(
    sections=240,
    unit_folders=750,
    leaf_folders=7_200,
    documents=177_000,
    news=34_000,
    files=30_500,
    file_sizes=(16_384,),
    largest=1_450,
    unit_spread=1.3,
    leaf_spread=0.1,
    editors=96,
    field_indexes=11,
    keyword_indexes=6,
    date_indexes=5,
    boolean_indexes=3,
    metadata_columns=21,
    pack_every=50_000,
)

PRESETS = {
    # A site that builds in a few minutes on a laptop.
    "small": SiteSpec(),
    # About ten times the items of `small`.
    "medium": SiteSpec(sections=8, depth=3, fanout=3, documents=10, news=3, files=1),
    # About a hundred times the items of `small`.
    "large": SiteSpec(
        sections=12, depth=3, fanout=4, documents=20, news=4, files=2, editors=96
    ),
    "xlarge": XLARGE,
    # The shape of `xlarge` at about 1/100, for the tests: 8 units, leaves
    # of the same size, and the other counts at 1/100.
    "xlarge-mini": dataclasses.replace(
        XLARGE,
        sections=8,
        unit_folders=25,
        leaf_folders=72,
        documents=1_770,
        news=340,
        files=305,
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


def shared_folder(spec: SiteSpec) -> str:
    """Return the path of the Folder of the shared Documents. In the shape
    of the units, it is a Folder of depth 3, as a leaf of a unit is."""
    if spec.units:
        return f"{BENCH}/{SHARED}/folder-00"
    return f"{BENCH}/{SHARED}"


def split(total: int, weights: list[float], least: int = 0, most: int = 0) -> list[int]:
    """Split `total` into whole parts in proportion to `weights`.

    Each part gets at least `least`. When `most` is not 0, no part gets
    more than `most`, and the excess goes to the other parts. The method
    of the largest remainder gives the rest of the whole parts, so the
    parts add up to `total`.
    """
    count = len(weights)
    if total < least * count or (most and total > most * count):
        raise ValueError(f"{total} does not split into {count} parts")
    capped: set[int] = set()
    while True:
        free = [i for i in range(count) if i not in capped]
        rest = total - most * len(capped) - least * len(free)
        scale = sum(weights[i] for i in free)
        exact = {i: rest * weights[i] / scale for i in free}
        parts = {i: math.floor(exact[i]) for i in free}
        largest_remainders = sorted(free, key=lambda i: parts[i] - exact[i])
        for i in largest_remainders[: rest - sum(parts.values())]:
            parts[i] += 1
        over = [i for i in free if most and parts[i] + least > most]
        if not over:
            break
        capped.update(over)
    return [most if i in capped else parts[i] + least for i in range(count)]


def quantile_weights(rng: random.Random, count: int, spread: float) -> list[float]:
    """Return `count` weights with a log-normal spread, in a shuffled order.

    The weights come from the quantiles of the normal distribution, not
    from draws. So the set of the weights does not depend on the seed;
    only their order does.
    """
    normal = NormalDist()
    quantiles = [normal.inv_cdf((i + 0.5) / count) for i in range(count)]
    rng.shuffle(quantiles)
    return [math.exp(spread * z) for z in quantiles]


def catalog_indexes(spec: SiteSpec) -> list[tuple[str, str]]:
    """Return the generic indexes of the catalog: the id and the kind of
    each. The id is also the name of the attribute that it indexes."""
    kinds = (
        ("FieldIndex", spec.field_indexes, "field"),
        ("KeywordIndex", spec.keyword_indexes, "keywords"),
        ("DateIndex", spec.date_indexes, "date"),
        ("BooleanIndex", spec.boolean_indexes, "flag"),
    )
    return [
        (f"bench_{stem}_{n:02d}", kind)
        for kind, count, stem in kinds
        for n in range(count)
    ]


def catalog_columns(spec: SiteSpec) -> list[str]:
    """Return the generic metadata columns: the first attributes of the
    generic indexes."""
    return [name for name, _ in catalog_indexes(spec)][: spec.metadata_columns]


def catalog_values(plan: Plan, path: str) -> dict[str, object]:
    """Return the values of the generic attributes of the item at `path`.

    The FieldIndexes get, in turn, one of 20 words and a number below
    10,000. A KeywordIndex gets 1 to 3 of 60 words, a BooleanIndex a flag
    that is true for 30 % of the items, and a DateIndex a number of days
    after the first date of the build. The values are a function of the
    seed and the path, so two builds give the same catalog.
    """
    seed = hashlib.sha256(f"{plan.spec.seed}:catalog:{path}".encode()).digest()
    rng = random.Random(seed)
    words = plan.words
    values: dict[str, object] = {}
    for n, (name, kind) in enumerate(catalog_indexes(plan.spec)):
        if kind == "FieldIndex":
            values[name] = rng.randrange(10_000) if n % 2 else words[rng.randrange(20)]
        elif kind == "KeywordIndex":
            values[name] = tuple(
                sorted(set(rng.sample(words[200:260], rng.randint(1, 3))))
            )
        elif kind == "DateIndex":
            values[name] = rng.uniform(0, 3650)
        else:
            values[name] = rng.random() < 0.3
    return values


def plan_site(spec: SiteSpec) -> Plan:
    """Return the plan of the site of `spec`. The same spec gives the same
    plan, item for item.

    The tree: each section holds `depth` levels of `fanout` Folders, and
    each Folder, the section included, holds `documents` Documents, `news`
    News Items and `files` Files.

    The units: each unit gets a weight from the spread of the units
    (`quantile_weights`). Its News Items and its leaves come from `news`
    and `leaf_folders` in proportion to its weight, at least 1 each, and
    at most `largest` News Items. Its Folders of depth 2 that
    hold leaves come from `unit_folders` minus the units in proportion to
    the square root of its leaves, at least 1, and share its leaves
    evenly. One more Folder of depth 2, `news`, holds its News Items. Each
    leaf gets a weight from the spread of the leaves; its Documents come
    from `documents` in proportion to it, at least 1, and its Files from
    `files`, with the weights in another order. Each split is exact, so
    the counts of the spec are the counts of the plan.

    In both shapes, each section ends with its Collection `recent`, and
    the Folder `bench` of the scenarios comes last.
    """
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

    def topics() -> tuple[str, ...]:
        return tuple(sorted(set(rng.sample(subjects, rng.randint(0, 3)))))

    def document(folder: Item, name: str, section: int) -> None:
        state, pool = state_and_pool()
        add(
            Item(
                path=f"{folder.path}/{name}",
                portal_type="Document",
                title=text.title(),
                description=text.phrase(8, 20).capitalize() + ".",
                text=text.paragraphs(spec),
                subjects=topics(),
                state=state,
                pool=pool,
                section=section,
            ),
            folder,
        )

    def news_item(folder: Item, name: str, section: int) -> None:
        add(
            Item(
                path=f"{folder.path}/{name}",
                portal_type="News Item",
                title=text.title(),
                description=text.phrase(8, 20).capitalize() + ".",
                text=text.paragraphs(spec),
                subjects=topics(),
                section=section,
            ),
            folder,
        )

    def file(folder: Item, name: str, section: int) -> None:
        add(
            Item(
                path=f"{folder.path}/{name}",
                portal_type="File",
                title=text.title(),
                description=text.phrase(4, 10).capitalize() + ".",
                size=rng.choice(spec.file_sizes),
                section=section,
            ),
            folder,
        )

    def folder(parent: Item, name: str, section: int) -> Item:
        return add(
            Item(
                path=f"{parent.path}/{name}",
                portal_type="Folder",
                title=text.title(),
                description=text.phrase(4, 10).capitalize() + ".",
                section=section,
            ),
            parent,
        )

    def section_folder(s: int) -> Item:
        return add(
            Item(
                path=f"section-{s:02d}",
                portal_type="Folder",
                title=f"Section {s:02d} " + text.title(),
                description=text.phrase(4, 10).capitalize() + ".",
                section=s,
            ),
            None,
        )

    def collection(section: Item, s: int) -> None:
        add(
            Item(
                path=f"{section.path}/recent",
                portal_type="Collection",
                title="Recent " + text.title(),
                section=s,
            ),
            section,
        )

    def fill(container: Item, section: int) -> None:
        for n in range(spec.documents):
            document(container, f"doc-{n:03d}", section)
        for n in range(spec.news):
            news_item(container, f"news-{n:03d}", section)
        for n in range(spec.files):
            file(container, f"file-{n:03d}", section)

    def tree(parent: Item, level: int, section: int) -> None:
        fill(parent, section)
        if level >= spec.depth:
            return
        for n in range(spec.fanout):
            tree(folder(parent, f"folder-{n:02d}", section), level + 1, section)

    def plan_units() -> None:
        weights = quantile_weights(rng, spec.sections, spec.unit_spread)
        news = split(spec.news, weights, 1, spec.largest)
        leaves = split(spec.leaf_folders, weights, 1)
        middles = split(
            spec.unit_folders - spec.sections, [math.sqrt(n) for n in leaves], 1
        )
        leaf_weights = quantile_weights(rng, spec.leaf_folders, spec.leaf_spread)
        documents = split(spec.documents, leaf_weights, 1)
        rng.shuffle(leaf_weights)
        files = split(spec.files, leaf_weights)
        leaf = 0
        for s in range(spec.sections):
            unit = section_folder(s)
            shares = split(leaves[s], [1.0] * middles[s])
            for m, share in enumerate(shares):
                middle = folder(unit, f"folder-{m:02d}", s)
                for n in range(share):
                    container = folder(middle, f"folder-{n:03d}", s)
                    for d in range(documents[leaf]):
                        document(container, f"doc-{d:03d}", s)
                    for f in range(files[leaf]):
                        file(container, f"file-{f:03d}", s)
                    leaf += 1
            holder = folder(unit, "news", s)
            for n in range(news[s]):
                news_item(holder, f"news-{n:04d}", s)
            collection(unit, s)

    if spec.units:
        plan_units()
    else:
        for s in range(spec.sections):
            section = section_folder(s)
            tree(section, 0, s)
            collection(section, s)

    bench = add(Item(path=BENCH, portal_type="Folder", title="Bench"), None)
    for name in (INBOX, HOT, SHARED, IMPORT):
        add(
            Item(path=f"{BENCH}/{name}", portal_type="Folder", title=name.capitalize()),
            bench,
        )
    containers = {HOT: f"{BENCH}/{HOT}", SHARED: shared_folder(spec)}
    if spec.units:
        add(
            Item(path=containers[SHARED], portal_type="Folder", title="Shared folder"),
            next(item for item in items if item.path == f"{BENCH}/{SHARED}"),
        )
    for name, count in ((HOT, spec.hot), (SHARED, spec.shared)):
        container = next(item for item in items if item.path == containers[name])
        for n in range(count):
            add(
                Item(
                    path=f"{container.path}/doc-{n:03d}",
                    portal_type="Document",
                    title=text.title(),
                    description=text.phrase(8, 20).capitalize() + ".",
                    text=text.paragraphs(spec),
                    pool=name,
                ),
                container,
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
