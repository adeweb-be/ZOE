"""Write the test data of `tests/metadata.rs`: the records of a catalog with
the metadata columns of a Plone 6.1 site that Products.ZCatalog writes, the
records of two commits, and the brains of ZCatalog for a set of queries.

    PYTHONHASHSEED=0 python metadata.py OUT.json

Run it with an interpreter that has Products.ZCatalog 7.2, ZODB 6, BTrees 6
and DateTime 6, such as the interpreter of the zoe scenario of `just plone`.
The script sets `TZ` to UTC, so a naive `DateTime` gets the same zone on
each machine.

The catalog is a `Products.ZCatalog.Catalog.Catalog` at `root["catalog"]`
of a `MappingStorage`, with the 44 metadata columns of a Plone 6.1 site
(`docs/spikes/catalog-metadata.md`), two FieldIndexes and a DateIndex. A
seeded random gives each entry its values: strs, ints, floats, bools,
bytes, tuples, dicts, `None`, `DateTime`s in several zones and naive,
`Message`s and `Missing.Value`. Some entries hold, in `getRemoteUrl`, a
value of a type that the table does not model: a list, a `datetime`, a
set, a frozenset, an instance of a class, an int beyond 64 bits, a
persistent object, or a dict that holds a list. One record of `data` is
one value short.

For each version (the base, then after each commit), `rows` gives the cells
of the brain of each rid, column by column, as `Catalog.instantiate` makes
it, with `getPath()`; a brain whose record has not one value for each
column, or that holds a value of another type, is `null`. Each query comes
with its plan in the form of ADR 0059, section 8, with `cols` and `rn`, and
the rids of `searchResults` at each version.

A cell is JSON: `null`, a bool, an int, a float, a str, a list for a tuple,
and `{"bytes": hex}`, `{"dict": [[key, value]...]}`, `{"missing": true}`,
`{"datetime": [_micros, _timezone_naive, _tz]}` and `{"message": [args]}`.

The commits: `edit` catalogs one entry again with a new `Title`, and
another with a new `Title` and `update_metadata=0`, so only one cell of
`data` changes; `more` catalogs new entries, removes some, and gives a
modelled value to an entry that held a value of another type.
"""

import json
import os
import struct
import sys
import time

os.environ["TZ"] = "UTC"
time.tzset()

import datetime  # noqa: E402
import random  # noqa: E402

import Missing  # noqa: E402
import Products.ZCatalog.Catalog as catalog_module  # noqa: E402
import transaction  # noqa: E402
from DateTime import DateTime  # noqa: E402
from persistent.mapping import PersistentMapping  # noqa: E402
from Products.PluginIndexes.DateIndex.DateIndex import DateIndex  # noqa: E402
from Products.PluginIndexes.FieldIndex.FieldIndex import FieldIndex  # noqa: E402
from Products.ZCatalog.Catalog import Catalog  # noqa: E402
from ZODB import DB  # noqa: E402
from ZODB.MappingStorage import MappingStorage  # noqa: E402
from zope.i18nmessageid import Message  # noqa: E402

out = sys.argv[1]
rnd = random.Random(289)
catalog_module.randint = random.Random(290).randint

# The metadata columns of a Plone 6.1 site
# (Products/CMFPlone/profiles/default/catalog.xml), in its order.
COLUMNS = (
    "CreationDate", "Creator", "Date", "Description", "EffectiveDate",
    "ExpirationDate", "ModificationDate", "Subject", "Title", "Type", "UID",
    "author_name", "cmf_uid", "commentators", "created", "effective", "end",
    "exclude_from_nav", "expires", "getIcon", "getId", "getObjSize",
    "getRemoteUrl", "hasPreviewImage", "head_title", "id", "image_field",
    "image_scales", "in_response_to", "is_folderish", "last_comment_date",
    "listCreators", "location", "mime_type", "modified", "nav_title",
    "portal_type", "review_state", "start", "sync_uid", "total_comments",
    "Language", "TranslationGroup", "is_working_copy",
)  # fmt: skip
TYPES = (("Document", "Page"), ("News Item", "News Item"), ("Event", "Event"))
STATES = ("private", "pending", "published")
WORDS = ("alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "théta")
# The zones of the DateTimes; a naive DateTime takes the zone of TZ.
ZONES = ("GMT+2", "UTC", "GMT-5", "GMT+11")


class Thing:
    """A value of a class that the table does not model."""

    def __init__(self, n):
        self.n = n


class Stub:
    def __init__(self, path, **values):
        self.__dict__.update(values)
        self._physical = tuple(path.split("/"))

    def getPhysicalPath(self):
        return self._physical


def date(naive=False):
    seconds = 1767225600 + rnd.randint(0, 365 * 86400 - 1)
    if naive:
        return DateTime(time.strftime("%Y/%m/%d %H:%M:%S", time.gmtime(seconds)))
    return DateTime(seconds, rnd.choice(ZONES))


def other(i):
    """A value of a type that the table does not model."""
    return [
        lambda: ["a", "list"],
        lambda: datetime.datetime(2026, 1, 2, 3, 4, 5),
        lambda: {"a", "set"},
        lambda: frozenset({1, 2}),
        lambda: Thing(i),
        lambda: 2**70 + i,
        lambda: PersistentMapping({"n": i}),
        lambda: {"image": [{"scales": {}}]},
    ][i % 8]()


def special(i):
    """A modelled value at the edges of its type."""
    return [
        Missing.Value,
        1.5,
        -0.0,
        1e300,
        2**63 - 1,
        -(2**63),
        2**40,
        b"\x00\xff",
        "x" * 300,
        "théta ☃",
        ((1, ("a",)), ()),
        {1: "a", "b": (2, None)},
        {},
        "",
        True,
        Message("label_n", domain="plone", default="${n} items", mapping={"n": 3}),
    ][i % 16]


def values(i):
    portal_type, title = rnd.choice(TYPES)
    naive = rnd.random() < 0.2
    created = date(naive)
    # A new item has one DateTime for created and modified.
    modified = created if rnd.random() < 0.5 else date(naive)
    v = {
        "CreationDate": created.ISO(),
        "Creator": f"editor-{rnd.randint(0, 5)}",
        "Date": modified.ISO(),
        "Description": " ".join(rnd.choices(WORDS, k=rnd.randint(0, 6))),
        "EffectiveDate": "None",
        "ExpirationDate": "None",
        "ModificationDate": modified.ISO(),
        "Subject": tuple(rnd.sample(WORDS, rnd.randint(0, 3))),
        "Title": f"{rnd.choice(WORDS)} {i}",
        "Type": Message(title, domain="plone"),
        "UID": f"{i:032x}",
        "author_name": rnd.choice([None, "Editor"]),
        "commentators": (),
        "created": created,
        "effective": rnd.choice([None, date()]),
        "exclude_from_nav": rnd.random() < 0.2,
        "expires": DateTime("2499/12/31 00:00:00 GMT+0"),
        "getIcon": "",
        "getId": f"item-{i:04d}",
        "getObjSize": f"{rnd.randint(1, 900)} KB",
        "hasPreviewImage": False,
        "head_title": None,
        "id": f"item-{i:04d}",
        "image_field": "",
        "image_scales": None,
        "in_response_to": None,
        "is_folderish": portal_type != "Document",
        "last_comment_date": None,
        "listCreators": ("editor",),
        "location": "",
        "mime_type": "text/plain",
        "modified": modified,
        "nav_title": None,
        "portal_type": portal_type,
        "review_state": rnd.choice(STATES),
        "sync_uid": None,
        "total_comments": 0,
        "Language": "",
        "is_working_copy": False,
    }
    if portal_type == "Event":
        v["start"] = date()
        v["end"] = date()
    if i % 5 == 0:
        v["getRemoteUrl"] = special(i // 5)
    if i % 11 == 3:
        v["getRemoteUrl"] = other(i // 11)
    if i % 13 == 0:
        v["image_scales"] = {"image": ({"scales": {"thumb": {"width": 128}}},)}
    return v


storage = MappingStorage()
db = DB(storage)
conn = db.open()
root = conn.root()
cat = Catalog()
root["catalog"] = cat
cat.addIndex("portal_type", FieldIndex("portal_type"))
cat.addIndex("getId", FieldIndex("getId"))
cat.addIndex("modified", DateIndex("modified"))
for name in COLUMNS:
    cat.addColumn(name)
transaction.commit()

entries = {}


def add(i):
    path = f"/site/folder-{i % 7}/item-{i:04d}"
    # A random start for the next rid, so the rids spread over the range.
    cat._v_nextid = rnd.randint(-2_000_000_000, 2_000_000_000)
    v = values(i)
    cat.catalogObject(Stub(path, **v), path)
    entries[path] = v


for i in range(300):
    add(i)
# One record of `data` one value short.
short = cat.uids[sorted(entries)[17]]
cat.data[short] = cat.data[short][:-1]
transaction.commit()


def records():
    return {struct.unpack(">Q", oid)[0]: storage.load(oid) for oid in storage._data}


def as_hex(found):
    return {f"{oid:016x}": data.hex() for oid, (data, _) in sorted(found.items())}


def cell(value):
    """The JSON of a cell, or raise TypeError for a value of another type."""
    if value is None or type(value) in (bool, str):
        return value
    if value is Missing.Value:
        return {"missing": True}
    if type(value) is int:
        if not -(2**63) <= value < 2**63:
            raise TypeError("an int beyond 64 bits")
        return value
    if type(value) is float:
        return value
    if type(value) is bytes:
        return {"bytes": value.hex()}
    if type(value) is tuple:
        return [cell(item) for item in value]
    if type(value) is dict:
        return {"dict": [[cell(k), cell(v)] for k, v in value.items()]}
    if type(value) is DateTime:
        micros, naive, tz = value.__getstate__()
        return {"datetime": [micros, naive, tz]}
    if type(value) is Message:
        return {"message": [cell(arg) for arg in value.__reduce__()[1]]}
    raise TypeError(type(value).__name__)


def brain_of(rid):
    """The cells of the brain of `rid`, as `Catalog.instantiate` makes it,
    or None."""
    record = cat.data[rid]
    klass = cat._v_result_class
    if len(klass.__record_schema__) != len(record) + 3:
        return None
    brain = klass(tuple(record) + (rid, 1, 1))
    try:
        return [cell(getattr(brain, name)) for name in COLUMNS]
    except TypeError:
        return None


def version():
    return {
        "rows": {str(rid): brain_of(rid) for rid in cat.paths.keys()},
        "paths": {str(rid): path for rid, path in cat.paths.items()},
    }


base = records()
versions = [version()]
assert versions[0]["rows"][str(short)] is None
assert sum(r is None for r in versions[0]["rows"].values()) > 20

# The catalog has no acquisition parent, so it cannot make brains: each
# item of a result is its rid.
Catalog.instantiate = lambda self, record, score_data=None: record[0]


def answer(query):
    return list(cat.searchResults(dict(query)))


QUERIES = {
    "listing": (
        {"portal_type": ["Document", "News Item", "Event"]},
        {"c": [{"k": "v", "i": "portal_type", "q": ["Document", "News Item", "Event"],
                "and": False, "r": None, "not": []}],
         "s": [], "l": None, "bs": 0, "bz": None, "rn": 1000},
    ),
    "modified": (
        {"portal_type": "Document", "sort_on": "modified", "sort_order": "reverse",
         "sort_limit": 20},
        {"c": [{"k": "v", "i": "portal_type", "q": ["Document"], "and": False,
                "r": None, "not": []}],
         "s": [["modified", True]], "l": 20, "bs": 0, "bz": None, "rn": 20},
    ),
    "batch": (
        {"portal_type": ["Document", "Event"], "sort_on": "getId", "b_start": 10,
         "b_size": 15},
        {"c": [{"k": "v", "i": "portal_type", "q": ["Document", "Event"],
                "and": False, "r": None, "not": []}],
         "s": [["getId", False]], "l": None, "bs": 10, "bz": 15, "rn": 15},
    ),
    "past-rn": (
        {"portal_type": "News Item", "sort_on": "getId"},
        {"c": [{"k": "v", "i": "portal_type", "q": ["News Item"], "and": False,
                "r": None, "not": []}],
         "s": [["getId", False]], "l": None, "bs": 0, "bz": None, "rn": 7},
    ),
}  # fmt: skip
for _, plan in QUERIES.values():
    plan["cols"] = list(COLUMNS)

answers = {name: [answer(query)] for name, (query, _) in QUERIES.items()}
commits = []


def commit(name, change):
    before = records()
    change()
    transaction.commit()
    after = records()
    changed = {oid: found for oid, found in after.items() if before.get(oid) != found}
    commits.append({"name": name, "records": as_hex(changed)})
    versions.append(version())
    for q, (query, _) in QUERIES.items():
        answers[q].append(answer(query))


paths = sorted(entries)
edited = paths[40]
quiet = paths[41]


def edit():
    v = dict(entries[edited])
    v["Title"] = "edited title"
    cat.catalogObject(Stub(edited, **v), edited)
    entries[edited] = v
    w = dict(entries[quiet])
    w["Title"] = "not in data"
    cat.catalogObject(Stub(quiet, **w), quiet, update_metadata=0)


commit("edit", edit)


def more():
    for i in range(300, 340):
        add(i)
    for path in rnd.sample(sorted(entries), 8):
        if path in (edited, quiet):
            continue
        cat.uncatalogObject(path)
        del entries[path]
    fixed = next(
        p for p in sorted(entries) if isinstance(entries[p].get("getRemoteUrl"), list)
    )
    v = dict(entries[fixed])
    v["getRemoteUrl"] = "a str now"
    cat.catalogObject(Stub(fixed, **v), fixed)
    entries[fixed] = v


commit("more", more)

edit_rid = cat.uids[edited]
title = COLUMNS.index("Title")
assert versions[1]["rows"][str(edit_rid)][title] == "edited title"
assert versions[0]["rows"][str(edit_rid)][title] != "edited title"

with open(out, "w") as f:
    json.dump(
        {
            "catalog": f"{struct.unpack('>Q', cat._p_oid)[0]:016x}",
            "columns": list(COLUMNS),
            "base": as_hex(base),
            "commits": commits,
            "versions": versions,
            "plans": {name: plan for name, (_, plan) in QUERIES.items()},
            "answers": answers,
            "edited": edit_rid,
            "short": short,
        },
        f,
        indent=0,
        sort_keys=True,
    )
db.close()
