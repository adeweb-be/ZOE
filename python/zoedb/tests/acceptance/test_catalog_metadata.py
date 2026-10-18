"""Acceptance test of the native merges of the metadata buckets of a catalog.

`Catalog.data` of Products.ZCatalog is an `IOBTree`: the rid of each object
maps to the tuple of its metadata columns. On a Plone site the tuple holds
`DateTime` objects (`created`, `modified`, `effective`, `expires`),
`Missing.Value` for each column that the object does not give, i18n
messages of zope.i18nmessageid (`Type`), strs, bools, ints, tuples and
`None`. Two saves of two objects whose rids are in one bucket change that
bucket from the same snapshot, so the second commit has a conflict on it.

ZOE merges such a conflict at the vote, in Rust (tier 1), as ZODB's
resolver merges it: the client runs no resolution for it.

FileStorage with ZODB's resolver is the reference. Each case runs the same
transactions on FileStorage and on ZOE: one makes a catalog with the
metadata columns of a Plone site and catalogs objects in it, then two
change the catalog from the same snapshot, and the second of them has a
conflict. A change catalogs an object again with changed attributes (a
save), catalogs a new object, or uncatalogs one. The test checks, for each
case:

- the outcome is the same: both merge, or both raise `ConflictError`;
- each record of the catalog that ZOE stores is the record that
  FileStorage stores, byte for byte: the tree of `data` and its buckets,
  and the trees of `uids` and `paths`, and `_length`;
- the second connection sees the merged metadata after its commit;
- when ZODB's resolver merges, the client calls `tryToResolveConflict`
  for no object: the store merged each conflict, the metadata buckets
  among them.

A random generator with a fixed seed makes the cases. The changes of both
sides touch objects whose rids are close, so most cases conflict on a
metadata bucket; a check of the reference counts them. The changes give
the comparisons that BTrees makes in a merge: a column that changes from
`Missing.Value` to a `DateTime`, a `DateTime` that is equal to the old one
in another time zone, a message that is equal to the old one as a str, one
object in two columns. A catalog holds 3 to 150 objects, so a conflict is
on the bucket that the tree holds inline, or on one bucket of several.

The test runs in-process on `zoe.Storage`, and through `zoe serve` with
`ZEO.ClientStorage` and with `zoe.zeo.ClientStorage` with the native
client inside. `just test` runs it.
"""

import dataclasses
import random
import re
import threading

import pytest

import zoe
import zoe.testing
from zoe.testing import ZoeServer

pytest.importorskip("Products.ZCatalog")

# The largest tid. loadBefore(oid, MAX_TID) loads the current record.
MAX_TID = (2**63 - 1).to_bytes(8, "big")
# The longest time that a test waits for the server.
TIMEOUT = 30
# The seed of the generator of the cases, and the number of cases.
SEED = 4213
CASES = 120
# An oid in a record: the opcode SHORT_BINBYTES with 8 bytes.
OID = re.compile(rb"C\x08(.{8})", re.DOTALL)
# The modes: in-process, and through zoe serve with each client.
MODES = ("in-process", "zeo-client", "native-client")
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
# The rid of the first object of a catalog. The objects of a catalog have
# the rids FIRST_RID, FIRST_RID + 2, ...; a new object takes a rid between
# them or after them. No rid is a multiple of 4000, which would make
# Products.ZCatalog draw a random rid.
FIRST_RID = 1_000_001
# The portal types and their titles, as the FTIs of Plone give them.
TYPES = (("Document", "Page"), ("News Item", "News Item"), ("Event", "Event"))
WORDS = ("alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "théta")
STATES = ("private", "pending", "published")
# The time zones of the DateTimes.
ZONES = ("GMT+2", "UTC", "GMT-5")


def date(rng, zone=None):
    """Return a DateTime of 2026, in the time zone zone or a drawn one."""
    from DateTime import DateTime

    seconds = rng.randint(0, 365 * 86400 - 1)
    return DateTime(1767225600 + seconds, zone or rng.choice(ZONES))


def message(title, domain="plone"):
    """Return the title of a type as a message of domain."""
    from zope.i18nmessageid import Message

    return Message(title, domain=domain)


class Content:
    """An object to catalog: an attribute for each column that it gives.

    `recordify` of Products.ZCatalog reads each column of the catalog from
    the object, and gives `Missing.Value` for a column that the object does
    not give.
    """

    def __init__(self, attributes):
        self.__dict__.update(attributes)


def new_attributes(rng, number):
    """Return the attributes of a new object, as a Plone content item gives
    them to the catalog."""
    portal_type, title = rng.choice(TYPES)
    created = date(rng)
    # A new item has one DateTime for created and modified.
    modified = created if rng.random() < 0.5 else date(rng)
    attributes = {
        "CreationDate": created.ISO(),
        "Creator": "admin",
        "Date": modified.ISO(),
        "Description": " ".join(rng.choices(WORDS, k=rng.randint(0, 6))),
        "EffectiveDate": "None",
        "ExpirationDate": "None",
        "ModificationDate": modified.ISO(),
        "Subject": tuple(rng.sample(WORDS, rng.randint(0, 3))),
        "Title": f"{rng.choice(WORDS)} {number}",
        "Type": message(title),
        "UID": f"{number:032x}",
        "commentators": (),
        "created": created,
        "effective": date(rng, "UTC"),
        "exclude_from_nav": False,
        "expires": date(rng, "UTC"),
        "getIcon": False,
        "getId": f"item-{number}",
        "getObjSize": f"{rng.randint(1, 9)} KB",
        "hasPreviewImage": False,
        "id": f"item-{number}",
        "image_field": "",
        "image_scales": None,
        "is_folderish": portal_type == "Event" and rng.random() < 0.5,
        "last_comment_date": None,
        "listCreators": ("admin",),
        "location": "",
        "mime_type": "text/plain",
        "modified": modified,
        "portal_type": portal_type,
        "review_state": rng.choice(STATES),
        "total_comments": 0,
        "Language": "",
    }
    if portal_type == "Event":
        attributes["start"] = date(rng)
        attributes["end"] = attributes["start"]
    return attributes


def saved(rng, attributes):
    """Return the attributes of an object after a save: one to three
    changes, as an edit form, a transition or a keyword change makes."""
    attributes = dict(attributes)
    for _ in range(rng.randint(1, 3)):
        change = rng.choice(
            (
                "title", "description", "state", "subjects", "modified",
                "same-instant", "start", "type", "navigation",
            )
        )  # fmt: skip
        if change == "title":
            attributes["Title"] = f"{rng.choice(WORDS)} {rng.randint(0, 99)}"
        elif change == "description":
            attributes["Description"] = " ".join(rng.choices(WORDS, k=3))
        elif change == "state":
            attributes["review_state"] = rng.choice(STATES)
        elif change == "subjects":
            attributes["Subject"] = tuple(rng.sample(WORDS, rng.randint(0, 3)))
        elif change == "modified":
            modified = date(rng)
            attributes["modified"] = modified
            attributes["ModificationDate"] = modified.ISO()
        elif change == "same-instant":
            # The same instant in another time zone: equal to the old
            # DateTime, with another state.
            old = attributes["modified"]
            attributes["modified"] = old.toZone(
                next(zone for zone in ZONES if zone != old.timezone())
            )
        elif change == "start":
            # A column that was Missing.Value takes a DateTime, or the
            # other way round.
            if "start" in attributes:
                del attributes["start"], attributes["end"]
            else:
                attributes["start"] = date(rng)
                attributes["end"] = date(rng)
        elif change == "type":
            # Equal as a str to the old message, in another domain.
            attributes["Type"] = message(
                str(attributes["Type"]), domain="plone.app.contenttypes"
            )
        else:
            attributes["exclude_from_nav"] = not attributes["exclude_from_nav"]
    return attributes


@dataclasses.dataclass(frozen=True)
class Case:
    """One conflict: the objects of the catalog and the changes of the two
    transactions.

    `base` is a list of `(uid, rid, attributes)`. A change is
    `("save", uid, attributes)`, `("add", uid, rid, attributes)` or
    `("delete", uid)`.
    """

    base: tuple
    committed: tuple
    new: tuple


def make_changes(rng, base, window, fresh, side):
    """Return one to three changes of the objects of `window`, indexes of
    `base`. A new object takes a rid of `fresh`, which both sides draw
    from, so that two inserts of one rid happen. One side inserts a rid
    once."""
    changes = []
    touched = set()
    for count in range(rng.randint(1, 3)):
        operation = rng.choice(("save", "save", "save", "add", "delete"))
        if operation == "add":
            rid = rng.choice(fresh)
            if rid in touched:
                continue
            touched.add(rid)
            uid = f"/plone/new-{side}-{count}"
            changes.append(("add", uid, rid, new_attributes(rng, rid)))
            continue
        index = rng.choice(window)
        uid, _, attributes = base[index]
        if uid in touched:
            continue
        touched.add(uid)
        if operation == "save":
            changes.append(("save", uid, saved(rng, attributes)))
        else:
            changes.append(("delete", uid))
    return tuple(changes)


def generated_cases():
    """Return the cases, the same list at each call."""
    rng = random.Random(SEED)
    cases = []
    for _ in range(CASES):
        size = rng.choice((3, 8, 40, 90, 150))
        base = tuple(
            (
                f"/plone/item-{number}",
                FIRST_RID + 2 * number,
                new_attributes(rng, number),
            )
            for number in range(size)
        )
        center = rng.randrange(size)
        window = list(range(max(0, center - 4), min(size, center + 5)))
        fresh = [FIRST_RID + 2 * i + 1 for i in window] + [FIRST_RID + 2 * size + 1]
        committed = make_changes(rng, base, window, fresh, "committed")
        new = make_changes(rng, base, window, fresh, "new")
        cases.append(Case(base, committed, new))
    return cases


def make_catalog():
    """Return an empty catalog with the metadata columns of Plone."""
    from Products.ZCatalog.Catalog import Catalog

    catalog = Catalog()
    for column in COLUMNS:
        catalog.addColumn(column)
    return catalog


def apply(catalog, changes):
    """Make the changes on catalog."""
    for change in changes:
        match change:
            case ("save", uid, attributes):
                catalog.catalogObject(Content(attributes), uid)
            case ("add", uid, rid, attributes):
                # Products.ZCatalog takes the rid of a new object from the
                # volatile attribute `_v_nextid`.
                assert rid % 4000
                catalog._v_nextid = rid
                catalog.catalogObject(Content(attributes), uid)
            case ("delete", uid):
                catalog.uncatalogObject(uid)


def persistent_parts(catalog):
    """Return the persistent objects of catalog that a change writes: the
    trees `data`, `uids` and `paths` with their buckets, and `_length`, in
    a fixed order."""
    from BTrees.Length import Length

    found = []
    todo = [catalog.data, catalog.uids, catalog.paths, catalog._length]
    while todo:
        obj = todo.pop(0)
        if any(obj is other for other in found):
            continue
        found.append(obj)
        if isinstance(obj, Length):
            continue
        # The state of a tree holds its children and its first bucket. The
        # state of a bucket holds its next bucket.
        state = obj.__getstate__() or ((),)
        todo.extend(
            part
            for part in (*state[0], *state[1:])
            if getattr(part, "_p_oid", None) is not None
        )
    return found


def numbered(records):
    """Return records, pairs of an oid and its record, with each oid in the
    records replaced by its index in the list.

    The two storages can give the objects of a case other oids. A record
    refers to another
    object by its oid, as the 8 bytes after the opcode SHORT_BINBYTES, so
    the index takes the place of these bytes, and the length of the record
    does not change.
    """
    index = {oid: number.to_bytes(8, "big") for number, (oid, _) in enumerate(records)}

    def replace(match):
        return b"C\x08" + index.get(match.group(1), match.group(1))

    return tuple((index[oid], OID.sub(replace, record)) for oid, record in records)


def data_oids(catalog):
    """Return the oids of the tree `data` of catalog and of its buckets."""
    return {catalog.data._p_oid} | {bucket._p_oid for bucket in buckets(catalog.data)}


def buckets(tree):
    """Return the buckets of tree, an IOBTree, in key order."""
    state = tree.__getstate__()
    if not state or len(state) < 2:
        return []
    found = []
    bucket = state[1]
    while bucket is not None:
        found.append(bucket)
        bucket_state = bucket.__getstate__()
        bucket = bucket_state[1] if len(bucket_state) > 1 else None
    return found


def metadata(catalog):
    """Return the metadata of catalog as plain data: each rid with the type
    and the repr of each value of its tuple."""
    return [
        (rid, tuple((type(value).__name__, repr(value)) for value in values))
        for rid, values in catalog.data.items()
    ]


class Calls:
    """Keep the oid of each call of `tryToResolveConflict`, in every
    thread."""

    def __init__(self):
        self.oids = []
        self._lock = threading.Lock()

    def install(self, monkeypatch):
        from ZODB.ConflictResolution import ConflictResolvingStorage

        original = ConflictResolvingStorage.tryToResolveConflict

        def kept(storage, oid, *args, **kwargs):
            with self._lock:
                self.oids.append(oid)
            return original(storage, oid, *args, **kwargs)

        monkeypatch.setattr(ConflictResolvingStorage, "tryToResolveConflict", kept)
        return self


@dataclasses.dataclass
class Result:
    """The end of one case on one storage."""

    outcome: str
    records: tuple
    state: list
    seen: list
    calls: tuple
    data_calls: int


def run(storage, cases, calls):
    """Run each case on storage, and close it. Return a list of Result."""
    import transaction
    import ZODB
    from ZODB.POSException import ConflictError

    db = ZODB.DB(storage)
    try:
        first_tm = transaction.TransactionManager()
        second_tm = transaction.TransactionManager()
        first = db.open(transaction_manager=first_tm)
        second = db.open(transaction_manager=second_tm)
        results = []
        for number, case in enumerate(cases):
            name = f"case-{number}"
            first_tm.begin()
            catalog = make_catalog()
            first.root()[name] = catalog
            for uid, rid, attributes in case.base:
                apply(catalog, [("add", uid, rid, attributes)])
            first_tm.commit()

            first_tm.begin()
            second_tm.begin()
            apply(first.root()[name], case.committed)
            apply(second.root()[name], case.new)
            first_tm.commit()
            before = len(calls.oids)
            try:
                second_tm.commit()
            except ConflictError:
                second_tm.abort()
                outcome = "conflict"
            else:
                outcome = "merged"
            used = tuple(calls.oids[before:])

            second_tm.begin()
            seen = metadata(second.root()[name])
            second_tm.abort()
            first_tm.begin()
            catalog = first.root()[name]
            state = metadata(catalog)
            records = numbered(
                [
                    (obj._p_oid, tuple(db.storage.loadBefore(obj._p_oid, MAX_TID))[0])
                    for obj in persistent_parts(catalog)
                ]
            )
            in_data = data_oids(catalog)
            first_tm.abort()
            results.append(
                Result(
                    outcome,
                    records,
                    state,
                    seen,
                    used,
                    sum(oid in in_data for oid in used),
                )
            )
        first.close()
        second.close()
        return results
    finally:
        db.close()


def run_on_zoe(mode, path, cases, calls):
    """Run the cases on ZOE in mode. Return the results."""
    if mode == "in-process":
        return run(zoe.Storage(str(path)), cases, calls)
    with ZoeServer(path) as server:
        if mode == "zeo-client":
            from ZEO.ClientStorage import ClientStorage

            storage = ClientStorage(server.address, wait_timeout=TIMEOUT)
        else:
            storage = zoe.testing.native_client(server.address, wait_timeout=TIMEOUT)
        return run(storage, cases, calls)


@pytest.fixture(scope="module")
def cases():
    return generated_cases()


@pytest.fixture(scope="module")
def reference(cases, tmp_path_factory):
    """The results of the cases on FileStorage with ZODB's resolver."""
    from ZODB.FileStorage import FileStorage

    path = tmp_path_factory.mktemp("reference") / "Data.fs"
    with pytest.MonkeyPatch.context() as monkeypatch:
        calls = Calls().install(monkeypatch)
        return run(FileStorage(str(path)), cases, calls)


def describe(number, case):
    """Return a short text of case: the operation and the object of each
    change."""

    def short(changes):
        return ", ".join(f"{change[0]} {change[1]}" for change in changes)

    return (
        f"case {number} ({len(case.base)} objects): "
        f"{short(case.committed)}, then {short(case.new)}"
    )


def test_the_cases_conflict_on_metadata_buckets(cases, reference):
    # The generator gives both outcomes, so the test checks both paths.
    outcomes = {result.outcome for result in reference}
    assert outcomes == {"merged", "conflict"}
    # Most merged cases resolved a conflict on the tree data or one of its
    # buckets, with values that hold DateTimes, Missing.Value and messages.
    merged = [result for result in reference if result.outcome == "merged"]
    on_data = [result for result in merged if result.data_calls]
    assert len(on_data) >= len(merged) // 2, (len(on_data), len(merged))
    assert len(on_data) >= 30


@pytest.mark.parametrize("mode", MODES)
def test_metadata_buckets_merge_in_the_store_as_zodb_merges_them(
    mode, cases, reference, tmp_path, monkeypatch
):
    calls = Calls().install(monkeypatch)
    results = run_on_zoe(mode, tmp_path / "store", cases, calls)

    problems = []
    for number, (case, ours, theirs) in enumerate(
        zip(cases, results, reference, strict=True)
    ):
        if ours.outcome != theirs.outcome:
            problems.append(
                f"{describe(number, case)}: {ours.outcome}, not {theirs.outcome}"
            )
            continue
        if ours.records != theirs.records:
            problems.append(f"{describe(number, case)}: a record is not FileStorage's")
        elif ours.seen != theirs.state:
            problems.append(
                f"{describe(number, case)}: the second connection sees an old state"
            )
        if theirs.outcome == "merged" and ours.calls:
            problems.append(
                f"{describe(number, case)}: the client ran tryToResolveConflict "
                f"{len(ours.calls)} time(s), {ours.data_calls} on metadata buckets"
            )
    shown = "\n".join(problems[:10])
    assert not problems, f"{len(problems)} of {len(cases)} cases:\n{shown}"
