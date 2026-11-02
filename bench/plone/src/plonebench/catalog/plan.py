"""The plan of the catalog fixture: a pure function of a `CatalogSpec`.

This module imports only the standard library and `plonebench.corpus`, so
the build script imports it in Zope and the tests import it outside Zope.

The fixture holds a Plone site whose `portal_catalog` has the indexes and
the metadata columns of the catalog of `xlarge`, 56 and 53, and one entry
for each item of the plan. No content object exists: the build catalogs a
stub for each entry with `Catalog.catalogObject` of ZCatalog, so ZCatalog's
own code writes every tree. The entries have the shape of `xlarge`, scaled
to the size of the spec:

    the site root    the first entry, `/Plone`
    units            Folders `section-NNN`, with a log-normal spread of sizes
    depth 2          Folders `folder-NN` that hold leaves, and the Folder
                     `news` of each unit
    leaves           Folders `folder-NNN` of depth 3, about 25 Documents and
                     4 Files each
    News Items       in the Folder `news` of their unit
    Collections      `recent`, one in each unit

Each count of `xlarge` scales with the size, the Documents take the rest,
so the plan has exactly `entries` entries. The units keep the spreads and
the largest Folder of `xlarge`, so a larger fixture has more units of the
same sizes.

The values of an entry are a function of the seed and of its position: its
type, path, workflow state, dates, subjects, the `bench_*` attributes of
`xlarge`, the security values that the groups and the local roles of
`xlarge` give, and text for `SearchableText`, `Title` and `Description`
made of the generated words of `corpus.make_words`. A date is a whole
minute, as an int of minutes since 1970-01-01 UTC; the build gives the stub
a `DateTime` in UTC.
"""

from __future__ import annotations

import dataclasses
import hashlib
import itertools
import json
import math
import random
import re
from collections import Counter
from collections.abc import Iterator
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta

from plonebench import corpus

SITE_ID = "Plone"
# The number of items that the counts of `corpus.XLARGE` describe, about.
XLARGE_ITEMS = 250_000
DAY = 1440


def minutes(moment: datetime) -> int:
    """Return the minutes from 1970-01-01 UTC to `moment`."""
    return int(moment.timestamp()) // 60


def iso(value: int) -> str:
    """Return the minute `value` as an ISO 8601 string in UTC."""
    return (datetime(1970, 1, 1, tzinfo=UTC) + timedelta(minutes=value)).isoformat()


def from_iso(text: str) -> int:
    """Return the minute of the ISO 8601 string `text`."""
    return minutes(datetime.fromisoformat(text))


# The first date of the entries, as in the build of the B5 fixtures.
EPOCH = minutes(datetime(2024, 1, 1, 8, 0, tzinfo=UTC))
# The effective date of an item that is not published, and the expiration
# date of an item that does not expire, as plone.dexterity gives them.
FLOOR = minutes(datetime(1969, 12, 31, tzinfo=UTC))
CEILING = minutes(datetime(2499, 12, 31, tzinfo=UTC))
# The bounds of DateRangeIndex: a date outside them indexes as no bound.
RANGE_FLOOR = -510_162_480
RANGE_CEILING = 278_751_600

# The types of the entries and the titles of their type information.
TYPES = {
    "Plone Site": "Plone Site",
    "Folder": "Folder",
    "Document": "Page",
    "News Item": "News Item",
    "File": "File",
    "Collection": "Collection",
}
WITH_TEXT = ("Document", "News Item")
# The size of a File, as in `xlarge`.
FILE_SIZE = 16_384

# The workflow states, and the roles that have "Access contents
# information" in each, in the simple publication workflow.
PUBLISHED = corpus.PUBLISHED
PRIVATE = corpus.PRIVATE
PENDING = corpus.PENDING
ROLES = {
    PRIVATE: ("Contributor", "Editor", "Manager", "Reader", "Site Administrator"),
    PENDING: (
        "Contributor",
        "Editor",
        "Manager",
        "Reader",
        "Reviewer",
        "Site Administrator",
    ),
}

# The indexes of the catalog of `xlarge`: the 31 of a stock site of Plone
# 6.1 with plone.app.contenttypes, plonetheme.barceloneta and plone.restapi,
# then the 25 generic ones.
STOCK_INDEXES = {
    "Creator": "FieldIndex",
    "Date": "DateIndex",
    "Description": "ZCTextIndex",
    "SearchableText": "ZCTextIndex",
    "Subject": "KeywordIndex",
    "Title": "ZCTextIndex",
    "Type": "FieldIndex",
    "UID": "UUIDIndex",
    "allowedRolesAndUsers": "KeywordIndex",
    "cmf_uid": "FieldIndex",
    "created": "DateIndex",
    "effective": "DateIndex",
    "effectiveRange": "DateRangeIndex",
    "end": "DateRecurringIndex",
    "exclude_from_nav": "BooleanIndex",
    "expires": "DateIndex",
    "getId": "FieldIndex",
    "getObjPositionInParent": "GopipIndex",
    "getRawRelatedItems": "KeywordIndex",
    "id": "FieldIndex",
    "in_reply_to": "FieldIndex",
    "is_default_page": "BooleanIndex",
    "is_folderish": "BooleanIndex",
    "modified": "DateIndex",
    "object_provides": "KeywordIndex",
    "path": "ExtendedPathIndex",
    "portal_type": "FieldIndex",
    "review_state": "FieldIndex",
    "sortable_title": "FieldIndex",
    "start": "DateRecurringIndex",
    "sync_uid": "FieldIndex",
}
GENERIC_INDEXES = dict(corpus.catalog_indexes(corpus.XLARGE))
INDEXES = {**STOCK_INDEXES, **GENERIC_INDEXES}
STOCK_COLUMNS = (
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
)
COLUMNS = STOCK_COLUMNS + tuple(corpus.catalog_columns(corpus.XLARGE))
# The attributes that hold a date. The stub gives each as a DateTime.
DATES = frozenset(
    ("Date", "created", "effective", "expires", "modified")
    + tuple(name for name, kind in GENERIC_INDEXES.items() if kind == "DateIndex")
)

# Plone's splitter of the text indexes finds the words with this pattern,
# and its normalizer makes them lower case.
WORD = re.compile(r"\w+")


@dataclass(frozen=True)
class CatalogSpec:
    """The size and the seed of the fixture, and the knobs of its values."""

    entries: int = 10_000
    seed: int = 1
    # The build commits after each `batch` entries.
    batch: int = 1_000
    # The text, as in `xlarge`: the words of the vocabulary follow a Zipf
    # distribution; a Document or a News Item has `paragraphs` paragraphs
    # of `words` words.
    vocabulary: int = 2_000
    paragraphs: tuple[int, int] = (3, 8)
    words: tuple[int, int] = (40, 120)
    # The share of Documents in each state; the rest is published.
    private: float = 0.2
    pending: float = 0.1
    # The share of the published Documents and News Items with an effective
    # date after the date of the queries, and with an expiration date.
    scheduled: float = 0.01
    expiring: float = 0.02

    def to_json(self) -> dict:
        return dataclasses.asdict(self)

    @classmethod
    def from_json(cls, data: dict) -> CatalogSpec:
        values = dict(data)
        for key in ("paragraphs", "words"):
            if key in values:
                values[key] = tuple(values[key])
        return cls(**values)

    def digest(self) -> str:
        """Return a short hash of the spec, for the name of a fixture."""
        text = json.dumps(self.to_json(), sort_keys=True)
        return hashlib.sha256(text.encode()).hexdigest()[:12]


PRESETS = {
    "catalog-1m": CatalogSpec(entries=1_000_000),
    # For the tests.
    "catalog-mini": CatalogSpec(entries=10_000),
}


@dataclass(frozen=True)
class Shape:
    """The counts of the entries of each kind."""

    units: int
    unit_folders: int
    leaves: int
    documents: int
    news: int
    files: int
    editors: int

    @property
    def collections(self) -> int:
        return self.units


def shape(spec: CatalogSpec) -> Shape:
    """Return the counts of `xlarge`, scaled to the entries of `spec`. The
    Documents take the rest, so the counts add up to `entries`, with the
    site root."""
    x = corpus.XLARGE
    scale = spec.entries / XLARGE_ITEMS
    units = max(1, round(x.sections * scale))
    unit_folders = max(2 * units, round(x.unit_folders * scale))
    leaves = max(units, round(x.leaf_folders * scale))
    news = max(units, round(x.news * scale))
    files = round(x.files * scale)
    editors = max(1, round(x.editors * scale))
    others = 1 + units + unit_folders + leaves + news + files + units
    documents = spec.entries - others
    if documents < leaves:
        raise ValueError(f"{spec.entries} entries are too few for the shape")
    return Shape(units, unit_folders, leaves, documents, news, files, editors)


@dataclass(frozen=True)
class Layout:
    """Where the entries go: the counts of each unit and of each leaf."""

    shape: Shape
    words: list[str]
    # The News Items and the Folders of depth 2 that hold leaves, of each
    # unit; the leaves of each such Folder; the Documents and the Files of
    # each leaf.
    news: list[int]
    middles: list[int]
    shares: list[int]
    documents: list[int]
    files: list[int]


def layout(spec: CatalogSpec) -> Layout:
    """Return the layout of the units and the leaves, as
    `corpus.plan_site` splits them for the units of `xlarge`."""
    s = shape(spec)
    x = corpus.XLARGE
    rng = random.Random(spec.seed)
    words = corpus.make_words(rng, spec.vocabulary)
    weights = corpus.quantile_weights(rng, s.units, x.unit_spread)
    news = corpus.split(s.news, weights, 1, x.largest)
    leaves = corpus.split(s.leaves, weights, 1)
    middles = corpus.split(s.unit_folders - s.units, [math.sqrt(n) for n in leaves], 1)
    # Each unit gives its leaves to its Folders of depth 2 evenly.
    shares = [
        share
        for unit in range(s.units)
        for share in corpus.split(leaves[unit], [1.0] * middles[unit])
    ]
    leaf_weights = corpus.quantile_weights(rng, s.leaves, x.leaf_spread)
    documents = corpus.split(s.documents, leaf_weights, 1)
    rng.shuffle(leaf_weights)
    files = corpus.split(s.files, leaf_weights)
    return Layout(s, words, news, middles, shares, documents, files)


def creator(s: Shape, unit: int) -> str:
    """The editor who owns the items of `unit`."""
    return f"{corpus.EDITOR}-{unit % s.editors:03d}"


def group(unit: int) -> str:
    """The group with local roles on `unit`."""
    return f"section-{unit:03d}-editors"


def sortable_title(title: str) -> str:
    """The value of `sortable_title`, as Plone's indexer gives it for a
    title of ASCII words: lower case, each number padded to 4 digits."""
    return re.sub(r"\d+", lambda match: match.group().zfill(4), title.lower().strip())


def object_size(size: int) -> str:
    """The value of the column `getObjSize`, as Plone gives it in KB."""
    if not size:
        return "0 KB"
    if size < 1024:
        return "1 KB"
    return f"{size / 1024:.1f} KB"


class Entries:
    """The entries of a spec, in the order of the build."""

    def __init__(self, spec: CatalogSpec):
        self.spec = spec
        self.layout = layout(spec)
        self.shape = self.layout.shape
        self.words = self.layout.words
        self.cumulative = list(
            itertools.accumulate(corpus.zipf_weights(len(self.words)))
        )
        # The date of the queries: 30 days after the creation of the last
        # entry. The entries are created one minute apart.
        self.now = EPOCH + spec.entries + 30 * DAY

    def __iter__(self) -> Iterator[dict]:
        s = self.shape
        lay = self.layout
        n = 0
        yield self.root()
        n += 1
        leaf = 0
        middle = 0
        for unit in range(s.units):
            base = f"section-{unit:03d}"
            yield self.entry(n, base, "Folder", unit)
            n += 1
            for m in range(lay.middles[unit]):
                holder = f"{base}/folder-{m:02d}"
                yield self.entry(n, holder, "Folder", unit)
                n += 1
                for f in range(lay.shares[middle]):
                    container = f"{holder}/folder-{f:03d}"
                    yield self.entry(n, container, "Folder", unit)
                    n += 1
                    for d in range(lay.documents[leaf]):
                        yield self.entry(
                            n, f"{container}/doc-{d:03d}", "Document", unit
                        )
                        n += 1
                    for d in range(lay.files[leaf]):
                        yield self.entry(n, f"{container}/file-{d:03d}", "File", unit)
                        n += 1
                    leaf += 1
                middle += 1
            yield self.entry(n, f"{base}/news", "Folder", unit)
            n += 1
            for d in range(lay.news[unit]):
                yield self.entry(n, f"{base}/news/news-{d:04d}", "News Item", unit)
                n += 1
            yield self.entry(n, f"{base}/recent", "Collection", unit)
            n += 1
        if n != self.spec.entries:
            raise AssertionError(f"the plan has {n} entries, not {self.spec.entries}")

    def rng(self, n: int) -> random.Random:
        return random.Random(f"{self.spec.seed}:entry:{n}")

    def phrase(self, rng: random.Random, low: int, high: int) -> str:
        count = rng.randint(low, high)
        return " ".join(rng.choices(self.words, cum_weights=self.cumulative, k=count))

    def uid(self, path: str) -> str:
        return hashlib.sha256(f"{self.spec.seed}:uid:{path}".encode()).hexdigest()[:32]

    def site_title(self) -> str:
        return self.phrase(random.Random(f"{self.spec.seed}:site"), 2, 3).capitalize()

    def root(self) -> dict:
        title = self.site_title()
        values = {
            "_path": "/" + SITE_ID,
            "id": SITE_ID,
            "getId": SITE_ID,
            "portal_type": "Plone Site",
            "Type": TYPES["Plone Site"],
            "Title": title,
            "sortable_title": sortable_title(title),
            "Description": "",
            "SearchableText": f"{SITE_ID} {title}",
            "UID": self.uid("/" + SITE_ID),
            "Creator": corpus.MAINTAINER,
            "listCreators": (corpus.MAINTAINER,),
            "Subject": (),
            "allowedRolesAndUsers": ["Anonymous"],
            "object_provides": "Plone Site",
            "exclude_from_nav": False,
            "is_default_page": False,
            "is_folderish": True,
            "getIcon": False,
            "getObjSize": object_size(0),
        }
        values.update(self.dates(EPOCH, EPOCH, None, None))
        return values

    def dates(
        self, created: int, modified: int, effective: int | None, expires: int | None
    ) -> dict:
        """The date attributes and the date columns. `effective` and
        `expires` are None when the item has no such date: the attribute
        then gives the floor or the ceiling, as plone.dexterity does."""
        return {
            "created": created,
            "modified": modified,
            "effective": FLOOR if effective is None else effective,
            "expires": CEILING if expires is None else expires,
            "Date": modified if effective is None else effective,
            "CreationDate": iso(created),
            "ModificationDate": iso(modified),
            "EffectiveDate": "None" if effective is None else iso(effective),
            "ExpirationDate": "None" if expires is None else iso(expires),
        }

    def entry(self, n: int, relative: str, portal_type: str, unit: int) -> dict:
        spec = self.spec
        rng = self.rng(n)
        name = relative.rpartition("/")[2]
        path = f"/{SITE_ID}/{relative}"
        title = self.phrase(rng, 2, 5).capitalize()
        texts = portal_type in WITH_TEXT
        low, high = (8, 20) if texts else (4, 10)
        description = self.phrase(rng, low, high).capitalize() + "."
        owner = creator(self.shape, unit)
        values = {
            "_path": path,
            "id": name,
            "getId": name,
            "portal_type": portal_type,
            "Type": TYPES[portal_type],
            "Title": title,
            "sortable_title": sortable_title(title),
            "Description": description,
            "UID": self.uid(path),
            "Creator": owner,
            "listCreators": (owner,),
            "object_provides": portal_type,
            "exclude_from_nav": False,
            "is_default_page": False,
            "is_folderish": portal_type == "Folder",
            "getIcon": False,
        }
        searchable = [name, title, description]
        subjects: tuple[str, ...] = ()
        size = 0
        state = None if portal_type == "File" else PUBLISHED
        if texts:
            paragraphs = [
                self.phrase(rng, *spec.words).capitalize() + "."
                for _ in range(rng.randint(*spec.paragraphs))
            ]
            body = " ".join(paragraphs)
            searchable.append(body)
            size = len(body) + 7 * len(paragraphs)
            pool = self.words[50:90]
            subjects = tuple(sorted(set(rng.sample(pool, rng.randint(0, 3)))))
            searchable.extend(subjects)
            # The history id of a versioned item: a number of its own.
            values["cmf_uid"] = n
            values["mime_type"] = "text/html"
            if portal_type == "Document":
                draw = rng.random()
                if draw < spec.private:
                    state = PRIVATE
                elif draw < spec.private + spec.pending:
                    state = PENDING
        elif portal_type == "File":
            size = FILE_SIZE
            values["mime_type"] = "application/octet-stream"
        values["Subject"] = subjects
        values["SearchableText"] = " ".join(searchable)
        values["getObjSize"] = object_size(size)
        if state is not None:
            values["review_state"] = state
        if state in ROLES:
            local = (f"user:{owner}", f"user:{group(unit)}")
            values["allowedRolesAndUsers"] = sorted(ROLES[state] + local)
        else:
            values["allowedRolesAndUsers"] = ["Anonymous"]
        created = EPOCH + n
        modified = created + min(
            int(rng.expovariate(1 / (20 * DAY))), self.now - created
        )
        effective = expires = None
        if state == PUBLISHED:
            effective = created + rng.randrange(60)
            if texts:
                draw = rng.random()
                if draw < spec.scheduled:
                    effective = self.now + rng.randint(1, 90) * DAY
                elif draw < spec.scheduled + spec.expiring:
                    expires = created + rng.randint(1, 60) * DAY
        values.update(self.dates(created, modified, effective, expires))
        values.update(self.generic(rng))
        return values

    def generic(self, rng: random.Random) -> dict:
        """The `bench_*` attributes, with the rules of
        `corpus.catalog_values`: a FieldIndex gets one of 20 words or a
        number below 10,000, in turn, a KeywordIndex 1 to 3 of 60 words, a
        BooleanIndex a flag that is true for 30 % of the entries, a
        DateIndex a minute of the 10 years after the first date."""
        words = self.words
        values: dict[str, object] = {}
        for n, (name, kind) in enumerate(GENERIC_INDEXES.items()):
            if kind == "FieldIndex":
                values[name] = (
                    rng.randrange(10_000) if n % 2 else words[rng.randrange(20)]
                )
            elif kind == "KeywordIndex":
                values[name] = tuple(
                    sorted(set(rng.sample(words[200:260], rng.randint(1, 3))))
                )
            elif kind == "DateIndex":
                values[name] = EPOCH + rng.randrange(3650 * DAY)
            else:
                values[name] = rng.random() < 0.3
        return values


def indexed(kind: str, name: str, values: dict) -> bool:
    """True when ZCatalog's index of `kind` holds an entry with `values`."""
    if kind == "GopipIndex":
        return False
    if kind in ("DateRangeIndex", "ExtendedPathIndex"):
        return True
    if name not in values:
        return False
    if kind == "KeywordIndex":
        return bool(values[name])
    return values[name] is not None


def range_minute(value: int) -> int | None:
    """The value of a date in DateRangeIndex: None outside its bounds."""
    return value if RANGE_FLOOR <= value <= RANGE_CEILING else None


def _terms(term) -> list:
    return list(term) if isinstance(term, (list, tuple)) else [term]


def matches(values: dict, query: dict) -> bool:
    """True when the entry with `values` is a hit of `query`, as ZCatalog
    reads the query: a list is any of its values, a dict with `range`
    holds its bounds, a date is an ISO 8601 string."""
    for name, term in query.items():
        if name.startswith("sort_"):
            continue
        kind = INDEXES[name]
        if kind == "DateRangeIndex":
            moment = from_iso(term)
            since = range_minute(values["effective"])
            until = range_minute(values["expires"])
            if (since is not None and since > moment) or (
                until is not None and until < moment
            ):
                return False
            continue
        if not indexed(kind, name, values):
            return False
        value = values[name]
        if isinstance(term, dict):
            bounds = _terms(term["query"])
            if kind == "DateIndex":
                bounds = [from_iso(bound) for bound in bounds]
            usage = term["range"]
            low = bounds[0] if usage.startswith("min") else None
            high = bounds[-1] if usage.endswith("max") else None
            if (low is not None and value < low) or (high is not None and value > high):
                return False
            continue
        terms = _terms(term)
        if kind == "KeywordIndex":
            if not set(terms) & set(value):
                return False
        elif kind == "BooleanIndex":
            if bool(value) not in [bool(t) for t in terms]:
                return False
        elif kind == "DateIndex":
            if value not in [from_iso(t) for t in terms]:
                return False
        elif value not in terms:
            return False
    return True


def intersections(entries: Entries) -> list[dict]:
    """The five-index intersections with a sort of the gate: each query
    holds five indexes, of the kinds that the mirror serves first
    (FieldIndex, KeywordIndex, DateIndex, DateRangeIndex, BooleanIndex),
    and a sort."""
    words = entries.words
    now = iso(entries.now)
    editor = creator(entries.shape, 0)
    middle = iso(EPOCH + entries.spec.entries // 2)
    return [
        {
            "name": "listing",
            "query": {
                "portal_type": "Document",
                "review_state": PUBLISHED,
                "allowedRolesAndUsers": ["Anonymous"],
                "effectiveRange": now,
                "is_folderish": False,
                "sort_on": "effective",
                "sort_order": "descending",
                "sort_limit": 20,
            },
        },
        {
            "name": "drafts",
            "query": {
                "Creator": editor,
                "portal_type": ["Document", "News Item"],
                "review_state": [PRIVATE, PENDING],
                "allowedRolesAndUsers": [f"user:{editor}"],
                "modified": {"query": iso(entries.now - 180 * DAY), "range": "min"},
                "sort_on": "modified",
                "sort_order": "descending",
                "sort_limit": 50,
            },
        },
        {
            "name": "subject",
            "query": {
                "Subject": words[57],
                "portal_type": "News Item",
                "review_state": PUBLISHED,
                "allowedRolesAndUsers": ["Anonymous"],
                "effectiveRange": now,
                "sort_on": "effective",
                "sort_order": "descending",
                "sort_limit": 10,
            },
        },
        {
            "name": "generic-words",
            "query": {
                "bench_field_00": words[3:7],
                "bench_keywords_00": words[210:214],
                "bench_flag_00": False,
                "bench_date_00": {
                    "query": [iso(EPOCH + 365 * DAY), iso(EPOCH + 4 * 365 * DAY)],
                    "range": "min:max",
                },
                "portal_type": "Document",
                "sort_on": "sortable_title",
                "sort_limit": 30,
            },
        },
        {
            "name": "generic-numbers",
            "query": {
                "bench_field_01": {"query": [0, 2_999], "range": "min:max"},
                "bench_keywords_01": words[230:233],
                "bench_flag_01": True,
                "created": {"query": middle, "range": "min"},
                "review_state": PUBLISHED,
                "sort_on": "bench_date_01",
                "sort_order": "descending",
                "sort_limit": 20,
            },
        },
        {
            "name": "folders",
            "query": {
                "portal_type": "Folder",
                "review_state": PUBLISHED,
                "is_folderish": True,
                "allowedRolesAndUsers": ["Anonymous"],
                "effectiveRange": now,
                "sort_on": "sortable_title",
                "sort_limit": 100,
            },
        },
    ]


# The bands of the text terms: the share of the entries whose
# `SearchableText` holds the term, from (inclusive) and to (exclusive).
BANDS = (
    ("rare", 0.0, 0.01),
    ("low", 0.01, 0.05),
    ("mid", 0.05, 0.2),
    ("high", 0.2, 0.6),
    ("top", 0.6, 1.01),
)
TERMS_PER_BAND = 3


class Tally:
    """The counts of the plan, added entry by entry: the entries of each
    index and of each column, the hits of each intersection, and the
    document frequency of each word of `SearchableText`."""

    def __init__(self, entries: Entries):
        self.entries = entries
        self.size = 0
        self.indexes = dict.fromkeys(INDEXES, 0)
        self.columns = dict.fromkeys(COLUMNS, 0)
        self.queries = intersections(entries)
        self.hits = [0] * len(self.queries)
        self.frequency: Counter[str] = Counter()

    def add(self, values: dict) -> None:
        self.size += 1
        for name, kind in INDEXES.items():
            if indexed(kind, name, values):
                self.indexes[name] += 1
        for name in COLUMNS:
            if name in values:
                self.columns[name] += 1
        for position, query in enumerate(self.queries):
            if matches(values, query["query"]):
                self.hits[position] += 1
        self.frequency.update(set(WORD.findall(values["SearchableText"].lower())))

    def query_set(self) -> list[dict]:
        """The query set of the fixture: the intersections with their hits,
        then the text terms of each band with their document frequency."""
        result = [
            {
                "name": q["name"],
                "kind": "intersection",
                "query": q["query"],
                "count": hits,
            }
            for q, hits in zip(self.queries, self.hits, strict=True)
        ]
        vocabulary = [(word, self.frequency[word]) for word in self.entries.words]
        for band, low, high in BANDS:
            inside = [
                (word, count)
                for word, count in vocabulary
                if count and low <= count / self.size < high
            ]
            middle = math.sqrt(max(low, 1 / self.size) * min(high, 1.0))
            inside.sort(key=lambda item: abs(math.log(item[1] / self.size / middle)))
            for word, count in inside[:TERMS_PER_BAND]:
                result.append(
                    {
                        "name": f"text-{band}-{word}",
                        "kind": "text",
                        "band": band,
                        "query": {"SearchableText": word},
                        "count": count,
                    }
                )
        return result

    def counts(self) -> dict:
        return {
            "size": self.size,
            "indexes": dict(self.indexes),
            "columns": dict(self.columns),
        }


def tally(spec: CatalogSpec) -> Tally:
    """Return the counts of the whole plan of `spec`."""
    entries = Entries(spec)
    result = Tally(entries)
    for values in entries:
        result.add(values)
    return result
